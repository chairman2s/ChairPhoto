//! External develop editors (darktable, RawTherapee, ART) and the RapidRAW round-trip.
//! The round-trips live in `external_edit` and `rapidraw`; these are their commands.
//!
//! `available_editors` and `rapidraw_available` read settings under the catalog lock, so
//! they are async like every other command that takes it (see `with_catalog`).

use super::*;
use crate::external_edit::AvailableEditor;
use crate::rapidraw::RapidRawStatus;
use tauri::State;

/// Which editors are configured/available, for the inspector "Edit in…" menu + Preferences.
#[tauri::command(async)]
pub fn available_editors(state: State<'_, AppState>) -> Result<Vec<AvailableEditor>, String> {
    crate::external_edit::available_editors(&state)
}

/// Launch an editor on a photo's original; render and stack the result if it changed.
#[tauri::command]
pub async fn develop_in_editor(
    state: State<'_, AppState>,
    photo_id: i64,
    editor_key: String,
) -> Result<Option<i64>, String> {
    crate::external_edit::develop_in_editor(state.inner().clone(), photo_id, editor_key).await
}

/// Render and stack the result from the current sidecar without relaunching the editor.
#[tauri::command]
pub async fn import_developed(
    state: State<'_, AppState>,
    photo_id: i64,
    editor_key: String,
) -> Result<i64, String> {
    crate::external_edit::import_developed(state.inner().clone(), photo_id, editor_key).await
}

/// Whether RapidRAW is configured/available.
#[tauri::command(async)]
pub fn rapidraw_available(state: State<'_, AppState>) -> Result<RapidRawStatus, String> {
    crate::rapidraw::rapidraw_available(&state)
}

/// Edit a photo in RapidRAW and stack the result; `None` if the wait was cancelled.
#[tauri::command]
pub async fn edit_in_rapidraw(state: State<'_, AppState>, photo_id: i64) -> Result<Option<i64>, String> {
    crate::rapidraw::edit_in_rapidraw(state.inner().clone(), photo_id).await
}

/// Cancel an in-flight RapidRAW wait for `photo_id`, scoped to whichever catalog is open now
/// (React carries no catalog identity for this command): a wait belonging to a catalog this
/// session has since switched away from is left alone (#188). `(async)`, like
/// `rapidraw_available` above: it takes the catalog mutex (`catalog_identity`), which must not
/// block the main thread (#188 follow-up).
#[tauri::command(async)]
pub fn cancel_rapidraw(state: State<'_, AppState>, photo_id: i64) -> Result<(), String> {
    crate::rapidraw::cancel_rapidraw(&state, photo_id)
}
