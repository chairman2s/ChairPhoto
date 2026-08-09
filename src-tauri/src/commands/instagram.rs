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

    // One temp directory per post, removed when `dir` drops — on the error paths below as
    // well. The old fixed `<temp>/chairphoto-instagram.jpg` let two concurrent posts render
    // over each other (the second post could upload the first post's photo) and put a
    // predictable, guessable path in a shared /tmp. The filename Chrome sees is unchanged.
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
    let outcome =
        crate::instagram::post(&img_path, &caption, &profile_dir, &chrome, publish).await?;

    // The Instagram module (frontend) records the publication when the outcome is "posted",
    // through the same api.recordPublication contract as the other publishers.
    Ok(serde_json::to_value(outcome)
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

// --- Flickr / SmugMug publishing (official OAuth 1.0a APIs) ----------------------------
// Shared helpers (compiled when either module's feature is on): render the selected version
// to a temp JPEG, and read/write the module's namespaced settings (api_key/secret/tokens).

