//! Folder scanning and card import.
//!
//! Scans run two-phase (I6): Phase A inserts rows fast so the grid populates, Phase B
//! detaches and enriches them with EXIF/IPTC/XMP. Both honour the generation abort flag
//! installed by `begin_scan_generation`, so a catalog switch or a second scan stops the
//! previous one before it can write to a torn-down catalog.

use super::*;
use std::path::PathBuf;
use tauri::{AppHandle, Emitter, Manager};

/// Scan a folder (recursively) into the open catalog. Read-only on photo files.
/// A leading "~" is expanded to $HOME.
///
/// Runs the scan on a blocking worker thread (off the UI/runtime thread) so the window
/// stays responsive even on a large library — see the AGENTS.md "UI thread is never
/// blocked" invariant.
#[tauri::command]
pub async fn scan_folder_cmd(app: AppHandle, folder: String) -> Result<ScanResult, String> {
    let expanded = expand_home(&folder);
    run_blocking_two_phase_scan(app, move |c, abort, progress| {
        crate::scanner::scan_folder_phase_a(c, &expanded, abort, progress)
    })
    .await
}

/// Index an existing archive that lives on a non-root volume (e.g. old photos already on
/// the NAS) **in place** — no files are copied. The photos are recorded as NAS-resident
/// (they appear under the "On NAS" tier). For the initial bring-your-NAS-archive scan.
#[tauri::command]
pub async fn scan_nas_folder_cmd(app: AppHandle, folder: String) -> Result<ScanResult, String> {
    let expanded = expand_home(&folder);
    run_blocking_two_phase_scan(app, move |c, abort, progress| {
        crate::scanner::scan_external_folder_phase_a(c, &expanded, abort, progress)
    })
    .await
}

/// Run a scan-like catalog operation on a blocking thread (off the UI/runtime thread,
/// so the window stays responsive), reaching the catalog through the AppHandle. The
/// catalog lock is held only for the duration of the operation on that worker thread.
pub(super) async fn run_blocking_scan<F>(app: AppHandle, op: F) -> Result<ScanResult, String>
where
    F: FnOnce(
            &Catalog,
            &std::sync::atomic::AtomicBool,
            &dyn Fn(crate::scanner::ScanProgress),
        ) -> Result<ScanResult, String>
        + Send
        + 'static,
{
    // Read the catalog path + root under a BRIEF lock, then release it. The scan then runs
    // on its OWN connection (see Catalog::open_secondary), so the shared connection stays
    // free to serve reads (grid, thumbnails) concurrently under WAL — the window no longer
    // freezes for the whole scan. See the AGENTS.md "UI thread is never blocked" invariant.
    //
    // Start a fresh scan generation: trip any earlier scan's flag and take a new one for
    // this scan. A subsequent `switch_catalog` / scan trips *this* flag to stop the scan
    // writing into a catalog that's being closed (or superseded).
    let (path, root, abort) = {
        let state = app.state::<AppState>();
        let abort = begin_scan_generation(&state)?;
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_ref().ok_or("No catalog is open")?;
        (catalog.db_path().to_path_buf(), catalog.root().to_path_buf(), abort)
    };
    let emit_app = app.clone();
    let res = tauri::async_runtime::spawn_blocking(move || {
        let scan_catalog = Catalog::open_secondary(&path, &root).map_err(|e| e.to_string())?;
        // Stream progress to the UI as `scan:progress` events (throttled per commit batch).
        let emit = move |p: crate::scanner::ScanProgress| {
            let _ = emit_app.emit("scan:progress", p);
        };
        op(&scan_catalog, &abort, &emit)
    })
    .await
    .map_err(|e| e.to_string())?;
    // Terminal event so the UI clears its progress indicator (whether the scan succeeded,
    // failed, or was aborted by a catalog switch).
    let _ = app.emit(
        "scan:progress",
        crate::scanner::ScanProgress { phase: "done".into(), done: 0, total: 0 },
    );
    res
}

/// Detach a Phase B enrichment worker for the given catalog `path`/`root`, using the
/// supplied `abort` flag. The worker opens its own secondary connection, loads the
/// pending-enrichment queue, and calls `phase_b_enrich` — streaming
/// `scan:progress {phase:"metadata"|"finalizing"}` events and the terminal
/// `scan:progress {phase:"done"}` event when it finishes (or is aborted).
///
/// Used by the auto-resume path on startup (I6d) and `drain_enrichment_queue`.
/// Does nothing (and emits no events) if the queue is empty.
pub(super) fn spawn_detached_phase_b(
    app: AppHandle,
    path: PathBuf,
    root: PathBuf,
    abort: Arc<AtomicBool>,
) {
    tauri::async_runtime::spawn_blocking(move || {
        let enrich_catalog = match Catalog::open_secondary(&path, &root) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("resume phase B: couldn't open enrichment connection: {e}");
                let _ = app.emit(
                    "scan:progress",
                    crate::scanner::ScanProgress { phase: "done".into(), done: 0, total: 0 },
                );
                return;
            }
        };
        let pending = match crate::scanner::resume_pending_enrichment(&enrich_catalog) {
            Ok(Some(p)) => p,
            Ok(None) => return, // queue is empty — nothing to do, no events emitted
            Err(e) => {
                eprintln!("resume phase B: couldn't load enrichment queue: {e}");
                let _ = app.emit(
                    "scan:progress",
                    crate::scanner::ScanProgress { phase: "done".into(), done: 0, total: 0 },
                );
                return;
            }
        };
        let emit = {
            let emit_app = app.clone();
            move |p: crate::scanner::ScanProgress| {
                let _ = emit_app.emit("scan:progress", p);
            }
        };
        if let Err(e) = crate::scanner::phase_b_enrich(&enrich_catalog, pending, &abort, &emit) {
            if e != crate::scanner::SCAN_ABORTED {
                eprintln!("resume phase B: enrichment failed: {e}");
            }
        }
        let _ = app.emit(
            "scan:progress",
            crate::scanner::ScanProgress { phase: "done".into(), done: 0, total: 0 },
        );
    });
}

/// Run a **two-phase** live scan (I6): Phase A (the fast walk) runs on a blocking worker
/// with its own secondary connection and this command returns as soon as it finishes — so
/// the grid shows the new rows immediately. Phase B (EXIF/IPTC/XMP extraction + finalizing)
/// is then **detached**: it runs on a *separate* blocking worker with its *own* secondary
/// connection, streaming `scan:progress {phase:"metadata"|"finalizing"}` and, when it
/// finishes (or aborts), the terminal `scan:progress {phase:"done"}` event.
///
/// Both phases share this scan generation's `scan_abort` flag (I4b/I6c). A fresh flag is
/// installed at the start (via `begin_scan_generation`), which also trips the *previous*
/// scan's flag — so a catalog switch or a second scan aborts an in-flight Phase B (and
/// Phase A) before it writes into a catalog that's being torn down or superseded.
pub(super) async fn run_blocking_two_phase_scan<F>(app: AppHandle, phase_a: F) -> Result<ScanResult, String>
where
    F: FnOnce(
            &Catalog,
            &AtomicBool,
            &dyn Fn(crate::scanner::ScanProgress),
        ) -> Result<(ScanResult, crate::scanner::PendingEnrich), String>
        + Send
        + 'static,
{
    // Read the catalog path + root under a BRIEF lock, then release it. Both phases run on
    // their OWN connections (Catalog::open_secondary), so the shared connection stays free
    // to serve reads (grid, thumbnails) concurrently under WAL. See the AGENTS.md "UI thread
    // is never blocked" invariant.
    let (path, root, abort) = {
        let state = app.state::<AppState>();
        // Start a fresh scan generation: trip the previous scan's flag (aborting any
        // still-running detached Phase B, so two enrichers never race on the same catalog)
        // and take a new flag for this scan. A later switch_catalog / scan trips *this* one.
        let abort = begin_scan_generation(&state)?;
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_ref().ok_or("No catalog is open")?;
        (catalog.db_path().to_path_buf(), catalog.root().to_path_buf(), abort)
    };

    // Phase A — awaited: build the pending-enrichment hand-off and return its ScanResult
    // to the caller so the UI's import flow completes as soon as the rows are visible.
    let phase_a_out = {
        let (path, root, abort) = (path.clone(), root.clone(), abort.clone());
        let emit_app = app.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let scan_catalog = Catalog::open_secondary(&path, &root).map_err(|e| e.to_string())?;
            let emit = move |p: crate::scanner::ScanProgress| {
                let _ = emit_app.emit("scan:progress", p);
            };
            phase_a(&scan_catalog, &abort, &emit)
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r)
    };
    let (result, mut pending) = match phase_a_out {
        Ok(v) => v,
        Err(e) => {
            // Phase A failed or aborted before Phase B could be spawned. Emit the terminal
            // `scan:progress {phase:"done"}` so the UI's topbar progress indicator always
            // clears (matching the old single-phase run_blocking_scan, which emitted "done"
            // whether the op succeeded, failed, or aborted), then propagate the error.
            let _ = app.emit(
                "scan:progress",
                crate::scanner::ScanProgress { phase: "done".into(), done: 0, total: 0 },
            );
            return Err(e);
        }
    };

    // I6d stale-queue drain: merge any pending_enrichment rows left over from a prior
    // aborted Phase B that Phase A did not touch (unchanged files are not re-enqueued by
    // phase_a_walk, so they would stay stuck at metadata_ready=0 indefinitely). We load
    // the full persistent queue and call merge_stale_pending, which filters out photo_ids
    // already covered by Phase A (needs_extract=true) and appends the rest so Phase B
    // enriches them in the same pass. This makes rescan_library act as the repair path
    // the spec promises: "rescan_library can drain the queue without re-walking."
    {
        // Load stale rows off the async executor: load_pending_enrichment resolves each
        // stale photo's path (SQL + a filesystem stat per volume candidate), which is O(N)
        // blocking IO for a large interrupted queue — never run that on the runtime thread.
        // Best-effort: if the load fails (e.g. catalog was closed mid-flight) we just
        // proceed with what Phase A produced — Phase B still enriches the new/changed files.
        let stale = {
            let app = app.clone();
            tauri::async_runtime::spawn_blocking(move || {
                let app_state = app.state::<AppState>();
                let Ok(guard) = app_state.catalog.lock() else {
                    return Vec::new();
                };
                match guard.as_ref() {
                    Some(c) => c.load_pending_enrichment().unwrap_or_default(),
                    None => Vec::new(),
                }
            })
            .await
            .unwrap_or_default()
        };
        pending.imported =
            crate::scanner::merge_stale_pending(pending.imported, stale);
    }

    // Phase B — detached: a separate worker on its own connection enriches the new rows in
    // the background. It emits the terminal `scan:progress {phase:"done"}` itself when it
    // finishes (or aborts), so the UI's progress indicator clears at the right time.
    let emit_app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let enrich_catalog = match Catalog::open_secondary(&path, &root) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("phase B: couldn't open enrichment connection: {e}");
                let _ = emit_app.emit(
                    "scan:progress",
                    crate::scanner::ScanProgress { phase: "done".into(), done: 0, total: 0 },
                );
                return;
            }
        };
        let emit = {
            let emit_app = emit_app.clone();
            move |p: crate::scanner::ScanProgress| {
                let _ = emit_app.emit("scan:progress", p);
            }
        };
        if let Err(e) = crate::scanner::phase_b_enrich(&enrich_catalog, pending, &abort, &emit) {
            // SCAN_ABORTED is a clean stop (catalog switch / second scan); anything else is
            // a real failure. Either way, clear the indicator with the terminal event below.
            if e != crate::scanner::SCAN_ABORTED {
                eprintln!("phase B: enrichment failed: {e}");
            }
        }
        let _ = emit_app.emit(
            "scan:progress",
            crate::scanner::ScanProgress { phase: "done".into(), done: 0, total: 0 },
        );
    });

    Ok(result)
}

/// Import from a card: copy supported images from `source` into `destBase` under a
/// YYYY/MM/DD tree, index them, batch them, and auto-queue backup. Both paths expand
/// a leading "~". `destBase` should be under the catalog root (your local disk).
///
/// Runs on a blocking worker thread (copies + exiftool) so the window stays responsive.
#[tauri::command]
pub async fn ingest_from_card_cmd(
    app: AppHandle,
    source: String,
    name: Option<String>,
    selected: Option<Vec<String>>,
) -> Result<ScanResult, String> {
    let source = expand_home(&source);

    // The destination is always the library root (catalog root = local volume base), so
    // imported files live under the root and resolve. Read it under a brief lock.
    let dest = {
        let state = app.state::<AppState>();
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_ref().ok_or("No catalog is open")?;
        catalog.root().to_path_buf()
    };

    // The chosen subset of card files (by full path), if the dialog made a selection.
    let selected: Option<std::collections::HashSet<String>> =
        selected.map(|v| v.into_iter().collect());

    // Phase 1 — copy off the card. This is the slow part (gigabytes); it touches only
    // the filesystem, so we run it WITHOUT the catalog lock. Holding the lock here would
    // block every grid command (get_image, list_photos, …) and freeze the window.
    // Progress is streamed to the UI via `import:progress` events.
    let (result, copied) = {
        let (source, dest) = (source.clone(), dest.clone());
        let app = app.clone();
        tauri::async_runtime::spawn_blocking(move || {
            crate::scanner::copy_from_card(&source, &dest, selected.as_ref(), |done, total| {
                let _ = app.emit("import:progress", ImportProgress { done, total });
            })
        })
        .await
        .map_err(|e| e.to_string())??
    };

    // Phase 2 — index the copies. Fast DB writes, so the catalog lock is held only briefly.
    // (Card import streams its own `import:progress`, so the scan progress arg is unused here.)
    run_blocking_scan(app, move |c, _abort, _progress| {
        crate::scanner::index_ingested(c, &dest, &source, copied, name.as_deref(), result)
    })
    .await
}

/// List the photos on a card/source folder for the import dialog, each flagged as a
/// duplicate (already in the library). FS + metadata only — runs on a worker thread.
#[tauri::command]
pub async fn list_card_photos_cmd(
    app: AppHandle,
    source: String,
) -> Result<Vec<crate::scanner::CardPhoto>, String> {
    let source = expand_home(&source);
    let dest = {
        let state = app.state::<AppState>();
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_ref().ok_or("No catalog is open")?;
        catalog.root().to_path_buf()
    };
    tauri::async_runtime::spawn_blocking(move || crate::scanner::list_card_photos(&source, &dest))
        .await
        .map_err(|e| e.to_string())?
}

/// A thumbnail (base64 data URL) for an arbitrary file path — used by the import dialog to
/// preview card photos that aren't in the catalog yet. Cached like other thumbnails.
#[tauri::command]
pub async fn card_thumbnail(path: String) -> Result<String, String> {
    let path = expand_home(&path);
    let bytes = tauri::async_runtime::spawn_blocking(move || crate::thumbnails::thumbnail_bytes(&path))
        .await
        .map_err(|e| e.to_string())??;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("data:image/jpeg;base64,{b64}"))
}

/// Progress event payload for card import, emitted as `import:progress` during the copy.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ImportProgress {
    pub(super) done: usize,
    pub(super) total: usize,
}

