//! Send a photo to a device on the LAN over LocalSend's documented v2 HTTP protocol
//! (the `localsend` Cargo feature). Pure transport: UDP multicast discovery + the
//! prepare-upload/upload handshake. **Send only** — there is no receiver here.
//!
//! Commands in `commands.rs` wire these to the catalog and the render path (LS2); the
//! frontend module renders the device picker and (for Snapchat) records the publication.
//! Endpoints per the LocalSend v2 protocol (github.com/localsend/protocol).
//!
//! HTTP reuses [`reqwest`] (already on for `ai`/`flickr`/…), UDP uses `tokio::net::UdpSocket`
//! from the already-on tokio "full" feature. `socket2` (discovery-socket options) and, on
//! unix, `libc` (interface enumeration) were promoted from transitive to direct dependencies
//! for this — both were already fully resolved in `Cargo.lock` via tokio/mio/hyper-util, so
//! this is no new download and no new compile unit (see `Cargo.toml`'s comments on each).

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::Path;
use std::time::Duration;

use socket2::{Domain, Protocol as SockProtocol, SockAddr, Socket, Type};
use tokio::net::UdpSocket;

/// LocalSend's well-known multicast group + port for discovery announcements.
const MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 167);
const MULTICAST_PORT: u16 = 53317;
/// The protocol version we speak / announce.
const PROTOCOL_VERSION: &str = "2.0";
/// Our advertised alias + nominal HTTP port for the send-only role. `OUR_PORT` is used as-is
/// in [`our_info`] (the send path's prepare-upload `info` block — there is no listener behind
/// it either way, see the module docs). `discover()` does **not** use it for the port it
/// announces: it advertises whatever port its discovery socket actually bound, via
/// [`our_info_with_port`], since that can differ from 53317 (see `open_reusable_udp_socket`).
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
    our_info_with_port(fingerprint, announce, OUR_PORT)
}

/// As [`our_info`], but advertising `port` instead of the hardcoded [`OUR_PORT`]. `discover()`
/// uses this to advertise the port its discovery socket actually bound — which may not be
/// [`OUR_PORT`] if 53317 was unavailable even with `SO_REUSEADDR`/`SO_REUSEPORT` — rather than
/// a port owned by whatever else is listening on 53317.
///
/// Deliberate, not accidental: `discover()` advertises a port on which ChairPhoto runs no TCP
/// listener at all — normally 53317, since we usually do win that bind. This is exactly why a
/// spec-conformant peer's HTTP `POST /api/localsend/v2/register` to that port fails and the
/// peer falls back to the UDP reply this module is built to receive. Absent a listener, this is
/// the least-bad choice: the announcement must name *some* port, and the port we actually bound
/// is at least honest about where we are (not) listening. What it doesn't cover: if a
/// co-resident LocalSend desktop app happens to serve real HTTP on that same port (its normal,
/// non-`--hidden` mode), the peer's register POST succeeds against *that* process instead of
/// failing, no UDP fallback is ever sent, and ChairPhoto is invisible again even though
/// everything in this module is working correctly. If that turns out to be the owner's actual
/// failure mode, the fix is an inbound register receiver, not another change here.
fn our_info_with_port(fingerprint: &str, announce: bool, port: u16) -> serde_json::Value {
    serde_json::json!({
        "alias": OUR_ALIAS,
        "version": PROTOCOL_VERSION,
        "deviceModel": "ChairPhoto",
        "deviceType": "desktop",
        "fingerprint": fingerprint,
        "port": port,
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

/// Discover LocalSend devices on the LAN for `timeout_ms`: join the multicast group on every
/// UP, multicast-capable interface, announce on each of them (so peers reply), then collect
/// replying announcements. Deduped by fingerprint (falling back to ip:port when a peer omits
/// a fingerprint). Tolerant of a blocked/degraded multicast join — that yields an empty list
/// rather than an error, and the manual-IP path is the fallback (handled in LS2/LS3). Socket
/// setup and join/announce failures are still surfaced (logged), so a persistently empty list
/// is diagnosable instead of indistinguishable from "no peers answered".
pub async fn discover(timeout_ms: u64) -> Result<Vec<Device>, String> {
    let our_fp = fingerprint();

    let socket = open_reusable_udp_socket(MULTICAST_PORT)
        .map_err(|e| format!("LocalSend discovery: couldn't open a UDP socket: {e}"))?;

    // Advertise the port we actually bound (may not be `MULTICAST_PORT`/`OUR_PORT` if that was
    // unavailable even with SO_REUSEADDR/SO_REUSEPORT) rather than a port some other process
    // owns.
    let bound_port = socket
        .local_addr()
        .ok()
        .and_then(|a| a.as_socket_ipv4())
        .map(|a| a.port())
        .unwrap_or(OUR_PORT);

    let interfaces = local_multicast_interfaces();
    let joined = join_multicast_on_all_interfaces(&socket, &interfaces);
    announce_on_interfaces(&socket, &joined, &our_info_with_port(&our_fp, true, bound_port));

    let socket = UdpSocket::from_std(socket.into())
        .map_err(|e| format!("LocalSend discovery: couldn't hand the socket to tokio: {e}"))?;

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
                Err(e) => {
                    eprintln!("localsend: discovery recv stopped early: {e}");
                    break;
                }
            }
        }
    };
    let _ = tokio::time::timeout(deadline, collect).await;

    Ok(seen.into_values().collect())
}

/// Open a UDP socket for LocalSend discovery with `SO_REUSEADDR` and (on unix) `SO_REUSEPORT`
/// set *before* binding, so we can bind `port` alongside another LocalSend-speaking process
/// that already holds it — notably the official desktop app, which autostarts and holds
/// `MULTICAST_PORT` for as long as the user is logged in (see `agent-notes` diagnosis for
/// issue #39; without this, discovery silently lands on an ephemeral port and can never
/// receive anything addressed to the well-known group port). Falls back to an ephemeral port
/// if even a reuse-enabled bind fails, logging why. Returns the raw `socket2` socket (not yet
/// handed to tokio) so the caller can still use `set_multicast_if_v4` per announce — an option
/// tokio's `UdpSocket` doesn't expose.
fn open_reusable_udp_socket(port: u16) -> std::io::Result<Socket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(SockProtocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;

    let wanted = SockAddr::from(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port));
    if let Err(e) = socket.bind(&wanted) {
        eprintln!(
            "localsend: couldn't bind UDP {port} even with SO_REUSEADDR/SO_REUSEPORT ({e}); \
             falling back to an ephemeral port — peers replying to the well-known port won't \
             reach us"
        );
        let fallback = SockAddr::from(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0));
        socket.bind(&fallback)?;
    }
    socket.set_nonblocking(true)?;
    Ok(socket)
}

/// Join LocalSend's multicast group on every address in `interfaces` (see
/// [`local_multicast_interfaces`]), instead of joining once via `Ipv4Addr::UNSPECIFIED` (which
/// the kernel resolves to a single OS-picked interface — on a multi-NIC host, silently missing
/// the others). Returns the interfaces actually joined; a failed join on one interface doesn't
/// stop the others, but is logged rather than silently dropped. Falls back to the old
/// `UNSPECIFIED` join when `interfaces` is empty (non-unix targets, or a host with no
/// qualifying interface reported), preserving prior behavior in that case. Takes the interface
/// list as a parameter (rather than calling [`local_multicast_interfaces`] itself) so the
/// join/fallback logic is testable without depending on the host's real network state.
fn join_multicast_on_all_interfaces(socket: &Socket, interfaces: &[Ipv4Addr]) -> Vec<Ipv4Addr> {
    let mut joined = Vec::new();
    if interfaces.is_empty() {
        match socket.join_multicast_v4(&MULTICAST_ADDR, &Ipv4Addr::UNSPECIFIED) {
            Ok(()) => joined.push(Ipv4Addr::UNSPECIFIED),
            Err(e) => eprintln!("localsend: couldn't join the multicast group: {e}"),
        }
        return joined;
    }
    for iface in interfaces {
        match socket.join_multicast_v4(&MULTICAST_ADDR, iface) {
            Ok(()) => joined.push(*iface),
            Err(e) => eprintln!("localsend: couldn't join the multicast group on {iface}: {e}"),
        }
    }
    joined
}

/// Send `announcement` to the multicast group once per interface in `joined`, selecting each
/// as the outgoing interface first (`IP_MULTICAST_IF`) — a single send on a wildcard-bound
/// socket only egresses via whichever interface the OS's default route picks, which under-
/// covers a multi-NIC host exactly like the join above. Errors are logged, not propagated:
/// one interface failing to send must not stop the others or fail discovery outright.
fn announce_on_interfaces(socket: &Socket, joined: &[Ipv4Addr], announcement: &serde_json::Value) {
    let bytes = announcement.to_string();
    let group = SockAddr::from(SocketAddrV4::new(MULTICAST_ADDR, MULTICAST_PORT));
    for iface in joined {
        if *iface != Ipv4Addr::UNSPECIFIED {
            if let Err(e) = socket.set_multicast_if_v4(iface) {
                eprintln!(
                    "localsend: couldn't select {iface} as the outgoing interface for the \
                     announce: {e}"
                );
                continue;
            }
        }
        if let Err(e) = socket.send_to(bytes.as_bytes(), &group) {
            eprintln!("localsend: announce over {iface} failed: {e}");
        }
    }
}

/// Every IPv4 address on this host whose interface qualifies per [`interface_flags_qualify`]
/// (up, running, multicast-capable, neither loopback nor point-to-point), via `getifaddrs(3)`.
/// Never errors: enumeration failures or a host with nothing qualifying return an empty list,
/// and callers fall back to the pre-existing `Ipv4Addr::UNSPECIFIED` join in that case.
#[cfg(unix)]
fn local_multicast_interfaces() -> Vec<Ipv4Addr> {
    let mut result = Vec::new();
    // SAFETY: `getifaddrs` either returns non-zero and leaves `head` untouched (the null we
    // initialized it with, which we then don't dereference), or returns 0 and sets `head` to
    // the first node of a valid linked list that must be freed with `freeifaddrs`. We only
    // read fields `getifaddrs(3)` documents as always present on a returned node, and we free
    // the list exactly once, on every path that obtained one.
    unsafe {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut head) != 0 {
            eprintln!(
                "localsend: getifaddrs failed ({}); joining only via the OS-picked interface",
                std::io::Error::last_os_error()
            );
            return result;
        }
        let mut cur = head;
        while !cur.is_null() {
            let ifa = &*cur;
            cur = ifa.ifa_next;

            if !interface_flags_qualify(ifa.ifa_flags as u32) || ifa.ifa_addr.is_null() {
                continue;
            }
            if (*ifa.ifa_addr).sa_family as i32 != libc::AF_INET {
                continue;
            }
            let sin = ifa.ifa_addr as *const libc::sockaddr_in;
            let addr = Ipv4Addr::from(u32::from_be((*sin).sin_addr.s_addr));
            if !result.contains(&addr) {
                result.push(addr);
            }
        }
        libc::freeifaddrs(head);
    }
    result
}

/// Whether an interface carrying `flags` (`ifaddrs.ifa_flags`, see `getifaddrs(3)`) qualifies
/// for a discovery join: administratively up, carrier-present (`IFF_RUNNING`), multicast-
/// capable, and neither loopback nor point-to-point. Pulled out of
/// [`local_multicast_interfaces`] as a pure function of the flag bits so this rule is
/// unit-testable without depending on this host's real interfaces. (It can't always be
/// exercised host-dependently: on at least one dev/test machine, `lo` doesn't carry
/// `IFF_MULTICAST` at all, so a test built only from that host's actual `getifaddrs()` output
/// cannot tell "loopback correctly excluded" apart from "already excluded by the multicast
/// check, loopback check untested".)
///
/// `IFF_RUNNING` (not just `IFF_UP`) excludes administratively-up-but-carrier-down interfaces —
/// measured on the machine this was written on: a Docker bridge (`docker0`) is `UP` with no
/// cable/link, and without this check it was being joined and announced on every discovery
/// call for no reachability benefit. `IFF_POINTOPOINT` excludes VPN/tunnel interfaces (`tun0`,
/// `tailscale0`-style WireGuard/OpenVPN devices) — multicast group membership on a
/// point-to-point link doesn't reach a LAN peer and only adds noise.
#[cfg(unix)]
fn interface_flags_qualify(flags: u32) -> bool {
    let up = flags & (libc::IFF_UP as u32) != 0;
    let running = flags & (libc::IFF_RUNNING as u32) != 0;
    let multicast = flags & (libc::IFF_MULTICAST as u32) != 0;
    let loopback = flags & (libc::IFF_LOOPBACK as u32) != 0;
    let point_to_point = flags & (libc::IFF_POINTOPOINT as u32) != 0;
    up && running && multicast && !loopback && !point_to_point
}

/// No `getifaddrs`-equivalent is wired up for non-unix targets — falls back to the
/// pre-existing `Ipv4Addr::UNSPECIFIED` join via [`join_multicast_on_all_interfaces`].
#[cfg(not(unix))]
fn local_multicast_interfaces() -> Vec<Ipv4Addr> {
    Vec::new()
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

    // --- issue #39: discovery-socket fixes -------------------------------------------------

    #[test]
    fn our_info_with_port_advertises_given_port_not_the_constant() {
        let info = our_info_with_port("fp", true, 51586);
        assert_eq!(info["port"], 51586);
        assert_ne!(info["port"].as_u64().unwrap(), OUR_PORT as u64);
        // `our_info` (the send-path helper) is unaffected — still the nominal constant.
        assert_eq!(our_info("fp", true)["port"], OUR_PORT);
    }

    #[cfg(unix)]
    #[test]
    fn interface_flags_qualify_requires_up_running_multicast_excludes_loopback_and_p2p() {
        let up = libc::IFF_UP as u32;
        let running = libc::IFF_RUNNING as u32;
        let multicast = libc::IFF_MULTICAST as u32;
        let loopback = libc::IFF_LOOPBACK as u32;
        let p2p = libc::IFF_POINTOPOINT as u32;

        assert!(
            interface_flags_qualify(up | running | multicast),
            "up + running + multicast alone must qualify"
        );
        assert!(
            !interface_flags_qualify(up | multicast),
            "carrier-down (no IFF_RUNNING) must not qualify — this is what excludes a \
             cable-unplugged NIC or an administratively-up-but-link-down bridge like docker0"
        );
        assert!(!interface_flags_qualify(multicast | running), "a down interface must not qualify");
        assert!(!interface_flags_qualify(up | running), "a non-multicast interface must not qualify");
        assert!(
            !interface_flags_qualify(up | running | multicast | loopback),
            "loopback must be excluded even when up, running, and multicast-capable"
        );
        assert!(
            !interface_flags_qualify(up | running | multicast | p2p),
            "point-to-point interfaces (tun0, tailscale0-style VPN devices) must be excluded \
             even when up, running, and multicast-capable"
        );
        assert!(!interface_flags_qualify(0));
    }

    #[cfg(unix)]
    #[test]
    fn local_multicast_interfaces_never_includes_loopback_or_unspecified() {
        // Best-effort against this host's real interfaces (a sandboxed runner may have zero
        // UP, non-loopback ones — that's fine, the function must simply not panic and must
        // never report loopback or the wildcard address as if it were a real interface).
        let ifaces = local_multicast_interfaces();
        for addr in &ifaces {
            assert_ne!(*addr, Ipv4Addr::LOCALHOST, "loopback must be excluded: {addr}");
            assert_ne!(*addr, Ipv4Addr::UNSPECIFIED, "must be a real interface address: {addr}");
        }
    }

    #[test]
    fn open_reusable_udp_socket_shares_a_port_already_held_by_another_reusable_socket() {
        // First holder: bind an ephemeral port the same way a second LocalSend-speaking
        // process would already have bound it (SO_REUSEADDR/SO_REUSEPORT before bind) — this
        // is what the real LocalSend desktop app does on the owner's machine (see the issue
        // #39 diagnosis), simulated here without depending on that process actually running.
        let holder = open_reusable_udp_socket(0).expect("first bind must succeed");
        let held_port = holder
            .local_addr()
            .unwrap()
            .as_socket_ipv4()
            .unwrap()
            .port();

        // A second reuse-enabled open of the *same* port must land on that exact port, not
        // silently fall back to a different one.
        let second =
            open_reusable_udp_socket(held_port).expect("reuse-enabled bind must succeed");
        let second_port = second
            .local_addr()
            .unwrap()
            .as_socket_ipv4()
            .unwrap()
            .port();

        assert_eq!(second_port, held_port, "expected to share the held port, not fall back");
    }

    #[test]
    fn open_reusable_udp_socket_falls_back_to_ephemeral_port_when_a_plain_socket_holds_it() {
        // A plain socket (no SO_REUSEADDR/SO_REUSEPORT) exclusively holds a port — a process
        // that hasn't opted into sharing. Verified separately that Linux actually refuses the
        // reuse-enabled second bind in this case (asymmetric reuse doesn't share); this test
        // pins that observed behavior against a regression, not the OS's general contract.
        let exclusive = std::net::UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        let held_port = exclusive.local_addr().unwrap().port();

        let socket =
            open_reusable_udp_socket(held_port).expect("must fall back, not error out");
        let bound_port = socket.local_addr().unwrap().as_socket_ipv4().unwrap().port();

        assert_ne!(bound_port, held_port, "must not silently claim to be on the held port");
        assert_ne!(bound_port, 0, "must report the real ephemeral port, not the wildcard");
    }

    #[test]
    fn advertised_port_matches_the_port_actually_bound() {
        // End-to-end across `open_reusable_udp_socket` + `our_info_with_port`, the way
        // `discover()` wires them together: whatever port the socket lands on (bound or
        // fallback) is exactly the port the announcement claims.
        let exclusive = std::net::UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        let contended_port = exclusive.local_addr().unwrap().port();

        let socket = open_reusable_udp_socket(contended_port).unwrap();
        let bound_port = socket.local_addr().unwrap().as_socket_ipv4().unwrap().port();
        let announcement = our_info_with_port("fp", true, bound_port);

        assert_eq!(announcement["port"].as_u64().unwrap(), bound_port as u64);
        assert_ne!(bound_port, contended_port);
    }

    #[test]
    fn join_multicast_falls_back_to_unspecified_when_no_interfaces_given() {
        let socket = open_reusable_udp_socket(0).unwrap();
        let joined = join_multicast_on_all_interfaces(&socket, &[]);
        assert_eq!(joined, vec![Ipv4Addr::UNSPECIFIED]);
    }

    #[test]
    fn join_multicast_skips_an_interface_it_cannot_join_without_panicking() {
        let socket = open_reusable_udp_socket(0).unwrap();
        // TEST-NET-3 (RFC 5737) — guaranteed not to be a local interface address.
        let bogus = Ipv4Addr::new(203, 0, 113, 42);
        let joined = join_multicast_on_all_interfaces(&socket, &[bogus]);
        assert!(
            joined.is_empty(),
            "a join on a non-local interface must not be reported as joined: {joined:?}"
        );
    }

    #[test]
    fn join_multicast_reports_exactly_the_interfaces_it_joined() {
        let socket = open_reusable_udp_socket(0).unwrap();
        // Loopback is a real, always-present, multicast-capable interface, so this join
        // should succeed — unlike the bogus-address case above.
        let joined = join_multicast_on_all_interfaces(&socket, &[Ipv4Addr::LOCALHOST]);
        assert_eq!(joined, vec![Ipv4Addr::LOCALHOST]);
    }

    #[test]
    fn announce_on_interfaces_handles_empty_and_unspecified_without_panicking() {
        let socket = open_reusable_udp_socket(0).unwrap();
        let info = our_info_with_port("fp", true, 12345);
        // Neither call should panic or return a value (both are fire-and-forget); this is a
        // smoke test that the empty-list no-op path and the UNSPECIFIED
        // skip-set_multicast_if_v4 path are both exercised safely.
        announce_on_interfaces(&socket, &[], &info);
        announce_on_interfaces(&socket, &[Ipv4Addr::UNSPECIFIED], &info);
    }

    // --- pre-existing coverage --------------------------------------------------------------

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
