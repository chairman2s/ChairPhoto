//! Catalog lifecycle commands: open / create / switch, the recent-catalogs registry,
//! the library root, rescan, the enrichment-queue drain, and VACUUM.
//!
//! Switching is a safe teardown → reinit: the outgoing catalog's scan generation is
//! tripped first so no in-flight Phase B can write to a torn-down catalog.

use super::*;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Emitter, Manager, State};

/// Switch the active catalog with a safe teardown → reinit lifecycle (I4b).
///
/// 1. Signals every in-flight background job to abort (scan, faces, sharpness, pHash and
///    Smart Tagging) so they stop writing before the old catalog is torn down — nothing
///    lands in a catalog that's about to be closed.
/// 2. Closes the active catalog: drops the mutex-held handle (flushing the WAL) so the
///    old connection is released before the new one is opened.
/// 3. Opens (or, when `create` is set, creates) the catalog at `catalog_path` rooted at
///    `root`, records it in the recent-catalogs registry, and drops stale volume health.
/// 4. Emits `catalog:switched` so the frontend resets all React state (selection, filters,
///    albums, scan progress) and refreshes against the new catalog.
///
/// The scan runs on its own secondary connection (see `run_blocking_scan`), so the shared
/// `catalog` mutex is free during a scan and the swap here does not block on it; setting
/// `scan_abort` first is what guarantees the worker stops touching the DB promptly.
#[tauri::command]
pub async fn switch_catalog(
    app: AppHandle,
    state: State<'_, AppState>,
    catalog_path: String,
    root: String,
    create: bool,
    name: Option<String>,
) -> Result<(), String> {
    let catalog_path_buf = PathBuf::from(&catalog_path);
    let root_buf = PathBuf::from(&root);

    // Existence checks mirror open_catalog / create_catalog so the error messages match.
    // They run before anything is mutated: previously they sat after the abort flags were
    // tripped, so a switch rejected here had already stopped every switch-managed job.
    if create {
        if catalog_path_buf.exists() {
            return Err(format!(
                "Catalog file already exists: {}",
                catalog_path_buf.display()
            ));
        }
    } else if !catalog_path_buf.exists() {
        return Err(format!(
            "Catalog file does not exist: {}",
            catalog_path_buf.display()
        ));
    }

    // 1 + 2. Detach the outgoing catalog: trip every switch-managed job generation and drop
    //        the catalog handle in ONE transition. See `detach_catalog_and_trip_jobs`.
    detach_catalog_and_trip_jobs(state.inner())?;

    // 3. Open (creating first if requested) the new catalog off the async executor.
    let catalog = tauri::async_runtime::spawn_blocking({
        let path = catalog_path_buf.clone();
        let root = root_buf.clone();
        move || {
            if create {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
            }
            Catalog::open(&path, &root).map_err(|e| e.to_string())
        }
    })
    .await
    .map_err(|e| e.to_string())??;

    // The name for the registry: caller-supplied, else inferred from the filename.
    let catalog_name = name.unwrap_or_else(|| {
        catalog_path_buf
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("catalog")
            .to_string()
    });
    let actual_root = catalog.root().to_path_buf();

    // 4. Publish the new catalog and its fresh un-tripped flags in ONE transition. See
    //    `publish_catalog_and_reset_jobs`; the returned flag is this catalog's new scan
    //    generation, which the auto-resume below hands to its detached Phase B.
    let fresh_abort = publish_catalog_and_reset_jobs(state.inner(), catalog)?;
    // A different catalog may have entirely different volumes — drop stale reachability.
    state.volume_health.invalidate();

    // Record in recent catalogs (non-fatal if it fails). Off the async executor.
    let _ = tauri::async_runtime::spawn_blocking({
        let name = catalog_name.clone();
        let path = catalog_path_buf.clone();
        let root = actual_root.clone();
        move || record_recent_catalog(&name, &path, &root)
    })
    .await;

    // 5. Tell the frontend to reset and refresh against the new catalog.
    let _ = app.emit("catalog:switched", &catalog_path);

    // I6d: auto-resume Phase B if the new catalog has pending enrichment rows (crash/quit
    // mid-scan in a prior session). Only start the detached worker (and burn an
    // abort-flag generation) if there is actually something to do — avoids a wasted
    // secondary connection on every catalog switch against a fully-enriched catalog.
    let pending_count = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let c = guard.as_ref().ok_or("No catalog is open")?;
        c.pending_enrichment_count().map_err(|e| e.to_string())?
    };
    if pending_count > 0 {
        spawn_detached_phase_b(app, catalog_path_buf, actual_root, fresh_abort);
    }

    Ok(())
}

/// Phase one of a catalog switch: trip the scan, faces, sharpness, pHash and Smart Tagging
/// generations AND drop the catalog handle in ONE transition, holding the catalog lock
/// across all of them.
///
/// Tripping and releasing separately is not enough for the Faces and Smart Tagging starts,
/// which take catalog -> abort -> slot. If the trip happened outside the catalog lock such a
/// start could install a fresh un-tripped generation *after* the only abort signal and then
/// index a catalog this function is about to close — and phase two's replacement would
/// overwrite that generation without tripping it, leaving the worker unreachable by Cancel,
/// by a later start, and by the next switch.
///
/// Holding the catalog lock across the trip gives those starts only two outcomes: one
/// completes first and is tripped here, or it blocks and then finds no catalog open.
/// Dropping the handle under the same lock also flushes the WAL before the new connection
/// opens, and leaves no stale handle if the open fails.
///
/// The scan, sharpness and pHash starts do the opposite — they install their generation and
/// release that lock *before* reading the catalog, so this phase cannot fence them. Phase
/// two covers them instead, by tripping whatever it finds installed before replacing it.
///
/// Every guard is acquired before anything is stored, so a poisoned mutex fails the whole
/// phase instead of leaving some generations tripped and others live.
///
/// Nested acquisition is always catalog -> abort -> slot, here and in every job start that
/// takes more than one, so this cannot invert. Both switch phases are also the only places
/// that hold two abort locks at once, and they take them in the same order (scan, faces,
/// sharpness, pHash, Smart Tagging).
///
/// Extracted from `switch_catalog` so the ownership transition can be driven directly by
/// the interleaving tests in `commands::smarttags`, which have no Tauri `AppHandle`.
pub(super) fn detach_catalog_and_trip_jobs(state: &AppState) -> Result<(), String> {
    let mut cat_guard = state.catalog.lock().map_err(|e| e.to_string())?;
    let scan_guard = state.scan_abort.lock().map_err(|e| e.to_string())?;
    #[cfg(feature = "faces")]
    let faces_guard = state.faces_abort.lock().map_err(|e| e.to_string())?;
    let sharpness_guard = state.sharpness_abort.lock().map_err(|e| e.to_string())?;
    let phash_guard = state.phash_abort.lock().map_err(|e| e.to_string())?;
    #[cfg(feature = "smarttags")]
    let smarttags_guard = state.smarttags_abort.lock().map_err(|e| e.to_string())?;

    scan_guard.store(true, Ordering::Relaxed);
    #[cfg(feature = "faces")]
    faces_guard.store(true, Ordering::Relaxed);
    sharpness_guard.store(true, Ordering::Relaxed);
    phash_guard.store(true, Ordering::Relaxed);
    #[cfg(feature = "smarttags")]
    smarttags_guard.store(true, Ordering::Relaxed);
    *cat_guard = None;
    Ok(())
}

/// Phase two of a catalog switch: publish `catalog` and its fresh un-tripped flags in ONE
/// transition, again holding the catalog lock across both. Returns the new scan generation
/// (the caller hands it to an auto-resumed Phase B).
///
/// Installing the flags after releasing the catalog lock would let a start that snapshotted
/// the new catalog install its own live generation first, only for the assignments here to
/// overwrite it without tripping it — leaving a worker running that nothing can reach.
///
/// Whatever is installed at this point is tripped before being replaced. Faces and Smart
/// Tagging take the catalog lock before claiming, so neither can have installed anything
/// while this function held it — but scan, sharpness and pHash install their generation
/// *before* reading the catalog. One of those can therefore hold a live, un-tripped
/// generation right now, blocked on the catalog read. Replacing it silently would leave its
/// worker running with a handle no Cancel, later start or subsequent switch can reach.
/// Tripping first means such a start loses this race with an already-aborted flag and stops
/// immediately; a start that arrives after the replacement becomes the current generation
/// and proceeds normally.
///
/// The flags tripped in phase one are deliberately not cleared: the old workers still hold
/// those Arcs and must stay aborted. Swapping in new ones lets future jobs start clean
/// against the new catalog. The Faces and Smart Tagging *status slots* are likewise left
/// alone — each aborted worker clears its own slot on the way out, and only if it still owns
/// it, so clearing here would race a job that has already been superseded by a newer start.
///
/// As in phase one, every guard is acquired before anything is written, so a poisoned mutex
/// cannot leave a prefix of the generations replaced.
pub(super) fn publish_catalog_and_reset_jobs(
    state: &AppState,
    catalog: Catalog,
) -> Result<Arc<AtomicBool>, String> {
    let fresh_abort = Arc::new(AtomicBool::new(false));

    let mut cat_guard = state.catalog.lock().map_err(|e| e.to_string())?;
    let mut scan_guard = state.scan_abort.lock().map_err(|e| e.to_string())?;
    #[cfg(feature = "faces")]
    let mut faces_guard = state.faces_abort.lock().map_err(|e| e.to_string())?;
    let mut sharpness_guard = state.sharpness_abort.lock().map_err(|e| e.to_string())?;
    let mut phash_guard = state.phash_abort.lock().map_err(|e| e.to_string())?;
    #[cfg(feature = "smarttags")]
    let mut smarttags_guard = state.smarttags_abort.lock().map_err(|e| e.to_string())?;

    scan_guard.store(true, Ordering::Relaxed);
    #[cfg(feature = "faces")]
    faces_guard.store(true, Ordering::Relaxed);
    sharpness_guard.store(true, Ordering::Relaxed);
    phash_guard.store(true, Ordering::Relaxed);
    #[cfg(feature = "smarttags")]
    smarttags_guard.store(true, Ordering::Relaxed);

    *scan_guard = fresh_abort.clone();
    #[cfg(feature = "faces")]
    {
        *faces_guard = Arc::new(AtomicBool::new(false));
    }
    *sharpness_guard = Arc::new(AtomicBool::new(false));
    *phash_guard = Arc::new(AtomicBool::new(false));
    #[cfg(feature = "smarttags")]
    {
        *smarttags_guard = Arc::new(AtomicBool::new(false));
    }
    *cat_guard = Some(catalog);

    Ok(fresh_abort)
}

/// List recently-accessed catalogs, ordered by last-opened (most recent first).
#[tauri::command]
pub async fn list_recent_catalogs() -> Result<Vec<RecentCatalog>, String> {
    tauri::async_runtime::spawn_blocking(|| load_recent_catalogs())
        .await
        .map_err(|e| e.to_string())?
}

/// Open the default catalog: stored under the user data dir, rooted at $HOME so
/// any folder under home can be scanned and stored as a relative path. This is a
/// convenience for the current single-catalog UI; multi-catalog support comes later.
/// Path of the default catalog database file (separate from the photo library root).
fn default_catalog_path() -> Result<PathBuf, String> {
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
fn load_recent_catalogs() -> Result<Vec<RecentCatalog>, String> {
    let app_data = app_data_dir()?;
    std::fs::create_dir_all(&app_data).map_err(|e| e.to_string())?;
    let path = app_data.join("recent_catalogs.json");

    if !path.exists() {
        return Ok(Vec::new());
    }

    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let catalogs = serde_json::from_str(&content).map_err(|e| e.to_string())?;
    Ok(catalogs)
}

/// Save the recent catalogs list to `app_data_dir()/recent_catalogs.json`.
fn save_recent_catalogs(catalogs: &[RecentCatalog]) -> Result<(), String> {
    let app_data = app_data_dir()?;
    std::fs::create_dir_all(&app_data).map_err(|e| e.to_string())?;
    let path = app_data.join("recent_catalogs.json");

    let json = serde_json::to_string_pretty(catalogs).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())?;
    Ok(())
}

/// Add a catalog to the recent catalogs list (or update its timestamp if already present).
/// Keeps only the 20 most recent.
fn record_recent_catalog(name: &str, catalog_path: &Path, root: &Path) -> Result<(), String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs() as i64;

    let mut catalogs = load_recent_catalogs()?;
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

    save_recent_catalogs(&catalogs)?;
    Ok(())
}

#[tauri::command]
pub async fn init_catalog(app: AppHandle, state: State<'_, AppState>) -> Result<String, String> {
    let catalog_path = default_catalog_path()?;
    // Default library root for a *fresh* catalog (an existing catalog keeps its stored
    // root; change it via set_library_root). The catalog DB lives elsewhere.
    let default_root = expand_home("~/Pictures/Raw");

    // Create the default root directory off the UI thread.
    let _ = tauri::async_runtime::spawn_blocking({
        let root = default_root.clone();
        move || std::fs::create_dir_all(&root).ok()
    })
    .await;

    let catalog = tauri::async_runtime::spawn_blocking({
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

    // Record in recent catalogs (non-fatal if it fails). Wrap in spawn_blocking to avoid
    // blocking the async executor on file I/O.
    let _ = tauri::async_runtime::spawn_blocking({
        let path = catalog_path.clone();
        let root = actual_root.clone();
        move || record_recent_catalog("default", &path, &root)
    })
    .await;

    // I6d: auto-resume Phase B if a previous run left pending enrichment rows (crash/quit
    // mid-scan). Only start the detached worker (and burn an abort-flag generation) if
    // there is actually something to do — avoids a wasted secondary connection on every
    // cold start against a freshly-created or already-fully-enriched catalog.
    let catalog_path_str = catalog_path.to_string_lossy().to_string();
    let pending_count = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let c = guard.as_ref().ok_or("No catalog is open")?;
        c.pending_enrichment_count().map_err(|e| e.to_string())?
    };
    if pending_count > 0 {
        let abort = begin_scan_generation(&state)?;
        spawn_detached_phase_b(app, catalog_path, actual_root, abort);
    }

    Ok(catalog_path_str)
}

/// The current library root (the catalog root = local volume base).
#[tauri::command]
pub fn get_library_root(state: State<'_, AppState>) -> Result<String, String> {
    with_catalog(&state, |c| Ok(c.root().to_string_lossy().to_string()))
}

/// Re-root the catalog at `path` (the library folder). Photos are stored relative to
/// the root, so this is a "point at my library" action — existing entries won't resolve
/// until re-scanned. Reopens the default catalog rooted there. `~` is expanded.
#[tauri::command]
pub async fn set_library_root(state: State<'_, AppState>, path: String) -> Result<(), String> {
    let new_root = expand_home(&path);
    let catalog_path = default_catalog_path()?;
    let catalog = state.catalog.clone();
    // mkdir + reopen (which runs migrations) are far too slow for the main thread, so the
    // whole critical section moves to a blocking worker. The lock is still held across
    // persist → reopen → swap inside that worker, so no concurrent command can observe the
    // catalog mid-reroot.
    tauri::async_runtime::spawn_blocking(move || {
        std::fs::create_dir_all(&new_root).map_err(|e| e.to_string())?;
        let mut guard = catalog.lock().map_err(|e| e.to_string())?;
        {
            let open = guard.as_ref().ok_or("No catalog is open")?;
            open.set_setting("catalog_root", &new_root.to_string_lossy())
                .map_err(|e| e.to_string())?;
        }
        let reopened = Catalog::open(&catalog_path, &new_root).map_err(|e| e.to_string())?;
        *guard = Some(reopened);
        Ok::<(), String>(())
    })
    .await
    .map_err(|e| e.to_string())??;
    // The catalog-root volume's base path just moved — drop cached reachability.
    state.volume_health.invalidate();
    Ok(())
}

/// Rescan the whole library (the catalog root) in place, on a blocking worker thread.
#[tauri::command]
pub async fn rescan_library(app: AppHandle) -> Result<ScanResult, String> {
    run_blocking_two_phase_scan(app, |c, abort, progress| {
        // Capture the root in the same lock as the scan (no re-root window).
        let root = c.root().to_path_buf();
        crate::scanner::scan_folder_phase_a(c, &root, abort, progress)
    })
    .await
}

/// Drain the pending-enrichment queue (I6d) without re-walking the library. Enriches
/// only the photos already in the queue (those whose Phase B was interrupted by a
/// crash, quit, or abort). Returns the count of photos that were (or still are) in the
/// queue. Use this when the user wants to pick up where enrichment left off without
/// triggering a full folder walk. A full rescan will also process these photos as part
/// of its own Phase B.
///
/// The worker runs detached (like the startup auto-resume), so this command returns
/// immediately and `scan:progress` events drive the UI indicator.
#[tauri::command]
pub async fn drain_enrichment_queue(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<usize, String> {
    let (path, root, count) = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let c = guard.as_ref().ok_or("No catalog is open")?;
        let count = c.pending_enrichment_count().map_err(|e| e.to_string())?;
        (c.db_path().to_path_buf(), c.root().to_path_buf(), count)
    };
    if count > 0 {
        // A fresh abort flag: trips any previous Phase B (startup resume or earlier drain),
        // and becomes the handle for this drain — a subsequent scan or catalog switch can
        // stop it.
        let abort = begin_scan_generation(&state)?;
        spawn_detached_phase_b(app, path, root, abort);
    }
    Ok(count)
}

/// Before/after on-disk catalog size for a VACUUM.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VacuumResult {
    pub before_bytes: i64,
    pub after_bytes: i64,
}

/// Compact the catalog (SQLite VACUUM): reclaim space from deleted rows + defragment.
/// Runs on a blocking worker thread (it holds the catalog connection for its duration, so
/// the window stays responsive). Returns the size before and after.
#[tauri::command]
pub async fn vacuum_catalog(app: AppHandle) -> Result<VacuumResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_ref().ok_or("No catalog is open")?;
        let before = catalog.db_size_bytes().map_err(|e| e.to_string())?;
        catalog.vacuum().map_err(|e| e.to_string())?;
        let after = catalog.db_size_bytes().map_err(|e| e.to_string())?;
        Ok(VacuumResult { before_bytes: before, after_bytes: after })
    })
    .await
    .map_err(|e| e.to_string())?
}

// ---------------------------------------------------------------------------
// I4 — Multi-catalog: unit tests for the recent-catalog registry helpers.
// These tests run inside `commands` to access the private helpers directly.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod catalog_registry_tests {
    use super::*;
    // Use the shared EnvGuard / ENV_LOCK from test_env_helpers so that env-var mutations
    // from this module and external_module_tests are serialized by one process-wide Mutex.
    use super::test_env_helpers::EnvGuard;

    /// Create a unique temp dir for a test's `XDG_DATA_HOME` override.
    fn temp_xdg(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("chairphoto-test-registry-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
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

