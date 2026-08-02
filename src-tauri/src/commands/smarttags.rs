//! Smart Tagging commands (H7) — local CLIP embeddings → kNN + classifier suggestions.
//!
//! Everything here is gated on the `smarttags` Cargo feature; see
//! `docs/ai-tagging.md` (Smart Tagging section) and `plugins/smarttags/`.

use super::*;
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

/// Read the `smarttags.model_path` setting under a brief catalog lock, treating "no catalog
/// open" the same as "setting unset" (→ the pinned default path applies). Shared by the two
/// model commands below; scoped so the guard is dropped before any await.
#[cfg(feature = "smarttags")]
fn smarttags_model_path_setting(state: &State<'_, AppState>) -> Result<Option<String>, String> {
    let guard = state.catalog.lock().map_err(|e| e.to_string())?;
    Ok(guard
        .as_ref()
        .and_then(|c| c.get_setting(crate::plugins::smarttags::MODEL_PATH_SETTING).ok().flatten()))
}

/// Report whether the Smart Tagging CLIP model is present, so the UI can offer a download
/// (default path) or point out a broken custom path. Never fails on a missing model — that
/// is a clean state. Cheap: presence + size only, no hashing (mirrors `faces_models_status`).
#[cfg(feature = "smarttags")]
#[tauri::command]
pub async fn smarttags_model_status(
    state: State<'_, AppState>,
) -> Result<crate::plugins::smarttags::ModelStatus, String> {
    let setting = smarttags_model_path_setting(&state)?;
    Ok(crate::plugins::smarttags::models::status(setting.as_deref()))
}

/// Download progress event payload for `smarttags:download_progress`. Sent approximately
/// every 1 MiB (or 1% of total) so the UI can render a live progress bar without event
/// spam. `total` is `None` when the server omitted `Content-Length`.
#[cfg(feature = "smarttags")]
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SmarttagsDownloadProgressEvent {
    pub done: u64,
    pub total: Option<u64>,
}

/// Download the pinned default CLIP model (once) with checksum verification, returning the
/// post-download status. Safe to re-invoke — an already-present verified model is a no-op.
/// A **custom** `smarttags.model_path` is never fetched; the command errors so the user
/// fixes or clears the path instead of silently shadowing it with the default.
///
/// Emits `smarttags:download_progress` events (`SmarttagsDownloadProgressEvent`) approximately
/// every 1 MiB (or 1% of total) so the UI can show a live progress indicator.
#[cfg(feature = "smarttags")]
#[tauri::command]
pub async fn smarttags_download_model(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<crate::plugins::smarttags::ModelStatus, String> {
    use crate::plugins::smarttags::models;
    let setting = smarttags_model_path_setting(&state)?;
    // Clone app handle so the closure can outlive this stack frame inside `ensure`.
    let app2 = app.clone();
    let progress_cb: Box<dyn Fn(u64, Option<u64>) + Send + Sync> = Box::new(move |done, total| {
        let _ = app2.emit(
            "smarttags:download_progress",
            SmarttagsDownloadProgressEvent { done, total },
        );
    });
    models::ensure(setting.as_deref(), Some(progress_cb.as_ref()))
        .await
        .map_err(|e| e.to_string())?;
    Ok(models::status(setting.as_deref()))
}

// ── H7b: Smart Tagging embedding-index job ───────────────────────────────────

/// Progress event payload for `smarttags:progress`. Carries the job id so the UI can
/// ignore stragglers from a superseded run.
#[cfg(feature = "smarttags")]
#[derive(Debug, Clone, serde::Serialize)]
pub struct SmarttagsProgressEvent {
    pub done: usize,
    pub total: usize,
    pub job: u64,
}

/// Terminal event payload for `smarttags:index_done`. Honest breakdown: `offline` /
/// `failed` / `aborted` say why `done < total` instead of leaving the UI to guess.
#[cfg(feature = "smarttags")]
#[derive(Debug, Clone, serde::Serialize)]
pub struct SmarttagsIndexDone {
    pub ok: bool,
    pub done: usize,
    pub total: usize,
    pub offline: usize,
    pub failed: usize,
    pub aborted: bool,
    pub job: u64,
    pub error: Option<String>,
}

/// Begin (or resume) the background Smart Tagging embedding-index job (H7b). Opens its
/// own secondary catalog connection so the UI thread is never blocked.
///
/// For each un-embedded photo the command:
///   1. Resolves the photo to a reachable path via the catalog resolver.
///   2. Loads the 2048 px cached preview (`thumbnails::preview_bytes`).
///   3. Encodes a CLIP embedding (`embed::encode_jpeg`).
///   4. Upserts the f32-LE BLOB into `smarttags__embeddings`.
///
/// Re-invoking while a job is running trips the old job's abort flag first (same pattern
/// as `index_sharpness`/`index_phashes`), then starts a fresh run. Photos already in
/// `smarttags__embeddings` are skipped automatically (the implicit queue is a LEFT JOIN).
///
/// Returns the new job's id (also carried by `smarttags:progress` and
/// `smarttags:index_done` events) so the caller can tell this run's events apart from a
/// superseded run's.
///
/// Returns an error if no catalog is open or the model is not yet downloaded.
#[cfg(feature = "smarttags")]
#[tauri::command]
pub async fn smarttags_index_photos(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<u64, String> {
    use crate::plugins::smarttags::models;

    // Check that the model is present before spinning up a worker.
    let model_path_setting: Option<String> = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let c = guard.as_ref().ok_or("No catalog is open")?;
        c.get_setting(models::MODEL_PATH_SETTING)
            .ok()
            .flatten()
    };
    let model_status = models::status(model_path_setting.as_deref());
    if !model_status.ready {
        return Err(
            "Smart Tagging model is not downloaded. Download it under Settings → Smart Tagging."
                .to_string(),
        );
    }

    // Read catalog path + root under a brief lock, then release it. This and the model
    // check above are the only fallible steps, and both run before the ownership
    // transition below — so a failure can never leave the previous job aborted with no
    // replacement installed.
    let (db_path, root) = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let c = guard.as_ref().ok_or("No catalog is open")?;
        (c.db_path().to_path_buf(), c.root().to_path_buf())
    };

    // Clone the job status slot so the worker can update it.
    let job_slot = state.smarttags_job.clone();

    // Allocate the job id, trip the previous job, install this job's abort flag and claim
    // the status slot as ONE transition, holding both locks throughout.
    //
    // Tauri dispatches commands onto its runtime, so two starts can run concurrently. If
    // the abort lock were released before the slot write, they could interleave: A
    // installs its flag, B trips A and claims the slot, then A — already aborted —
    // overwrites the slot with itself and the panel tracks a dead job. Job ids cannot
    // arbitrate that, because they are allocated after the flag is installed. Both locks
    // are taken before anything is mutated, so a poisoned slot cannot leave the previous
    // job aborted with no successor. Workers only ever take the slot lock, never the
    // abort lock, so this order introduces no inversion.
    let (abort, job) = {
        let mut abort_guard = state.smarttags_abort.lock().map_err(|e| e.to_string())?;
        let mut slot_guard = job_slot.lock().map_err(|e| e.to_string())?;
        let job = state.smarttags_job_seq.fetch_add(1, Ordering::Relaxed) + 1;
        abort_guard.store(true, Ordering::Relaxed);
        let fresh = Arc::new(AtomicBool::new(false));
        *abort_guard = fresh.clone();
        // Claim the slot (total is unknown until the queue is populated) so a status query
        // between "command returned" and "first progress event" already sees it running.
        *slot_guard = Some(SmarttagsJobStatus { job, done: 0, total: 0 });
        (fresh, job)
    };

    tauri::async_runtime::spawn_blocking(move || {
        use crate::catalog::Catalog;
        use crate::plugins::faces::indexer as faces_indexer;
        use crate::plugins::smarttags::{embed, indexer, models};

        // Release the status slot — but only if a newer job hasn't already claimed it.
        //
        // Always called BEFORE emitting the terminal event, never after. A reattaching
        // panel registers its listener and then re-reads status; if the slot were still
        // set when the one terminal event had already been emitted, that panel would
        // adopt a job it can never see finish and sit in "indexing" forever. Clearing
        // first means a missed terminal necessarily reads back as idle, or as a newer
        // job that is genuinely still running.
        let clear_job_slot = || {
            if let Ok(mut slot) = job_slot.lock() {
                if slot.as_ref().map(|s| s.job) == Some(job) {
                    *slot = None;
                }
            }
        };

        // Secondary connection — never contends with the primary's UI reads.
        let sec = match Catalog::open_secondary(&db_path, &root) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("smarttags_index: couldn't open secondary connection: {e}");
                clear_job_slot();
                let _ = app.emit(
                    "smarttags:index_done",
                    SmarttagsIndexDone {
                        ok: false,
                        done: 0,
                        total: 0,
                        offline: 0,
                        failed: 0,
                        aborted: false,
                        job,
                        error: Some(format!("couldn't open catalog connection: {e}")),
                    },
                );
                return;
            }
        };

        // Read the model path setting on the secondary connection and configure the pool.
        let model_path_setting: Option<String> = sec
            .get_setting(models::MODEL_PATH_SETTING)
            .ok()
            .flatten();

        // Size the CLIP session pool from the shared `indexing.speed` setting (same knob
        // as face indexing). `configure` is a no-op once a pool exists (first run wins).
        let plan = faces_indexer::load_indexing_plan(sec.conn());
        embed::configure(plan.parallelism, plan.intra_threads);

        // Embed closure: JPEG → L2-normalized CLIP embedding. Errors degrade gracefully.
        let model_path_clone = model_path_setting.clone();
        let embed_fn = move |jpeg: &[u8]| -> Option<Vec<f32>> {
            match embed::encode_jpeg(jpeg, model_path_clone.as_deref()) {
                Ok(emb) => Some(emb),
                Err(e) => {
                    eprintln!("smarttags_index: embed error: {e}");
                    None
                }
            }
        };

        let emit_app = app.clone();
        let emit_app_clone = emit_app.clone();
        let job_slot_clone = job_slot.clone();
        let emit_fn = move |p: indexer::SmarttagsProgress| {
            let _ = emit_app_clone.emit(
                "smarttags:progress",
                SmarttagsProgressEvent { done: p.done, total: p.total, job },
            );
            // Update the job status so UI can query it on remount — but never overwrite a
            // newer job's slot. Starting a new run trips this one's abort flag, and the
            // indexer emits progress for the current photo before it checks that flag
            // (plugins/smarttags/indexer.rs:186-189), so a superseded run can still arrive
            // here. Without the guard it would rewrite the slot back to itself and then
            // clear it on the way out, leaving the newer run invisible to status queries.
            if let Ok(mut slot) = job_slot_clone.lock() {
                if slot.as_ref().map(|s| s.job) == Some(job) {
                    *slot = Some(SmarttagsJobStatus {
                        job,
                        done: p.done as usize,
                        total: p.total as usize,
                    });
                }
            }
        };

        let resolve_fn = |photo_id: i64| {
            sec.resolve_photo_path(photo_id).map_err(|e| e.to_string())
        };
        let preview_fn = |path: &std::path::Path| crate::thumbnails::preview_bytes(path);

        let result = indexer::run_index(
            sec.conn(),
            plan.parallelism,
            resolve_fn,
            preview_fn,
            embed_fn,
            &abort,
            emit_fn,
        );

        clear_job_slot();
        match result {
            Ok(o) => {
                let _ = emit_app.emit(
                    "smarttags:index_done",
                    SmarttagsIndexDone {
                        ok: true,
                        done: o.done,
                        total: o.total,
                        offline: o.offline,
                        failed: o.failed,
                        aborted: o.aborted,
                        job,
                        error: None,
                    },
                );
            }
            Err(e) => {
                eprintln!("smarttags_index: job failed: {e}");
                let _ = emit_app.emit(
                    "smarttags:index_done",
                    SmarttagsIndexDone {
                        ok: false,
                        done: 0,
                        total: 0,
                        offline: 0,
                        failed: 0,
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

/// Trip the abort flag of any running Smart Tagging index job. The worker stops cleanly
/// after the current chunk; un-embedded photos remain for the next run. No-op if no job
/// is running.
#[cfg(feature = "smarttags")]
#[tauri::command]
pub async fn smarttags_index_cancel(state: State<'_, AppState>) -> Result<(), String> {
    let guard = state
        .smarttags_abort
        .lock()
        .map_err(|e| e.to_string())?;
    guard.store(true, Ordering::Relaxed);
    Ok(())
}

/// Query the live status of any running Smart Tagging indexing job, or `None` if idle.
/// Allows the UI to re-attach to a job after a panel remount.
#[cfg(feature = "smarttags")]
#[tauri::command]
pub async fn smarttags_index_status(
    state: State<'_, AppState>,
) -> Result<Option<SmarttagsJobStatus>, String> {
    Ok(*state.smarttags_job.lock().map_err(|e| e.to_string())?)
}

// ── H7c — kNN suggestion engine ──────────────────────────────────────────────

/// One Smart Tagging kNN suggestion as returned to the frontend (mirrors `AiSuggestion`).
#[cfg(feature = "smarttags")]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SmarttagsSuggestion {
    /// Full tag path, e.g. `"Animals/Birds/Gull"`.
    pub path: String,
    /// Normalized kNN score in [0, 1].
    pub confidence: f32,
    /// `None` when the tag is not (yet) in the catalog; always `Some` in practice because
    /// kNN only propagates tags that neighbours actually carry.
    pub existing_tag_id: Option<i64>,
    /// Photo IDs of the neighbours that drove this suggestion (provenance for H7d).
    pub source_photo_ids: Vec<i64>,
}

/// Run the kNN tag-suggestion engine for a single photo (H7c).
///
/// 1. Loads the photo's CLIP embedding from `smarttags__embeddings`.
/// 2. Brute-force cosine scan over all other embeddings, keeping up to 20
///    neighbours with similarity ≥ 0.60.
/// 3. Aggregates the neighbours' tags, normalizes scores, ancestor-deduplicates,
///    filters out tags already on the photo and previously rejected suggestions.
/// 4. Upserts pending rows into `smarttags__suggestions`.
///
/// Returns the number of new pending suggestions stored (may be 0 when the photo
/// has no embedding or no close neighbours with useful tags).
#[cfg(feature = "smarttags")]
#[tauri::command]
pub async fn smarttags_suggest_tags(
    state: State<'_, AppState>,
    photo_id: i64,
) -> Result<usize, String> {
    with_catalog_blocking(&state, move |c| {
        crate::plugins::smarttags::suggest_tags(c.conn(), photo_id)
            .map_err(|e| crate::catalog::CatalogError::Validation(e))
    })
    .await
}

/// Load the pending Smart Tagging kNN suggestions for a photo (H7c).
#[cfg(feature = "smarttags")]
#[tauri::command]
pub fn smarttags_load_suggestions(
    state: State<'_, AppState>,
    photo_id: i64,
) -> Result<Vec<SmarttagsSuggestion>, String> {
    with_catalog(&state, |c| {
        let rows = crate::plugins::smarttags::load_pending_suggestions(c.conn(), photo_id)
            .map_err(|e| crate::catalog::CatalogError::Validation(e))?;
        Ok(rows
            .into_iter()
            .map(|s| SmarttagsSuggestion {
                path: s.path,
                confidence: s.confidence,
                existing_tag_id: s.existing_tag_id,
                source_photo_ids: s.source_photo_ids,
            })
            .collect())
    })
}

/// Accept a Smart Tagging kNN suggestion: assign the tag (creating it if new) and
/// mark it `accepted` so it is not shown again (H7c).
#[cfg(feature = "smarttags")]
#[tauri::command]
pub fn smarttags_accept_suggestion(
    state: State<'_, AppState>,
    photo_id: i64,
    path: String,
) -> Result<i64, String> {
    with_catalog(&state, |c| {
        crate::plugins::smarttags::ensure_suggestions_schema(c.conn())
            .map_err(crate::catalog::CatalogError::Sqlite)?;
        let tag_id = match c.find_tag_id_by_path(&path)? {
            Some(id) => id,
            None => c.create_tag(&path)?, // forward-compat: create if somehow absent
        };
        c.assign_tag(photo_id, tag_id)?;
        crate::plugins::smarttags::set_suggestion_state(
            c.conn(),
            photo_id,
            &path,
            "accepted",
            now_secs(),
        )
        .map_err(|e| crate::catalog::CatalogError::Sqlite(e))?;
        Ok(tag_id)
    })
}

/// Reject a Smart Tagging kNN suggestion: marks it `rejected` so it is not
/// re-proposed for this photo on future suggest runs (H7c).
#[cfg(feature = "smarttags")]
#[tauri::command]
pub fn smarttags_reject_suggestion(
    state: State<'_, AppState>,
    photo_id: i64,
    path: String,
) -> Result<(), String> {
    with_catalog(&state, |c| {
        crate::plugins::smarttags::ensure_suggestions_schema(c.conn())
            .map_err(crate::catalog::CatalogError::Sqlite)?;
        crate::plugins::smarttags::set_suggestion_state(
            c.conn(),
            photo_id,
            &path,
            "rejected",
            now_secs(),
        )
        .map_err(|e| crate::catalog::CatalogError::Sqlite(e))
    })
}

/// Delete the entire Smart Tagging embedding index (all `smarttags__embeddings`,
/// `smarttags__suggestions`, and `smarttags__classifiers` rows). Useful when
/// retraining with a different model. A fresh `smarttags_index_photos` run
/// rebuilds from scratch.
#[cfg(feature = "smarttags")]
#[tauri::command]
pub fn smarttags_delete_index(state: State<'_, AppState>) -> Result<(), String> {
    with_catalog(&state, |c| {
        crate::plugins::smarttags::delete_index(c.conn())
            .map_err(|e| crate::catalog::CatalogError::Sqlite(e))
    })
}

// ── H7e — Per-tag logistic classifier ────────────────────────────────────────

/// Summary returned from `smarttags_train_classifiers`.
#[cfg(feature = "smarttags")]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SmarttagsTrainResult {
    /// Tags examined (those with ≥ `min_samples` confirmed CLIP embeddings).
    pub examined: usize,
    /// Tags where a classifier was (re)trained.
    pub trained: usize,
    /// Tags whose classifier was still fresh (skipped).
    pub skipped: usize,
}

/// Train (or refresh) logistic-regression binary classifiers for every tag with
/// at least `smarttags.min_train_samples` (default 10) confirmed CLIP embeddings
/// (H7e).
///
/// A classifier is rebuilt whenever:
/// - it doesn't exist yet, **or**
/// - there has been an accept/reject interaction on that tag's suggestions since
///   the classifier was last trained (`trained_at` < most-recent suggestion
///   `updated_at` with state `accepted`/`rejected`).
///
/// Fresh classifiers with no newer feedback are skipped. The command is
/// intentionally synchronous (runs inside `spawn_blocking`): training 512-d
/// logistic regression over a few hundred photos takes well under a second per
/// tag, and the blended suggestions are only computed on-demand from the frontend.
#[cfg(feature = "smarttags")]
#[tauri::command]
pub async fn smarttags_train_classifiers(
    state: State<'_, AppState>,
) -> Result<SmarttagsTrainResult, String> {
    use crate::plugins::smarttags::{train_all, MIN_TRAIN_SAMPLES_DEFAULT, MIN_TRAIN_SAMPLES_KEY};

    with_catalog_blocking(&state, move |c| {
        // Read the min-samples setting (stored as a string); fall back to the default.
        let min_samples: usize = c
            .get_setting(MIN_TRAIN_SAMPLES_KEY)
            .ok()
            .flatten()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(MIN_TRAIN_SAMPLES_DEFAULT);

        let outcome = train_all(c.conn(), min_samples)
            .map_err(crate::catalog::CatalogError::Validation)?;

        Ok(SmarttagsTrainResult {
            examined: outcome.examined,
            trained: outcome.trained,
            skipped: outcome.skipped,
        })
    })
    .await
}

