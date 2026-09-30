//! Which catalog is open: the default catalog's location, the recent-catalogs registry, and
//! opening the default catalog at startup — shared by every front end.
//!
//! [`open_default_catalog`] is what the Tauri `init_catalog` command runs and what the GPUI
//! app calls after [`boot`](super::boot). It is not a catalog *switch*: nothing can be open
//! yet, so it publishes the handle directly (the switch protocol lives with the
//! `switch_catalog` command). It emits no `catalog:switched`; a front end reads the result.

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

/// Detach a Phase B enrichment worker for the given catalog `path`/`root`, using the
/// supplied `abort` flag. The worker opens its own secondary connection, loads the
/// pending-enrichment queue, and calls `phase_b_enrich` — streaming
/// `scan:progress {phase:"metadata"|"finalizing"}` events and the terminal
/// `scan:progress {phase:"done"}` event when it finishes (or is aborted).
///
/// Used by the auto-resume path on startup (I6d) and `drain_enrichment_queue`.
/// Does nothing (and emits no events) if the queue is empty.
pub fn spawn_detached_phase_b(
    events: AppState,
    path: PathBuf,
    root: PathBuf,
    abort: Arc<AtomicBool>,
) {
    crate::app::spawn_blocking(move || {
        let enrich_catalog = match Catalog::open_secondary(&path, &root) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("resume phase B: couldn't open enrichment connection: {e}");
                events.send(CoreEvent::ScanProgress(crate::scanner::ScanProgress { phase: "done".into(), done: 0, total: 0 }));
                return;
            }
        };
        let pending = match crate::scanner::resume_pending_enrichment(&enrich_catalog) {
            Ok(Some(p)) => p,
            Ok(None) => return, // queue is empty — nothing to do, no events emitted
            Err(e) => {
                eprintln!("resume phase B: couldn't load enrichment queue: {e}");
                events.send(CoreEvent::ScanProgress(crate::scanner::ScanProgress { phase: "done".into(), done: 0, total: 0 }));
                return;
            }
        };
        let emit = {
            let events = events.clone();
            move |p: crate::scanner::ScanProgress| events.send(CoreEvent::ScanProgress(p))
        };
        if let Err(e) = crate::scanner::phase_b_enrich(&enrich_catalog, pending, &abort, &emit) {
            if e != crate::scanner::SCAN_ABORTED {
                eprintln!("resume phase B: enrichment failed: {e}");
            }
        }
        events.send(CoreEvent::ScanProgress(crate::scanner::ScanProgress { phase: "done".into(), done: 0, total: 0 }));
    });
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
pub fn record_recent_catalog(name: &str, catalog_path: &Path, root: &Path) -> Result<(), String> {
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
