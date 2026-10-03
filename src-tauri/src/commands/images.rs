//! Image-serving commands: the bulk thumbnail/preview cache warm-up, the data-URI
//! fallbacks, and the loopback video-server port.
//!
//! The fast path for pixels is NOT here — thumbnails, previews and zoom are served
//! natively over the `thumb://` / `preview://` / `zoom://` URI schemes (`protocol.rs`)
//! so image bytes never cross the IPC boundary as base64.

use super::*;
use std::path::Path;
use tauri::State;

/// The loopback port serving catalog videos (`http://127.0.0.1:<port>/<photo_id>`), for the
/// frontend `<video>` player. 0 if the server failed to start.
#[tauri::command]
pub fn video_server_port() -> u16 {
    crate::protocol::video_server_port()
}

/// Pre-generate cached images for every photo in the catalog, in parallel across
/// CPU cores. Thumbnails are always generated; previews too when `include_previews`
/// is set (this is the "cache on import" option — slower but makes the loupe
/// instant). Progress is streamed to the frontend via `cache:progress` events, numbered by
/// the warm-up's job id. The body is the core's owned job (`app::cache`): a newer warm-up or
/// a catalog switch stops this one, which then answers `CACHE_CANCELLED`.
///
/// `async` + `spawn_blocking` so the heavy work runs off both the UI thread and
/// the async runtime.
#[tauri::command]
pub async fn cache_images(state: State<'_, AppState>, include_previews: bool) -> Result<(), String> {
    let state = state.inner().clone();
    crate::app::spawn_blocking(move || crate::app::cache::cache_images(&state, include_previews))
        .await
        .map_err(|e| e.to_string())?
        .map(|_| ())
}

/// Return a photo's grid thumbnail as a `data:image/jpeg;base64,...` URI, ready to
/// drop straight into an <img src>. Generated and cached on first request.
#[tauri::command]
pub async fn get_thumbnail(
    state: State<'_, AppState>,
    photo_id: i64,
) -> Result<String, String> {
    image_data_uri(&state, photo_id, thumbnail_bytes).await
}

/// Return a photo's large loupe preview as a data URI. Same shape as
/// `get_thumbnail` but at full preview resolution for the single-image view.
#[tauri::command]
pub async fn get_preview(
    state: State<'_, AppState>,
    photo_id: i64,
) -> Result<String, String> {
    image_data_uri(&state, photo_id, preview_bytes).await
}

/// Shared body for thumbnail/preview: resolve the absolute path under the lock,
/// then do the heavy decode/extract on a blocking thread so neither the main
/// thread nor the async runtime is stalled. The lock guard is dropped before the
/// `.await`, so it never crosses the await point.
async fn image_data_uri(
    state: &State<'_, AppState>,
    photo_id: i64,
    render: fn(&Path) -> Result<Vec<u8>, String>,
) -> Result<String, String> {
    // Path candidates under a brief lock (pure SQL); the existence stats happen off the
    // lock in `pick_existing` so a slow/offline NAS can't serialize the app.
    let candidates = with_catalog(state, |c| c.photo_path_candidates(photo_id))?;
    let health = state.volume_health.clone();
    // OriginalRequired: this is the base64 fallback for the `thumb://`/`preview://`
    // protocols, so it is the last thing standing between the caller and an error — it
    // must not report "unreachable" on the strength of a cached flag alone.
    let bytes = crate::app::spawn_blocking(move || {
        let absolute = crate::volume_health::pick_existing(
            &candidates,
            &health,
            crate::catalog::ResolveMode::OriginalRequired,
        )
        .ok_or_else(|| format!("no reachable copy of photo {photo_id}"))?;
        render(&absolute)
    })
    .await
    .map_err(|e| e.to_string())??;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("data:image/jpeg;base64,{b64}"))
}

