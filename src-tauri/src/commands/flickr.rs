//! Flickr publishing commands — OAuth 1.0a sign-in, photo upload, native-tag prefill, and
//! importing an existing Flickr photostream back into the catalog.
//!
//! Gated on the `flickr` Cargo feature; see `docs/flickr.md`. Thin wrappers: the bodies are
//! core's `app::flickr` / `app::oauth` (shared with the GPUI Flickr module), run on a blocking
//! worker against whichever catalog is open.

#[cfg(feature = "flickr")]
use crate::app::flickr::{FlickrImportMatch, FlickrImportResult};
#[cfg(feature = "flickr")]
use crate::app::{flickr::LiveFlickr, oauth, uploads::CatalogSettings, AppState};
#[cfg(feature = "flickr")]
use tauri::State;

#[cfg(feature = "flickr")]
fn settings(state: &AppState) -> CatalogSettings {
    CatalogSettings::new(state, crate::app::flickr::SERVICE)
}

/// Run `f` on a blocking worker.
#[cfg(feature = "flickr")]
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    crate::app::spawn_blocking(f).await.map_err(|e| e.to_string())?
}

/// Begin Flickr OAuth: returns the authorize URL the user opens (then pastes a verifier).
/// Stashes the request token/secret in settings for the completion step.
#[cfg(feature = "flickr")]
#[tauri::command]
pub async fn flickr_begin_auth(state: State<'_, AppState>) -> Result<String, String> {
    let s = settings(&state);
    blocking(move || oauth::begin_auth(&LiveFlickr, &s, "flickr")).await
}

/// Finish Flickr OAuth with the verifier the user pasted; stores the access token/secret.
#[cfg(feature = "flickr")]
#[tauri::command]
pub async fn flickr_complete_auth(state: State<'_, AppState>, verifier: String) -> Result<(), String> {
    let s = settings(&state);
    blocking(move || oauth::complete_auth(&LiveFlickr, &s, "flickr", &verifier)).await
}

#[cfg(feature = "flickr")]
#[tauri::command(async)]
pub fn flickr_connected(state: State<'_, AppState>) -> Result<bool, String> {
    oauth::connected(&settings(&state))
}

/// Render the selected version and upload it to Flickr. Returns the photo's page URL
/// (canonical `/photos/<nsid>/<id>/` when the NSID is known, else the `photo.gne?id=`
/// redirect form). The frontend records the publication with this URL.
#[cfg(feature = "flickr")]
#[tauri::command]
pub async fn post_to_flickr(
    state: State<'_, AppState>,
    photo_id: i64,
    version_id: Option<i64>,
    title: String,
    description: String,
    tags: String,
) -> Result<String, String> {
    let state = state.inner().clone();
    // Checks the connection before it claims a job; another publish running meanwhile is
    // neither stopped nor stops this one (each is its own job, `app::uploads`).
    blocking(move || crate::app::flickr::post(&LiveFlickr, &settings(&state), &state, photo_id, version_id, &title, &description, &tags))
        .await
}

/// Suggested Flickr tags for a photo: its export keywords in Flickr's `tags` format.
#[cfg(feature = "flickr")]
#[tauri::command(async)]
pub fn flickr_suggest_tags(state: State<'_, AppState>, photo_id: i64) -> Result<String, String> {
    crate::app::flickr::suggest_tags(&state, None, photo_id)
}

/// Fetch the user's Flickr photostream, match it against the catalog, and return a preview
/// plan. Read-only toward Flickr; records nothing — `flickr_import_apply` does.
#[cfg(feature = "flickr")]
#[tauri::command]
pub async fn flickr_import_published(state: State<'_, AppState>) -> Result<FlickrImportResult, String> {
    let state = state.inner().clone();
    blocking(move || crate::app::flickr::import_preview(&LiveFlickr, &settings(&state), &state, None, "flickr")).await
}

/// Apply a previewed plan: a `flickr` publication per entry with its historical upload date.
/// Every entry is recorded on its own; if any failed, the answer says how many and why the
/// first did (the rest stay recorded).
#[cfg(feature = "flickr")]
#[tauri::command]
pub async fn flickr_import_apply(state: State<'_, AppState>, plan: Vec<FlickrImportMatch>) -> Result<usize, String> {
    let state = state.inner().clone();
    let applied = blocking(move || crate::app::flickr::import_apply(&state, None, "flickr", &plan)).await?;
    match applied.failed.first() {
        None => Ok(applied.applied),
        Some(first) => Err(format!("Imported {}; {} could not be recorded ({first})", applied.applied, applied.failed.len())),
    }
}
