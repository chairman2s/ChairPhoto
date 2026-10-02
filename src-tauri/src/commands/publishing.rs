//! Helpers shared by the publishing modules (Flickr, SmugMug, LocalSend, Instagram):
//! app-settings access for OAuth credentials, and rendering a photo to an upload JPEG.
//!
//! Not commands themselves — `pub(super)` so the sibling publishing modules can use
//! them, but nothing here is re-exported to `lib.rs`. The job-scoped temp directory, the
//! upload filename and the render itself live in the core (`crate::publishing`), which the
//! GPUI app's publish targets share; these are thin wrappers over it.

#[cfg(any(feature = "flickr", feature = "smugmug"))]
use super::*;
#[cfg(any(feature = "flickr", feature = "smugmug", feature = "instagram"))]
pub(super) use crate::publishing::JobTempDir;
#[cfg(any(feature = "flickr", feature = "smugmug"))]
pub(super) use crate::publishing::RenderedUpload;
#[cfg(any(feature = "flickr", feature = "smugmug"))]
use tauri::{AppHandle, Manager};

/// Render `photo_id` (the chosen version) to an upload JPEG for `service` on a blocking
/// worker — `crate::publishing::render_upload_jpeg` against whichever catalog is open.
#[cfg(any(feature = "flickr", feature = "smugmug"))]
pub(super) async fn render_export_jpeg(
    app: &AppHandle,
    photo_id: i64,
    version_id: Option<i64>,
    service: &str,
    max_long_edge: Option<u32>,
) -> Result<RenderedUpload, String> {
    let state = app.state::<AppState>().inner().clone();
    let service = service.to_string();
    crate::app::spawn_blocking(move || {
        crate::publishing::render_upload_jpeg(&state, None, photo_id, version_id, &service, max_long_edge)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Read the user-configured max long edge (px) for a publishing module. Setting key is
/// `<prefix>.max_long_edge`. Returns `None` when the setting is absent, empty, or "0"
/// (= full resolution, the default).
#[cfg(any(feature = "flickr", feature = "smugmug"))]
pub(super) fn read_max_long_edge(app: &AppHandle, prefix: &str) -> Option<u32> {
    let raw = read_setting(app, &format!("{prefix}.max_long_edge")).unwrap_or_default();
    let v: u32 = raw.trim().parse().ok()?;
    if v == 0 { None } else { Some(v) }
}

#[cfg(any(feature = "flickr", feature = "smugmug"))]
pub(super) fn read_setting(app: &AppHandle, key: &str) -> Result<String, String> {
    let state = app.state::<AppState>();
    with_catalog(&state, |c| Ok(c.get_setting(key)?.unwrap_or_default()))
}

#[cfg(any(feature = "flickr", feature = "smugmug"))]
pub(super) fn write_setting(app: &AppHandle, key: &str, value: &str) -> Result<(), String> {
    let state = app.state::<AppState>();
    with_catalog(&state, |c| c.set_setting(key, value))
}

/// Read the user-entered app key + secret for a module (settings keys `<prefix>.api_key` /
/// `<prefix>.api_secret`), erroring with a clear hint if they're not set.
#[cfg(any(feature = "flickr", feature = "smugmug"))]
pub(super) fn read_app_keys(app: &AppHandle, prefix: &str) -> Result<(String, String), String> {
    let key = read_setting(app, &format!("{prefix}.api_key"))?;
    let secret = read_setting(app, &format!("{prefix}.api_secret"))?;
    if key.is_empty() || secret.is_empty() {
        return Err(format!(
            "Enter your {prefix} API key and secret in the module settings first."
        ));
    }
    Ok((key, secret))
}

/// Read a connected module's access token + secret, erroring if not connected.
#[cfg(any(feature = "flickr", feature = "smugmug"))]
pub(super) fn read_access(app: &AppHandle, prefix: &str) -> Result<(String, String), String> {
    let token = read_setting(app, &format!("{prefix}.access_token"))?;
    let secret = read_setting(app, &format!("{prefix}.access_secret"))?;
    if token.is_empty() || secret.is_empty() {
        return Err(format!("Connect {prefix} in the module settings first."));
    }
    Ok((token, secret))
}
