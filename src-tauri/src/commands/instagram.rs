//! Instagram publishing commands — drives a real Chrome over the DevTools Protocol.
//!
//! Gated on the `instagram` Cargo feature; see `docs/instagram.md`. Brittle web automation by
//! nature: it is supervised by default and stops before Share. Thin wrappers: the bodies are
//! core's `app::instagram` (shared with the GPUI Instagram module).

#[cfg(feature = "instagram")]
use crate::app::AppState;
#[cfg(feature = "instagram")]
use tauri::State;

/// Render the selected version to an Instagram-sized JPEG and post it by driving Chrome.
/// `publish` clicks Share; otherwise the post is composed and left for review. Returns a
/// status string: "posted", "awaitingReview", or "needsLogin". The render's directory is kept
/// while the composer may still read it (`app::instagram::render_still_needed`).
#[cfg(feature = "instagram")]
#[tauri::command]
pub async fn post_to_instagram(
    state: State<'_, AppState>,
    photo_id: i64,
    version_id: Option<i64>,
    caption: String,
    publish: bool,
) -> Result<String, String> {
    use crate::app::instagram;
    let state = state.inner().clone();
    crate::app::spawn_blocking(move || {
        let job = crate::app::uploads::claim_upload(&state, None, instagram::SERVICE, photo_id, version_id)?;
        let rendered = instagram::render(job)?;
        // The Instagram module (frontend) records the publication when the outcome is
        // "posted", through the same api.recordPublication contract as the other publishers.
        instagram::post(&instagram::ChromeDriver, rendered, &caption, publish).map(|o| instagram::outcome_name(o).to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Build a suggested Instagram caption for a photo: its title/description followed by its
/// keywords as `#hashtags` (de-duped, capped at 30). Used to prefill the caption box.
#[cfg(feature = "instagram")]
#[tauri::command(async)]
pub fn build_instagram_caption(state: State<'_, AppState>, photo_id: i64) -> Result<String, String> {
    crate::app::instagram::caption(&state, None, photo_id)
}
