//! Which catalog is open: the default catalog's location, the recent-catalogs registry, and
//! opening the default catalog at startup — shared by every front end.
//!
//! [`open_default_catalog`] is what the Tauri `init_catalog` command runs and what the GPUI
//! app calls after [`boot`](super::boot). It is not a catalog *switch*: nothing can be open
//! yet, so it publishes the handle directly and emits no `catalog:switched`; a front end
//! reads the result. A switch is [`switch_catalog`], whose two ownership phases
//! ([`detach_catalog_and_trip_jobs`], [`publish_catalog_and_reset_jobs`]) `set_library_root`
//! runs too.

use super::{app_data_dir, begin_scan_generation, expand_home, spawn_blocking, AppState};
use super::events::{CoreEvent, EventSink};
use crate::catalog::Catalog;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// Open the default catalog (`<app data>/default.chairphoto`) and make it the open one.
///
/// A fresh catalog is rooted at `~/Pictures/Raw` (created if missing); an existing catalog
/// keeps its stored root (`Catalog::open` adopts it). Records the catalog as the most recent
/// ("default"), then auto-resumes enrichment Phase B (I6d) if a previous run left pending rows
/// — that worker reports through `state`'s event sink as `scan:progress`. Every blocking step
/// runs on the blocking pool; call it from an async task, never the UI thread. Returns the
/// catalog's database path.
pub async fn open_default_catalog(state: &AppState) -> Result<PathBuf, String> {
    let catalog_path = default_catalog_path()?;
    // Default library root for a *fresh* catalog (an existing catalog keeps its stored
    // root; change it via set_library_root). The catalog DB lives elsewhere.
    let default_root = expand_home("~/Pictures/Raw");

    // Create the default root directory off the UI thread.
    let _ = spawn_blocking({
        let root = default_root.clone();
        move || std::fs::create_dir_all(&root).ok()
    })
    .await;

    let catalog = spawn_blocking({
        let path = catalog_path.clone();
        let root = default_root.clone();
        move || Catalog::open(&path, &root)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;

    // Use the catalog's actual root (which may differ from default_root if the catalog
    // was previously opened and had its root changed). Clone it before moving the catalog.
    let actual_root = catalog.root().to_path_buf();

    *state.catalog.lock().map_err(|e| e.to_string())? = Some(catalog);

    // Record in recent catalogs (non-fatal if it fails), off the async executor.
    let _ = spawn_blocking({
        let path = catalog_path.clone();
        let root = actual_root.clone();
        move || record_recent_catalog("default", &path, &root)
    })
    .await;

    // I6d: auto-resume Phase B if a previous run left pending enrichment rows (crash/quit
    // mid-scan). Only start the detached worker (and burn an abort-flag generation) if
    // there is actually something to do — avoids a wasted secondary connection on every
    // cold start against a freshly-created or already-fully-enriched catalog.
    let pending_count = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let c = guard.as_ref().ok_or("No catalog is open")?;
        c.pending_enrichment_count().map_err(|e| e.to_string())?
    };
    if pending_count > 0 {
        let abort = begin_scan_generation(state)?;
        spawn_detached_phase_b(state.clone(), catalog_path.clone(), actual_root, abort);
    }

    Ok(catalog_path)
}

/// Detach a Phase B enrichment worker for the given catalog `path`/`root` on the core
/// runtime's blocking pool: [`EnrichJob::resume`], run there. Used by the auto-resume path on
/// startup (I6d) and `drain_enrichment_queue`. Does nothing (and emits no events) if the
/// queue is empty.
pub fn spawn_detached_phase_b(
    events: AppState,
    path: PathBuf,
    root: PathBuf,
    abort: Arc<AtomicBool>,
) {
    let job = EnrichJob::resume(events, path, root, abort);
    crate::app::spawn_blocking(move || job.run());
}

/// A Phase B enrichment pass (I6), ready to run: EXIF/IPTC/XMP extraction and finalizing on
/// its own secondary connection, under one scan generation's abort flag. It streams
/// `scan:progress {phase:"metadata"|"finalizing"}` and ends with the terminal
/// `scan:progress {phase:"done"}` whether it finishes, fails or is aborted.
///
/// A value rather than a spawned thread so each front end runs it on its own worker: the
/// Tauri shell on the core runtime's blocking pool, the GPUI app on its job runner.
/// [`run`](Self::run) blocks for the whole pass — never call it on a UI thread.
pub struct EnrichJob {
    events: AppState,
    path: PathBuf,
    root: PathBuf,
    abort: Arc<AtomicBool>,
    /// `None`: resume whatever the persistent queue holds (startup auto-resume, a drain, a
    /// switch to a catalog with pending rows). `Some`: a scan's Phase A hand-off.
    pending: Option<crate::scanner::PendingEnrich>,
}

impl EnrichJob {
    /// Resume the catalog's persistent pending-enrichment queue. An empty queue emits
    /// nothing, not even `done`.
    pub fn resume(events: AppState, path: PathBuf, root: PathBuf, abort: Arc<AtomicBool>) -> Self {
        EnrichJob { events, path, root, abort, pending: None }
    }

    /// Enrich what a scan's Phase A handed over.
    pub fn after_scan(
        events: AppState,
        path: PathBuf,
        root: PathBuf,
        abort: Arc<AtomicBool>,
        pending: crate::scanner::PendingEnrich,
    ) -> Self {
        EnrichJob { events, path, root, abort, pending: Some(pending) }
    }

    /// Run the pass to its end. Blocking.
    pub fn run(self) {
        let EnrichJob { events, path, root, abort, pending } = self;
        let enrich_catalog = match Catalog::open_secondary(&path, &root) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("phase B: couldn't open enrichment connection: {e}");
                send_scan_done(&events);
                return;
            }
        };
        let pending = match pending {
            Some(p) => p,
            None => match crate::scanner::resume_pending_enrichment(&enrich_catalog) {
                Ok(Some(p)) => p,
                Ok(None) => return, // queue is empty — nothing to do, no events emitted
                Err(e) => {
                    eprintln!("resume phase B: couldn't load enrichment queue: {e}");
                    send_scan_done(&events);
                    return;
                }
            },
        };
        let emit = {
            let events = events.clone();
            move |p: crate::scanner::ScanProgress| events.send(CoreEvent::ScanProgress(p))
        };
        if let Err(e) = crate::scanner::phase_b_enrich(&enrich_catalog, pending, &abort, &emit) {
            // SCAN_ABORTED is a clean stop (catalog switch / second scan); anything else is
            // a real failure. Either way, the terminal event below clears the indicator.
            if e != crate::scanner::SCAN_ABORTED {
                eprintln!("phase B: enrichment failed: {e}");
            }
        }
        send_scan_done(&events);
    }
}

/// The scan's terminal `scan:progress {phase:"done"}`.
pub(crate) fn send_scan_done(events: &AppState) {
    events.send(CoreEvent::ScanProgress(crate::scanner::ScanProgress { phase: "done".into(), done: 0, total: 0 }));
}

// ── Catalog switching ────────────────────────────────────────────────────────

/// Switch the active catalog with a safe teardown → reinit lifecycle (I4b). The body of the
/// Tauri `switch_catalog` command and of the GPUI catalog switcher. **Blocking** (it opens
/// and may migrate a SQLite file): run it on a worker, never the UI thread.
///
/// 1. Checks the target exists (or, with `create`, does not) before anything is mutated, so
///    a rejected switch leaves every job running.
/// 2. Detaches the outgoing catalog: trips every job generation, clears every status slot
///    and drops the handle (flushing the WAL) as one transition —
///    [`detach_catalog_and_trip_jobs`].
/// 3. Opens (or creates, with its folders) the new catalog.
/// 4. Publishes it with fresh un-tripped generations ([`publish_catalog_and_reset_jobs`]),
///    drops stale volume health and records it in the recent-catalogs registry.
/// 5. Emits `catalog:switched`, so every front end resets and re-reads.
///
/// Returns the Phase B auto-resume (I6d) when the new catalog has pending enrichment rows;
/// the caller runs it on a worker. A failed open after step 2 leaves **no** catalog open —
/// the old handle is gone by then, as it always was in the Tauri command.
pub fn switch_catalog(
    state: &AppState,
    catalog_path: &Path,
    root: &Path,
    create: bool,
    name: Option<String>,
) -> Result<Option<EnrichJob>, String> {
    switch_catalog_in(state, None, catalog_path, root, create, name)
}

/// [`switch_catalog`], recording into the recent-catalogs registry in `registry_dir`
/// (`None`: the app data dir).
pub fn switch_catalog_in(
    state: &AppState,
    registry_dir: Option<&Path>,
    catalog_path: &Path,
    root: &Path,
    create: bool,
    name: Option<String>,
) -> Result<Option<EnrichJob>, String> {
    if create {
        if catalog_path.exists() {
            return Err(format!("Catalog file already exists: {}", catalog_path.display()));
        }
    } else if !catalog_path.exists() {
        return Err(format!("Catalog file does not exist: {}", catalog_path.display()));
    }

    detach_catalog_and_trip_jobs(state)?;

    if create {
        if let Some(parent) = catalog_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::create_dir_all(root).map_err(|e| e.to_string())?;
    }
    let catalog = Catalog::open(catalog_path, root).map_err(|e| e.to_string())?;

    // The name for the registry: caller-supplied, else inferred from the filename.
    let catalog_name = name.unwrap_or_else(|| {
        catalog_path.file_stem().and_then(|s| s.to_str()).unwrap_or("catalog").to_string()
    });
    let actual_root = catalog.root().to_path_buf();

    let fresh_abort = publish_catalog_and_reset_jobs(state, catalog)?;
    // A different catalog may have entirely different volumes — drop stale reachability.
    state.volume_health.invalidate();
    // Non-fatal if it fails.
    let _ = match registry_dir {
        Some(dir) => record_recent_catalog_in(dir, &catalog_name, catalog_path, &actual_root),
        None => record_recent_catalog(&catalog_name, catalog_path, &actual_root),
    };

    state.send(CoreEvent::CatalogSwitched(catalog_path.to_string_lossy().to_string()));

    // Only start the resume (and burn a generation) when there is something to do.
    let pending_count = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let c = guard.as_ref().ok_or("No catalog is open")?;
        c.pending_enrichment_count().map_err(|e| e.to_string())?
    };
    Ok((pending_count > 0).then(|| {
        EnrichJob::resume(state.clone(), catalog_path.to_path_buf(), actual_root, fresh_abort)
    }))
}

/// Phase one of a catalog switch: trip every job generation, clear every status slot AND
/// drop the catalog handle in ONE transition, holding the catalog lock across all of them.
///
/// Tripping and releasing separately is not enough for the families that claim under the
/// catalog lock (Faces, Smart Tagging, identity repair), which take catalog -> abort -> slot.
/// If the trip happened outside the catalog lock such a start could install a fresh
/// un-tripped generation *after* the only abort signal and then work on a catalog this
/// function is about to close — and phase two's replacement would overwrite that generation
/// without tripping it, leaving the worker unreachable by Cancel, by a later start, and by
/// the next switch. Holding the catalog lock gives those starts two outcomes: one completes
/// first and is tripped here, or it blocks and then finds no catalog open. Dropping the handle
/// under the same lock also flushes the WAL before the new connection opens, and leaves no
/// stale handle if the open fails.
///
/// The scan, sharpness, pHash and import starts install their generation and release that
/// lock *before* reading the catalog, so this phase cannot fence them; phase two covers them
/// by tripping whatever it finds installed before replacing it.
///
/// Every guard is acquired before anything is stored, so a poisoned mutex fails the whole
/// phase instead of leaving some generations tripped and others live. Nested acquisition is
/// always catalog -> abort -> slot (the order `jobs` documents).
pub fn detach_catalog_and_trip_jobs(state: &AppState) -> Result<(), String> {
    detach_catalog_and_trip_jobs_with(state, |_| Ok(()))
}

/// As [`detach_catalog_and_trip_jobs`], but runs `before_drop` against the outgoing catalog
/// while this phase still holds its lock, immediately before the handle is dropped.
///
/// `set_library_root` needs exactly this: it has to persist `catalog_root` **through the
/// still-open handle** — `Catalog::open` adopts the stored setting over the `root` argument —
/// so persist, trip and drop stay one transition (issue #22).
///
/// `before_drop` runs after every guard is acquired but before the first mutation, so a
/// failing callback aborts the whole phase with nothing tripped and the catalog still open.
/// It must not take any `AppState` lock: this holds the catalog lock, every abort lock and
/// every status-slot lock, so anything reaching back for one deadlocks.
pub fn detach_catalog_and_trip_jobs_with(
    state: &AppState,
    before_drop: impl FnOnce(Option<&Catalog>) -> Result<(), String>,
) -> Result<(), String> {
    let mut cat_guard = state.catalog.lock().map_err(|e| e.to_string())?;
    // Every abort generation AND every status slot, locked and unmutated. The status slots
    // matter as much as the flags: tripping alone leaves the old job reachable as a slot's
    // owner, so a remounting panel would adopt a job belonging to the catalog the user has
    // left (issue #51). `JobRegistry` enumerates both groups exhaustively.
    let job_guards = state.jobs.lock_for_detach()?;

    // `Option`, not `&Catalog`: this phase tolerates running with no catalog open (it ends
    // with an unconditional `*cat_guard = None`), and `switch_catalog` relies on that.
    before_drop(cat_guard.as_ref())?;

    job_guards.trip_and_clear_all();
    *cat_guard = None;
    Ok(())
}

/// Phase two of a catalog switch: publish `catalog` and its fresh un-tripped generations in
/// ONE transition, holding the catalog lock across both. Returns the new scan generation
/// (the caller hands it to an auto-resumed Phase B).
///
/// Whatever is installed at this point is tripped before being replaced: the scan,
/// sharpness, pHash and import starts install their generation *before* reading the catalog,
/// so one of those can hold a live generation right now, blocked on the catalog read.
/// Replacing it silently would leave its worker running with a handle nothing can reach.
/// The status slots are left alone — each aborted worker clears its own on the way out, and
/// only if it still owns it.
pub fn publish_catalog_and_reset_jobs(state: &AppState, catalog: Catalog) -> Result<Arc<AtomicBool>, String> {
    let mut cat_guard = state.catalog.lock().map_err(|e| e.to_string())?;
    let job_guards = state.jobs.lock_for_publish()?;
    let fresh_abort = job_guards.trip_and_replace_all();
    *cat_guard = Some(catalog);
    Ok(fresh_abort)
}

/// Re-root the catalog at `new_root` (the library folder; `set_library_root`): persist the
/// root through the outgoing handle, trip every job, reopen the catalog at `catalog_path`
/// rooted there and publish it. Photos are stored relative to the root, so existing entries
/// won't resolve until re-scanned. **Blocking** (a directory create, a SQLite open with
/// migrations): run it on the blocking pool, never the UI thread.
///
/// This replaces the active catalog handle, so it runs the **same two-phase ownership
/// transition as [`switch_catalog`]** rather than a hand-rolled swap (issue #22). It used to
/// hold the catalog lock across persist → reopen → swap and trip nothing, which left a scan,
/// face index, face match, Smart Tagging index, sharpness or pHash job running against a
/// catalog the app had replaced — the invariant in AGENTS.md ("a newer job start or catalog
/// switch must make older workers abortable and unreachable as owners") applied to a function
/// nobody had connected to it.
///
/// The one thing it does that a switch does not: `catalog_root` must be written **through the
/// outgoing handle**, before the reopen, because `Catalog::open` adopts the stored setting
/// over the root argument it is passed. That write rides inside phase one via
/// [`detach_catalog_and_trip_jobs_with`], so persist, trip and drop stay one transition.
///
/// Emits no `catalog:switched` (the Tauri command never did); the caller refreshes what it
/// shows.
pub fn reroot_library(state: &AppState, new_root: PathBuf, catalog_path: &Path) -> Result<(), String> {
    // Before anything is tripped, so a bad path fails with every job still running.
    std::fs::create_dir_all(&new_root).map_err(|e| e.to_string())?;

    // Phase one: persist the new root through the outgoing handle, trip every job generation,
    // clear every status slot, drop the handle — one transition under the catalog lock. A
    // failed persist leaves the catalog open and nothing tripped.
    detach_catalog_and_trip_jobs_with(state, |catalog| {
        catalog
            .ok_or("No catalog is open")?
            .set_setting("catalog_root", &new_root.to_string_lossy())
            .map_err(|e| e.to_string())
    })?;

    // Runs migrations.
    let reopened = Catalog::open(catalog_path, &new_root).map_err(|e| e.to_string())?;

    // Phase two: publish it with fresh un-tripped generations, tripping whatever a racing
    // start installed while the catalog was `None` so it cannot survive unreachable.
    publish_catalog_and_reset_jobs(state, reopened)?;
    // The catalog-root volume's base path just moved — drop cached reachability.
    state.volume_health.invalidate();
    Ok(())
}

/// Before/after on-disk catalog size for a VACUUM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VacuumResult {
    pub before_bytes: i64,
    pub after_bytes: i64,
}

/// Compact the open catalog (SQLite VACUUM): reclaim space from deleted rows, defragment, and
/// shed retired columns. Holds the catalog lock for its duration — **blocking**, run it on the
/// blocking pool. Returns the size before and after.
pub fn vacuum_catalog(state: &AppState) -> Result<VacuumResult, String> {
    // `&mut`: compaction also sheds retired columns, which changes the connection's own view
    // of the table shape (see `Catalog::vacuum`).
    let mut guard = state.catalog.lock().map_err(|e| e.to_string())?;
    let catalog = guard.as_mut().ok_or("No catalog is open")?;
    let before = catalog.db_size_bytes().map_err(|e| e.to_string())?;
    catalog.vacuum().map_err(|e| e.to_string())?;
    let after = catalog.db_size_bytes().map_err(|e| e.to_string())?;
    Ok(VacuumResult { before_bytes: before, after_bytes: after })
}

/// Path of the default catalog database file (separate from the photo library root).
pub fn default_catalog_path() -> Result<PathBuf, String> {
    Ok(app_data_dir()?.join("default.chairphoto"))
}

/// A recently-accessed catalog entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentCatalog {
    /// The catalog's user-given name.
    pub name: String,
    /// The path to the `.chairphoto` database file.
    pub catalog_path: String,
    /// The library root (photo folder) this catalog is rooted at.
    pub root: String,
    /// Unix timestamp of the last time this catalog was opened.
    pub last_opened: i64,
}

/// Load the recent catalogs list from `app_data_dir()/recent_catalogs.json`.
pub fn load_recent_catalogs() -> Result<Vec<RecentCatalog>, String> {
    load_recent_catalogs_in(&app_data_dir()?)
}

/// [`load_recent_catalogs`] from the registry in `dir` — a front end's tests point this at a
/// scratch directory rather than the user's app data.
pub fn load_recent_catalogs_in(dir: &Path) -> Result<Vec<RecentCatalog>, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path = dir.join("recent_catalogs.json");

    if !path.exists() {
        return Ok(Vec::new());
    }

    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let catalogs = serde_json::from_str(&content).map_err(|e| e.to_string())?;
    Ok(catalogs)
}

/// Save the recent catalogs list to `dir/recent_catalogs.json`.
fn save_recent_catalogs_in(dir: &Path, catalogs: &[RecentCatalog]) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path = dir.join("recent_catalogs.json");

    let json = serde_json::to_string_pretty(catalogs).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())?;
    Ok(())
}

/// Add a catalog to the recent catalogs list (or update its timestamp if already present).
/// Keeps only the 20 most recent.
pub fn record_recent_catalog(name: &str, catalog_path: &Path, root: &Path) -> Result<(), String> {
    record_recent_catalog_in(&app_data_dir()?, name, catalog_path, root)
}

/// [`record_recent_catalog`] into the registry in `dir`.
pub fn record_recent_catalog_in(dir: &Path, name: &str, catalog_path: &Path, root: &Path) -> Result<(), String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs() as i64;

    let mut catalogs = load_recent_catalogs_in(dir)?;
    let catalog_path_str = catalog_path.to_string_lossy().to_string();
    let root_str = root.to_string_lossy().to_string();

    // Remove if already present so we can re-add with updated timestamp.
    catalogs.retain(|c| c.catalog_path != catalog_path_str);

    // Add to front.
    catalogs.insert(0, RecentCatalog {
        name: name.to_string(),
        catalog_path: catalog_path_str,
        root: root_str,
        last_opened: now,
    });

    // Keep only the 20 most recent.
    catalogs.truncate(20);

    save_recent_catalogs_in(dir, &catalogs)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// I4 — Multi-catalog: unit tests for the recent-catalog registry helpers.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod catalog_registry_tests {
    use super::*;
    // Use the shared EnvGuard / ENV_LOCK from test_env_helpers so that env-var mutations
    // from this module and external_module_tests are serialized by one process-wide Mutex.
    use crate::app::test_env_helpers::EnvGuard;

    /// Create a unique temp dir for a test's `XDG_DATA_HOME` override.
    fn temp_xdg(tag: &str) -> crate::test_support::TestTmpDir {
        crate::test_support::TestTmpDir::new(&format!("registry-{tag}"))
    }

    // -----------------------------------------------------------------
    // record_recent_catalog: first entry is stored and read back.
    // -----------------------------------------------------------------
    #[test]
    fn record_then_list_round_trips() {
        let xdg = temp_xdg("round-trip");
        let _g = EnvGuard::set("XDG_DATA_HOME", xdg.to_str().unwrap());

        let cat = xdg.join("a.chairphoto");
        let root = xdg.join("photos");
        record_recent_catalog("My Catalog", &cat, &root).unwrap();

        let list = load_recent_catalogs().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "My Catalog");
        assert_eq!(list[0].catalog_path, cat.to_string_lossy());
        assert_eq!(list[0].root, root.to_string_lossy());
        assert!(list[0].last_opened > 0, "timestamp must be non-zero");
    }

    // -----------------------------------------------------------------
    // Recording the same catalog twice updates its timestamp and keeps
    // only one entry — the dedup rule.
    // -----------------------------------------------------------------
    #[test]
    fn re_recording_deduplicates_and_updates_timestamp() {
        let xdg = temp_xdg("dedup");
        let _g = EnvGuard::set("XDG_DATA_HOME", xdg.to_str().unwrap());

        let cat = xdg.join("dedup.chairphoto");
        let root = xdg.join("photos");
        record_recent_catalog("Catalog", &cat, &root).unwrap();
        let t1 = load_recent_catalogs().unwrap()[0].last_opened;

        // Small sleep so `now` differs from the first record.
        std::thread::sleep(std::time::Duration::from_millis(5));
        record_recent_catalog("Catalog", &cat, &root).unwrap();

        let list = load_recent_catalogs().unwrap();
        assert_eq!(list.len(), 1, "same path must not produce a duplicate entry");
        let t2 = list[0].last_opened;
        assert!(t2 >= t1, "re-record must not decrease the timestamp");
    }

    // -----------------------------------------------------------------
    // Multiple different catalogs are ordered most-recent-first.
    // -----------------------------------------------------------------
    #[test]
    fn multiple_catalogs_ordered_most_recent_first() {
        let xdg = temp_xdg("order");
        let _g = EnvGuard::set("XDG_DATA_HOME", xdg.to_str().unwrap());

        let root = xdg.join("photos");
        let cat_a = xdg.join("a.chairphoto");
        let cat_b = xdg.join("b.chairphoto");
        let cat_c = xdg.join("c.chairphoto");

        // Record A, then B, then C — C is the most-recently opened.
        record_recent_catalog("A", &cat_a, &root).unwrap();
        record_recent_catalog("B", &cat_b, &root).unwrap();
        record_recent_catalog("C", &cat_c, &root).unwrap();

        let list = load_recent_catalogs().unwrap();
        assert_eq!(list.len(), 3);
        // Most recent first.
        assert_eq!(list[0].name, "C");
        assert_eq!(list[1].name, "B");
        assert_eq!(list[2].name, "A");
    }

    // -----------------------------------------------------------------
    // Re-opening A after B makes A the most-recent entry.
    // -----------------------------------------------------------------
    #[test]
    fn re_opening_older_catalog_promotes_it_to_front() {
        let xdg = temp_xdg("promote");
        let _g = EnvGuard::set("XDG_DATA_HOME", xdg.to_str().unwrap());

        let root = xdg.join("photos");
        let cat_a = xdg.join("a.chairphoto");
        let cat_b = xdg.join("b.chairphoto");

        record_recent_catalog("A", &cat_a, &root).unwrap();
        record_recent_catalog("B", &cat_b, &root).unwrap();
        // Now reopen A — it must move to the front.
        record_recent_catalog("A", &cat_a, &root).unwrap();

        let list = load_recent_catalogs().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "A", "re-opened catalog must be first");
        assert_eq!(list[1].name, "B");
    }

    // -----------------------------------------------------------------
    // The list is capped at 20 entries; oldest entries are dropped.
    // -----------------------------------------------------------------
    #[test]
    fn list_is_capped_at_twenty() {
        let xdg = temp_xdg("cap");
        let _g = EnvGuard::set("XDG_DATA_HOME", xdg.to_str().unwrap());

        let root = xdg.join("photos");
        for i in 0u32..25 {
            let cat = xdg.join(format!("cat{i}.chairphoto"));
            record_recent_catalog(&format!("C{i}"), &cat, &root).unwrap();
        }

        let list = load_recent_catalogs().unwrap();
        assert_eq!(list.len(), 20, "list must be capped at 20");
        // The most recently recorded entry (C24) is first.
        assert_eq!(list[0].name, "C24");
        // The earliest entries (C0..C4) were dropped.
        assert!(!list.iter().any(|c| c.name == "C0"), "C0 must be evicted");
    }

    // -----------------------------------------------------------------
    // An absent `recent_catalogs.json` returns an empty list (fresh install).
    // -----------------------------------------------------------------
    #[test]
    fn missing_registry_file_returns_empty_list() {
        let xdg = temp_xdg("missing");
        let _g = EnvGuard::set("XDG_DATA_HOME", xdg.to_str().unwrap());
        // No file written — fresh install simulation.
        let list = load_recent_catalogs().unwrap();
        assert!(list.is_empty(), "no registry file → empty list");
    }
}

#[cfg(test)]
mod switch_tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Names(Mutex<Vec<String>>);
    impl EventSink for Names {
        fn send(&self, event: CoreEvent) {
            self.0.lock().unwrap().push(event.name().to_string());
        }
    }

    /// A rejected switch mutates nothing: the job generations stay live and the catalog open.
    #[test]
    fn a_switch_to_a_missing_catalog_leaves_everything_running() {
        let dir = crate::test_support::TestTmpDir::new("switch-missing");
        let state = AppState::default();
        *state.catalog.lock().unwrap() = Some(Catalog::open(&dir.join("a.chairphoto"), &dir.join("a")).unwrap());
        let import = state.jobs.import.install_fresh().unwrap();
        let err = switch_catalog_in(&state, Some(&dir.join("reg")), &dir.join("nope.chairphoto"), &dir, false, None)
            .err()
            .unwrap();
        assert!(err.starts_with("Catalog file does not exist"), "{err}");
        assert!(!import.load(Ordering::Relaxed));
        assert!(state.catalog.lock().unwrap().is_some());
    }

    /// Create: the new catalog is open, the old jobs are tripped, the registry in the given
    /// directory records it, and `catalog:switched` goes out.
    #[test]
    fn a_switch_that_creates_records_and_announces_it() {
        let dir = crate::test_support::TestTmpDir::new("switch-create");
        let state = AppState::default();
        let names = Arc::new(Names::default());
        state.set_events(names.clone());
        *state.catalog.lock().unwrap() = Some(Catalog::open(&dir.join("a.chairphoto"), &dir.join("a")).unwrap());
        let import = state.jobs.import.install_fresh().unwrap();
        let new_path = dir.join("b/B.chairphoto");
        let resume =
            switch_catalog_in(&state, Some(&dir.join("reg")), &new_path, &dir.join("b"), true, Some("B".into())).unwrap();
        assert!(resume.is_none(), "a fresh catalog has nothing to enrich");
        assert!(import.load(Ordering::Relaxed), "the old import was tripped");
        assert_eq!(state.catalog.lock().unwrap().as_ref().unwrap().db_path(), new_path.as_path());
        let recent = load_recent_catalogs_in(&dir.join("reg")).unwrap();
        assert_eq!((recent.len(), recent[0].name.as_str()), (1, "B"));
        assert_eq!(names.0.lock().unwrap().as_slice(), ["catalog:switched"]);
    }

    /// A re-root trips the running jobs, publishes the same catalog file rooted at the new
    /// folder (the root persisted through the outgoing handle, or `Catalog::open` would have
    /// adopted the old one), creates the folder, and announces nothing.
    #[test]
    fn a_reroot_trips_jobs_and_reopens_the_catalog_at_the_new_root() {
        let dir = crate::test_support::TestTmpDir::new("reroot");
        let state = AppState::default();
        let names = Arc::new(Names::default());
        state.set_events(names.clone());
        let db = dir.join("a.chairphoto");
        *state.catalog.lock().unwrap() = Some(Catalog::open(&db, &dir.join("old")).unwrap());
        let import = state.jobs.import.install_fresh().unwrap();
        let new_root = dir.join("new/root");
        reroot_library(&state, new_root.clone(), &db).unwrap();
        assert!(import.load(Ordering::Relaxed), "the running import was tripped");
        assert!(new_root.is_dir());
        let guard = state.catalog.lock().unwrap();
        let c = guard.as_ref().unwrap();
        assert_eq!((c.db_path(), c.root()), (db.as_path(), new_root.as_path()));
        assert!(names.0.lock().unwrap().is_empty(), "a re-root is not a switch");
    }

    /// With nothing open, a re-root fails before it trips anything.
    #[test]
    fn a_reroot_without_a_catalog_fails_and_trips_nothing() {
        let dir = crate::test_support::TestTmpDir::new("reroot-none");
        let state = AppState::default();
        let import = state.jobs.import.install_fresh().unwrap();
        let err = reroot_library(&state, dir.join("r"), &dir.join("a.chairphoto")).unwrap_err();
        assert_eq!(err, "No catalog is open");
        assert!(!import.load(Ordering::Relaxed));
    }

    /// VACUUM reports the file's size before and after, and needs an open catalog.
    #[test]
    fn vacuum_reports_the_size_before_and_after() {
        let dir = crate::test_support::TestTmpDir::new("vacuum");
        let state = AppState::default();
        assert_eq!(vacuum_catalog(&state).unwrap_err(), "No catalog is open");
        *state.catalog.lock().unwrap() = Some(Catalog::open(&dir.join("v.chairphoto"), &dir.join("v")).unwrap());
        let r = vacuum_catalog(&state).unwrap();
        assert!(r.before_bytes > 0 && r.after_bytes > 0, "{r:?}");
        assert!(r.after_bytes <= r.before_bytes, "{r:?}");
    }
}
