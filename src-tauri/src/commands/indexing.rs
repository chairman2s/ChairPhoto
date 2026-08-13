//! Batch index jobs for the derived per-photo signals: tiled-Laplacian sharpness
//! (H16) and the perceptual hash used for burst grouping (H15).
//!
//! Both also run opportunistically as thumbnail-pool analyzers (see `lib.rs`); these
//! commands are the cancellable backfill for photos imported before that hook existed.

use super::*;
use tauri::{AppHandle, Emitter, State};

/// Progress event payload for `sharpness:progress`. Carries the job id so the UI can
/// ignore stragglers from a superseded run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SharpnessProgressEvent {
    pub done: usize,
    pub total: usize,
    pub job: u64,
}

/// Terminal event for the sharpness-indexing job.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SharpnessIndexDone {
    pub ok: bool,
    pub done: usize,
    pub total: usize,
    pub failed: usize,
    pub offline: usize,
    pub aborted: bool,
    pub job: u64,
    pub error: Option<String>,
}

/// Begin (or resume) the background sharpness-indexing job. Opens its own secondary
/// catalog connection so the UI thread is never blocked. Scoring runs on the ~1024–2048px
/// cached preview (never the 256px thumbnail — micro-blur is invisible there).
///
/// Re-invoking while a job is running trips the old job's abort flag first (same pattern
/// as `faces_index_photos`), then starts a fresh run. Photos already scored
/// (`sharpness IS NOT NULL`) are skipped automatically — the implicit queue is
/// `SELECT id FROM photos WHERE sharpness IS NULL`.
///
/// Returns the new job's id (also carried by progress/done events) so the caller can
/// tell this run's events apart from a superseded run's.
#[tauri::command]
pub async fn index_sharpness(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<u64, String> {
    // Install a fresh abort flag, tripping any in-flight job. This family publishes no
    // queryable status slot, so the whole ownership transition is `install_fresh` — the
    // abort lock is taken and released before the catalog is read below, which is why the
    // switch covers this family in phase two rather than phase one (see `jobs`' lock order).
    let abort = state.jobs.sharpness.install_fresh()?;
    let job = state.jobs.sharpness.next_job_id();

    // Read catalog path + root under a brief lock, then release it.
    let (db_path, root) = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let c = guard.as_ref().ok_or("No catalog is open")?;
        (c.db_path().to_path_buf(), c.root().to_path_buf())
    };

    tauri::async_runtime::spawn_blocking(move || {
        use crate::catalog::Catalog;
        use crate::sharpness_indexer;

        // Secondary connection — never contends with the primary's UI reads.
        let sec = match Catalog::open_secondary(&db_path, &root) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("sharpness_index: couldn't open secondary connection: {e}");
                let _ = app.emit(
                    "sharpness:index_done",
                    SharpnessIndexDone {
                        ok: false,
                        done: 0,
                        total: 0,
                        failed: 0,
                        offline: 0,
                        aborted: false,
                        job,
                        error: Some(format!("couldn't open catalog connection: {e}")),
                    },
                );
                return;
            }
        };

        // Score closure: decode JPEG bytes and apply the region-aware priority chain
        // (H16c) — score inside face boxes, else at the AF point, else tiles.
        let score_fn = |jpeg: &[u8], regions: &sharpness_indexer::RegionInputs| {
            sharpness_indexer::score_jpeg_regions(jpeg, regions)
        };

        // Progress callback: emit a Tauri event after each scored photo.
        let emit_app = app.clone();
        let emit_fn = move |p: sharpness_indexer::SharpnessProgress| {
            let _ = emit_app.emit(
                "sharpness:progress",
                SharpnessProgressEvent { done: p.done, total: p.total, job },
            );
        };

        let resolve_fn = |photo_id: i64| {
            sec.resolve_photo_path(photo_id).map_err(|e| e.to_string())
        };
        let preview_fn = |path: &std::path::Path| crate::thumbnails::preview_bytes(path);

        // Region-gather closure (sequential — may touch the DB). Reads the AF point from
        // photo_metadata always; face boxes only when the `faces` feature is compiled in.
        // Without `faces`, `face_boxes` stays empty and the chain falls through to AF/tile.
        let regions_fn = |photo_id: i64| -> sharpness_indexer::RegionInputs {
            let af_point = sharpness_indexer::photo_af_point(sec.conn(), photo_id);
            #[cfg(feature = "faces")]
            let face_boxes = crate::plugins::faces::store::face_boxes_for_photo(sec.conn(), photo_id)
                .unwrap_or_default();
            #[cfg(not(feature = "faces"))]
            let face_boxes = Vec::new();
            sharpness_indexer::RegionInputs { face_boxes, af_point }
        };

        let result = sharpness_indexer::run_index(
            sec.conn(),
            resolve_fn,
            regions_fn,
            &preview_fn,
            &score_fn,
            &abort,
            emit_fn,
        );

        match result {
            Ok(o) => {
                let _ = app.emit(
                    "sharpness:index_done",
                    SharpnessIndexDone {
                        ok: true,
                        done: o.done,
                        total: o.total,
                        failed: o.failed,
                        offline: o.offline,
                        aborted: o.aborted,
                        job,
                        error: None,
                    },
                );
            }
            Err(e) => {
                eprintln!("sharpness_index: job failed: {e}");
                let _ = app.emit(
                    "sharpness:index_done",
                    SharpnessIndexDone {
                        ok: false,
                        done: 0,
                        total: 0,
                        failed: 0,
                        offline: 0,
                        aborted: false,
                        job,
                        error: Some(e.to_string()),
                    },
                );
            }
        }
    });

    Ok(job)
}

/// Trip the abort flag of any running sharpness-indexing job. The worker stops cleanly
/// after the current chunk finishes; unscored photos remain as `sharpness IS NULL` for
/// the next run. No-op if no job is running.
#[tauri::command]
pub async fn sharpness_index_cancel(state: State<'_, AppState>) -> Result<(), String> {
    state.jobs.sharpness.trip()
}

// ── H15a: Perceptual-hash index job ──────────────────────────────────────────

/// Progress event payload for `phash:progress`. Carries the job id so the UI can ignore
/// stragglers from a superseded run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PhashProgressEvent {
    pub done: usize,
    pub total: usize,
    pub job: u64,
}

/// Terminal event for the perceptual-hash indexing job.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PhashIndexDone {
    pub ok: bool,
    pub done: usize,
    pub total: usize,
    pub failed: usize,
    pub offline: usize,
    pub aborted: bool,
    pub job: u64,
    pub error: Option<String>,
}

/// Begin (or resume) the background perceptual-hash indexing job (H15a). Opens its own
/// secondary catalog connection so the UI thread is never blocked. Hashing runs on the
/// cached 512px thumbnail — a dHash is resolution-invariant, so the cheapest cached decode
/// is enough.
///
/// Re-invoking while a job runs trips the old job's abort flag first (same pattern as
/// `index_sharpness`), then starts a fresh run. Photos already hashed (`phash IS NOT NULL`)
/// are skipped automatically — the implicit queue is `SELECT id FROM photos WHERE phash
/// IS NULL`. Returns the new job's id (also carried by progress/done events).
#[tauri::command]
pub async fn index_phashes(app: AppHandle, state: State<'_, AppState>) -> Result<u64, String> {
    // Install a fresh abort flag, tripping any in-flight job. Slot-less family, same shape
    // as `index_sharpness` above.
    let abort = state.jobs.phash.install_fresh()?;
    let job = state.jobs.phash.next_job_id();

    let (db_path, root) = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let c = guard.as_ref().ok_or("No catalog is open")?;
        (c.db_path().to_path_buf(), c.root().to_path_buf())
    };

    tauri::async_runtime::spawn_blocking(move || {
        use crate::catalog::Catalog;
        use crate::phash_indexer;

        let sec = match Catalog::open_secondary(&db_path, &root) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("phash_index: couldn't open secondary connection: {e}");
                let _ = app.emit(
                    "phash:index_done",
                    PhashIndexDone {
                        ok: false,
                        done: 0,
                        total: 0,
                        failed: 0,
                        offline: 0,
                        aborted: false,
                        job,
                        error: Some(format!("couldn't open catalog connection: {e}")),
                    },
                );
                return;
            }
        };

        let hash_fn = |jpeg: &[u8]| phash_indexer::hash_jpeg(jpeg);

        let emit_app = app.clone();
        let emit_fn = move |p: phash_indexer::PhashProgress| {
            let _ = emit_app.emit(
                "phash:progress",
                PhashProgressEvent { done: p.done, total: p.total, job },
            );
        };

        let resolve_fn =
            |photo_id: i64| sec.resolve_photo_path(photo_id).map_err(|e| e.to_string());
        let thumb_fn = |path: &std::path::Path| crate::thumbnails::thumbnail_bytes(path);

        let result = phash_indexer::run_index(
            sec.conn(),
            resolve_fn,
            &thumb_fn,
            &hash_fn,
            &abort,
            emit_fn,
        );

        match result {
            Ok(o) => {
                let _ = app.emit(
                    "phash:index_done",
                    PhashIndexDone {
                        ok: true,
                        done: o.done,
                        total: o.total,
                        failed: o.failed,
                        offline: o.offline,
                        aborted: o.aborted,
                        job,
                        error: None,
                    },
                );
            }
            Err(e) => {
                eprintln!("phash_index: job failed: {e}");
                let _ = app.emit(
                    "phash:index_done",
                    PhashIndexDone {
                        ok: false,
                        done: 0,
                        total: 0,
                        failed: 0,
                        offline: 0,
                        aborted: false,
                        job,
                        error: Some(e.to_string()),
                    },
                );
            }
        }
    });

    Ok(job)
}

/// Trip the abort flag of any running perceptual-hash indexing job. The worker stops
/// cleanly after the current chunk; unhashed photos remain as `phash IS NULL` for the
/// next run. No-op if no job is running.
#[tauri::command]
pub async fn phash_index_cancel(state: State<'_, AppState>) -> Result<(), String> {
    state.jobs.phash.trip()
}

// ── H7a: Smart Tagging model manager ─────────────────────────────────────────

