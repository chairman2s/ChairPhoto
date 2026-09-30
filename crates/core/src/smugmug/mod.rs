//! Publish a photo to SmugMug via its official OAuth 1.0a API (the `smugmug` Cargo
//! feature). Pure transport: the auth dance, listing the user's albums, and the upload —
//! all signed with [`crate::oauth1`]. Commands in `commands.rs` wire these to the catalog
//! and the render path; the frontend module records the publication. Endpoints per the
//! SmugMug API v2 docs (api.smugmug.com/api/v2/doc).

use crate::oauth1;
use std::collections::BTreeMap;
use std::path::Path;

const REQUEST_TOKEN_URL: &str = "https://secure.smugmug.com/services/oauth/1.0a/getRequestToken";
const AUTHORIZE_URL: &str = "https://secure.smugmug.com/services/oauth/1.0a/authorize";
const ACCESS_TOKEN_URL: &str = "https://secure.smugmug.com/services/oauth/1.0a/getAccessToken";
const API_BASE: &str = "https://api.smugmug.com";
const AUTHUSER_URL: &str = "https://api.smugmug.com/api/v2!authuser";
const UPLOAD_URL: &str = "https://upload.smugmug.com/";

pub struct RequestToken {
    pub token: String,
    pub secret: String,
    pub authorize_url: String,
}

pub struct AccessToken {
    pub token: String,
    pub secret: String,
}

/// An album the user can upload into. `uri` is the SmugMug AlbumUri (e.g. `/api/v2/album/abc`).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Album {
    pub uri: String,
    pub name: String,
}

/// Step 1: request token (callback "oob") + the authorize URL (full access, add permission).
pub async fn begin_auth(key: &str, secret: &str) -> Result<RequestToken, String> {
    let params = oauth1::signed_params(
        "GET",
        REQUEST_TOKEN_URL,
        key,
        secret,
        None,
        "",
        &[("oauth_callback", "oob")],
    );
    let url = format!("{REQUEST_TOKEN_URL}?{}", oauth1::query_string(&params));
    let body = http_get_text(&url, None).await?;
    let kv = oauth1::parse_kv(&body);
    let token = kv
        .get("oauth_token")
        .cloned()
        .ok_or_else(|| format!("SmugMug returned no request token: {body}"))?;
    let secret = kv.get("oauth_token_secret").cloned().unwrap_or_default();
    let authorize_url = format!(
        "{AUTHORIZE_URL}?oauth_token={}&Access=Full&Permissions=Add",
        oauth1::percent_encode(&token)
    );
    Ok(RequestToken {
        token,
        secret,
        authorize_url,
    })
}

/// Step 2: exchange the authorized request token + verifier for an access token.
pub async fn complete_auth(
    key: &str,
    secret: &str,
    request_token: &str,
    request_secret: &str,
    verifier: &str,
) -> Result<AccessToken, String> {
    let params = oauth1::signed_params(
        "GET",
        ACCESS_TOKEN_URL,
        key,
        secret,
        Some(request_token),
        request_secret,
        &[("oauth_verifier", verifier)],
    );
    let url = format!("{ACCESS_TOKEN_URL}?{}", oauth1::query_string(&params));
    let body = http_get_text(&url, None).await?;
    let kv = oauth1::parse_kv(&body);
    let token = kv
        .get("oauth_token")
        .cloned()
        .ok_or_else(|| format!("SmugMug authorization failed: {body}"))?;
    let secret = kv.get("oauth_token_secret").cloned().unwrap_or_default();
    Ok(AccessToken { token, secret })
}

/// List the authenticated user's albums (resolves the user, then their album list).
pub async fn list_albums(
    key: &str,
    secret: &str,
    token: &str,
    token_secret: &str,
) -> Result<Vec<Album>, String> {
    // 1. Who am I → the UserAlbums URI.
    let me = signed_get_json(AUTHUSER_URL, &[], key, secret, token, token_secret).await?;
    let albums_path = me["Response"]["User"]["Uris"]["UserAlbums"]["Uri"]
        .as_str()
        .ok_or("SmugMug: couldn't find the user's albums URI")?;
    let albums_url = format!("{API_BASE}{albums_path}");

    // 2. The album list (cap the page; album management/creation is out of scope).
    let list = signed_get_json(
        &albums_url,
        &[("count", "500"), ("_filter", "Name,Uri"), ("_verbosity", "1")],
        key,
        secret,
        token,
        token_secret,
    )
    .await?;
    let arr = list["Response"]["Album"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    Ok(arr
        .iter()
        .filter_map(|a| {
            let uri = a["Uri"].as_str()?.to_string();
            let name = a["Name"].as_str().unwrap_or("(untitled)").to_string();
            Some(Album { uri, name })
        })
        .collect())
}

/// Create a new album under the user's root folder. Returns the new album (uri + name).
pub async fn create_album(
    key: &str,
    secret: &str,
    token: &str,
    token_secret: &str,
    name: &str,
) -> Result<Album, String> {
    // The user's root folder is where top-level albums are created.
    let me = signed_get_json(AUTHUSER_URL, &[], key, secret, token, token_secret).await?;
    let folder = me["Response"]["User"]["Uris"]["Folder"]["Uri"]
        .as_str()
        .ok_or("SmugMug: couldn't find the user's root folder")?;
    let create_url = format!("{API_BASE}{folder}!albums");

    // JSON body is NOT part of the OAuth signature (only form-encoded bodies are), so we
    // sign the bare POST like the binary upload.
    let params = oauth1::signed_params("POST", &create_url, key, secret, Some(token), token_secret, &[]);
    let body = serde_json::json!({ "Name": name, "UrlName": sanitize_url_name(name) });

    let client = reqwest::Client::new();
    let resp = client
        .post(&create_url)
        .header("Authorization", oauth1::auth_header(&params))
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&body).unwrap_or_default())
        .send()
        .await
        .map_err(|e| format!("SmugMug create-album request failed: {e}"))?;
    let text = resp.text().await.map_err(|e| e.to_string())?;
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("SmugMug: bad JSON ({e}): {text}"))?;
    let album = &v["Response"]["Album"];
    let uri = album["Uri"]
        .as_str()
        .ok_or_else(|| format!("SmugMug create-album failed: {text}"))?
        .to_string();
    let name = album["Name"].as_str().unwrap_or(name).to_string();
    Ok(Album { uri, name })
}

/// SmugMug UrlName must be a URL-safe TitleCase-ish token starting with a letter.
fn sanitize_url_name(name: &str) -> String {
    let s = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut ch = w.chars();
            match ch.next() {
                Some(f) => f.to_uppercase().collect::<String>() + ch.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("-");
    let s = if s.is_empty() { "Album".to_string() } else { s };
    if s.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false) {
        s
    } else {
        format!("A{s}")
    }
}

/// Upload `image` into `album_uri`. Returns the new image's URL (or URI).
pub async fn upload(
    key: &str,
    secret: &str,
    token: &str,
    token_secret: &str,
    album_uri: &str,
    image: &Path,
    title: &str,
    caption: &str,
) -> Result<String, String> {
    let bytes = std::fs::read(image).map_err(|e| format!("couldn't read render: {e}"))?;
    let filename = image
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("photo.jpg")
        .to_string();

    // The upload body is raw binary (not a signed parameter); only oauth_* are signed.
    let params = oauth1::signed_params("POST", UPLOAD_URL, key, secret, Some(token), token_secret, &[]);

    let client = reqwest::Client::new();
    let resp = client
        .post(UPLOAD_URL)
        .header("Authorization", oauth1::auth_header(&params))
        .header("X-Smug-AlbumUri", album_uri)
        .header("X-Smug-ResponseType", "JSON")
        .header("X-Smug-Version", "v2")
        .header("X-Smug-FileName", filename)
        .header("X-Smug-Title", title)
        .header("X-Smug-Caption", caption)
        .header("Content-Type", "image/jpeg")
        .body(bytes)
        .send()
        .await
        .map_err(|e| format!("SmugMug upload request failed: {e}"))?;
    let text = resp.text().await.map_err(|e| e.to_string())?;
    parse_upload_response(&text)
}

async fn signed_get_json(
    url_base: &str,
    query: &[(&str, &str)],
    key: &str,
    secret: &str,
    token: &str,
    token_secret: &str,
) -> Result<serde_json::Value, String> {
    let params = oauth1::signed_params("GET", url_base, key, secret, Some(token), token_secret, query);
    // Request URL carries the non-oauth query params; oauth_* go in the Authorization header.
    let req_query: BTreeMap<String, String> = query
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    let url = if req_query.is_empty() {
        url_base.to_string()
    } else {
        format!("{url_base}?{}", oauth1::query_string(&req_query))
    };
    let text = http_get_text(&url, Some(&oauth1::auth_header(&params))).await?;
    serde_json::from_str(&text).map_err(|e| format!("SmugMug: bad JSON ({e}): {text}"))
}

async fn http_get_text(url: &str, auth: Option<&str>) -> Result<String, String> {
    let client = reqwest::Client::new();
    let mut req = client.get(url).header("Accept", "application/json");
    if let Some(a) = auth {
        req = req.header("Authorization", a);
    }
    req.send()
        .await
        .map_err(|e| format!("SmugMug request failed: {e}"))?
        .text()
        .await
        .map_err(|e| e.to_string())
}

/// Upload reply is JSON: `{"stat":"ok","Image":{"URL":"…","ImageUri":"…"}}` on success.
fn parse_upload_response(text: &str) -> Result<String, String> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("SmugMug: bad upload response ({e}): {text}"))?;
    if v["stat"].as_str() == Some("ok") {
        let img = &v["Image"];
        let url = img["URL"]
            .as_str()
            .or_else(|| img["ImageUri"].as_str())
            .unwrap_or("")
            .to_string();
        return Ok(url);
    }
    let msg = v["message"].as_str().unwrap_or(text);
    Err(format!("SmugMug upload failed: {msg}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ok_and_error() {
        let ok = r#"{"stat":"ok","Image":{"URL":"https://example.smugmug.com/x","ImageUri":"/api/v2/image/Y"}}"#;
        assert_eq!(parse_upload_response(ok).unwrap(), "https://example.smugmug.com/x");
        let fail = r#"{"stat":"fail","message":"Album not found"}"#;
        assert!(parse_upload_response(fail).unwrap_err().contains("Album not found"));
    }

    #[test]
    fn url_name_is_titlecased_and_starts_with_letter() {
        assert_eq!(sanitize_url_name("My Trip 2026"), "My-Trip-2026");
        assert_eq!(sanitize_url_name("2026 summer"), "A2026-Summer");
        assert_eq!(sanitize_url_name("   "), "Album");
    }
}
