//! Send a photo to a device on the LAN over LocalSend's documented v2 HTTP protocol
//! (the `localsend` Cargo feature). Pure transport: UDP multicast discovery + the
//! prepare-upload/upload handshake. **Send only** — there is no receiver here.
//!
//! Commands in `commands.rs` wire these to the catalog and the render path (LS2); the
//! frontend module renders the device picker and (for Snapchat) records the publication.
//! Endpoints per the LocalSend v2 protocol (github.com/localsend/protocol).
//!
//! No new crates: HTTP reuses [`reqwest`] (already on for `ai`/`flickr`/…), UDP uses
//! `tokio::net::UdpSocket` from the already-on tokio "full" feature.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::Path;
use std::time::Duration;

use tokio::net::UdpSocket;

/// LocalSend's well-known multicast group + port for discovery announcements.
const MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 167);
const MULTICAST_PORT: u16 = 53317;
/// The protocol version we speak / announce.
const PROTOCOL_VERSION: &str = "2.0";
/// Our advertised alias + (nominal) HTTP port for the send-only role.
const OUR_ALIAS: &str = "ChairPhoto";
const OUR_PORT: u16 = 53317;

/// A LocalSend peer discovered on the LAN (or entered manually). The fields handed to the
/// UI are camelCase to match the rest of the frontend contract (mirrors `smugmug::Album`).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub alias: String,
    #[serde(default)]
    pub device_model: Option<String>,
    #[serde(default)]
    pub device_type: Option<String>,
    /// Filled in from the UDP sender address (announcements don't carry their own IP).
    #[serde(default)]
    pub ip: String,
    pub port: u16,
    /// `"http"` or `"https"` — honor what the device announces (LAN self-signed for https).
    pub protocol: String,
    pub fingerprint: String,
}

/// Per-file metadata sent in the prepare-upload request (one entry per outgoing file).
#[derive(Debug, Clone)]
pub struct FileMeta {
    /// Stable id we choose for this file within the session (we use the file index / a uuid).
    pub id: String,
    pub file_name: String,
    pub size: u64,
    pub file_type: String,
}

impl FileMeta {
    /// Build a [`FileMeta`] for a file on disk, deriving name/size/mime from the path.
    pub fn from_path(id: impl Into<String>, path: &Path) -> Result<FileMeta, String> {
        let meta = std::fs::metadata(path).map_err(|e| format!("couldn't stat {path:?}: {e}"))?;
        let file_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("photo.jpg")
            .to_string();
        Ok(FileMeta {
            id: id.into(),
            file_type: mime_for(&file_name),
            file_name,
            size: meta.len(),
        })
    }
}

/// Minimal MIME guess for the file types we actually send (JPEG renders). LocalSend only
/// uses this for display; the bytes are sent raw regardless.
fn mime_for(name: &str) -> String {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "tif" | "tiff" => "image/tiff",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Generate a fresh random fingerprint for our announcements. LocalSend identifies peers by
/// fingerprint; as a send-only client a random per-process value is fine (we reuse `uuid`).
pub fn fingerprint() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Our own announcement / `info` object, as sent in discovery and prepare-upload.
/// `announce` is true when broadcasting (so peers reply), false inside prepare-upload.
pub fn our_info(fingerprint: &str, announce: bool) -> serde_json::Value {
    serde_json::json!({
        "alias": OUR_ALIAS,
        "version": PROTOCOL_VERSION,
        "deviceModel": "ChairPhoto",
        "deviceType": "desktop",
        "fingerprint": fingerprint,
        "port": OUR_PORT,
        "protocol": "http",
        "download": false,
        "announce": announce,
    })
}

/// Parse one announcement JSON (as received over UDP) into a [`Device`], stamping the
/// sender's `ip`. Returns `None` for our own announcement or malformed payloads.
fn parse_announcement(json: &str, ip: &str, our_fingerprint: &str) -> Option<Device> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let fingerprint = v.get("fingerprint")?.as_str()?.to_string();
    // Ignore the echo of our own announcement.
    if fingerprint == our_fingerprint {
        return None;
    }
    let alias = v.get("alias")?.as_str()?.to_string();
    let port = v.get("port").and_then(|p| p.as_u64())? as u16;
    let protocol = v
        .get("protocol")
        .and_then(|p| p.as_str())
        .unwrap_or("http")
        .to_string();
    Some(Device {
        alias,
        device_model: v.get("deviceModel").and_then(|s| s.as_str()).map(String::from),
        device_type: v.get("deviceType").and_then(|s| s.as_str()).map(String::from),
        ip: ip.to_string(),
        port,
        protocol,
        fingerprint,
    })
}

/// Discover LocalSend devices on the LAN for `timeout_ms`: join the multicast group, send our
/// announcement (so peers reply), then collect replying announcements. Deduped by fingerprint
/// (falling back to ip:port when a peer omits a fingerprint). Tolerant — a blocked multicast
/// just yields an empty list, and the manual-IP path is the fallback (handled in LS2/LS3).
pub async fn discover(timeout_ms: u64) -> Result<Vec<Device>, String> {
    let our_fp = fingerprint();

    let socket = bind_multicast()
        .await
        .map_err(|e| format!("LocalSend discovery: couldn't open multicast socket: {e}"))?;

    // Announce ourselves so listening peers send their announcement back.
    let announcement = our_info(&our_fp, true).to_string();
    let group = SocketAddrV4::new(MULTICAST_ADDR, MULTICAST_PORT);
    let _ = socket.send_to(announcement.as_bytes(), group).await;

    let mut seen: HashMap<String, Device> = HashMap::new();
    let deadline = Duration::from_millis(timeout_ms);
    let mut buf = vec![0u8; 64 * 1024];

    // Collect for up to `timeout_ms`; each recv is bounded so the loop can't hang past it.
    let collect = async {
        loop {
            match socket.recv_from(&mut buf).await {
                Ok((n, from)) => {
                    let text = String::from_utf8_lossy(&buf[..n]);
                    let ip = from.ip().to_string();
                    if let Some(dev) = parse_announcement(&text, &ip, &our_fp) {
                        let key = if dev.fingerprint.is_empty() {
                            format!("{}:{}", dev.ip, dev.port)
                        } else {
                            dev.fingerprint.clone()
                        };
                        seen.entry(key).or_insert(dev);
                    }
                }
                Err(_) => break,
            }
        }
    };
    let _ = tokio::time::timeout(deadline, collect).await;

    Ok(seen.into_values().collect())
}

/// Bind a UDP socket joined to LocalSend's multicast group. Uses std to set the reuse-addr +
/// membership options (so multiple LocalSend apps can share the port), then hands it to tokio.
async fn bind_multicast() -> std::io::Result<UdpSocket> {
    use std::net::UdpSocket as StdUdpSocket;
    // SO_REUSEADDR via socket2 isn't available without a new dep; std's UdpSocket can still
    // join the group and receive replies addressed to the group. Bind to the wildcard so we
    // get unicast replies too.
    let std_sock = StdUdpSocket::bind((Ipv4Addr::UNSPECIFIED, MULTICAST_PORT))
        .or_else(|_| StdUdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)))?;
    std_sock.set_nonblocking(true)?;
    let _ = std_sock.join_multicast_v4(&MULTICAST_ADDR, &Ipv4Addr::UNSPECIFIED);
    UdpSocket::from_std(std_sock)
}

/// Build the prepare-upload request body: `{ info, files: { <fileId>: { id, fileName, size,
/// fileType } } }`. The `files` map is keyed by each file's id (LocalSend v2 shape).
pub fn prepare_upload_body(our_fingerprint: &str, files: &[FileMeta]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for f in files {
        map.insert(
            f.id.clone(),
            serde_json::json!({
                "id": f.id,
                "fileName": f.file_name,
                "size": f.size,
                "fileType": f.file_type,
            }),
        );
    }
    // `announce` belongs only to UDP discovery packets, not the prepare-upload `info` body.
    let mut info = our_info(our_fingerprint, false);
    if let Some(obj) = info.as_object_mut() {
        obj.remove("announce");
    }
    serde_json::json!({
        "info": info,
        "files": serde_json::Value::Object(map),
    })
}

/// Base URL for a device's v2 API, e.g. `http://192.168.1.5:53317/api/localsend/v2`.
fn base_url(device: &Device) -> String {
    let scheme = if device.protocol.eq_ignore_ascii_case("https") {
        "https"
    } else {
        "http"
    };
    format!(
        "{scheme}://{}:{}/api/localsend/v2",
        device.ip, device.port
    )
}

/// An HTTP client honoring the device's protocol — for https LAN peers (self-signed certs,
/// pinned by fingerprint at the LocalSend layer) we accept invalid certs.
fn client_for(device: &Device) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder();
    if device.protocol.eq_ignore_ascii_case("https") {
        builder = builder.danger_accept_invalid_certs(true);
    }
    builder
        .build()
        .map_err(|e| format!("LocalSend: couldn't build HTTP client: {e}"))
}

/// Parsed prepare-upload reply: a session id + a per-file upload token.
#[derive(Debug, Clone)]
pub struct UploadSession {
    pub session_id: String,
    /// fileId → token
    pub tokens: HashMap<String, String>,
}

/// Parse the prepare-upload response `{ sessionId, files: { <fileId>: <token> } }`.
fn parse_prepare_response(text: &str) -> Result<UploadSession, String> {
    let v: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| format!("LocalSend: bad prepare-upload response ({e}): {text}"))?;
    let session_id = v
        .get("sessionId")
        .and_then(|s| s.as_str())
        .ok_or_else(|| format!("LocalSend: prepare-upload returned no sessionId: {text}"))?
        .to_string();
    let mut tokens = HashMap::new();
    if let Some(obj) = v.get("files").and_then(|f| f.as_object()) {
        for (file_id, tok) in obj {
            if let Some(t) = tok.as_str() {
                tokens.insert(file_id.clone(), t.to_string());
            }
        }
    }
    Ok(UploadSession { session_id, tokens })
}

/// Send `paths` to `device` over LocalSend v2: prepare-upload (retrying with `?pin=` on a 401
/// from a PIN-protected receiver), then upload each file's raw bytes. `progress(done, total)`
/// is called after each file completes (LS2 forwards it to a `localsend:progress` event).
pub async fn send_files(
    device: &Device,
    paths: &[std::path::PathBuf],
    pin: Option<&str>,
    mut progress: impl FnMut(usize, usize),
) -> Result<(), String> {
    if paths.is_empty() {
        return Err("LocalSend: nothing to send".into());
    }

    // Build per-file metadata, keying each by a stable id (its index).
    let metas: Vec<FileMeta> = paths
        .iter()
        .enumerate()
        .map(|(i, p)| FileMeta::from_path(i.to_string(), p))
        .collect::<Result<_, _>>()?;

    let our_fp = fingerprint();
    let body = prepare_upload_body(&our_fp, &metas);
    let client = client_for(device)?;
    let base = base_url(device);

    // prepare-upload (retry once with the PIN if the receiver demands one).
    let session = prepare_upload(&client, &base, &body, pin).await?;

    let total = metas.len();
    for (i, (meta, path)) in metas.iter().zip(paths.iter()).enumerate() {
        let token = session.tokens.get(&meta.id).cloned().ok_or_else(|| {
            format!("LocalSend: receiver returned no upload token for {}", meta.file_name)
        })?;
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|e| format!("couldn't read {path:?}: {e}"))?;
        let url = format!(
            "{base}/upload?sessionId={}&fileId={}&token={}",
            urlencode(&session.session_id),
            urlencode(&meta.id),
            urlencode(&token),
        );
        let resp = client
            .post(&url)
            .header("Content-Type", "application/octet-stream")
            .body(bytes)
            .send()
            .await
            .map_err(|e| format!("LocalSend upload failed ({}): {e}", meta.file_name))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!(
                "LocalSend: upload of {} rejected ({status}): {text}",
                meta.file_name
            ));
        }
        progress(i + 1, total);
    }
    Ok(())
}

/// POST prepare-upload; on a `401` retry once with `?pin=<pin>` (PIN-protected receiver).
async fn prepare_upload(
    client: &reqwest::Client,
    base: &str,
    body: &serde_json::Value,
    pin: Option<&str>,
) -> Result<UploadSession, String> {
    let url = format!("{base}/prepare-upload");
    let resp = client
        .post(&url)
        .json(body)
        .send()
        .await
        .map_err(|e| format!("LocalSend prepare-upload request failed: {e}"))?;

    if resp.status().as_u16() == 401 {
        let pin = pin.ok_or("LocalSend: receiver requires a PIN")?;
        let url = format!("{base}/prepare-upload?pin={}", urlencode(pin));
        let resp = client
            .post(&url)
            .json(body)
            .send()
            .await
            .map_err(|e| format!("LocalSend prepare-upload (pin) request failed: {e}"))?;
        if resp.status().as_u16() == 401 {
            return Err("LocalSend: incorrect PIN".into());
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("LocalSend prepare-upload failed ({status}): {text}"));
        }
        let text = resp.text().await.map_err(|e| e.to_string())?;
        return parse_prepare_response(&text);
    }

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("LocalSend prepare-upload failed ({status}): {text}"));
    }
    let text = resp.text().await.map_err(|e| e.to_string())?;
    parse_prepare_response(&text)
}

/// Minimal percent-encoding for the few query values we put in upload URLs (session id,
/// file id, token, PIN). Avoids pulling in a URL-encoding crate for our limited needs.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_non_empty_and_unique() {
        let a = fingerprint();
        let b = fingerprint();
        assert!(!a.is_empty());
        assert_ne!(a, b);
    }

    #[test]
    fn parses_announcement_into_device_with_sender_ip() {
        let json = r#"{
            "alias":"Pixel 8","version":"2.0","deviceModel":"Pixel 8",
            "deviceType":"mobile","fingerprint":"abc123","port":53317,
            "protocol":"https","download":true,"announce":true
        }"#;
        let dev = parse_announcement(json, "192.168.1.42", "our-fp").unwrap();
        assert_eq!(dev.alias, "Pixel 8");
        assert_eq!(dev.device_model.as_deref(), Some("Pixel 8"));
        assert_eq!(dev.device_type.as_deref(), Some("mobile"));
        assert_eq!(dev.ip, "192.168.1.42");
        assert_eq!(dev.port, 53317);
        assert_eq!(dev.protocol, "https");
        assert_eq!(dev.fingerprint, "abc123");
    }

    #[test]
    fn ignores_our_own_announcement_echo() {
        let json = r#"{"alias":"ChairPhoto","fingerprint":"mine","port":53317,"protocol":"http"}"#;
        assert!(parse_announcement(json, "192.168.1.1", "mine").is_none());
    }

    #[test]
    fn announcement_protocol_defaults_to_http() {
        let json = r#"{"alias":"Laptop","fingerprint":"xyz","port":53317}"#;
        let dev = parse_announcement(json, "10.0.0.2", "our-fp").unwrap();
        assert_eq!(dev.protocol, "http");
        assert!(dev.device_model.is_none());
    }

    #[test]
    fn malformed_announcement_is_skipped() {
        assert!(parse_announcement("not json", "1.2.3.4", "fp").is_none());
        // Missing required fields (no fingerprint).
        assert!(parse_announcement(r#"{"alias":"x","port":1}"#, "1.2.3.4", "fp").is_none());
    }

    #[test]
    fn our_info_shape() {
        let info = our_info("fp-123", true);
        assert_eq!(info["alias"], "ChairPhoto");
        assert_eq!(info["version"], "2.0");
        assert_eq!(info["fingerprint"], "fp-123");
        assert_eq!(info["protocol"], "http");
        assert_eq!(info["announce"], true);
        assert_eq!(our_info("fp", false)["announce"], false);
    }

    #[test]
    fn prepare_upload_body_has_filemap_and_sizes() {
        let files = vec![
            FileMeta {
                id: "0".into(),
                file_name: "a.jpg".into(),
                size: 111,
                file_type: "image/jpeg".into(),
            },
            FileMeta {
                id: "1".into(),
                file_name: "b.png".into(),
                size: 222,
                file_type: "image/png".into(),
            },
        ];
        let body = prepare_upload_body("fp", &files);
        // info present
        assert_eq!(body["info"]["alias"], "ChairPhoto");
        // files keyed by id, with size + name + type preserved
        let map = body["files"].as_object().unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(body["files"]["0"]["fileName"], "a.jpg");
        assert_eq!(body["files"]["0"]["size"], 111);
        assert_eq!(body["files"]["0"]["id"], "0");
        assert_eq!(body["files"]["1"]["fileType"], "image/png");
        assert_eq!(body["files"]["1"]["size"], 222);
    }

    #[test]
    fn base_url_distinguishes_http_and_https() {
        let mut dev = Device {
            alias: "x".into(),
            device_model: None,
            device_type: None,
            ip: "192.168.1.5".into(),
            port: 53317,
            protocol: "http".into(),
            fingerprint: "f".into(),
        };
        assert_eq!(
            base_url(&dev),
            "http://192.168.1.5:53317/api/localsend/v2"
        );
        dev.protocol = "https".into();
        assert_eq!(
            base_url(&dev),
            "https://192.168.1.5:53317/api/localsend/v2"
        );
        // Case-insensitive on the announced protocol.
        dev.protocol = "HTTPS".into();
        assert!(base_url(&dev).starts_with("https://"));
    }

    #[test]
    fn parses_prepare_response_session_and_tokens() {
        let text = r#"{"sessionId":"sess-1","files":{"0":"tok-a","1":"tok-b"}}"#;
        let s = parse_prepare_response(text).unwrap();
        assert_eq!(s.session_id, "sess-1");
        assert_eq!(s.tokens.get("0").map(String::as_str), Some("tok-a"));
        assert_eq!(s.tokens.get("1").map(String::as_str), Some("tok-b"));
    }

    #[test]
    fn prepare_response_without_session_errors() {
        assert!(parse_prepare_response(r#"{"files":{}}"#).is_err());
        assert!(parse_prepare_response("garbage").is_err());
    }

    #[test]
    fn prepare_response_tolerates_missing_or_nonstring_files() {
        // A session with no `files` key at all is valid (zero tokens) — the caller errors
        // later, per-file, if a needed token is absent.
        let s = parse_prepare_response(r#"{"sessionId":"only-session"}"#).unwrap();
        assert_eq!(s.session_id, "only-session");
        assert!(s.tokens.is_empty());

        // Non-string token values are skipped rather than crashing the parse.
        let s = parse_prepare_response(r#"{"sessionId":"s","files":{"0":"tok","1":123}}"#).unwrap();
        assert_eq!(s.tokens.get("0").map(String::as_str), Some("tok"));
        assert!(!s.tokens.contains_key("1"));
    }

    #[test]
    fn prepare_upload_body_is_empty_filemap_for_no_files() {
        let body = prepare_upload_body("fp", &[]);
        assert_eq!(body["files"].as_object().unwrap().len(), 0);
        // `info` is always present (so a receiver can identify the sender).
        assert_eq!(body["info"]["deviceType"], "desktop");
    }

    #[test]
    fn base_url_unknown_protocol_falls_back_to_http() {
        let dev = Device {
            alias: "x".into(),
            device_model: None,
            device_type: None,
            ip: "10.0.0.9".into(),
            port: 8080,
            protocol: "ftp".into(), // anything not https → http
            fingerprint: "f".into(),
        };
        assert_eq!(base_url(&dev), "http://10.0.0.9:8080/api/localsend/v2");
    }

    #[test]
    fn urlencode_handles_pin_and_token_shapes() {
        // PINs / tokens stay intact when alphanumeric…
        assert_eq!(urlencode("123456"), "123456");
        // …and slashes / plus (base64-ish tokens) are escaped.
        assert_eq!(urlencode("a/b+c"), "a%2Fb%2Bc");
        // Empty input is a no-op.
        assert_eq!(urlencode(""), "");
    }

    #[test]
    fn mime_for_known_extensions() {
        assert_eq!(mime_for("x.JPG"), "image/jpeg");
        assert_eq!(mime_for("x.png"), "image/png");
        assert_eq!(mime_for("x.bin"), "application/octet-stream");
    }

    #[test]
    fn urlencode_escapes_reserved() {
        assert_eq!(urlencode("a b&c=d"), "a%20b%26c%3Dd");
        assert_eq!(urlencode("safe-_.~AZ09"), "safe-_.~AZ09");
    }
}
