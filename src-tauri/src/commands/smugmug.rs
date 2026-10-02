//! SmugMug publishing commands — OAuth 1.0a sign-in, album listing/creation, and upload.
//!
//! Gated on the `smugmug` Cargo feature; see `docs/smugmug.md`. Thin wrappers: the bodies are
//! core's `app::smugmug` / `app::oauth` (shared with the GPUI SmugMug module), run on a
//! blocking worker against whichever catalog is open.

#[cfg(feature = "smugmug")]
use crate::app::{oauth, smugmug::LiveSmugMug, uploads::CatalogSettings, AppState};
#[cfg(feature = "smugmug")]
use tauri::State;

#[cfg(feature = "smugmug")]
fn settings(state: &AppState) -> CatalogSettings {
    CatalogSettings::new(state, crate::app::smugmug::SERVICE)
}

#[cfg(feature = "smugmug")]
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    crate::app::spawn_blocking(f).await.map_err(|e| e.to_string())?
}

#[cfg(feature = "smugmug")]
#[tauri::command]
pub async fn smugmug_begin_auth(state: State<'_, AppState>) -> Result<String, String> {
    let s = settings(&state);
    blocking(move || oauth::begin_auth(&LiveSmugMug, &s, "smugmug")).await
}

#[cfg(feature = "smugmug")]
#[tauri::command]
pub async fn smugmug_complete_auth(state: State<'_, AppState>, verifier: String) -> Result<(), String> {
    let s = settings(&state);
    blocking(move || oauth::complete_auth(&LiveSmugMug, &s, "smugmug", &verifier)).await
}

#[cfg(feature = "smugmug")]
#[tauri::command(async)]
pub fn smugmug_connected(state: State<'_, AppState>) -> Result<bool, String> {
    oauth::connected(&settings(&state))
}

/// The authenticated user's albums (upload targets) — `uri` + `name`.
#[cfg(feature = "smugmug")]
#[tauri::command]
pub async fn smugmug_list_albums(state: State<'_, AppState>) -> Result<Vec<crate::smugmug::Album>, String> {
    let s = settings(&state);
    blocking(move || crate::app::smugmug::list_albums(&LiveSmugMug, &s)).await
}

/// Create a new SmugMug album under the user's root folder; returns it for the picker.
#[cfg(feature = "smugmug")]
#[tauri::command]
pub async fn smugmug_create_album(state: State<'_, AppState>, name: String) -> Result<crate::smugmug::Album, String> {
    let s = settings(&state);
    blocking(move || crate::app::smugmug::create_album(&LiveSmugMug, &s, &name)).await
}

/// Render the selected version and upload it to a SmugMug album. Returns the image URL.
#[cfg(feature = "smugmug")]
#[tauri::command]
pub async fn post_to_smugmug(
    state: State<'_, AppState>,
    photo_id: i64,
    version_id: Option<i64>,
    album_uri: String,
    title: String,
    caption: String,
) -> Result<String, String> {
    let state = state.inner().clone();
    // Checks the album and the connection before it claims a job; another publish running
    // meanwhile is neither stopped nor stops this one (each is its own job, `app::uploads`).
    blocking(move || {
        crate::app::smugmug::post(&LiveSmugMug, &settings(&state), &state, photo_id, version_id, &album_uri, &title, &caption)
    })
    .await
}
