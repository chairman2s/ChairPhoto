//! Smart Tagging — the bodies the GPUI Smart Tagging module runs (#126): the CLIP model's
//! status and download, the embedding-index job, the per-photo kNN suggestions with
//! accept/reject, deleting the index and training the per-tag classifiers.
//!
//! **Blocking.** Everything here takes the catalog or does disk/model work: call it on a
//! worker, never a UI thread. The per-photo verbs take a `&Catalog`, so a caller picks the
//! lock: the GPUI app runs them under `with_catalog_as` with the
//! [`CatalogIdentity`] the suggestions were read under, so a photo
//! id read from one catalog is never written into the next (map #92, "Catalog identity").
//!
//! **The index job** is the shared catalog → abort → slot ownership transition
//! ([`super::jobs::JobFamily::begin_as`]). Its status slot is cleared — only while this job
//! still owns it — **before** the terminal `smarttags:index_done`, so a re-attaching panel
//! never adopts a run whose end it already missed.
//!
//! See docs/ai-tagging.md (Smart Tagging section).

use super::{
    now_secs, spawn_blocking, AppState, CatalogIdentity, CoreEvent, EventSink, JobClaim, SmarttagsDownloadProgressEvent,
    SmarttagsIndexDone, SmarttagsJobStatus, SmarttagsProgressEvent,
};
use crate::catalog::{Catalog, CatalogError, Result as CatalogResult};
use crate::plugins::smarttags::{self, classifier, embed, indexer, models, ModelStatus};
use std::sync::{Arc, Mutex};

/// Why an index run is refused before it starts: the model is not on disk.
pub const MODEL_MISSING: &str = "Smart Tagging model is not downloaded. Download it under Settings → Smart Tagging.";

/// Returned when the open catalog was replaced while a training run had the lock released.
pub const SWITCHED_MID_TRAIN: &str = "The catalog changed while classifiers were training; no classifiers were written.";

// ── The model ────────────────────────────────────────────────────────────────

/// The `smarttags.model_path` setting, under a brief catalog lock. "No catalog open" reads as
/// "unset" (the pinned default path applies).
pub fn model_path_setting(state: &AppState) -> Result<Option<String>, String> {
    let guard = state.catalog.lock().map_err(|e| e.to_string())?;
    Ok(guard.as_ref().and_then(|c| c.get_setting(models::MODEL_PATH_SETTING).ok().flatten()))
}

/// Whether the CLIP model is present (`smarttags_model_status`): presence and size only, no
/// hashing. A missing model is a clean state, never an error.
pub fn model_status(state: &AppState) -> Result<ModelStatus, String> {
    let setting = model_path_setting(state)?;
    Ok(models::status(setting.as_deref()))
}

/// Download the pinned default CLIP model (once), SHA-256-verified with an atomic rename
/// (`models::ensure`), and return the post-download status (`smarttags_download_model`).
/// Progress goes out as `smarttags:download_progress` through `state`'s sink. A **custom**
/// `smarttags.model_path` is never fetched: it errors so the user fixes or clears the path.
/// The only network use in Smart Tagging, and only on the user's click.
pub async fn download_model(state: &AppState) -> Result<ModelStatus, String> {
    let setting = model_path_setting(state)?;
    let sink = state.clone();
    let progress: Box<dyn Fn(u64, Option<u64>) + Send + Sync> = Box::new(move |done, total| {
        sink.send(CoreEvent::SmarttagsDownloadProgress(SmarttagsDownloadProgressEvent { done, total }));
    });
    models::ensure(setting.as_deref(), Some(progress.as_ref())).await.map_err(|e| e.to_string())?;
    Ok(models::status(setting.as_deref()))
}

// ── The index job ────────────────────────────────────────────────────────────

/// Claim ownership of the index job: snapshot the catalog, allocate the job id, trip the
/// previous job, install this job's abort flag and claim the status slot as ONE transition,
/// holding catalog → abort → slot throughout ([`super::jobs::JobFamily::begin_as`]). With
/// `expected`, only while that catalog is open.
///
/// `total` is unknown until the queue is populated; claiming anyway is what lets a status
/// query between "start returned" and "first progress event" already see it running.
pub fn begin_index_job(
    state: &AppState,
    expected: Option<CatalogIdentity>,
) -> Result<JobClaim<SmarttagsJobStatus>, String> {
    state.jobs.smarttags.begin_as(&state.catalog, expected, |job| SmarttagsJobStatus { job, done: 0, total: 0 })
}

/// Begin (or resume) the embedding-index job and return its id (`smarttags_index_photos`).
/// Progress goes out as `smarttags:progress` and the end as a terminal
/// `smarttags:index_done`, both carrying the id, through `state`'s sink.
///
/// A start supersedes a running index (its flag is tripped); a catalog switch trips it too.
/// Photos already embedded are skipped (the queue is a LEFT JOIN). Fails — touching nothing —
/// when no catalog is open, the model is missing, or (with `expected`) another catalog is
/// open. The model check runs before the claim, so a refused start never leaves the previous
/// job aborted with no replacement.
pub fn start_index(state: &AppState, expected: Option<CatalogIdentity>) -> Result<u64, String> {
    let setting = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let c = guard.as_ref().ok_or("No catalog is open")?;
        c.get_setting(models::MODEL_PATH_SETTING).ok().flatten()
    };
    if !models::status(setting.as_deref()).ready {
        return Err(MODEL_MISSING.to_string());
    }
    let claim = begin_index_job(state, expected)?;
    let job = claim.job;
    let sink = state.clone();
    spawn_blocking(move || run_index_job(&sink, claim));
    Ok(job)
}

/// The running index's status, `None` when idle (`smarttags_index_status`): a panel
/// re-attaches to it instead of believing it is idle.
pub fn index_status(state: &AppState) -> Result<Option<SmarttagsJobStatus>, String> {
    state.jobs.smarttags.status()
}

/// Trip whatever index is running (`smarttags_index_cancel`). It stops after the current
/// chunk; un-embedded photos stay queued for the next run.
pub fn cancel_index(state: &AppState) -> Result<(), String> {
    state.jobs.smarttags.cancel()
}

/// Cancel index `job` only — a no-op (`false`) once a newer job owns the family.
pub fn cancel_index_job(state: &AppState, job: u64) -> Result<bool, String> {
    state.jobs.smarttags.cancel_job(job)
}

/// The index worker: everything after the claim. Blocking — runs on a worker thread, over
/// its own secondary connection so it never contends with the primary's UI reads.
pub fn run_index_job(sink: &(impl EventSink + ?Sized), claim: JobClaim<SmarttagsJobStatus>) {
    use crate::plugins::indexing as shared_indexing;

    let JobClaim { db_path, root, abort, job, slot } = claim;
    let send_done = |d: SmarttagsIndexDone| sink.send(CoreEvent::SmarttagsIndexDone(d));
    let failed = |error: String| SmarttagsIndexDone {
        ok: false,
        done: 0,
        total: 0,
        offline: 0,
        failed: 0,
        aborted: false,
        job,
        error: Some(error),
    };

    let sec = match Catalog::open_secondary(&db_path, &root) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("smarttags_index: couldn't open secondary connection: {e}");
            slot.clear();
            send_done(failed(format!("couldn't open catalog connection: {e}")));
            return;
        }
    };

    let model_path_setting: Option<String> = sec.get_setting(models::MODEL_PATH_SETTING).ok().flatten();

    // Size the CLIP session pool from the shared `indexing.speed` setting (same knob as face
    // indexing). The pool is keyed by this configuration (`embed::PoolKey`, issue #18), so a
    // value that changed since the last run rebuilds it instead of reusing a stale pool;
    // `model_path_setting` is part of the same key.
    let plan = shared_indexing::load_indexing_plan(sec.conn());
    embed::configure(plan.parallelism, plan.intra_threads);

    // JPEG → L2-normalized CLIP embedding. Errors degrade to "skip this photo".
    let embed_fn = move |jpeg: &[u8]| -> Option<Vec<f32>> {
        match embed::encode_jpeg(jpeg, model_path_setting.as_deref()) {
            Ok(emb) => Some(emb),
            Err(e) => {
                eprintln!("smarttags_index: embed error: {e}");
                None
            }
        }
    };

    let progress_slot = slot.clone();
    let emit_fn = |p: indexer::SmarttagsProgress| {
        sink.send(CoreEvent::SmarttagsProgress(SmarttagsProgressEvent { done: p.done, total: p.total, job }));
        // Keep the slot current for a re-attaching panel — but never a newer job's: the
        // indexer emits progress for the current photo before it checks the abort flag, so a
        // superseded run can still arrive here. `JobSlot::publish` refuses it.
        progress_slot.publish(|job| SmarttagsJobStatus { job, done: p.done as usize, total: p.total as usize });
    };

    let resolve_fn = |photo_id: i64| sec.resolve_photo_path(photo_id).map_err(|e| e.to_string());
    let preview_fn = |path: &std::path::Path| crate::thumbnails::preview_bytes(path);

    let result = indexer::run_index(sec.conn(), plan.parallelism, resolve_fn, preview_fn, embed_fn, &abort, emit_fn);

    slot.clear();
    match result {
        Ok(o) => send_done(SmarttagsIndexDone {
            ok: true,
            done: o.done,
            total: o.total,
            offline: o.offline,
            failed: o.failed,
            aborted: o.aborted,
            job,
            error: None,
        }),
        Err(e) => {
            eprintln!("smarttags_index: job failed: {e}");
            send_done(failed(e.to_string()));
        }
    }
}

// ── Suggestions ──────────────────────────────────────────────────────────────

/// One kNN suggestion as a front end shows it (`SmarttagsSuggestion` in smartTagging.tsx).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SmarttagsSuggestion {
    /// Full tag path, e.g. `"Animals/Birds/Gull"`.
    pub path: String,
    /// Normalized kNN score in [0, 1].
    pub confidence: f32,
    /// `None` when the tag is not (yet) in the catalog; `Some` in practice, because kNN only
    /// propagates tags that neighbours carry.
    pub existing_tag_id: Option<i64>,
    /// The neighbours that drove this suggestion (provenance).
    pub source_photo_ids: Vec<i64>,
}

/// Run the kNN engine for one photo and store its pending suggestions
/// (`smarttags_suggest_tags`); the number stored. CPU work under the catalog lock: call it
/// from a worker.
pub fn suggest_tags(c: &Catalog, photo_id: i64) -> CatalogResult<usize> {
    smarttags::suggest_tags(c.conn(), photo_id).map_err(CatalogError::Validation)
}

/// A photo's pending suggestions, best first (`smarttags_load_suggestions`). A pending
/// suggestion of an auto-tag (stored before #181) is left out: accepting it is refused, so
/// it could never be settled. Its row stays pending and untouched — not a rejection, which
/// would be user feedback to the classifiers.
pub fn load_suggestions(c: &Catalog, photo_id: i64) -> CatalogResult<Vec<SmarttagsSuggestion>> {
    let rows = smarttags::load_pending_suggestions(c.conn(), photo_id).map_err(CatalogError::Validation)?;
    let mut out = Vec::with_capacity(rows.len());
    for s in rows {
        if c.is_auto_tag_path(&s.path)? {
            continue;
        }
        out.push(SmarttagsSuggestion {
            path: s.path,
            confidence: s.confidence,
            existing_tag_id: s.existing_tag_id,
            source_photo_ids: s.source_photo_ids,
        });
    }
    Ok(out)
}

/// Accept: assign the tag (creating the path if it is somehow absent) and mark the
/// suggestion `accepted` (`smarttags_accept_suggestion`); the tag's id.
pub fn accept_suggestion(c: &Catalog, photo_id: i64, path: &str) -> CatalogResult<i64> {
    smarttags::ensure_suggestions_schema(c.conn()).map_err(CatalogError::Sqlite)?;
    let tag_id = match c.find_tag_id_by_path(path)? {
        Some(id) => id,
        None => c.create_tag(path)?,
    };
    c.assign_tag(photo_id, tag_id)?;
    smarttags::set_suggestion_state(c.conn(), photo_id, path, "accepted", now_secs()).map_err(CatalogError::Sqlite)?;
    Ok(tag_id)
}

/// Reject: never re-proposed for this photo (`smarttags_reject_suggestion`).
pub fn reject_suggestion(c: &Catalog, photo_id: i64, path: &str) -> CatalogResult<()> {
    smarttags::ensure_suggestions_schema(c.conn()).map_err(CatalogError::Sqlite)?;
    smarttags::set_suggestion_state(c.conn(), photo_id, path, "rejected", now_secs()).map_err(CatalogError::Sqlite)
}

/// Drop the whole index — embeddings, suggestions and classifiers (`smarttags_delete_index`).
pub fn delete_index(c: &Catalog) -> CatalogResult<()> {
    smarttags::delete_index(c.conn()).map_err(CatalogError::Sqlite)
}

// ── Classifiers ──────────────────────────────────────────────────────────────

/// What a training run did (`smarttags_train_classifiers`).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SmarttagsTrainResult {
    /// Tags examined (those with ≥ `min_samples` confirmed CLIP embeddings).
    pub examined: usize,
    /// Tags where a classifier was (re)trained.
    pub trained: usize,
    /// Tags whose classifier was still fresh (skipped).
    pub skipped: usize,
}

/// Train (or refresh) the per-tag logistic classifiers (`smarttags_train_classifiers`). With
/// `expected`, only against that catalog: a run that finds another catalog open, at the start
/// or at the write, fails closed and writes nothing. Blocking (CPU): call it from a worker.
///
/// ## Why the catalog lock is held only twice
///
/// Training is CPU-bound — 200 full-data passes of 512-d dot products per tag, over every
/// stale tag. Holding the primary catalog mutex for that would queue every UI read behind it.
/// So the run has three phases, and the lock is held only for the first and last:
///
/// 1. **Locate** (locked, trivial) — the database path and the min-samples setting.
/// 2. **Read and train** (unlocked) — on a secondary connection, inside one deferred read
///    transaction: decide which tags are stale, then per tag load its embeddings and train.
///    WAL gives the transaction a stable snapshot without blocking writers (the index job
///    writes through its own secondary connection and never takes the lock), so the
///    staleness scan and every per-tag load agree; loading per tag keeps peak memory at one
///    training set.
/// 3. **Persist** (locked, one transaction) — every classifier at once, only if the open
///    catalog is still the one the run read ([`SWITCHED_MID_TRAIN`] otherwise).
pub fn train_classifiers(state: &AppState, expected: Option<CatalogIdentity>) -> Result<SmarttagsTrainResult, String> {
    train_classifiers_phased(&state.catalog, expected, || {})
}

/// [`train_classifiers`] with a hook. `between_load_and_train` runs at the one moment that
/// matters — after a tag's embeddings were read and the lock released, before the CPU-bound
/// training — so tests can observe that the lock is free there, and force a catalog switch
/// into that exact window. Production passes a no-op.
pub fn train_classifiers_phased(
    catalog: &Arc<Mutex<Option<Catalog>>>,
    expected: Option<CatalogIdentity>,
    mut between_load_and_train: impl FnMut(),
) -> Result<SmarttagsTrainResult, String> {
    use crate::plugins::smarttags::{MIN_TRAIN_SAMPLES_DEFAULT, MIN_TRAIN_SAMPLES_KEY};

    // Phase 1 — locked, trivial. The path is what the write is checked against, so a switch
    // mid-run ends the run instead of writing into the new catalog.
    let (db_path, root, min_samples) = {
        let guard = catalog.lock().map_err(|e| e.to_string())?;
        let c = guard.as_ref().ok_or("No catalog is open")?;
        if expected.is_some_and(|e| !e.is(c)) {
            return Err(super::CATALOG_CHANGED.into());
        }
        let min_samples: usize = c
            .get_setting(MIN_TRAIN_SAMPLES_KEY)
            .ok()
            .flatten()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(MIN_TRAIN_SAMPLES_DEFAULT);
        (c.db_path().to_path_buf(), c.root().to_path_buf(), min_samples)
    };

    // Phase 2 — unlocked: one secondary connection, one deferred read transaction.
    let reader = Catalog::open_secondary(&db_path, &root).map_err(|e| e.to_string())?;
    let snapshot = reader.conn().unchecked_transaction().map_err(|e| e.to_string())?;

    let scan = classifier::scan_stale_tags(&snapshot, min_samples)?;
    let mut trained = Vec::new();
    for tag_path in &scan.stale {
        let Some(set) = classifier::load_training_set(&snapshot, tag_path)? else {
            continue;
        };
        between_load_and_train();
        if let Some(clf) = classifier::train(&set.positives, &set.negatives) {
            trained.push((set.tag_path, clf));
        }
    }
    // Read-only: end the snapshot explicitly so the WAL stops being pinned now.
    snapshot.finish().map_err(|e| e.to_string())?;
    drop(reader);

    // Phase 3 — locked, one transaction, only into the catalog the run read.
    let written = {
        let guard = catalog.lock().map_err(|e| e.to_string())?;
        let c = guard.as_ref().ok_or("No catalog is open")?;
        if c.db_path() != db_path || expected.is_some_and(|e| !e.is(c)) {
            return Err(SWITCHED_MID_TRAIN.to_string());
        }
        classifier::persist_classifiers(c.conn(), &trained)?
    };

    Ok(SmarttagsTrainResult { examined: scan.examined, trained: written, skipped: scan.skipped })
}

#[cfg(test)]
mod tests;
