//! LocalSend commands — send a photo to a device on the LAN over LocalSend's
//! documented v2 HTTP protocol (UDP multicast discovery + prepare-upload/upload).
//!
//! Send-only. Gated on the `localsend` Cargo feature; see `docs/localsend.md`.

use super::*;
use std::path::PathBuf;
use tauri::{AppHandle, Emitter, Manager};

/// Discover LocalSend devices on the LAN. `timeout_ms` defaults to ~2.5s — long enough for
/// peers to reply to our announcement, short enough to keep Refresh snappy. A blocked
/// multicast just yields an empty list (the UI's manual-IP field is the fallback).
#[cfg(feature = "localsend")]
#[tauri::command]
pub async fn localsend_discover(
    timeout_ms: Option<u64>,
) -> Result<Vec<crate::localsend::Device>, String> {
    let timeout = timeout_ms.unwrap_or(2500);
    crate::localsend::discover(timeout).await
}

/// What a send produced: how many files reached the device and how many failed. A partial
/// send (some files uploaded, one rejected) surfaces as `failed > 0` with an error.
#[cfg(feature = "localsend")]
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendResult {
    pub sent: usize,
    pub failed: usize,
}

/// Progress event payload for a LocalSend transfer, emitted as `localsend:progress`.
#[cfg(feature = "localsend")]
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct LocalSendProgress {
    done: usize,
    total: usize,
}

/// Send the selected photos (the chosen version) to a LocalSend `device`. Renders each photo
/// to a temp full-res JPEG (named after the source like the Flickr/SmugMug path), then runs
/// the prepare-upload/upload handshake. `pin` is forwarded for PIN-protected receivers.
/// Records nothing — it's a transfer (Snapchat layers `recordPublication` on top in the UI).
#[cfg(feature = "localsend")]
#[tauri::command]
pub async fn localsend_send(
    app: AppHandle,
    photo_ids: Vec<i64>,
    version_id: Option<i64>,
    device: crate::localsend::Device,
    pin: Option<String>,
) -> Result<SendResult, String> {
    if photo_ids.is_empty() {
        return Err("Select at least one photo to send.".into());
    }

    // Render every selected photo to a temp JPEG off the catalog lock (RAW decode is slow).
    // `selected_version_id` applies only to its own photo; the rest send unedited (same
    // semantics as export). Unreachable originals are skipped and counted as failed.
    let paths = render_localsend_jpegs(&app, &photo_ids, version_id).await?;
    let skipped = photo_ids.len().saturating_sub(paths.len());
    if paths.is_empty() {
        return Err("None of the selected photos are available (originals offline?).".into());
    }

    let total = paths.len();
    let app_for_progress = app.clone();
    let pin = pin.filter(|p| !p.trim().is_empty());

    // The handshake + uploads are network IO (tokio); run them on the async runtime, emitting
    // `localsend:progress` after each file completes.
    let send_paths = paths.clone();
    let result = tauri::async_runtime::spawn(async move {
        crate::localsend::send_files(&device, &send_paths, pin.as_deref(), |done, total| {
            let _ = app_for_progress.emit("localsend:progress", LocalSendProgress { done, total });
        })
        .await
    })
    .await
    .map_err(|e| e.to_string())?;

    // Clean up the temp renders regardless of outcome.
    for p in &paths {
        let _ = std::fs::remove_file(p);
    }

    result?;
    Ok(SendResult {
        sent: total,
        failed: skipped,
    })
}

/// Render `photo_ids` (the selected version where it matches) to temp full-res JPEGs, named
/// after each source for a meaningful filename on the receiving device. Mirrors the
/// Flickr/SmugMug `render_export_jpeg` naming, but produces one path per resolvable photo.
#[cfg(feature = "localsend")]
async fn render_localsend_jpegs(
    app: &AppHandle,
    photo_ids: &[i64],
    version_id: Option<i64>,
) -> Result<Vec<PathBuf>, String> {
    let resolved = {
        let state = app.state::<AppState>();
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_ref().ok_or("No catalog is open")?;
        crate::export::resolve_originals(catalog, photo_ids, &[], version_id)
    };

    let dir = std::env::temp_dir().join("chairphoto-upload").join("localsend");
    let mut out = Vec::with_capacity(resolved.items.len());
    for item in resolved.items {
        // Name the file after the source (with the version suffix), like the service path.
        let stem = item
            .original
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "photo".into());
        let mut name = stem;
        if let Some(v) = &item.version_name {
            name.push_str(" - ");
            name.push_str(v);
        }
        let name = format!("{}.jpg", sanitize_upload_filename(&name));
        let target = dir.join(name);
        let t = target.clone();
        // Full resolution (no downscale): the device decides what to do with it (Snapchat
        // downscales to 9:16 on the phone). Skip a photo whose render fails rather than
        // aborting the whole send.
        match tauri::async_runtime::spawn_blocking(move || {
            crate::export::write_item_jpeg(&item, None, &t)
        })
        .await
        .map_err(|e| e.to_string())?
        {
            Ok(()) => out.push(target),
            Err(e) => eprintln!("localsend: render failed for a photo: {e}"),
        }
    }
    Ok(out)
}

/// Keep a filename to safe ASCII for the outgoing render (filename-friendly), collapsing
/// anything else to `_`. Standalone copy so it compiles without the flickr/smugmug features.
#[cfg(feature = "localsend")]
fn sanitize_upload_filename(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.' | '(' | ')') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let s = s.trim().to_string();
    if s.is_empty() {
        "photo".into()
    } else {
        s
    }
}

