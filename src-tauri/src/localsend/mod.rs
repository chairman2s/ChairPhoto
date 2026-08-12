//! Send a photo to a device on the LAN over LocalSend's documented v2 HTTP protocol
//! (the `localsend` Cargo feature). Pure transport: UDP multicast discovery + the
//! prepare-upload/upload handshake. **Send only** — no photo is ever accepted *from* the
//! network. Discovery does bind a short-lived inbound port to hear peers' register replies
//! (see [`register`], issue #55); that port answers `register` and refuses uploads.
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

use socket2::{Domain, Protocol as SockProtocol, SockAddr, SockRef, Socket, Type};
use tokio::net::UdpSocket;

mod identity;
mod register;

/// LocalSend's well-known multicast group + port for discovery announcements.
const MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 167);
const MULTICAST_PORT: u16 = 53317;
/// The protocol version we speak / announce.
const PROTOCOL_VERSION: &str = "2.0";
/// The reference LocalSend implementation bursts 3 announcements over ~2.6s at app start / on
/// a user-triggered scan rather than announcing once, and has no periodic re-announcement
/// timer. 802.11 multicast is unacknowledged, sent at the lowest basic rate, and commonly
/// deferred to DTIM or dropped by AP multicast filtering — a single datagram plus a short reply
/// window is a coin flip on wifi. `discover()` matches the reference's burst count and roughly
/// its spacing; see its doc comment for the resulting worst-case duration.
const ANNOUNCE_BURST_COUNT: usize = 3;
/// Spacing between each announcement in the burst. `(ANNOUNCE_BURST_COUNT - 1) *
/// ANNOUNCE_BURST_INTERVAL` = 2.5s total span, matching the reference's ~2.6s.
const ANNOUNCE_BURST_INTERVAL: Duration = Duration::from_millis(1250);
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

/// As [`our_info`], but advertising `port` instead of the hardcoded [`OUR_PORT`].
///
/// `discover()` passes the port of the [`register`] listener it bound for that pass — an
/// ephemeral port ChairPhoto genuinely serves HTTP on, so a spec-conformant peer's `POST
/// /api/localsend/v2/register` reaches *us*. That is the point of issue #55: advertising
/// 53317 while running no listener there meant a co-resident LocalSend desktop app answered
/// those POSTs instead, the peer therefore never sent its UDP `announce:false` fallback, and
/// ChairPhoto stayed invisible with every line of this module working correctly.
///
/// If the listener can't be bound at all, `discover()` falls back to advertising the port its
/// UDP discovery socket bound — no listener behind it, so peers fail their register and use
/// the UDP fallback, which is the pre-#55 behavior and still works whenever nothing else holds
/// the port. Either way the advertised port is one ChairPhoto actually bound, never a port
/// owned by another process.
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

/// Discover LocalSend devices on the LAN: join the multicast group on every UP, multicast-
/// capable interface, then announce and collect *concurrently* on the same socket — a burst of
/// [`ANNOUNCE_BURST_COUNT`] announcements spaced [`ANNOUNCE_BURST_INTERVAL`] apart, while
/// continuously listening for replies the whole time (not burst-then-listen: a fast reply to
/// the first announcement must not be missed while later ones are still going out). The collect
/// window then continues for `timeout_ms` *past the last announcement*, not from the start of
/// the call, so a reply to that last announcement still has time to land.
///
/// Worst-case duration is bounded and deterministic: `(ANNOUNCE_BURST_COUNT - 1) *
/// ANNOUNCE_BURST_INTERVAL + timeout_ms`. With the `timeout_ms` default used by
/// `commands::localsend::localsend_discover` (2500ms), that's `2 * 1250ms + 2500ms = 5000ms`
/// (5s) worst case for one Refresh — up from the previous single-announcement design's ~2.5s,
/// traded for actually being able to hear back on wifi.
///
/// Deduped by fingerprint (falling back to ip:port when a peer omits a fingerprint). Tolerant
/// of a blocked/degraded multicast join — that yields an empty list rather than an error, and
/// the manual-IP path is the fallback (handled in LS2/LS3); see [`announce_on_interfaces`] for
/// why a degraded join still announces. Socket setup and join/announce failures are still
/// surfaced (logged), so a persistently empty list is diagnosable instead of indistinguishable
/// from "no peers answered".
pub async fn discover(timeout_ms: u64) -> Result<Vec<Device>, String> {
    discover_on(timeout_ms, MULTICAST_PORT, &local_multicast_interfaces()).await
}

/// As [`discover`], but taking the local bind port and interface list as parameters instead of
/// reaching for [`MULTICAST_PORT`] / [`local_multicast_interfaces`] itself — mirrors
/// [`join_multicast_on_all_interfaces`]'s own testability seam. `discover()` always passes
/// `MULTICAST_PORT`; tests pass `0` (an ephemeral port) and a loopback-only interface list, so
/// the whole join/burst/collect sequence is exercisable against a same-host peer without
/// depending on real network hardware or contending with whatever else may hold the well-known
/// port on the host running the test.
async fn discover_on(
    timeout_ms: u64,
    bind_port: u16,
    interfaces: &[Ipv4Addr],
) -> Result<Vec<Device>, String> {
    let our_fp = fingerprint();

    let socket = open_reusable_udp_socket(bind_port)
        .map_err(|e| format!("LocalSend discovery: couldn't open a UDP socket: {e}"))?;

    // The UDP port we actually bound (may not be `bind_port`/`OUR_PORT` if that was
    // unavailable even with SO_REUSEADDR/SO_REUSEPORT). Only the fallback advertisement below
    // uses it; normally we advertise the register listener's port instead.
    let udp_port = socket
        .local_addr()
        .ok()
        .and_then(|a| a.as_socket_ipv4())
        .map(|a| a.port())
        .unwrap_or(OUR_PORT);

    // Bind the inbound register listener for the length of this pass and advertise *its*
    // port, so a peer's HTTP register reply reaches ChairPhoto rather than whatever else may
    // hold 53317 (issue #55). A bind failure is not fatal: fall back to the pre-#55
    // advertisement and rely on peers' UDP fallback, degraded rather than broken.
    let (listener, advertised_port) = match register::bind().await {
        Ok((listener, port)) => (Some(listener), port),
        Err(e) => {
            eprintln!(
                "localsend: couldn't bind the register listener ({e}); falling back to \
                 advertising the UDP port with no listener behind it — peers that can't \
                 register will still be heard via their UDP fallback"
            );
            (None, udp_port)
        }
    };

    let joined = join_multicast_on_all_interfaces(&socket, interfaces);

    let socket = UdpSocket::from_std(socket.into())
        .map_err(|e| format!("LocalSend discovery: couldn't hand the socket to tokio: {e}"))?;

    let announcement = our_info_with_port(&our_fp, true, advertised_port);

    // Peers now reach us two ways: a UDP `announce:false` datagram on the shared socket, or an
    // HTTP register POST on the listener above. Both feed the same `seen` map. Rather than have
    // two futures contend for a mutable borrow of `seen`, both *send* on one channel and a
    // single `collect` future owns the map and drains it. That also keeps the dedupe rule in
    // one place, so a peer heard on both paths still yields one Device.
    let (found_tx, mut found_rx) = tokio::sync::mpsc::unbounded_channel::<Device>();
    let udp_tx = found_tx.clone();

    let mut seen: HashMap<String, Device> = HashMap::new();

    // Announce a burst, concurrently with collecting replies — not burst-then-listen (see the
    // doc comment on `discover`).
    let announce_burst = async {
        for i in 0..ANNOUNCE_BURST_COUNT {
            announce_on_interfaces(&socket, &joined, &announcement);
            if i + 1 < ANNOUNCE_BURST_COUNT {
                tokio::time::sleep(ANNOUNCE_BURST_INTERVAL).await;
            }
        }
    };

    // The UDP half: unchanged in behavior, but it now hands finds to `collect` instead of
    // inserting them itself. Owns its own receive buffer so nothing is borrowed across the
    // concurrent futures below.
    let udp_collect = async {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match socket.recv_from(&mut buf).await {
                Ok((n, from)) => {
                    let text = String::from_utf8_lossy(&buf[..n]);
                    let ip = from.ip().to_string();
                    if let Some(dev) = parse_announcement(&text, &ip, &our_fp) {
                        if udp_tx.send(dev).is_err() {
                            break; // collector gone; nothing left to report to
                        }
                    }
                }
                Err(e) => {
                    eprintln!("localsend: discovery recv stopped early: {e}");
                    break;
                }
            }
        }
    };

    // The HTTP half (issue #55). Serves only for this pass: when the deadline below fires,
    // this future is dropped, taking the listener and every in-flight handler with it.
    let serve_register = async {
        match listener {
            Some(listener) => {
                register::serve(listener, our_fp.clone(), advertised_port, found_tx).await
            }
            // No listener bound. Drop our sender so the channel can close once the UDP half
            // is done, instead of holding `collect` open on a source that will never produce.
            None => drop(found_tx),
        }
    };

    // Collect indefinitely; the outer `timeout` below is what bounds it.
    let collect = async {
        while let Some(dev) = found_rx.recv().await {
            let key = if dev.fingerprint.is_empty() {
                format!("{}:{}", dev.ip, dev.port)
            } else {
                dev.fingerprint.clone()
            };
            seen.entry(key).or_insert(dev);
        }
    };

    // Everything that listens, as one future, so `burst_and_collect`'s two-argument shape (and
    // the guarantee its doc comment makes about concurrent progress) still describes what runs.
    let collect = async {
        tokio::join!(udp_collect, serve_register, collect);
    };

    // Bounded total: the burst span plus a reply window past the last announcement (see the
    // `discover` doc comment for the worst-case number). `announce_burst` finishes well inside
    // this window and `collect` never finishes on its own, so this deadline is what ends the
    // call either way.
    let burst_span = ANNOUNCE_BURST_INTERVAL * (ANNOUNCE_BURST_COUNT - 1) as u32;
    let deadline = burst_span + Duration::from_millis(timeout_ms);
    let _ = tokio::time::timeout(deadline, burst_and_collect(announce_burst, collect)).await;

    Ok(seen.into_values().collect())
}

/// Run `announce` and `collect` to completion *concurrently* — i.e. both make progress from
/// the first poll onward, not "run `announce` to completion, then start `collect`"
/// (burst-then-listen). Pulled out of [`discover_on`] as its own named function purely so this
/// one property has a test (`burst_and_collect_polls_both_futures_before_either_completes`)
/// that doesn't depend on real UDP timing or kernel socket buffering to observe it: a reply
/// that arrives on a live socket sits in the kernel receive buffer whether or not `collect` has
/// been polled yet, since the socket is already bound and joined before either future starts —
/// so an end-to-end test built only from "did a loopback peer's reply come back" cannot tell
/// concurrent execution apart from sequential `announce.await; collect.await`, which happens to
/// return the same reply just later. `tokio::join!` is what actually gives the concurrency
/// (it polls every branch on each wake — see its own docs); this wrapper exists so a mutation
/// that replaces the `join!` with sequential awaits has something guarding it directly.
async fn burst_and_collect(announce: impl std::future::Future<Output = ()>, collect: impl std::future::Future<Output = ()>) {
    tokio::join!(announce, collect);
}

/// Open a UDP socket for LocalSend discovery with `SO_REUSEADDR` and (on unix) `SO_REUSEPORT`
/// set *before* binding, so we can bind `port` alongside another LocalSend-speaking process
/// that already holds it — notably the official desktop app, which autostarts and holds
/// `MULTICAST_PORT` for as long as the user is logged in. Without this, discovery silently
/// lands on an ephemeral port and can never receive anything addressed to the well-known group
/// port. Falls back to an ephemeral port if even a reuse-enabled bind fails, logging why.
/// Returns the raw `socket2` socket (not yet handed to tokio) so the caller can still use
/// `set_multicast_if_v4` per announce — an option tokio's `UdpSocket` doesn't expose directly
/// (see [`announce_on_interfaces`], which recovers it from the tokio socket via
/// [`SockRef`]).
///
/// unix only: on Windows, `SO_REUSEADDR` is *not* the BSD "share the port" semantic — Winsock
/// lets a later binder forcibly capture datagrams away from an earlier one (Microsoft's own
/// guidance is to use `SO_EXCLUSIVEADDRUSE` for the sharing case instead, which this doesn't
/// set). The entire motivation here — coexisting with a co-resident LocalSend desktop app on
/// `:53317` — is a unix scenario we can build and test; setting `SO_REUSEADDR` on Windows would
/// risk ChairPhoto silently stealing the port from a co-resident LocalSend desktop app rather
/// than sharing it. We cannot build or test Windows in this environment, so Windows
/// deliberately keeps its pre-existing behavior (fall back to an ephemeral port on contention)
/// rather than risk that on a platform we can't verify.
fn open_reusable_udp_socket(port: u16) -> std::io::Result<Socket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(SockProtocol::UDP))?;
    #[cfg(unix)]
    {
        socket.set_reuse_address(true)?;
        socket.set_reuse_port(true)?;
    }

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
/// as the outgoing interface first (`IP_MULTICAST_IF`, via a [`SockRef`] borrowed from the
/// tokio socket — tokio's `UdpSocket` doesn't expose that option itself, but does implement
/// `AsFd`/`AsSocket`, which is all `SockRef` needs) — a single send on a wildcard-bound socket
/// only egresses via whichever interface the OS's default route picks, which under-covers a
/// multi-NIC host exactly like the join this mirrors. Verified concretely on the machine this
/// was written on: a send *without* an explicit `IP_MULTICAST_IF` follows the unicast default
/// route (a wired NIC) and does not reach a receiver joined only on loopback — the selection
/// isn't cosmetic. Errors are logged, not propagated: one interface failing to send must not
/// stop the others or fail discovery outright.
///
/// If `joined` is empty — every multicast join failed, or [`join_multicast_on_all_interfaces`]
/// was never able to join any interface at all — still sends one unconditional announcement on
/// the socket's default route, matching the base (pre-multi-interface) implementation's
/// `let _ = socket.send_to(...)`. Announcing does not require having joined: joining is what
/// lets *us* receive group-addressed replies, but a peer only needs to be a group member to
/// *hear* our announcement, so a fully blocked join on our side must not mean total silence —
/// this is what keeps `discover()`'s own doc comment claim of tolerating "a blocked/degraded
/// multicast join" actually true, rather than aspirational.
fn announce_on_interfaces(socket: &UdpSocket, joined: &[Ipv4Addr], announcement: &serde_json::Value) {
    let bytes = announcement.to_string();
    let group = SockAddr::from(SocketAddrV4::new(MULTICAST_ADDR, MULTICAST_PORT));
    let sock_ref = SockRef::from(socket);

    if joined.is_empty() {
        if let Err(e) = sock_ref.send_to(bytes.as_bytes(), &group) {
            eprintln!("localsend: announce (no interface joined) failed: {e}");
        }
        return;
    }

    for iface in joined {
        if *iface != Ipv4Addr::UNSPECIFIED {
            if let Err(e) = sock_ref.set_multicast_if_v4(iface) {
                eprintln!(
                    "localsend: couldn't select {iface} as the outgoing interface for the \
                     announce: {e}"
                );
                continue;
            }
        }
        if let Err(e) = sock_ref.send_to(bytes.as_bytes(), &group) {
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

/// An HTTP client honoring the device's protocol.
///
/// For https LAN peers (self-signed certs, pinned by fingerprint at the LocalSend layer) we
/// accept their certificate *and* present one of our own. Some peers — LocalSend mobile among
/// them — require a client certificate and abort the handshake with `certificate required`
/// without one, before any LocalSend request is read; `danger_accept_invalid_certs` governs
/// only how we treat theirs. Peers that do not require one ignore it. See [`identity`].
///
/// A missing identity is not fatal: the client is still built, so http peers and https peers
/// that do not demand a certificate keep working (see [`identity::client_identity`]).
fn client_for(device: &Device) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder();
    if device.protocol.eq_ignore_ascii_case("https") {
        builder = builder.danger_accept_invalid_certs(true);
        if let Some(id) = identity::client_identity() {
            builder = builder.identity(id);
        }
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
    use std::sync::Mutex;

    // Several tests below bind the *real* well-known multicast port (`MULTICAST_PORT`, 53317)
    // with SO_REUSEADDR/SO_REUSEPORT so a loopback-joined test peer can actually receive what
    // `discover_on` sends — that's the whole point (see `collect_on_loopback`). Because multiple
    // reuse-enabled sockets bound to the same port form a fanout group (every member gets its
    // own copy of an incoming multicast datagram — verified: this is *not* the load-balanced,
    // single-recipient behavior SO_REUSEPORT gives unicast UDP), two such tests running
    // concurrently on the same host would cross-talk and see each other's traffic. `cargo test`
    // runs tests in parallel by default, so serialize just these with a lock rather than
    // reaching for a `#[serial]`-style crate this module doesn't otherwise need.
    static NETWORK_TEST_LOCK: Mutex<()> = Mutex::new(());
    fn lock_network_tests() -> std::sync::MutexGuard<'static, ()> {
        NETWORK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Test-only "fake LocalSend peer": joins the multicast group on loopback and binds
    /// [`MULTICAST_PORT`] (reuse-enabled, via the same [`open_reusable_udp_socket`] production
    /// uses, so it coexists with `discover_on`'s own socket and with anything real already on
    /// that port on the host running the test), then collects every datagram arriving within
    /// `window` as (parsed JSON body, sender's actual UDP source port). The source port is
    /// ground truth for "what port did the sender's socket really bind" — the OS controls it,
    /// not anything the payload claims — which is what makes the port-advertisement test below
    /// meaningful rather than circular.
    async fn collect_on_loopback(window: Duration) -> Vec<(serde_json::Value, u16)> {
        let peer = open_reusable_udp_socket(MULTICAST_PORT).expect("peer socket must bind");
        peer.join_multicast_v4(&MULTICAST_ADDR, &Ipv4Addr::LOCALHOST)
            .expect("peer must join the group on loopback");
        let peer = UdpSocket::from_std(peer.into()).expect("peer must hand off to tokio");
        let mut buf = vec![0u8; 64 * 1024];
        let mut out = Vec::new();
        let collect = async {
            loop {
                if let Ok((n, from)) = peer.recv_from(&mut buf).await {
                    if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&buf[..n]) {
                        out.push((json, from.port()));
                    }
                }
            }
        };
        let _ = tokio::time::timeout(window, collect).await;
        out
    }

    /// Probe whether multicast actually round-trips over loopback on this host: join the group
    /// on `Ipv4Addr::LOCALHOST` on one socket, send one datagram to the group selecting
    /// loopback as the egress interface from another, and confirm the first socket receives
    /// *something* within a short window (any datagram counts — this only asks "does loopback
    /// multicast work here", not "did our own datagram arrive", so concurrent traffic from
    /// other tests or the real LocalSend desktop app is a valid positive signal too).
    ///
    /// False in environments where it doesn't — observed directly in a private network
    /// namespace (`unshare -rn`) with `lo` administratively down, where `announce_on_interfaces`
    /// and `discover_on` both degrade gracefully in production (an empty `joined` list, a failed
    /// send logged and swallowed — see their doc comments) but a test asserting on what a
    /// loopback peer *received* has nothing to receive against, which is a fact about this host,
    /// not a ChairPhoto defect. The four `#[tokio::test]`s that depend on a loopback round trip
    /// call this first and skip loudly (`SKIPPED:`) when it's false, rather than failing on an
    /// assertion that's really testing the host's network stack.
    async fn loopback_multicast_round_trips() -> bool {
        let receiver = match open_reusable_udp_socket(MULTICAST_PORT) {
            Ok(s) => s,
            Err(_) => return false,
        };
        if receiver
            .join_multicast_v4(&MULTICAST_ADDR, &Ipv4Addr::LOCALHOST)
            .is_err()
        {
            return false;
        }
        let receiver = match UdpSocket::from_std(receiver.into()) {
            Ok(s) => s,
            Err(_) => return false,
        };

        let sender = match open_reusable_udp_socket(0) {
            Ok(s) => s,
            Err(_) => return false,
        };
        if sender.set_multicast_if_v4(&Ipv4Addr::LOCALHOST).is_err() {
            return false;
        }
        let group = SockAddr::from(SocketAddrV4::new(MULTICAST_ADDR, MULTICAST_PORT));
        if sender.send_to(b"loopback-multicast-probe", &group).is_err() {
            return false;
        }

        let mut buf = [0u8; 64];
        tokio::time::timeout(Duration::from_millis(200), receiver.recv_from(&mut buf))
            .await
            .is_ok()
    }

    /// Common guard for the four `#[tokio::test]`s below that depend on a loopback peer
    /// actually receiving something: prints a `SKIPPED:` line and returns `true` (telling the
    /// caller to `return` immediately) when [`loopback_multicast_round_trips`] says this host
    /// can't do that. Takes the test's own name only so the printed line says which test
    /// skipped.
    async fn skip_if_no_loopback_multicast(test_name: &str) -> bool {
        if loopback_multicast_round_trips().await {
            return false;
        }
        eprintln!(
            "SKIPPED: {test_name} — multicast doesn't round-trip over loopback on this host \
             (observed: a network namespace with lo down; lo administratively down would also \
             do it). This test needs a loopback peer to actually receive something; that's an \
             environment fact, not something discover_on/announce_on_interfaces can fix — both \
             already degrade gracefully (see their doc comments)."
        );
        true
    }

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

    // unix only: SO_REUSEADDR/SO_REUSEPORT sharing is a unix-only behavior of
    // `open_reusable_udp_socket` (see its doc comment) — on Windows, `SO_REUSEADDR` steals a
    // port instead of sharing it, so we deliberately don't set it there, and this test's
    // "second open lands on the same port" expectation would not hold.
    #[cfg(unix)]
    #[test]
    fn open_reusable_udp_socket_shares_a_port_already_held_by_another_reusable_socket() {
        // First holder: bind an ephemeral port the same way a second LocalSend-speaking
        // process would already have bound it (SO_REUSEADDR/SO_REUSEPORT before bind) — this
        // is what the real LocalSend desktop app does on the owner's machine, simulated here
        // without depending on that process actually running.
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

    // `discover()` itself had no test at all before this — `advertised_port_matches_the_port_
    // actually_bound` (the test this replaces) re-implemented `discover`'s own three lines
    // (open the socket, read back its port, build the announcement) in its own body instead of
    // calling `discover`/`discover_on`, so it could not regress with the code it was meant to
    // guard. The tests below drive `discover_on` itself against a loopback fake peer.
    //
    // `discover_on(_, 0, &[Ipv4Addr::LOCALHOST])`'s own worst case is `(ANNOUNCE_BURST_COUNT -
    // 1) * ANNOUNCE_BURST_INTERVAL + timeout_ms` = 2 * 1250ms + 200ms = 2700ms; the peer's own
    // collect window below is given headroom past that.
    //
    // Discovered empirically on the machine these were written on (and left in place rather
    // than "fixed away", because it's real signal, not test pollution to eliminate): the real,
    // already-running LocalSend desktop app receives these loopback-sent test announcements
    // too (its own interface enumeration apparently doesn't exclude loopback the way ours
    // deliberately does) and replies over multicast — multiple times per announcement, from its
    // real LAN address. That's independent, unplanned confirmation that a real LocalSend peer
    // does react to an announcement reaching it, which is reassuring for whether the R2 burst
    // fix helps on the phone. For test determinism it means every assertion below must select
    // *our own* announcement/reply (`alias == OUR_ALIAS`) out of whatever else the loopback
    // peer receives, rather than assuming the peer hears from nobody else.
    #[tokio::test]
    async fn discover_sends_an_announcement_a_loopback_peer_can_receive() {
        // Kills three otherwise-unobserved regressions in the announce path, all of which leave
        // a loopback peer with nothing from us: `announce_on_interfaces` never calling
        // `send_to` at all; `discover_on` never calling `announce_on_interfaces` at all; and
        // dropping the `set_multicast_if_v4` interface selection, which sends via the OS's
        // default unicast route instead of the interface actually joined — confirmed separately
        // (probing this exact host) that a send without that explicit selection does not reach
        // a receiver joined only on loopback.
        //
        // Asserts "at least one", not "exactly `ANNOUNCE_BURST_COUNT`": on the machine this was
        // written on, the real background LocalSend app's own reply flood (see the module note
        // above) occasionally crowds out one of our 3 announcements before the test peer's
        // `recv_from` gets to it — a real receive-buffer race under load, not a defect in
        // `discover_on`. The burst count/spacing itself is pinned separately, by timing, in
        // `discover_takes_the_full_burst_span_before_returning` below.
        let _guard = lock_network_tests();
        if skip_if_no_loopback_multicast("discover_sends_an_announcement_a_loopback_peer_can_receive").await {
            return;
        }

        let discover_fut = discover_on(200, 0, &[Ipv4Addr::LOCALHOST]);
        let collect_fut = collect_on_loopback(Duration::from_millis(3200));
        let (devices, received) = tokio::join!(discover_fut, collect_fut);
        devices.expect("discover_on must not error");

        let ours: Vec<_> = received.iter().filter(|(json, _)| json["alias"] == OUR_ALIAS).collect();
        assert!(
            !ours.is_empty(),
            "expected at least one of our own announcements, got none out of {} total \
             received: {received:?}",
            received.len()
        );
        for (json, _source_port) in &ours {
            assert_eq!(json["announce"], true);
        }
    }

    #[tokio::test]
    async fn discover_takes_the_full_burst_span_before_returning() {
        // Timing-based check for the R2 burst, independent of actual packet delivery (this host
        // has an already-running, very chatty real LocalSend peer that makes exact
        // received-datagram counts unreliable — see the module note above, and the previous
        // test). `discover_on`'s deadline is `(ANNOUNCE_BURST_COUNT - 1) *
        // ANNOUNCE_BURST_INTERVAL + timeout_ms`, and announcing runs concurrently with (not
        // before) collecting, so the call cannot return before the full burst span has elapsed.
        // A regression back to a single announcement (no burst, or a burst collapsed to one
        // send) would return right after `timeout_ms` instead, far short of the burst span.
        //
        // Also takes the network-test lock: this call sends real datagrams to
        // `224.0.0.167:MULTICAST_PORT` over loopback (an ephemeral bind port, but the send
        // still targets the shared well-known group/port) — the same traffic the lock exists
        // to isolate. Previously this was the one new socket test that didn't take it, and was
        // the unguarded source of cross-talk that let the R5 mutation survive against
        // `announce_on_interfaces_still_announces_when_nothing_joined`'s old unfiltered
        // assertion (now fixed on both sides — see that test).
        let _guard = lock_network_tests();

        // A literal, not `ANNOUNCE_BURST_INTERVAL * (ANNOUNCE_BURST_COUNT - 1)`: computing the
        // expectation from the same constants the burst loop uses means a mutation that
        // collapses either constant (e.g. `ANNOUNCE_BURST_INTERVAL` 1250ms -> 1ms) collapses
        // the expectation right along with it and this assertion never notices. 2500ms is
        // `(3 - 1) * 1250ms` written out, matching the doc comments on
        // `ANNOUNCE_BURST_COUNT`/`ANNOUNCE_BURST_INTERVAL`/`discover`; `burst_count_and_
        // interval_are_pinned` below pins the two factors themselves so a deliberate change to
        // either is a conscious edit here too, not a silent one-line mutation.
        let expected_burst_span = Duration::from_millis(2500);

        let started = std::time::Instant::now();
        discover_on(50, 0, &[Ipv4Addr::LOCALHOST])
            .await
            .expect("discover_on must not error");
        let elapsed = started.elapsed();

        assert!(
            elapsed >= expected_burst_span,
            "discover_on returned after {elapsed:?}, before the expected burst span \
             {expected_burst_span:?} even elapsed — the burst isn't actually spaced out over \
             multiple sends"
        );
        // Generous upper bound: this pins gross regressions (e.g. the burst span being applied
        // twice), not tight scheduling precision.
        let generous_ceiling =
            expected_burst_span + Duration::from_millis(50) + Duration::from_secs(2);
        assert!(
            elapsed < generous_ceiling,
            "discover_on took {elapsed:?}, far past its documented worst case"
        );
    }

    // `discover_takes_the_full_burst_span_before_returning` above pins the *product*
    // `(ANNOUNCE_BURST_COUNT - 1) * ANNOUNCE_BURST_INTERVAL` against an independent literal.
    // It does not pin the two factors individually — collapsing `ANNOUNCE_BURST_COUNT` to 1
    // happens to fail three reception tests too, but incidentally: `collect_on_loopback`'s peer
    // socket is only created when the collect future is first polled, which races
    // `discover_on`'s first announcement rather than actually observing the burst being
    // collapsed. Pin both constants directly instead of relying on that.
    #[test]
    fn burst_count_and_interval_are_pinned() {
        assert_eq!(
            ANNOUNCE_BURST_COUNT, 3,
            "matches the reference LocalSend implementation's burst count — see the const's \
             doc comment"
        );
        assert_eq!(
            ANNOUNCE_BURST_INTERVAL,
            Duration::from_millis(1250),
            "(ANNOUNCE_BURST_COUNT - 1) * ANNOUNCE_BURST_INTERVAL = 2.5s total span, matching \
             the reference's ~2.6s — see the const's doc comment"
        );
    }

    // `discover_takes_the_full_burst_span_before_returning` proves the *duration* matches a
    // burst run concurrently with collecting. It does not prove the concurrency itself: an
    // implementation that runs `announce_burst.await` fully, *then* `collect.await`
    // (burst-then-listen — exactly what `discover`'s doc comment says this is not) takes the
    // same total wall time, because `collect` never terminates on its own and the outer
    // `timeout` is what ends the call either way — see `burst_and_collect`'s doc comment for
    // why an end-to-end UDP test can't tell the two apart. This test proves the concurrency
    // directly, with no sockets or real timers involved: `tokio::join!` polls every branch on
    // each wake of the combined future, so both mock futures below must be polled at least
    // once even though neither one ever completes. Sequential `.await`s would leave the second
    // one — `collect_probe` here — untouched, since the first `.await` (`announce_probe`) never
    // resolves either.
    #[tokio::test]
    async fn burst_and_collect_polls_both_futures_before_either_completes() {
        use std::future::Future;
        use std::pin::Pin;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::task::{Context, Poll};

        /// Records that it was polled, then stays `Pending` forever — standing in for
        /// `collect`, which likewise never completes on its own (only the caller's `timeout`
        /// ends it).
        struct RecordPoll<'a>(&'a AtomicBool);
        impl Future for RecordPoll<'_> {
            type Output = ();
            fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
                self.0.store(true, Ordering::SeqCst);
                Poll::Pending
            }
        }

        let announce_polled = AtomicBool::new(false);
        let collect_polled = AtomicBool::new(false);

        // Bounded from the outside only to end the test — `burst_and_collect` never returns on
        // its own here, matching how `discover_on` relies on its own outer `timeout` rather
        // than `collect` completing.
        let _ = tokio::time::timeout(
            Duration::from_millis(20),
            burst_and_collect(RecordPoll(&announce_polled), RecordPoll(&collect_polled)),
        )
        .await;

        assert!(announce_polled.load(Ordering::SeqCst), "the announce branch was never polled");
        assert!(
            collect_polled.load(Ordering::SeqCst),
            "the collect branch was never polled — burst_and_collect is running its arguments \
             sequentially (announce to completion, then collect) instead of concurrently; a \
             correct implementation polls both from the very first poll, and neither of these \
             mock futures ever returns Ready, so `collect` can only have been touched if it was \
             actually polled alongside `announce`"
        );
    }

    // --- issue #55: the inbound register listener ------------------------------------------

    #[tokio::test]
    async fn discover_advertises_a_port_it_actually_serves_register_on() {
        // Supersedes the pre-#55 assertion that the announced port equalled the *UDP* socket's
        // own source port. That invariant is deliberately gone: we now announce the TCP
        // register listener's port, which is a different port by construction. What replaces
        // it is stronger — the announced port must be one ChairPhoto has really bound and
        // really answers `register` on — and this asserts it end to end, from the peer's side.
        //
        // The fake peer here sends **no UDP reply at all**. It is discovered purely because its
        // HTTP register POST reached us, which is the exact path issue #55 was about: with a
        // co-resident LocalSend app holding 53317, that POST is the only way a peer can arrive.
        let _guard = lock_network_tests();
        if skip_if_no_loopback_multicast("discover_advertises_a_port_it_actually_serves_register_on").await {
            return;
        }

        let peer_task = tokio::spawn(async {
            let peer = open_reusable_udp_socket(MULTICAST_PORT).expect("peer socket must bind");
            peer.join_multicast_v4(&MULTICAST_ADDR, &Ipv4Addr::LOCALHOST)
                .expect("peer must join the group on loopback");
            let peer = UdpSocket::from_std(peer.into()).expect("peer must hand off to tokio");

            // Wait specifically for *our* announcement and take the port it advertises — a real
            // LocalSend process on this host announces on the same group (see the module note).
            let mut buf = vec![0u8; 64 * 1024];
            let wait_for_ours = async {
                loop {
                    if let Ok((n, _)) = peer.recv_from(&mut buf).await {
                        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&buf[..n]) {
                            if json["alias"] == OUR_ALIAS {
                                return json["port"].as_u64().map(|p| p as u16);
                            }
                        }
                    }
                }
            };
            let advertised = tokio::time::timeout(Duration::from_secs(3), wait_for_ours)
                .await
                .ok()
                .flatten()?;

            // Reply the way a spec-conformant peer does: HTTP register to the advertised port.
            // Independently written rather than built from `our_info_with_port`, so this tests
            // the wire shape a real peer sends and not our own serializer against itself.
            let body = serde_json::json!({
                "alias": "Register Peer Phone",
                "version": "2.0",
                "deviceModel": "Test",
                "deviceType": "mobile",
                "fingerprint": "register-peer-fingerprint",
                "port": 53317,
                "protocol": "http",
                "download": false,
            })
            .to_string();
            let mut client = tokio::net::TcpStream::connect(("127.0.0.1", advertised))
                .await
                .ok()?;
            let request = format!(
                "POST /api/localsend/v2/register HTTP/1.1\r\nHost: localhost\r\n\
                 Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            tokio::io::AsyncWriteExt::write_all(&mut client, request.as_bytes())
                .await
                .ok()?;
            let mut response = Vec::new();
            tokio::io::AsyncReadExt::read_to_end(&mut client, &mut response)
                .await
                .ok()?;
            Some((advertised, String::from_utf8_lossy(&response).into_owned()))
        });

        let devices = discover_on(1500, 0, &[Ipv4Addr::LOCALHOST])
            .await
            .expect("discover_on must not error");
        let (advertised, response) = peer_task
            .await
            .expect("peer task must not panic")
            .expect("the peer must have heard an announcement and reached the advertised port");

        assert_ne!(advertised, OUR_PORT, "an ephemeral listener must not land on 53317");
        assert!(
            response.starts_with("HTTP/1.1 200 OK"),
            "the advertised port must answer register with 200; got: {response}"
        );
        assert!(
            devices.iter().any(|d| d.alias == "Register Peer Phone"),
            "a peer that replied only by HTTP register must still be discovered, got: {devices:?}"
        );

        // The listener is scoped to the pass: `discover_on` has returned, so the port it
        // advertised must be free again. Nothing of ChairPhoto's stays LAN-reachable between
        // discoveries.
        tokio::net::TcpListener::bind(("0.0.0.0", advertised))
            .await
            .expect("the register listener must be released when the discovery pass ends");
    }

    #[tokio::test]
    async fn discover_still_hears_the_udp_fallback() {
        // The register listener must not have displaced the UDP path — that stays the fallback
        // for peers whose register fails, which is every peer whenever the listener could not
        // be bound at all. Same round trip as the pre-#55 test, re-asserted after the rewrite
        // of the collect loop into channel-fed form.
        let _guard = lock_network_tests();
        if skip_if_no_loopback_multicast("discover_still_hears_the_udp_fallback").await {
            return;
        }

        let peer_task = tokio::spawn(async {
            let peer = open_reusable_udp_socket(MULTICAST_PORT).expect("peer socket must bind");
            peer.join_multicast_v4(&MULTICAST_ADDR, &Ipv4Addr::LOCALHOST)
                .expect("peer must join the group on loopback");
            peer.set_multicast_if_v4(&Ipv4Addr::LOCALHOST)
                .expect("peer must select loopback as its outgoing interface");
            let peer = UdpSocket::from_std(peer.into()).expect("peer must hand off to tokio");

            let mut buf = vec![0u8; 64 * 1024];
            let wait_for_ours = async {
                loop {
                    if let Ok((n, _from)) = peer.recv_from(&mut buf).await {
                        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&buf[..n]) {
                            if json["alias"] == OUR_ALIAS {
                                return;
                            }
                        }
                    }
                }
            };
            let _ = tokio::time::timeout(Duration::from_secs(3), wait_for_ours).await;

            let reply = serde_json::json!({
                "alias": "UDP Fallback Phone",
                "version": "2.0",
                "deviceModel": "Test",
                "deviceType": "mobile",
                "fingerprint": "udp-fallback-fingerprint",
                "port": 9999,
                "protocol": "http",
                "download": false,
                "announce": false,
            })
            .to_string();
            let group = SockAddr::from(SocketAddrV4::new(MULTICAST_ADDR, MULTICAST_PORT));
            let sock_ref = SockRef::from(&peer);
            let _ = sock_ref.send_to(reply.as_bytes(), &group);
        });

        // Needs the real MULTICAST_PORT for the same reason as the round-trip test below.
        let devices = discover_on(500, MULTICAST_PORT, &[Ipv4Addr::LOCALHOST])
            .await
            .expect("discover_on must not error");
        peer_task.await.expect("peer task must not panic");

        assert!(
            devices.iter().any(|d| d.alias == "UDP Fallback Phone"),
            "the UDP fallback must still produce a Device, got: {devices:?}"
        );
    }

    #[tokio::test]
    async fn discover_returns_a_device_when_a_loopback_peer_replies() {
        // Full round trip against a same-host fake peer: join, announce, the peer replies with
        // its own announcement (mimicking a real peer's UDP `announce:false` fallback reply),
        // and `discover_on`'s own collect loop must parse that reply into a returned `Device`.
        // The tests above only check that *we* transmit; this checks that we can also *receive*.
        let _guard = lock_network_tests();
        if skip_if_no_loopback_multicast("discover_returns_a_device_when_a_loopback_peer_replies").await {
            return;
        }

        let peer_task = tokio::spawn(async {
            let peer = open_reusable_udp_socket(MULTICAST_PORT).expect("peer socket must bind");
            peer.join_multicast_v4(&MULTICAST_ADDR, &Ipv4Addr::LOCALHOST)
                .expect("peer must join the group on loopback");
            peer.set_multicast_if_v4(&Ipv4Addr::LOCALHOST)
                .expect("peer must select loopback as its outgoing interface");
            let peer = UdpSocket::from_std(peer.into()).expect("peer must hand off to tokio");

            let mut buf = vec![0u8; 64 * 1024];
            // Wait specifically for *our* announcement (see the module note above on why: a
            // real, unrelated LocalSend process on this host also answers on this group), then
            // reply once with a distinct fake device — a minimal, independently-written
            // payload, not built from `our_info_with_port`, so this doesn't just test our own
            // serialization against itself. Bounded so a broken send path fails fast rather
            // than hanging the test.
            let wait_for_ours = async {
                loop {
                    if let Ok((n, _from)) = peer.recv_from(&mut buf).await {
                        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&buf[..n]) {
                            if json["alias"] == OUR_ALIAS {
                                return;
                            }
                        }
                    }
                }
            };
            let _ = tokio::time::timeout(Duration::from_secs(3), wait_for_ours).await;

            let reply = serde_json::json!({
                "alias": "Fake Peer Phone",
                "version": "2.0",
                "deviceModel": "Test",
                "deviceType": "mobile",
                "fingerprint": "fake-peer-fingerprint",
                "port": 9999,
                "protocol": "http",
                "download": false,
                "announce": false,
            })
            .to_string();
            let group = SockAddr::from(SocketAddrV4::new(MULTICAST_ADDR, MULTICAST_PORT));
            let sock_ref = SockRef::from(&peer);
            let _ = sock_ref.send_to(reply.as_bytes(), &group);
        });

        // Unlike the send-only tests above, this needs the *real* MULTICAST_PORT: the peer's
        // reply is addressed to `224.0.0.167:MULTICAST_PORT` (protocol-fixed, see
        // `announce_on_interfaces`), and UDP delivery is filtered by destination port matching
        // the receiver's own bound port — a `discover_on` bound to an ephemeral port (as the
        // other tests use, to sidestep contending for the well-known port) could never receive
        // it, no matter how correct the rest of the round trip is. `open_reusable_udp_socket`'s
        // SO_REUSEADDR/SO_REUSEPORT is what lets this coexist with the peer above and with the
        // real background LocalSend app, all three bound to 53317 at once.
        let devices = discover_on(500, MULTICAST_PORT, &[Ipv4Addr::LOCALHOST])
            .await
            .expect("discover_on must not error");
        peer_task.await.expect("peer task must not panic");

        assert!(
            devices.iter().any(|d| d.alias == "Fake Peer Phone"),
            "expected the loopback peer's reply to show up as a discovered Device, got: \
             {devices:?}"
        );
    }

    #[tokio::test]
    async fn announce_on_interfaces_still_announces_when_nothing_joined() {
        // Kills the R5 regression: previously, an empty `joined` meant the for loop in
        // `announce_on_interfaces` ran zero times, so a fully blocked multicast join produced
        // total silence. The base (pre-multi-interface) implementation announced
        // unconditionally, and `discover()`'s own doc comment claims tolerance of a blocked
        // join — this pins that it's actually true. Loopback is pre-selected as the socket's
        // own default multicast egress (`set_multicast_if_v4`, *not* through the function under
        // test) standing in for whatever a real default route would pick on a real host —
        // probed directly on this machine, a send that does *not* select an interface does not
        // loop back to a local receiver at all, host-route-dependent and not something a
        // hermetic test should rely on. What's under test here is only whether the empty-
        // `joined` branch calls `send_to` at all.
        let _guard = lock_network_tests();
        if skip_if_no_loopback_multicast("announce_on_interfaces_still_announces_when_nothing_joined").await {
            return;
        }

        let socket = open_reusable_udp_socket(0).unwrap();
        socket.set_multicast_if_v4(&Ipv4Addr::LOCALHOST).unwrap();
        let socket = UdpSocket::from_std(socket.into()).unwrap();
        // A distinctive port (not a real bound ephemeral port `discover_on` would ever
        // advertise, and not anything the real background LocalSend app announces) so the
        // assertion below can pick our own datagram out of whatever else is on the group —
        // see the module note above `discover_sends_an_announcement_a_loopback_peer_can_receive`
        // on why `received` can't be trusted unfiltered: this is the one new assertion in this
        // round that used to skip that filter, and a concurrently-running sibling test's own
        // (differently-ported, but still `alias == OUR_ALIAS`) traffic satisfied it just as well
        // as this test's own — i.e. it passed under default `cargo test` parallelism even with
        // the R5 fallback deleted. Filtering on the exact port makes "our own datagram" mean
        // *this test's* datagram, not merely "something with our alias".
        const DISTINCTIVE_PORT: u16 = 12345;
        let info = our_info_with_port("fp", true, DISTINCTIVE_PORT);

        let collect_fut = collect_on_loopback(Duration::from_millis(500));
        let announce_fut = async {
            // Give the peer a moment to finish binding/joining before we send.
            tokio::time::sleep(Duration::from_millis(50)).await;
            announce_on_interfaces(&socket, &[], &info);
        };
        let (_, received) = tokio::join!(announce_fut, collect_fut);

        let ours = received
            .iter()
            .any(|(json, _)| json["alias"] == OUR_ALIAS && json["port"] == DISTINCTIVE_PORT);
        assert!(
            ours,
            "announce_on_interfaces sent nothing when `joined` was empty — a fully blocked \
             multicast join must not mean total silence (received {} datagrams total, none of \
             them ours: {received:?})",
            received.len()
        );
    }

    #[test]
    fn join_multicast_falls_back_to_unspecified_when_no_interfaces_given() {
        let socket = open_reusable_udp_socket(0).unwrap();
        let joined = join_multicast_on_all_interfaces(&socket, &[]);
        // `join_multicast_v4(group, UNSPECIFIED)` asks the kernel to pick an interface from its
        // route table; a host with no default route (e.g. a network namespace carrying only
        // `lo` up and no route table entries — the closest available model of a minimal CI
        // container) has no interface for the kernel to pick, so the join fails with `ENODEV`
        // and `joined_multicast_on_all_interfaces`'s own error-logging fallback (not a defect —
        // see its doc comment) leaves `joined` empty. That's a fact about this host's routing,
        // not about the function under test, so skip loudly rather than fail.
        if joined.is_empty() {
            eprintln!(
                "SKIPPED: join_multicast_v4(group, UNSPECIFIED) failed on this host — no route \
                 for the kernel to pick an interface from (e.g. no default route), so the \
                 empty-interfaces fallback this test exercises can't reach its happy path here"
            );
            return;
        }
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
        // Loopback is a real, almost-always-present, multicast-capable interface, so this join
        // should normally succeed — unlike the bogus-address case above. Not guaranteed,
        // though: `lo` administratively down (a bare `unshare -n` with no interface brought up
        // at all — the closest available model of a minimal CI container) makes even this join
        // fail. That's a fact about this host, not about the function under test.
        let joined = join_multicast_on_all_interfaces(&socket, &[Ipv4Addr::LOCALHOST]);
        if joined.is_empty() {
            eprintln!(
                "SKIPPED: joining the multicast group on loopback failed on this host (lo down \
                 or otherwise unusable) — this test's happy path needs a usable loopback \
                 interface"
            );
            return;
        }
        assert_eq!(joined, vec![Ipv4Addr::LOCALHOST]);
    }

    #[tokio::test]
    async fn announce_on_interfaces_handles_unspecified_in_joined_without_panicking() {
        // `[Ipv4Addr::UNSPECIFIED]` is a *non-empty* `joined` list whose one entry happens to be
        // the wildcard address (the shape `join_multicast_on_all_interfaces` produces on its own
        // empty-interfaces fallback) — distinct from the truly-empty-`joined` case covered by
        // `announce_on_interfaces_still_announces_when_nothing_joined` above. This is a smoke
        // test that the in-loop UNSPECIFIED case (skip `set_multicast_if_v4`, just send) doesn't
        // panic; it doesn't assert delivery, since a wildcard-selected send's destination
        // interface is host-route-dependent (see the R5 test's comment).
        let socket = open_reusable_udp_socket(0).unwrap();
        let socket = UdpSocket::from_std(socket.into()).unwrap();
        let info = our_info_with_port("fp", true, 12345);
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
