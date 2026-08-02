//! Publication commands — where a photo was posted, and which version went to each
//! platform. Platform markers are declared by the publishing module, never hardcoded
//! here. See `docs/publications.md`.

use super::*;
use tauri::State;

#[tauri::command]
pub fn list_publications(
    state: State<'_, AppState>,
    photo_id: i64,
) -> Result<Vec<Publication>, String> {
    with_catalog(&state, |c| c.list_publications(photo_id))
}

/// Record (or update) that a photo's `version_id` (None = Original) was published to
/// `platform`. The `platform` marker is supplied by the caller (a publishing module, or
/// the user marking a manual post); core rejects an empty one.
#[tauri::command]
pub fn record_publication(
    state: State<'_, AppState>,
    photo_id: i64,
    version_id: Option<i64>,
    platform: String,
    url: Option<String>,
) -> Result<i64, String> {
    with_catalog(&state, |c| {
        c.record_publication(photo_id, version_id, &platform, url.as_deref())
    })
}

#[tauri::command]
pub fn delete_publication(state: State<'_, AppState>, id: i64) -> Result<(), String> {
    with_catalog(&state, |c| c.delete_publication(id))
}

// --- albums ---------------------------------------------------------------

