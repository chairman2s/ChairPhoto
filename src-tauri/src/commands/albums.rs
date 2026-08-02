//! Album and smart-album commands. Albums are manual, ordered photo collections;
//! smart albums are saved rules evaluated on read. See `docs/smart-albums.md`.

use super::*;
use tauri::State;

#[tauri::command]
pub fn list_albums(state: State<'_, AppState>) -> Result<Vec<Album>, String> {
    with_catalog(&state, |c| c.list_albums())
}

#[tauri::command]
pub fn create_album(state: State<'_, AppState>, name: String) -> Result<i64, String> {
    with_catalog(&state, |c| c.create_album(&name))
}

#[tauri::command]
pub fn rename_album(state: State<'_, AppState>, album_id: i64, name: String) -> Result<(), String> {
    with_catalog(&state, |c| c.rename_album(album_id, &name))
}

#[tauri::command]
pub fn delete_album(state: State<'_, AppState>, album_id: i64) -> Result<(), String> {
    with_catalog(&state, |c| c.delete_album(album_id))
}

/// Add the given photos to an album (idempotent). Works with the app's multi-select.
#[tauri::command]
pub fn add_photos_to_album(
    state: State<'_, AppState>,
    album_id: i64,
    photo_ids: Vec<i64>,
) -> Result<(), String> {
    with_catalog(&state, |c| c.add_photos_to_album(album_id, &photo_ids))
}

#[tauri::command]
pub fn remove_photos_from_album(
    state: State<'_, AppState>,
    album_id: i64,
    photo_ids: Vec<i64>,
) -> Result<(), String> {
    with_catalog(&state, |c| c.remove_photos_from_album(album_id, &photo_ids))
}

// --- smart albums ---------------------------------------------------------
// Saved, named RULES evaluated LIVE (no membership table). The shared contract is
// the Rule JSON in docs/smart-albums.md; `rule_json` is opaque to the command layer
// and validated by the translator on create/set. `list_photos` gains a `smartAlbumId`
// param above.

#[tauri::command]
pub fn list_smart_albums(state: State<'_, AppState>) -> Result<Vec<SmartAlbum>, String> {
    with_catalog(&state, |c| c.list_smart_albums())
}

#[tauri::command]
pub fn create_smart_album(
    state: State<'_, AppState>,
    name: String,
    rule_json: String,
) -> Result<i64, String> {
    with_catalog(&state, |c| c.create_smart_album(&name, &rule_json))
}

#[tauri::command]
pub fn rename_smart_album(
    state: State<'_, AppState>,
    smart_album_id: i64,
    name: String,
) -> Result<(), String> {
    with_catalog(&state, |c| c.rename_smart_album(smart_album_id, &name))
}

#[tauri::command]
pub fn set_smart_album_rule(
    state: State<'_, AppState>,
    smart_album_id: i64,
    rule_json: String,
) -> Result<(), String> {
    with_catalog(&state, |c| c.set_smart_album_rule(smart_album_id, &rule_json))
}

#[tauri::command]
pub fn delete_smart_album(state: State<'_, AppState>, smart_album_id: i64) -> Result<(), String> {
    with_catalog(&state, |c| c.delete_smart_album(smart_album_id))
}

#[tauri::command]
pub fn reorder_smart_albums(
    state: State<'_, AppState>,
    ordered_ids: Vec<i64>,
) -> Result<(), String> {
    with_catalog(&state, |c| c.reorder_smart_albums(&ordered_ids))
}

/// Live match count for an arbitrary rule — the builder's preview, computed without
/// persisting a smart album. Runs the translated predicates as a COUNT.
#[tauri::command]
pub fn smart_album_count(state: State<'_, AppState>, rule_json: String) -> Result<i64, String> {
    with_catalog(&state, |c| c.smart_album_count(&rule_json))
}

