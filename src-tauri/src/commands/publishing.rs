//! Helpers shared by the publishing modules (Flickr, SmugMug, LocalSend, Instagram):
//! app-settings access for OAuth credentials, and rendering a photo to an upload JPEG.
//!
//! Not commands themselves — `pub(super)` so the sibling publishing modules can use
//! them, but nothing here is re-exported to `lib.rs`.

use super::*;
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

#[cfg(any(feature = "flickr", feature = "smugmug"))]
pub(super) async fn render_export_jpeg(
    app: &AppHandle,
    photo_id: i64,
    version_id: Option<i64>,
    service: &str,
    max_long_edge: Option<u32>,
) -> Result<PathBuf, String> {
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

    // Name the upload after the source (with the version suffix), so the service shows a
    // meaningful filename (e.g. "DSC01234.jpg", "DSC01234 - Punchy crop.jpg") instead of a
    // temp name. A per-service temp subdir avoids any clash between concurrent uploads.
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
    let name = format!("{}.jpg", sanitize_filename(&name));
    let out = std::env::temp_dir().join("chairphoto-upload").join(service).join(name);

    let o = out.clone();
    // Render the JPEG, applying the per-module long-edge limit when set (0/None = full
    // resolution, the default for portfolio/archival services like Flickr/SmugMug).
    tauri::async_runtime::spawn_blocking(move || {
        crate::export::write_item_jpeg_with_long_edge(&item, max_long_edge, &o)
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(out)
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

/// Keep a filename to safe ASCII (filename- and HTTP-header-friendly: SmugMug sends it as a
/// header), collapsing anything else to `_`.
#[cfg(any(feature = "flickr", feature = "smugmug"))]
pub(super) fn sanitize_filename(name: &str) -> String {
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

// --- Flickr ---

