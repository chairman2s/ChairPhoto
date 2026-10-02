//! Smart Tagging commands (H7) — local CLIP embeddings → kNN + classifier suggestions.
//!
//! Everything here is gated on the `smarttags` Cargo feature; see
//! `docs/ai-tagging.md` (Smart Tagging section) and `plugins/smarttags/`.
//!
//! The model, index-job, suggestion and training bodies live in the core
//! (`app::smarttags`), which the GPUI app runs too (#126); the commands here are their thin
//! Tauri wrappers. The job-ownership and training-lock tests moved with the bodies.

use super::*;
use crate::app::smarttags as core_smarttags;
pub use crate::app::smarttags::{SmarttagsSuggestion, SmarttagsTrainResult};
use tauri::State;

/// Report whether the Smart Tagging CLIP model is present, so the UI can offer a download
/// (default path) or point out a broken custom path. Never fails on a missing model — that
/// is a clean state. Cheap: presence + size only, no hashing (mirrors `faces_models_status`).
#[tauri::command]
pub async fn smarttags_model_status(
    state: State<'_, AppState>,
) -> Result<crate::plugins::smarttags::ModelStatus, String> {
    core_smarttags::model_status(&state)
}

/// Download the pinned default CLIP model (once) with checksum verification, returning the
/// post-download status (`app::smarttags::download_model`). Safe to re-invoke; a custom
/// `smarttags.model_path` is never fetched. Emits `smarttags:download_progress`.
#[tauri::command]
pub async fn smarttags_download_model(
    state: State<'_, AppState>,
) -> Result<crate::plugins::smarttags::ModelStatus, String> {
    core_smarttags::download_model(&state).await
}

/// Begin (or resume) the background Smart Tagging embedding-index job and return its id;
/// progress arrives as `smarttags:progress` and the end as `smarttags:index_done`
/// (`app::smarttags::start_index`). Errors when no catalog is open or the model is missing.
#[tauri::command]
pub async fn smarttags_index_photos(state: State<'_, AppState>) -> Result<u64, String> {
    let state = state.inner().clone();
    crate::app::spawn_blocking(move || core_smarttags::start_index(&state, None))
        .await
        .map_err(|e| e.to_string())?
}

/// Trip the abort flag of any running Smart Tagging index job. The worker stops cleanly
/// after the current chunk; un-embedded photos remain for the next run. No-op if idle.
#[tauri::command]
pub async fn smarttags_index_cancel(state: State<'_, AppState>) -> Result<(), String> {
    core_smarttags::cancel_index(&state)
}

/// Query the live status of any running Smart Tagging indexing job, or `None` if idle.
/// Allows the UI to re-attach to a job after a panel remount.
#[tauri::command]
pub async fn smarttags_index_status(
    state: State<'_, AppState>,
) -> Result<Option<SmarttagsJobStatus>, String> {
    core_smarttags::index_status(&state)
}

/// Run the kNN tag-suggestion engine for a single photo (H7c) and store its pending
/// suggestions; the number stored (`app::smarttags::suggest_tags`).
#[tauri::command]
pub async fn smarttags_suggest_tags(state: State<'_, AppState>, photo_id: i64) -> Result<usize, String> {
    with_catalog_blocking(&state, move |c| core_smarttags::suggest_tags(c, photo_id)).await
}

/// Load the pending Smart Tagging kNN suggestions for a photo (H7c).
#[tauri::command(async)]
pub fn smarttags_load_suggestions(
    state: State<'_, AppState>,
    photo_id: i64,
) -> Result<Vec<SmarttagsSuggestion>, String> {
    with_catalog(&state, |c| core_smarttags::load_suggestions(c, photo_id))
}

/// Accept a Smart Tagging kNN suggestion: assign the tag (creating it if new) and mark it
/// `accepted` so it is not shown again (H7c).
#[tauri::command(async)]
pub fn smarttags_accept_suggestion(
    state: State<'_, AppState>,
    photo_id: i64,
    path: String,
) -> Result<i64, String> {
    with_catalog(&state, |c| core_smarttags::accept_suggestion(c, photo_id, &path))
}

/// Reject a Smart Tagging kNN suggestion: marks it `rejected` so it is not re-proposed for
/// this photo on future suggest runs (H7c).
#[tauri::command(async)]
pub fn smarttags_reject_suggestion(
    state: State<'_, AppState>,
    photo_id: i64,
    path: String,
) -> Result<(), String> {
    with_catalog(&state, |c| core_smarttags::reject_suggestion(c, photo_id, &path))
}

/// Delete the entire Smart Tagging embedding index (embeddings, suggestions and
/// classifiers). A fresh `smarttags_index_photos` run rebuilds from scratch.
#[tauri::command(async)]
pub fn smarttags_delete_index(state: State<'_, AppState>) -> Result<(), String> {
    with_catalog(&state, core_smarttags::delete_index)
}

/// Train (or refresh) the per-tag logistic classifiers (H7e) on a blocking worker, holding
/// the catalog lock only for its two short SQLite phases (`app::smarttags::train_classifiers`
/// says why).
#[tauri::command]
pub async fn smarttags_train_classifiers(
    state: State<'_, AppState>,
) -> Result<SmarttagsTrainResult, String> {
    let state = state.inner().clone();
    crate::app::spawn_blocking(move || core_smarttags::train_classifiers(&state, None))
        .await
        .map_err(|e| e.to_string())?
}
