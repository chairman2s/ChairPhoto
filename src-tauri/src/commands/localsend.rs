//! LocalSend commands — send a photo to a device on the LAN over LocalSend's
//! documented v2 HTTP protocol (UDP multicast discovery + prepare-upload/upload).
//!
//! Send-only. Gated on the `localsend` Cargo feature; see `docs/localsend.md`.

use super::publishing::{upload_file_name, JobTempDir};
use super::*;
use std::path::{Path, PathBuf};
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
    // `_dir` is this send's own temp directory; it is removed when this function returns,
    // whichever way it returns.
    let (_dir, paths) = render_localsend_jpegs(&app, &photo_ids, version_id).await?;
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

    // The temp renders (and any sidecar exiftool wrote beside them) go with `_dir`.
    result?;
    Ok(SendResult {
        sent: total,
        failed: skipped,
    })
}

/// Render `photo_ids` (the selected version where it matches) to temp full-res JPEGs, named
/// after each source for a meaningful filename on the receiving device. Mirrors the
/// Flickr/SmugMug `render_export_jpeg` naming, but produces one path per resolvable photo.
/// Returns the send's own temp directory alongside the paths — hold it until the transfer
/// is done; dropping it deletes the renders.
#[cfg(feature = "localsend")]
async fn render_localsend_jpegs(
    app: &AppHandle,
    photo_ids: &[i64],
    version_id: Option<i64>,
) -> Result<(JobTempDir, Vec<PathBuf>), String> {
    let resolved = {
        let state = app.state::<AppState>();
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_ref().ok_or("No catalog is open")?;
        crate::export::resolve_originals(catalog, photo_ids, &[], version_id)
    };

    let dir = JobTempDir::new("localsend")?;
    let mut out = Vec::with_capacity(resolved.items.len());
    for item in resolved.items {
        // Name the file after the source (with the version suffix), like the service path.
        // Two selected photos can share a stem (the same basename in different folders), so
        // disambiguate inside this send's directory rather than rendering over the earlier
        // one and sending it twice under the other photo's name.
        let name = upload_file_name(&item.original, item.version_name.as_deref());
        let target = match unique_in_dir(&dir, &name) {
            Ok(t) => t,
            // Skip this photo, like a failed render below: it is then counted as failed,
            // which is the truth. Sending it under a name another render already owns is
            // the one thing we must not do.
            Err(e) => {
                eprintln!("localsend: {e}");
                continue;
            }
        };
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
    Ok((dir, out))
}

/// The highest `" (n)"` suffix tried before `unique_in_dir` gives up.
#[cfg(feature = "localsend")]
const MAX_DISAMBIGUATION: u32 = 9_999;

/// `dir/name`, suffixed `" (2)"`, `" (3)"`… if a previous render in this send already took
/// it. The directory belongs to this send alone, so `exists()` sees only our own renders.
///
/// Erroring when every suffix is taken is the point: returning the taken path instead would
/// reintroduce exactly the overwrite this exists to prevent, in the one case where the
/// collision is not hypothetical but proven.
#[cfg(feature = "localsend")]
fn unique_in_dir(dir: &JobTempDir, name: &str) -> Result<PathBuf, String> {
    unique_in_dir_up_to(dir, name, MAX_DISAMBIGUATION)
}

#[cfg(feature = "localsend")]
fn unique_in_dir_up_to(dir: &JobTempDir, name: &str, max: u32) -> Result<PathBuf, String> {
    let first = dir.join(name);
    if !first.exists() {
        return Ok(first);
    }
    let base = Path::new(name);
    let stem = base.file_stem().and_then(|s| s.to_str()).unwrap_or("photo");
    let ext = base.extension().and_then(|s| s.to_str()).unwrap_or("jpg");
    for n in 2..=max {
        let candidate = dir.join(&format!("{stem} ({n}).{ext}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "no free name for \"{name}\" in this send's temp directory after {max} tries — \
         skipping this photo rather than overwriting an earlier render"
    ))
}

#[cfg(all(test, feature = "localsend"))]
mod tests {
    use super::*;

    /// Two selected photos sharing a basename must reach the device as two files.
    #[test]
    fn a_taken_name_is_disambiguated_rather_than_reused() {
        let dir = JobTempDir::new("localsend").unwrap();
        let first = unique_in_dir(&dir, "DSC01234.jpg").unwrap();
        std::fs::write(&first, b"first").unwrap();
        let second = unique_in_dir(&dir, "DSC01234.jpg").unwrap();
        assert_ne!(first, second, "the second render would overwrite the first");
        assert_eq!(second.file_name().unwrap(), "DSC01234 (2).jpg");
        std::fs::write(&second, b"second").unwrap();
        assert_eq!(std::fs::read(&first).unwrap(), b"first");
    }

    /// Running out of suffixes must fail, not hand back the name already in use.
    #[test]
    fn exhausting_the_suffixes_errors_instead_of_colliding() {
        let dir = JobTempDir::new("localsend").unwrap();
        for name in ["DSC01234.jpg", "DSC01234 (2).jpg", "DSC01234 (3).jpg"] {
            std::fs::write(dir.join(name), b"taken").unwrap();
        }
        let err = unique_in_dir_up_to(&dir, "DSC01234.jpg", 3)
            .expect_err("every candidate name is taken");
        assert!(err.contains("DSC01234.jpg"), "{err}");
    }
}

