//! SmugMug publishing commands — OAuth 1.0a token dance, album listing/creation,
//! and upload.
//!
//! Gated on the `smugmug` Cargo feature; see `docs/smugmug.md` and `smugmug/`.

use super::publishing::*;
use tauri::AppHandle;

#[cfg(feature = "smugmug")]
#[tauri::command]
pub async fn smugmug_begin_auth(app: AppHandle) -> Result<String, String> {
    let (key, secret) = read_app_keys(&app, "smugmug")?;
    let rt = crate::smugmug::begin_auth(&key, &secret).await?;
    write_setting(&app, "smugmug.request_token", &rt.token)?;
    write_setting(&app, "smugmug.request_secret", &rt.secret)?;
    Ok(rt.authorize_url)
}

#[cfg(feature = "smugmug")]
#[tauri::command]
pub async fn smugmug_complete_auth(app: AppHandle, verifier: String) -> Result<(), String> {
    let (key, secret) = read_app_keys(&app, "smugmug")?;
    let req_token = read_setting(&app, "smugmug.request_token")?;
    let req_secret = read_setting(&app, "smugmug.request_secret")?;
    if req_token.is_empty() {
        return Err("Start with Connect before entering the verifier.".into());
    }
    let at = crate::smugmug::complete_auth(&key, &secret, &req_token, &req_secret, verifier.trim()).await?;
    write_setting(&app, "smugmug.access_token", &at.token)?;
    write_setting(&app, "smugmug.access_secret", &at.secret)?;
    write_setting(&app, "smugmug.request_token", "")?;
    write_setting(&app, "smugmug.request_secret", "")?;
    Ok(())
}

#[cfg(feature = "smugmug")]
#[tauri::command]
pub fn smugmug_connected(app: AppHandle) -> Result<bool, String> {
    Ok(!read_setting(&app, "smugmug.access_token")?.is_empty())
}

/// The authenticated user's albums (upload targets) — `uri` + `name`.
#[cfg(feature = "smugmug")]
#[tauri::command]
pub async fn smugmug_list_albums(app: AppHandle) -> Result<Vec<crate::smugmug::Album>, String> {
    let (key, secret) = read_app_keys(&app, "smugmug")?;
    let (token, token_secret) = read_access(&app, "smugmug")?;
    crate::smugmug::list_albums(&key, &secret, &token, &token_secret).await
}

/// Create a new SmugMug album under the user's root folder; returns it for the picker.
#[cfg(feature = "smugmug")]
#[tauri::command]
pub async fn smugmug_create_album(
    app: AppHandle,
    name: String,
) -> Result<crate::smugmug::Album, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Enter an album name.".into());
    }
    let (key, secret) = read_app_keys(&app, "smugmug")?;
    let (token, token_secret) = read_access(&app, "smugmug")?;
    crate::smugmug::create_album(&key, &secret, &token, &token_secret, name).await
}

/// Render the selected version and upload it to a SmugMug album. Returns the image URL.
#[cfg(feature = "smugmug")]
#[tauri::command]
pub async fn post_to_smugmug(
    app: AppHandle,
    photo_id: i64,
    version_id: Option<i64>,
    album_uri: String,
    title: String,
    caption: String,
) -> Result<String, String> {
    if album_uri.trim().is_empty() {
        return Err("Choose a SmugMug album to upload into.".into());
    }
    let (key, secret) = read_app_keys(&app, "smugmug")?;
    let (token, token_secret) = read_access(&app, "smugmug")?;
    let max_long_edge = read_max_long_edge(&app, "smugmug");
    let img = render_export_jpeg(&app, photo_id, version_id, "smugmug", max_long_edge).await?;
    crate::smugmug::upload(
        &key, &secret, &token, &token_secret, &album_uri, &img, &title, &caption,
    )
    .await
}

// --- LocalSend (send photos to a LAN device over the LocalSend v2 HTTP protocol) -------
// A *transfer*, not a publication: these commands render the selected version(s) to temp
// full-res JPEG(s) and hand the paths to `localsend::send_files`. The Snapchat module reuses
// `localsend_send` and records the publication itself (the host stamps its marker). All
// network/IO is in Rust; the slow render runs off the catalog lock via spawn_blocking.

