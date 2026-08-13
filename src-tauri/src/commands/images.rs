//! Image-serving commands: the bulk thumbnail/preview cache warm-up, the data-URI
//! fallbacks, and the loopback video-server port.
//!
//! The fast path for pixels is NOT here — thumbnails, previews and zoom are served
//! natively over the `thumb://` / `preview://` / `zoom://` URI schemes (`protocol.rs`)
//! so image bytes never cross the IPC boundary as base64.

use super::*;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Emitter, State};

/// The loopback port serving catalog videos (`http://127.0.0.1:<port>/<photo_id>`), for the
/// frontend `<video>` player. 0 if the server failed to start.
#[tauri::command]
pub fn video_server_port() -> u16 {
    crate::protocol::video_server_port()
}

/// Progress event payload for batch caching, emitted as `cache:progress`.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct CacheProgress {
    done: usize,
    total: usize,
}

/// Pre-generate cached images for every photo in the catalog, in parallel across
/// CPU cores. Thumbnails are always generated; previews too when `include_previews`
/// is set (this is the "cache on import" option — slower but makes the loupe
/// instant). Progress is streamed to the frontend via `cache:progress` events.
///
/// `async` + `spawn_blocking` so the heavy work runs off both the UI thread and
/// the async runtime.
#[tauri::command]
pub async fn cache_images(
    app: AppHandle,
    state: State<'_, AppState>,
    include_previews: bool,
) -> Result<(), String> {
    // Gather each photo's path CANDIDATES under one brief lock (pure SQL — no stats), so
    // resolving the whole library never holds the lock across NAS stats. The existence
    // check then happens off the lock, on the blocking worker, via `pick_existing`.
    let candidate_lists: Vec<(i64, Vec<crate::catalog::PathCandidate>)> = with_catalog(&state, |c| {
        let mut lists = Vec::new();
        for photo in c.list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None)? {
            lists.push((photo.id, c.photo_path_candidates(photo.id)?));
        }
        Ok(lists)
    })?;
    let health = state.volume_health.clone();
    let items: Vec<(i64, PathBuf)> = tauri::async_runtime::spawn_blocking(move || {
        candidate_lists
            .into_iter()
            .filter_map(|(id, cands)| {
                // Skip photos whose originals aren't currently reachable (e.g. offline NAS).
                // OriginalRequired: this builds the cache *from* originals, so a stale
                // reachability flag must not silently drop a photo from the warm-up.
                crate::volume_health::pick_existing(
                    &cands,
                    &health,
                    crate::catalog::ResolveMode::OriginalRequired,
                )
                .map(|abs| (id, abs))
            })
            .collect()
    })
    .await
    .map_err(|e| e.to_string())?;

    let total = items.len();
    if total == 0 {
        return Ok(());
    }

    // (photo_id, is_grayscale) computed from each thumbnail, applied to the catalog
    // after the parallel pass (workers don't hold the lock).
    let grayscale = std::sync::Mutex::new(Vec::<(i64, bool)>::with_capacity(total));
    let grayscale = tauri::async_runtime::spawn_blocking(move || {
        let done = AtomicUsize::new(0);
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .min(total);
        let chunk_size = total.div_ceil(workers);

        std::thread::scope(|scope| {
            for chunk in items.chunks(chunk_size) {
                let done = &done;
                let app = &app;
                let grayscale = &grayscale;
                scope.spawn(move || {
                    for (photo_id, path) in chunk {
                        // Generate every requested size from ONE extraction + decode (I7b):
                        // when previews are wanted, `warm_all_sizes` reads the RAW off the
                        // NAS once and downscales thumb+preview(+zoom) from the single decode,
                        // instead of a separate read/decode per size.
                        if include_previews {
                            let _ = crate::thumbnails::warm_all_sizes(path);
                        }
                        // Compute the B&W flag from the thumbnail (cheap, now cached by the
                        // step above when previews were warmed). A decode failure records
                        // `false` (can't confirm B&W) so the flag is always set, not NULL.
                        let gray = thumbnail_bytes(path)
                            .map(|t| crate::thumbnails::is_grayscale_jpeg(&t))
                            .unwrap_or(false);
                        grayscale.lock().unwrap().push((*photo_id, gray));
                        let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                        let _ = app.emit("cache:progress", CacheProgress { done: n, total });
                    }
                });
            }
        });
        grayscale.into_inner().unwrap()
    })
    .await
    .map_err(|e| e.to_string())?;

    // Persist the flags and refresh the monochrome auto-tag.
    with_catalog(&state, |c| {
        for (photo_id, gray) in &grayscale {
            c.set_grayscale(*photo_id, *gray)?;
        }
        c.apply_auto_tags()
    })?;

    Ok(())
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
    let bytes = tauri::async_runtime::spawn_blocking(move || {
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

