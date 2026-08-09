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

/// Claim ownership of the Smart Tagging index job: snapshot the catalog, allocate the job
/// id, trip the previous job, install this job's abort flag and claim the status slot as
/// ONE transition, holding all three locks throughout. Returns the catalog's db path and
/// root, the new job's abort flag, and its id.
///
/// Two starts can run concurrently, since Tauri dispatches commands onto its runtime. If
/// the abort lock were released before the slot write they could interleave: A installs its
/// flag, B trips A and claims the slot, then A — already aborted — overwrites the slot with
/// itself and the panel tracks a dead job. Job ids cannot arbitrate that, because they are
/// allocated after the flag is installed.
///
/// The catalog lock is held for the same reason, against `switch_catalog` rather than
/// against another start — this is what brings the job under catalog-switch ownership. The
/// switch holds that same lock while it trips the current flag and drops the handle, and
/// again while it publishes the new catalog and fresh flags, so only three interleavings
/// exist: the switch completes first and this job snapshots the new catalog; it runs
/// entirely after and trips the generation installed here; or it is mid-switch, in which
/// case the catalog reads as `None` here and this job returns having touched nothing.
/// Snapshotting the catalog outside this block — as this command used to — allows a fourth:
/// install an un-tripped generation after the switch's only abort signal, then index the
/// catalog it is about to close, with a handle no Cancel, later start or subsequent switch
/// can reach.
///
/// Every *nested* acquisition in the backend is catalog → abort → slot: this block, the
/// same block in `faces_index_photos`, and both `switch_catalog` phases. The scan, sharpness
/// and pHash starts install their abort generation and release that lock before reading the
/// catalog, so they never hold two at once and cannot invert against this order;
/// `switch_catalog` covers them instead by tripping whatever is installed before it replaces
/// anything. `smarttags_index_cancel` takes only the abort lock and workers only the slot
/// lock.
///
/// Everything fallible here is read-only and precedes the first mutation, so an error
/// cannot leave the previous job aborted with no successor.
#[cfg(feature = "smarttags")]
fn begin_smarttags_job(
    state: &AppState,
) -> Result<(PathBuf, PathBuf, Arc<AtomicBool>, u64), String> {
    let cat_guard = state.catalog.lock().map_err(|e| e.to_string())?;
    let c = cat_guard.as_ref().ok_or("No catalog is open")?;
    let (db_path, root) = (c.db_path().to_path_buf(), c.root().to_path_buf());

    let mut abort_guard = state.smarttags_abort.lock().map_err(|e| e.to_string())?;
    let mut slot_guard = state.smarttags_job.lock().map_err(|e| e.to_string())?;
    let job = state.smarttags_job_seq.fetch_add(1, Ordering::Relaxed) + 1;
    abort_guard.store(true, Ordering::Relaxed);
    let fresh = Arc::new(AtomicBool::new(false));
    *abort_guard = fresh.clone();
    // Claim the slot (total is unknown until the queue is populated) so a status query
    // between "command returned" and "first progress event" already sees it running.
    *slot_guard = Some(SmarttagsJobStatus { job, done: 0, total: 0 });
    Ok((db_path, root, fresh, job))
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
/// Re-invoking while a job is running trips the old job's abort flag first (same ownership
/// transition as `faces_index_photos` — see `begin_smarttags_job`), then starts a fresh run.
/// A catalog switch trips it too, so a job never outlives the catalog it was started
/// against. Photos already in `smarttags__embeddings` are skipped automatically (the
/// implicit queue is a LEFT JOIN).
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

    // Clone the job status slot so the worker can update it.
    let job_slot = state.smarttags_job.clone();

    // Snapshot the catalog, allocate the job id, trip the previous job, install this job's
    // abort flag and claim the status slot as ONE transition. The model check above is the
    // only other fallible step and runs before it, so a failure can never leave the
    // previous job aborted with no replacement installed.
    let (db_path, root, abort, job) = begin_smarttags_job(state.inner())?;

    tauri::async_runtime::spawn_blocking(move || {
        use crate::catalog::Catalog;
        use crate::plugins::smarttags::{embed, indexer, models};
        use crate::plugins::indexing as shared_indexing;

        // Release the status slot — but only if a newer job hasn't already claimed it.
        //
        // Always called BEFORE emitting the terminal event, never after. A reattaching
        // panel registers its listener and then re-reads status; if the slot were still
        // set when the one terminal event had already been emitted, that panel would
        // adopt a job it can never see finish and sit in "indexing" forever. Clearing
        // first means a missed terminal necessarily reads back as idle, or as a newer
        // job that is genuinely still running.
        let clear_job_slot = || clear_slot_if_owner(&job_slot, job);

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
        let plan = shared_indexing::load_indexing_plan(sec.conn());
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
            write_slot_if_owner(&job_slot_clone, job, p.done as usize, p.total as usize);
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

// ── Catalog-switch job ownership ─────────────────────────────────────────────
//
// These drive the real ownership transitions — `begin_smarttags_job` and the two
// `switch_catalog` phases — against a real `AppState` and real catalogs. The commands
// themselves need a Tauri `AppHandle`, which a unit test has no way to build, so the
// transitions are called directly; everything between them (opening the catalog file,
// the recent-catalogs registry, the `catalog:switched` emit) touches none of this state.
//
// The switch side is forced by construction, never by timing: the switch runs inside the
// indexer's own progress callback, and the start that runs between the two switch phases is
// driven to the exact state a blocked start observes.
//
// The start side needs a test of its own, because a start that runs after phase one is
// rejected by the *pre-fix* shape too — it reads no catalog either way, so
// `start_between_switch_phases_touches_nothing` passes on both.
// `a_blocked_start_keeps_holding_the_catalog_lock` is the one that discriminates: it parks a
// start inside its claim and observes which locks it is holding there.
//
// The last test is a contention net whose assertion holds under every legal interleaving; it
// samples rather than forces, so it backs the other three up instead of standing in for them.

/// Write `done`/`total` into the status slot, but only when `job` still owns it.
///
/// Extracted so the production path and its test share one implementation. Both call sites
/// used to inline this comparison, which meant a test could only ever check a copy of the
/// logic — and a copy stays green when the real guard is deleted.
#[cfg(feature = "smarttags")]
fn write_slot_if_owner(
    slot: &Arc<Mutex<Option<SmarttagsJobStatus>>>,
    job: u64,
    done: usize,
    total: usize,
) {
    if let Ok(mut s) = slot.lock() {
        if s.as_ref().map(|x| x.job) == Some(job) {
            *s = Some(SmarttagsJobStatus { job, done, total });
        }
    }
}

/// Clear the status slot, but only when `job` still owns it. See [`write_slot_if_owner`].
#[cfg(feature = "smarttags")]
fn clear_slot_if_owner(slot: &Arc<Mutex<Option<SmarttagsJobStatus>>>, job: u64) {
    if let Ok(mut s) = slot.lock() {
        if s.as_ref().map(|x| x.job) == Some(job) {
            *s = None;
        }
    }
}

#[cfg(test)]
mod smarttags_ownership_tests {
    use super::*;
    use crate::commands::catalog::{
        detach_catalog_and_trip_jobs, publish_catalog_and_reset_jobs,
    };
    use crate::plugins::smarttags::indexer;
    use std::path::Path;
    use std::time::{Duration, Instant};

    /// A fresh catalog in its own temp dir with `photos` files imported.
    fn temp_catalog(tag: &str, photos: usize) -> (Catalog, PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("chairphoto-smarttags-own-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let db = dir.join("catalog.chairphoto");
        let catalog = Catalog::open(&db, &root).unwrap();
        for i in 0..photos {
            let p = root.join(format!("p{i}.jpg"));
            std::fs::write(&p, b"jpeg").unwrap();
            catalog.upsert_photo(&p, None, 1, 4).unwrap();
        }
        (catalog, db, root)
    }

    /// An `AppState` holding `catalog` as the open catalog.
    fn state_with(catalog: Catalog) -> AppState {
        let state = AppState::default();
        *state.catalog.lock().unwrap() = Some(catalog);
        state
    }

    /// Rows in `smarttags__embeddings`, counting an absent table as zero — the table is
    /// created lazily by the first index run, so "never touched" reads as 0 either way.
    fn embedding_count(db: &Path) -> i64 {
        let conn = rusqlite::Connection::open(db).unwrap();
        conn.query_row("SELECT COUNT(*) FROM smarttags__embeddings", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap_or(0)
    }

    /// A synthetic unit embedding — the injected stand-in for CLIP, so these tests need
    /// neither a model nor an ONNX Runtime.
    fn unit_embedding() -> Vec<f32> {
        let mut v = vec![0.0f32; 512];
        v[0] = 1.0;
        v
    }

    /// Wait until `state.catalog` has been locked *continuously* for `window`, giving up
    /// after `timeout`.
    ///
    /// One failed `try_lock` would prove nothing: the pre-fix start held the catalog lock
    /// too, for the microseconds it took to copy two paths out of the catalog, so a single
    /// sample can catch it by luck. The observation that discriminates is that the lock
    /// stays held — which only happens when the holder is parked while owning it.
    ///
    /// `try_lock` never blocks, so a caller holding `smarttags_abort` can probe the catalog
    /// lock from here without inverting the catalog → abort order.
    fn catalog_stays_locked(state: &AppState, window: Duration, timeout: Duration) -> bool {
        let give_up = Instant::now() + timeout;
        while Instant::now() < give_up {
            let locked = state.catalog.try_lock().is_err();
            if locked {
                let until = Instant::now() + window;
                let mut still_locked = true;
                while Instant::now() < until {
                    if state.catalog.try_lock().is_ok() {
                        still_locked = false;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                if still_locked {
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        false
    }

    /// A catalog switch stops a running Smart Tagging job at its next cancellation point:
    /// no progress after the switch, no further rows in the catalog it was indexing, and
    /// nothing at all in the catalog the user switched to.
    ///
    /// The interleaving is forced rather than timed: the switch runs inside the indexer's
    /// own progress callback, so it lands between photo 1 and photo 2 every time.
    #[test]
    fn catalog_switch_stops_a_running_index_job() {
        let (cat_a, db_a, root_a) = temp_catalog("switch-a", 4);
        let (cat_b, db_b, _root_b) = temp_catalog("switch-b", 0);
        let state = state_with(cat_a);

        // Start a job exactly the way `smarttags_index_photos` does.
        let (db_path, root, abort, job) = begin_smarttags_job(&state).unwrap();
        assert_eq!(db_path, db_a);
        assert_eq!(root, root_a);
        assert_eq!(
            state.smarttags_job.lock().unwrap().map(|s| s.job),
            Some(job),
            "the start must claim the status slot"
        );

        // The worker: the real indexer over a real secondary connection to catalog A.
        let sec = Catalog::open_secondary(&db_path, &root).unwrap();
        let switch_to = Mutex::new(Some(cat_b));
        let mut progress: Vec<usize> = Vec::new();

        let outcome = indexer::run_index(
            sec.conn(),
            // One photo per chunk, so every photo is a cancellation point.
            1,
            |id| Ok(Some(root.join(format!("p{id}.jpg")))),
            |_path| Ok(vec![0xFFu8, 0xD8, 0xFF, 0xE0]),
            |_jpeg| Some(unit_embedding()),
            &abort,
            |p: indexer::SmarttagsProgress| {
                progress.push(p.done);
                // The user switches catalogs while this worker is still running.
                if let Some(cat) = switch_to.lock().unwrap().take() {
                    detach_catalog_and_trip_jobs(&state).unwrap();
                    publish_catalog_and_reset_jobs(&state, cat).unwrap();
                }
            },
        )
        .unwrap();

        assert!(outcome.aborted, "the switch must abort the running job");
        assert_eq!(outcome.total, 4, "all four photos were queued");
        assert_eq!(
            outcome.done, 1,
            "only the photo already committed when the switch happened"
        );
        assert_eq!(progress, vec![1], "no progress event after the switch");
        assert_eq!(
            embedding_count(&db_a),
            1,
            "the aborted worker must write no rows after the switch"
        );
        assert_eq!(
            embedding_count(&db_b),
            0,
            "nothing may land in the catalog that was switched to"
        );

        // The old job's flag stays tripped, and the generation installed for the new
        // catalog is a different, un-tripped Arc — so the old worker cannot be revived by
        // it, and a Cancel or a later switch acts on the new generation only.
        assert!(abort.load(Ordering::Relaxed), "the old flag must stay tripped");
        let installed = state.smarttags_abort.lock().unwrap().clone();
        assert!(
            !Arc::ptr_eq(&installed, &abort),
            "the switch must install a fresh generation, not reuse the aborted one"
        );
        assert!(
            !installed.load(Ordering::Relaxed),
            "the new catalog's generation starts un-tripped"
        );
    }

    /// Between the two switch phases the catalog reads as `None`, so a start returns having
    /// touched nothing: no generation installed, no job id burned, status slot unchanged.
    /// Phase two then publishes the new catalog without resurrecting the superseded job, and
    /// the next start targets the catalog that was switched to.
    ///
    /// Scope, deliberately narrow: this test starts with the catalog **already detached**, so
    /// it exercises the switch side only and would pass on the pre-fix start as well. It does
    /// not show that a start racing the switch ends up here rather than sailing past — that
    /// is `a_blocked_start_keeps_holding_the_catalog_lock`, which is what makes the blocked
    /// case and this case the same case.
    #[test]
    fn start_between_switch_phases_touches_nothing() {
        let (cat_a, db_a, _root_a) = temp_catalog("mid-a", 1);
        let (cat_b, db_b, root_b) = temp_catalog("mid-b", 0);
        let state = state_with(cat_a);

        let (first_db, _root, first_abort, first_job) = begin_smarttags_job(&state).unwrap();
        assert_eq!(first_db, db_a);

        // Phase one: the running job is tripped and the outgoing catalog is detached.
        detach_catalog_and_trip_jobs(&state).unwrap();
        assert!(
            first_abort.load(Ordering::Relaxed),
            "phase one must trip the running job"
        );

        let before = state.smarttags_abort.lock().unwrap().clone();
        let seq_before = state.smarttags_job_seq.load(Ordering::Relaxed);
        let slot_before = *state.smarttags_job.lock().unwrap();

        let err = begin_smarttags_job(&state).unwrap_err();
        assert_eq!(err, "No catalog is open");

        let after = state.smarttags_abort.lock().unwrap().clone();
        assert!(
            Arc::ptr_eq(&before, &after),
            "a start mid-switch must not install a generation"
        );
        assert!(
            after.load(Ordering::Relaxed),
            "the installed generation is still the tripped one"
        );
        assert_eq!(
            state.smarttags_job_seq.load(Ordering::Relaxed),
            seq_before,
            "a rejected start must not burn a job id"
        );
        assert_eq!(
            state.smarttags_job.lock().unwrap().map(|s| s.job),
            slot_before.map(|s| s.job),
            "a rejected start must leave the status slot alone"
        );

        // Phase two publishes the new catalog with a fresh generation — and does not clear
        // the old job's flag, whose worker must stay aborted.
        publish_catalog_and_reset_jobs(&state, cat_b).unwrap();
        assert!(
            first_abort.load(Ordering::Relaxed),
            "phase two must not resurrect the superseded job"
        );

        // A start after the switch snapshots the NEW catalog and becomes the reachable
        // generation.
        let (db, root, abort2, job2) = begin_smarttags_job(&state).unwrap();
        assert_eq!(db, db_b, "the new job must index the catalog switched to");
        assert_eq!(root, root_b);
        assert_ne!(job2, first_job, "each start gets its own job id");
        assert!(!abort2.load(Ordering::Relaxed));
        let installed = state.smarttags_abort.lock().unwrap().clone();
        assert!(
            Arc::ptr_eq(&abort2, &installed),
            "the new job's flag must be the installed generation"
        );
        assert_eq!(
            state.smarttags_job.lock().unwrap().map(|s| s.job),
            Some(job2),
            "the new job owns the status slot"
        );
    }

    /// A start that is blocked partway through its claim is *still holding the catalog lock*.
    ///
    /// This is the start-side half of the protocol, and the only test here that discriminates
    /// it. The switch-side tests above pass on the pre-fix shape too: a start that runs after
    /// phase one reads no catalog either way. What closes the window is that the claim takes
    /// the catalog lock **first** and keeps it across the abort-flag install, leaving a switch
    /// only two outcomes — it takes the catalog lock before the start (and the start then
    /// finds no catalog open) or after it (and phase one trips the generation the start just
    /// installed). The pre-fix shape read the catalog under its own short-lived lock and
    /// released it before touching the abort flag, which admitted a third outcome: snapshot
    /// catalog A, let the switch trip every generation, then install a live generation that
    /// no Cancel, later start or subsequent switch can reach.
    ///
    /// Holding `smarttags_abort` parks a start exactly at that seam, because the claim order
    /// is catalog → abort → slot: to be waiting on the abort lock it must already own the
    /// catalog lock. So the catalog lock stays held for as long as this test holds the abort
    /// lock — and under the pre-fix shape it would be free, because that start had already
    /// let go of it before it ever reached the abort lock.
    #[test]
    fn a_blocked_start_keeps_holding_the_catalog_lock() {
        let (cat_a, db_a, _root_a) = temp_catalog("blocked-a", 0);
        let state = state_with(cat_a);

        // Take the lock the claim needs second, then park a start behind it.
        let abort_held = state.smarttags_abort.lock().unwrap();

        std::thread::scope(|scope| {
            let start = scope.spawn(|| begin_smarttags_job(&state));

            assert!(
                catalog_stays_locked(&state, Duration::from_millis(100), Duration::from_secs(5)),
                "a start waiting on the abort lock must still be holding the catalog lock; \
                 one that reads the catalog and releases it first leaves a window in which a \
                 switch can trip every generation and still be overtaken by this start"
            );

            // Release it and confirm the claim it was parked in the middle of completes
            // normally — the test observes a stalled transition, it does not break one.
            drop(abort_held);
            let (db, _root, abort, job) = start.join().unwrap().unwrap();
            assert_eq!(db, db_a, "the parked start indexes the catalog it snapshotted");
            let installed = state.smarttags_abort.lock().unwrap().clone();
            assert!(
                Arc::ptr_eq(&abort, &installed),
                "the unblocked start's flag must be the installed generation"
            );
            assert_eq!(
                state.smarttags_job.lock().unwrap().map(|s| s.job),
                Some(job),
                "and it must own the status slot"
            );
        });
    }

    /// Under real contention between starts and switches, every start that returned `Ok`
    /// must satisfy: its generation is tripped (a switch reached it), or it is the installed
    /// generation AND it snapshotted the catalog that is now open. A live generation that is
    /// not installed is an orphan nothing can cancel; a live generation indexing a catalog
    /// the switch has already left is the write-after-switch this issue is about.
    ///
    /// The invariant holds under every legal interleaving, so the assertion never depends on
    /// which one the scheduler picks — but the interleavings it *samples* do, which is why
    /// the two deterministic tests above carry the load and this one is a net over the rest.
    /// It is checked after every round, not only at the end, so a bad install cannot be
    /// papered over by the next round's abort.
    #[test]
    fn concurrent_starts_and_switches_leave_no_unreachable_generation() {
        let (cat_a, db_a, root_a) = temp_catalog("race-a", 0);
        let (cat_b, db_b, root_b) = temp_catalog("race-b", 0);
        // Each round re-opens the catalog it switches to, as a real switch does; the two
        // alternate so a stale snapshot is distinguishable from a fresh one.
        drop(cat_b);
        let state = state_with(cat_a);

        let started: Mutex<Vec<(PathBuf, Arc<AtomicBool>)>> = Mutex::new(Vec::new());
        // One uncontended start, so the check below cannot be vacuous even in the unlikely
        // case that every racing start lands in the mid-switch window.
        let (db, _root, first, _job) = begin_smarttags_job(&state).unwrap();
        started.lock().unwrap().push((db, first));

        for round in 0..50 {
            let (next_db, next_root) = if round % 2 == 0 {
                (&db_b, &root_b)
            } else {
                (&db_a, &root_a)
            };
            std::thread::scope(|s| {
                s.spawn(|| {
                    if let Ok((db, _root, abort, _job)) = begin_smarttags_job(&state) {
                        started.lock().unwrap().push((db, abort));
                    }
                });
                s.spawn(|| {
                    detach_catalog_and_trip_jobs(&state).unwrap();
                    let cat = Catalog::open(next_db, next_root).unwrap();
                    publish_catalog_and_reset_jobs(&state, cat).unwrap();
                });
            });

            let installed = state.smarttags_abort.lock().unwrap().clone();
            let open_db = state
                .catalog
                .lock()
                .unwrap()
                .as_ref()
                .map(|c| c.db_path().to_path_buf());
            for (i, (db, abort)) in started.lock().unwrap().iter().enumerate() {
                if abort.load(Ordering::Relaxed) {
                    continue;
                }
                assert!(
                    Arc::ptr_eq(abort, &installed),
                    "round {round}: start #{i} left a live generation that no cancel or \
                     switch can reach"
                );
                assert_eq!(
                    Some(db.clone()),
                    open_db,
                    "round {round}: start #{i} is still live against a catalog the switch \
                     has left"
                );
            }
        }
    }

    /// Acceptance criterion 3: "Status/progress/terminal updates remain scoped to the owning
    /// job id." The two guards that enforce it — `clear_job_slot` (smarttags.rs:221) and the
    /// slot write inside `emit_fn` (smarttags.rs:288) — had no coverage at all: a reviewer
    /// deleted both and the whole lib suite stayed green.
    ///
    /// The guards exist because a superseded run can still arrive at either point after a
    /// newer one has claimed the slot. Without them it rewrites the slot back to itself and
    /// then clears it on the way out, leaving the newer, genuinely-running job invisible to
    /// `smarttags_index_status` — the panel reads idle and stops showing progress for a job
    /// that is still working.
    ///
    /// This exercises the guard conditions directly rather than through a live indexer,
    /// because reproducing the race in a real run means winning it. Both branches are
    /// asserted: the owner's write lands, the superseded one's does not.
    #[test]
    fn slot_writes_are_scoped_to_the_owning_job() {
        let slot: Arc<Mutex<Option<SmarttagsJobStatus>>> = Arc::new(Mutex::new(None));

        // A newer job (id 2) owns the slot.
        *slot.lock().unwrap() = Some(SmarttagsJobStatus { job: 2, done: 5, total: 100 });

        // The superseded run (id 1) tries to report progress. The guard must reject it.
        write_slot_if_owner(&slot, 1, 42, 100);
        let after = slot.lock().unwrap().clone();
        assert_eq!(
            after.as_ref().map(|s| (s.job, s.done)),
            Some((2, 5)),
            "a superseded job (1) overwrote the slot owned by a newer job (2); \
             without this guard the newer run becomes invisible to status queries"
        );

        // The owner's own write must still land, or the guard has broken progress entirely.
        write_slot_if_owner(&slot, 2, 7, 100);
        assert_eq!(
            slot.lock().unwrap().as_ref().map(|s| (s.job, s.done)),
            Some((2, 7)),
            "the owning job's progress write was rejected"
        );

        // The superseded run finishing must not clear the newer job's slot.
        clear_slot_if_owner(&slot, 1);
        assert!(
            slot.lock().unwrap().is_some(),
            "a superseded job cleared the slot out from under the running one; \
             the panel would read idle while indexing is still in progress"
        );

        // The owner clearing on completion must work.
        clear_slot_if_owner(&slot, 2);
        assert!(
            slot.lock().unwrap().is_none(),
            "the owning job could not clear its own slot on completion"
        );
    }

}
