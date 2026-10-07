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

/// Where each call goes: [`Endpoints::LIVE`] for every public function, a loopback stub in
/// tests. Every signed request sends its OAuth protocol params (the token and signature
/// included) in the `Authorization` header, never the URL (#190).
struct Endpoints<'a> {
    request_token: &'a str,
    authorize: &'a str,
    access_token: &'a str,
    api_base: &'a str,
    authuser: &'a str,
    upload: &'a str,
}

impl Endpoints<'static> {
    const LIVE: Endpoints<'static> = Endpoints {
        request_token: REQUEST_TOKEN_URL,
        authorize: AUTHORIZE_URL,
        access_token: ACCESS_TOKEN_URL,
        api_base: API_BASE,
        authuser: AUTHUSER_URL,
        upload: UPLOAD_URL,
    };
}

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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Album {
    pub uri: String,
    pub name: String,
}

/// Step 1: request token (callback "oob") + the authorize URL (full access, add permission).
pub async fn begin_auth(key: &str, secret: &str) -> Result<RequestToken, String> {
    begin_auth_at(&Endpoints::LIVE, key, secret).await
}

async fn begin_auth_at(ep: &Endpoints<'_>, key: &str, secret: &str) -> Result<RequestToken, String> {
    let params = oauth1::signed_params(
        "GET",
        ep.request_token,
        key,
        secret,
        None,
        "",
        &[("oauth_callback", "oob")],
    );
    let body = signed_get_text(ep.request_token, &params).await?;
    let kv = oauth1::parse_kv(&body);
    let token = kv
        .get("oauth_token")
        .cloned()
        .ok_or_else(|| format!("SmugMug returned no request token: {body}"))?;
    let secret = kv.get("oauth_token_secret").cloned().unwrap_or_default();
    // The one URL that carries a token: the short-lived request token, which the user's
    // browser must present to authorize it (RFC 5849 §2.2). Never the access token.
    let authorize_url = format!(
        "{}?oauth_token={}&Access=Full&Permissions=Add",
        ep.authorize,
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
    complete_auth_at(&Endpoints::LIVE, key, secret, request_token, request_secret, verifier).await
}

async fn complete_auth_at(
    ep: &Endpoints<'_>,
    key: &str,
    secret: &str,
    request_token: &str,
    request_secret: &str,
    verifier: &str,
) -> Result<AccessToken, String> {
    let params = oauth1::signed_params(
        "GET",
        ep.access_token,
        key,
        secret,
        Some(request_token),
        request_secret,
        &[("oauth_verifier", verifier)],
    );
    let body = signed_get_text(ep.access_token, &params).await?;
    let kv = oauth1::parse_kv(&body);
    let token = kv
        .get("oauth_token")
        .cloned()
        .ok_or_else(|| {
            oauth1::redact(&format!("SmugMug authorization failed: {body}"), &[request_token, verifier])
        })?;
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
    list_albums_at(&Endpoints::LIVE, key, secret, token, token_secret).await
}

async fn list_albums_at(
    ep: &Endpoints<'_>,
    key: &str,
    secret: &str,
    token: &str,
    token_secret: &str,
) -> Result<Vec<Album>, String> {
    // 1. Who am I → the UserAlbums URI.
    let me = signed_get_json(ep.authuser, &[], key, secret, token, token_secret).await?;
    let albums_path = me["Response"]["User"]["Uris"]["UserAlbums"]["Uri"]
        .as_str()
        .ok_or("SmugMug: couldn't find the user's albums URI")?;
    let albums_url = api_url(ep.api_base, albums_path)?;

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
    create_album_at(&Endpoints::LIVE, key, secret, token, token_secret, name).await
}

async fn create_album_at(
    ep: &Endpoints<'_>,
    key: &str,
    secret: &str,
    token: &str,
    token_secret: &str,
    name: &str,
) -> Result<Album, String> {
    // The user's root folder is where top-level albums are created.
    let me = signed_get_json(ep.authuser, &[], key, secret, token, token_secret).await?;
    let folder = me["Response"]["User"]["Uris"]["Folder"]["Uri"]
        .as_str()
        .ok_or("SmugMug: couldn't find the user's root folder")?;
    let create_url = format!("{}!albums", api_url(ep.api_base, folder)?);

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
        .map_err(|e| format!("SmugMug create-album request failed: {}", e.without_url()))?;
    let text = resp.text().await.map_err(|e| e.without_url().to_string())?;
    let scrub = |e: String| oauth1::redact(&e, &[token]);
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| scrub(format!("SmugMug: bad JSON ({e}): {text}")))?;
    let album = &v["Response"]["Album"];
    let uri = album["Uri"]
        .as_str()
        .ok_or_else(|| scrub(format!("SmugMug create-album failed: {text}")))?
        .to_string();
    let name = album["Name"].as_str().unwrap_or(name).to_string();
    Ok(Album { uri, name })
}

/// `api_base` joined with a Uri from a SmugMug reply. The reply's Uri must be an absolute
/// path (`/api/v2/…`): anything else (`.evil.example/x`, `@evil.example/x`, `//evil.example`)
/// could move the request, and its signed `Authorization` header, to another host.
fn api_url(api_base: &str, uri: &str) -> Result<String, String> {
    if uri.starts_with('/') && !uri.starts_with("//") && !uri.contains('\\') {
        Ok(format!("{api_base}{uri}"))
    } else {
        Err(format!("SmugMug returned an unexpected URI: {uri:?}"))
    }
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
    upload_at(&Endpoints::LIVE, key, secret, token, token_secret, album_uri, image, title, caption).await
}

#[allow(clippy::too_many_arguments)]
async fn upload_at(
    ep: &Endpoints<'_>,
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
    let params = oauth1::signed_params("POST", ep.upload, key, secret, Some(token), token_secret, &[]);

    let client = reqwest::Client::new();
    let resp = client
        .post(ep.upload)
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
        .map_err(|e| format!("SmugMug upload request failed: {}", e.without_url()))?;
    let text = resp.text().await.map_err(|e| e.without_url().to_string())?;
    parse_upload_response(&text).map_err(|e| oauth1::redact(&e, &[token]))
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
    let text = signed_get_text(url_base, &params).await?;
    serde_json::from_str(&text)
        .map_err(|e| oauth1::redact(&format!("SmugMug: bad JSON ({e}): {text}"), &[token]))
}

/// GET `url` with the request params of `params` as its query and the protocol params in the
/// `Authorization` header (RFC 5849 §3.5.1). Errors drop the URL (`without_url`).
async fn signed_get_text(url: &str, params: &BTreeMap<String, String>) -> Result<String, String> {
    reqwest::Client::new()
        .get(oauth1::request_url(url, params))
        .header("Accept", "application/json")
        .header("Authorization", oauth1::auth_header(params))
        .send()
        .await
        .map_err(|e| format!("SmugMug request failed: {}", e.without_url()))?
        .text()
        .await
        .map_err(|e| e.without_url().to_string())
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

    // ── OAuth on the wire (#190): protocol params in the header, never the URL ──
    //
    // Every call goes to a loopback stub (`oauth1::stub`), which records the request and
    // re-derives the HMAC-SHA1 signature from what was actually sent.

    use crate::oauth1::stub::{assert_oauth_in_header_only, Server};

    const KEY: &str = "consumer-key";
    const SECRET: &str = "consumer-secret";
    const TOKEN: &str = "smug-access-token";
    const TOKEN_SECRET: &str = "access-secret";

    struct Urls {
        request_token: String,
        authorize: String,
        access_token: String,
        api_base: String,
        authuser: String,
        upload: String,
    }

    fn urls(s: &Server) -> Urls {
        Urls {
            request_token: s.url("/services/oauth/1.0a/getRequestToken"),
            authorize: s.url("/services/oauth/1.0a/authorize"),
            access_token: s.url("/services/oauth/1.0a/getAccessToken"),
            api_base: s.url(""),
            authuser: s.url("/api/v2!authuser"),
            upload: s.url("/"),
        }
    }

    fn ep(u: &Urls) -> Endpoints<'_> {
        Endpoints {
            request_token: &u.request_token,
            authorize: &u.authorize,
            access_token: &u.access_token,
            api_base: &u.api_base,
            authuser: &u.authuser,
            upload: &u.upload,
        }
    }

    async fn stub(routes: &[(&str, &str)]) -> Server {
        Server::start(routes.iter().map(|(p, b)| (p.to_string(), b.to_string())).collect()).await
    }

    const AUTHUSER: &str = r#"{"Response":{"User":{"Uris":{"UserAlbums":{"Uri":"/api/v2/user/me!albums"},"Folder":{"Uri":"/api/v2/folder/user/me"}}}}}"#;

    #[tokio::test]
    async fn token_steps_send_callback_and_verifier_in_header_only() {
        let s = stub(&[
            ("/services/oauth/1.0a/getRequestToken", "oauth_token=req-token&oauth_token_secret=req-secret"),
            ("/services/oauth/1.0a/getAccessToken", "oauth_token=acc&oauth_token_secret=acc-secret"),
        ])
        .await;
        let u = urls(&s);

        let rt = begin_auth_at(&ep(&u), KEY, SECRET).await.unwrap();
        assert_eq!(
            rt.authorize_url,
            format!("{}?oauth_token=req-token&Access=Full&Permissions=Add", u.authorize)
        );
        let at = complete_auth_at(&ep(&u), KEY, SECRET, "req-token", "req-secret", "654321")
            .await
            .unwrap();
        assert_eq!((at.token.as_str(), at.secret.as_str()), ("acc", "acc-secret"));

        let log = s.log();
        assert_eq!(log.len(), 2);
        assert_oauth_in_header_only(&log[0], None);
        assert_eq!(log[0].target, "/services/oauth/1.0a/getRequestToken");
        assert_eq!(log[0].oauth_header()["oauth_callback"], "oob");
        assert!(log[0].verify_signature(&u.request_token, &BTreeMap::new(), SECRET, ""));
        assert_oauth_in_header_only(&log[1], Some("req-token"));
        assert_eq!(log[1].target, "/services/oauth/1.0a/getAccessToken");
        assert_eq!(log[1].oauth_header()["oauth_verifier"], "654321");
        assert!(log[1].verify_signature(&u.access_token, &BTreeMap::new(), SECRET, "req-secret"));
    }

    #[tokio::test]
    async fn api_calls_and_upload_send_access_token_in_header_only() {
        let s = stub(&[
            ("/api/v2!authuser", AUTHUSER),
            (
                "/api/v2/user/me!albums",
                r#"{"Response":{"Album":[{"Uri":"/api/v2/album/a1","Name":"Trip"}]}}"#,
            ),
            (
                "/api/v2/folder/user/me!albums",
                r#"{"Response":{"Album":{"Uri":"/api/v2/album/new","Name":"New"}}}"#,
            ),
            ("/", r#"{"stat":"ok","Image":{"URL":"https://example.invalid/i"}}"#),
        ])
        .await;
        let u = urls(&s);

        let albums = list_albums_at(&ep(&u), KEY, SECRET, TOKEN, TOKEN_SECRET).await.unwrap();
        assert_eq!(albums, vec![Album { uri: "/api/v2/album/a1".into(), name: "Trip".into() }]);
        let made = create_album_at(&ep(&u), KEY, SECRET, TOKEN, TOKEN_SECRET, "New").await.unwrap();
        assert_eq!(made.uri, "/api/v2/album/new");

        let dir = std::env::temp_dir().join(format!("chairphoto-smugmug-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let jpeg = dir.join("a.jpg");
        std::fs::write(&jpeg, b"jpeg bytes").unwrap();
        let url = upload_at(&ep(&u), KEY, SECRET, TOKEN, TOKEN_SECRET, "/api/v2/album/a1", &jpeg, "t", "c")
            .await
            .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(url, "https://example.invalid/i");

        let log = s.log();
        let paths: Vec<&str> = log.iter().map(|r| r.path()).collect();
        assert_eq!(
            paths,
            ["/api/v2!authuser", "/api/v2/user/me!albums", "/api/v2!authuser", "/api/v2/folder/user/me!albums", "/"]
        );
        for req in &log {
            assert_oauth_in_header_only(req, Some(TOKEN));
            let base = format!("{}{}", u.api_base, req.path());
            // JSON and binary bodies are not signed (only form-encoded bodies would be).
            assert!(req.verify_signature(&base, &BTreeMap::new(), SECRET, TOKEN_SECRET), "{req:?}");
        }
        // The album list keeps its request params in the query, signed.
        assert_eq!(log[1].query()["count"], "500");
        assert_eq!((log[3].method.as_str(), log[4].method.as_str()), ("POST", "POST"));
        assert_eq!(log[4].body, b"jpeg bytes");
    }

    // Review claude-fix190 Low-4: a reply Uri that is not an absolute path is refused before
    // any signed request goes to it.
    #[tokio::test]
    async fn reply_uri_that_could_change_host_is_refused() {
        for bad in [".evil.example/x", "@evil.example/x", "//evil.example/x"] {
            let reply = format!(
                r#"{{"Response":{{"User":{{"Uris":{{"UserAlbums":{{"Uri":"{bad}"}},"Folder":{{"Uri":"{bad}"}}}}}}}}}}"#
            );
            let s = stub(&[("/api/v2!authuser", reply.as_str())]).await;
            let u = urls(&s);
            let err = list_albums_at(&ep(&u), KEY, SECRET, TOKEN, TOKEN_SECRET).await.unwrap_err();
            assert!(err.contains("unexpected URI"), "{bad}: {err}");
            let err = create_album_at(&ep(&u), KEY, SECRET, TOKEN, TOKEN_SECRET, "N").await.unwrap_err();
            assert!(err.contains("unexpected URI"), "{bad}: {err}");
            let paths: Vec<String> = s.log().iter().map(|r| r.path().to_string()).collect();
            assert_eq!(paths, ["/api/v2!authuser", "/api/v2!authuser"], "{bad}");
        }
        assert_eq!(api_url("https://h", "/api/v2/x").unwrap(), "https://h/api/v2/x");
    }

    #[tokio::test]
    async fn errors_echoing_a_reply_never_carry_the_token() {
        let echo = format!("not json: oauth_token%3D{TOKEN}");
        let s = stub(&[
            ("/api/v2!authuser", echo.as_str()),
            ("/services/oauth/1.0a/getAccessToken", "oauth_problem=token_rejected&debug=req-token%26654321"),
        ])
        .await;
        let u = urls(&s);
        let err = list_albums_at(&ep(&u), KEY, SECRET, TOKEN, TOKEN_SECRET).await.unwrap_err();
        assert!(err.contains("[redacted]") && !err.contains(TOKEN), "{err}");
        let err = complete_auth_at(&ep(&u), KEY, SECRET, "req-token", "rs", "654321")
            .await
            .err()
            .unwrap();
        assert!(!err.contains("req-token") && !err.contains("654321"), "{err}");
    }
}
