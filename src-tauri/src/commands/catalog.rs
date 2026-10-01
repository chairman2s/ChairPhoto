//! Catalog lifecycle commands: open / create / switch, the recent-catalogs registry,
//! the library root, rescan, the enrichment-queue drain, and VACUUM.
//!
//! Switching is a safe teardown → reinit, and its body is the core's
//! `app::catalogs::switch_catalog` (the GPUI app runs the same one): every background job
//! generation is tripped under the catalog lock before the handle is dropped, so nothing in
//! flight can write to a torn-down catalog. See `detach_catalog_and_trip_jobs` and
//! `publish_catalog_and_reset_jobs`, which `set_library_root` runs too.

use super::*;
use std::path::PathBuf;
use tauri::{AppHandle, Manager, State};

// The two switch phases, by their old paths: the interleaving tests in other command
// modules (`smarttags`, `storage`) drive them directly.
#[allow(unused_imports)]
pub(super) use crate::app::{detach_catalog_and_trip_jobs, detach_catalog_and_trip_jobs_with, publish_catalog_and_reset_jobs};

/// Switch the active catalog with a safe teardown → reinit lifecycle (I4b): the core's
/// [`crate::app::catalogs::switch_catalog`] on a blocking worker — trip every job and drop the
/// old handle as one transition, open (or `create`) the catalog at `catalog_path` rooted at
/// `root`, publish it with fresh job generations, record it in the recent catalogs, and emit
/// `catalog:switched`. When the new catalog has pending enrichment rows (I6d), the Phase B
/// resume it returns is detached here.
#[tauri::command]
pub async fn switch_catalog(
    state: State<'_, AppState>,
    catalog_path: String,
    root: String,
    create: bool,
    name: Option<String>,
) -> Result<(), String> {
    let state = state.inner().clone();
    let resume = crate::app::spawn_blocking(move || {
        crate::app::catalogs::switch_catalog(&state, &PathBuf::from(&catalog_path), &PathBuf::from(&root), create, name)
    })
    .await
    .map_err(|e| e.to_string())??;
    if let Some(resume) = resume {
        crate::app::spawn_blocking(move || resume.run());
    }
    Ok(())
}

/// List recently-accessed catalogs, ordered by last-opened (most recent first).
#[tauri::command]
pub async fn list_recent_catalogs() -> Result<Vec<RecentCatalog>, String> {
    crate::app::spawn_blocking(|| load_recent_catalogs())
        .await
        .map_err(|e| e.to_string())?
}

/// Open the default catalog at startup (the React shell calls this once on mount). The body
/// is the core's [`open_default_catalog`](crate::app::open_default_catalog), which the GPUI
/// app calls too; its `scan:progress` events reach the webview through the installed sink.
#[tauri::command]
pub async fn init_catalog(state: State<'_, AppState>) -> Result<String, String> {
    crate::app::open_default_catalog(state.inner())
        .await
        .map(|path| path.to_string_lossy().to_string())
}

/// The current library root (the catalog root = local volume base).
#[tauri::command(async)]
pub fn get_library_root(state: State<'_, AppState>) -> Result<String, String> {
    with_catalog(&state, |c| Ok(c.root().to_string_lossy().to_string()))
}

/// Re-root the catalog at `path` (the library folder). Photos are stored relative to
/// the root, so this is a "point at my library" action — existing entries won't resolve
/// until re-scanned. Reopens the default catalog rooted there. `~` is expanded.
///
/// This replaces the active catalog handle, so it runs the **same two-phase ownership
/// transition as `switch_catalog`** rather than a hand-rolled swap (issue #22). It used to
/// hold the catalog lock across persist → reopen → swap and trip nothing, which left a scan,
/// face index, face match, Smart Tagging index, sharpness or pHash job running against a
/// catalog the app had replaced — the invariant in AGENTS.md ("a newer job start or catalog
/// switch must make older workers abortable and unreachable as owners") applied to a function
/// nobody had connected to it.
///
/// The one thing it does that a switch does not: `catalog_root` must be written **through the
/// outgoing handle**, before the reopen, because `Catalog::open` adopts the stored setting
/// over the root argument it is passed. That write rides inside phase one via
/// `detach_catalog_and_trip_jobs_with`, so persist, trip and drop stay one transition.
#[tauri::command]
pub async fn set_library_root(state: State<'_, AppState>, path: String) -> Result<(), String> {
    reroot_library(state.inner(), expand_home(&path), default_catalog_path()?).await
}

/// The body of [`set_library_root`], taking `&AppState` instead of a Tauri `State`.
///
/// Extracted for the same reason `detach_catalog_and_trip_jobs` was: so a test can drive the
/// real thing. Testing the two phases directly is not enough here — #22 *is* "this function
/// does not run the shared transition", so a test that calls the transition itself would
/// still pass if this body were reverted to a hand-rolled swap. Verified: with the test
/// pointed at the phases, restoring the pre-#22 body left the suite green.
pub(super) async fn reroot_library(
    state: &AppState,
    new_root: PathBuf,
    catalog_path: PathBuf,
) -> Result<(), String> {
    // Creating the directory is disk work; keep it off the async executor, and do it before
    // anything is tripped so a bad path fails with every job still running.
    crate::app::spawn_blocking({
        let root = new_root.clone();
        move || std::fs::create_dir_all(&root).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;

    // Phase one: persist the new root through the outgoing handle, trip every job generation,
    // clear every status slot, drop the handle — one transition under the catalog lock. A
    // failed persist leaves the catalog open and nothing tripped.
    detach_catalog_and_trip_jobs_with(state, |catalog| {
        catalog
            .ok_or("No catalog is open")?
            .set_setting("catalog_root", &new_root.to_string_lossy())
            .map_err(|e| e.to_string())
    })?;

    // Reopen off the async executor — this runs migrations.
    let reopened = crate::app::spawn_blocking({
        let path = catalog_path.clone();
        let root = new_root.clone();
        move || Catalog::open(&path, &root).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;

    // Phase two: publish it with fresh un-tripped generations, tripping whatever a racing
    // start installed while the catalog was `None` so it cannot survive unreachable.
    publish_catalog_and_reset_jobs(state, reopened)?;
    // The catalog-root volume's base path just moved — drop cached reachability.
    state.volume_health.invalidate();
    Ok(())
}

/// Rescan the whole library (the catalog root) in place, on a blocking worker thread.
#[tauri::command]
pub async fn rescan_library(app: AppHandle) -> Result<ScanResult, String> {
    super::scan::run_two_phase(app, crate::app::scans::rescan_library).await
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
pub async fn drain_enrichment_queue(state: State<'_, AppState>) -> Result<usize, String> {
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
        spawn_detached_phase_b(state.inner().clone(), path, root, abort);
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
    crate::app::spawn_blocking(move || {
        let state = app.state::<AppState>();
        // `&mut`: compaction also sheds retired columns, which changes the connection's own
        // view of the table shape (see `Catalog::vacuum`).
        let mut guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_mut().ok_or("No catalog is open")?;
        let before = catalog.db_size_bytes().map_err(|e| e.to_string())?;
        catalog.vacuum().map_err(|e| e.to_string())?;
        let after = catalog.db_size_bytes().map_err(|e| e.to_string())?;
        Ok(VacuumResult { before_bytes: before, after_bytes: after })
    })
    .await
    .map_err(|e| e.to_string())?
}
