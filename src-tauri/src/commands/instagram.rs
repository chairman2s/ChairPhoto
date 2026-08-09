//! Instagram publishing commands — drives a real Chrome over the DevTools Protocol.
//!
//! Gated on the `instagram` Cargo feature; see `docs/instagram.md`. Brittle web automation by nature: it is
//! supervised by default and stops before Share. See `instagram/`.

use super::publishing::JobTempDir;
use super::*;
use std::path::PathBuf;
use tauri::{AppHandle, Manager, State};

/// Render the selected version to an Instagram-sized JPEG and post it by driving Chrome
/// (see `instagram`). `publish` clicks Share; otherwise the post is composed and left for
/// review. Returns a status string: "posted", "awaitingReview", or "needsLogin".
#[cfg(feature = "instagram")]
#[tauri::command]
pub async fn post_to_instagram(
    app: AppHandle,
    photo_id: i64,
    version_id: Option<i64>,
    caption: String,
    publish: bool,
) -> Result<String, String> {
    // Resolve the photo under the lock, then render the JPEG off-lock (RAW decode is slow).
    let resolved = {
        let state = app.state::<AppState>();
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_ref().ok_or("No catalog is open")?;
        crate::export::resolve_originals(catalog, &[photo_id], &[], version_id)
    };
    let item = resolved
        .items
        .into_iter()
        .next()
        .ok_or("Photo is unavailable (original offline?)")?;

    // One temp directory per post, removed when `dir` drops — including on the render error
    // below, which happens before Chrome has ever seen the path. The old fixed
    // `<temp>/chairphoto-instagram.jpg` let two concurrent posts render over each other (the
    // second post could upload the first post's photo) and put a predictable, guessable path
    // in a shared /tmp. The filename Chrome sees is unchanged. Whether the directory survives
    // the *post* depends on the outcome — see the match below.
    let dir = JobTempDir::new("instagram")?;
    let img_path = dir.join("chairphoto-instagram.jpg");
    let out = img_path.clone();
    tauri::async_runtime::spawn_blocking(move || {
        crate::export::write_item_jpeg(&item, Some(1080), &out)
    })
    .await
    .map_err(|e| e.to_string())??;

    let profile_dir = instagram_profile_dir()?;
    let chrome = find_chrome().ok_or(
        "Chrome/Chromium not found — install Google Chrome or Chromium to post to Instagram",
    )?;
    let outcome = crate::instagram::post(&img_path, &caption, &profile_dir, &chrome, publish).await;

    // Unlike an API upload, this command returning does not mean the bytes have been sent.
    // `DOM.setFileInputFiles` hands Chrome a *path*: the `File` on the composer's input
    // reads from disk lazily, so the render has to outlive us whenever the post is still
    // open in a browser we deliberately do not own.
    //
    // Measured over CDP against Chrome 150 with the same call `crate::instagram` makes —
    // attach the file, draw it as a preview, delete it, then read it back: `arrayBuffer()`
    // fails with `NotFoundError`, a `FormData` POST fails with `TypeError: Failed to fetch`,
    // and `file.size` reads 0. Displaying the image does not cache it. So deleting the
    // render at `AwaitingReview` would leave the user a composed post whose Share click
    // cannot work.
    //
    // Posted — Instagram has the bytes and confirmed it. NeedsLogin — we returned before
    // touching the file input. Both are done with the render. An error can land either side
    // of the attach and leaves the composer on screen, so it is treated as still in use.
    // Nothing is leaked: `sweep_abandoned` reclaims a kept directory once it is stale.
    match &outcome {
        Ok(crate::instagram::PostOutcome::Posted | crate::instagram::PostOutcome::NeedsLogin) => {}
        Ok(crate::instagram::PostOutcome::AwaitingReview) | Err(_) => dir.keep(),
    }

    // The Instagram module (frontend) records the publication when the outcome is "posted",
    // through the same api.recordPublication contract as the other publishers.
    Ok(serde_json::to_value(outcome?)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| "posted".into()))
}

/// Build a suggested Instagram caption for a photo: its title/description followed by its
/// keywords as `#hashtags` (de-duped, capped at 30). Used to prefill the caption box.
#[cfg(feature = "instagram")]
#[tauri::command]
pub fn build_instagram_caption(
    state: State<'_, AppState>,
    photo_id: i64,
) -> Result<String, String> {
    with_catalog(&state, |c| {
        let iptc = c.get_iptc(photo_id).unwrap_or_default();
        let keywords = c
            .assemble_export_keywords(photo_id, &[])
            .map(|k| k.flat)
            .unwrap_or_default();
        let title = if iptc.title.trim().is_empty() {
            iptc.headline
        } else {
            iptc.title
        };
        Ok(crate::instagram::build_caption(
            &title,
            &iptc.description,
            &keywords,
            30,
        ))
    })
}

/// The persistent Chrome profile for Instagram (keeps the login between posts).
#[cfg(feature = "instagram")]
fn instagram_profile_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME is not set")?;
    Ok(std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"))
        .join("chairphoto")
        .join("ig-profile"))
}

/// Find a Chrome/Chromium executable on PATH.
#[cfg(feature = "instagram")]
fn find_chrome() -> Option<String> {
    for bin in [
        "google-chrome-stable",
        "google-chrome",
        "chromium",
        "chromium-browser",
        "brave",
    ] {
        if let Ok(out) = std::process::Command::new("which").arg(bin).output() {
            if out.status.success() {
                let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !path.is_empty() {
                    return Some(path);
                }
            }
        }
    }
    None
}

