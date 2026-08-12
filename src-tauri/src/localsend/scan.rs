//! Subnet scan: find LocalSend peers that never answer multicast (issue #58).
//!
//! Multicast discovery is two-sided — we announce, the peer replies. A peer that does not
//! participate in multicast at all is invisible to it no matter how long we listen, even when
//! it is fully reachable at a known address on the same subnet. Measured on 2026-08-12: an
//! iPhone running LocalSend v2.2 answered `ping`, accepted TCP on `:53317` in 6–49 ms, and
//! returned HTTP 200 from `GET /api/localsend/v2/info` — while [`super::discover`] against the
//! live LAN returned only the co-resident desktop app. The reference LocalSend implementation
//! carries a legacy subnet scan for exactly this case.
//!
//! # Why this always runs, rather than only when multicast finds nothing
//!
//! "Scan only as a fallback" is the cheaper-sounding option and it does not work. On the
//! machine this was reported from, multicast *always* returns at least one device — the
//! LocalSend desktop app installed on that same host answers our announcement — so a
//! `found_nothing` trigger would never fire, and the phone that motivated this issue would
//! stay undiscovered. Making the fallback conditional on an empty result set makes it
//! conditional on a state the reporting machine never reaches. So the sweep runs on every
//! pass, concurrently with the announcement burst.
//!
//! That costs nothing in wall clock. [`super::discover`]'s worst case is already
//! `(ANNOUNCE_BURST_COUNT - 1) * ANNOUNCE_BURST_INTERVAL + timeout_ms` = 5 s at the default,
//! and a full /24 sweep is ⌈253 ÷ [`CONCURRENCY`]⌉ = 3 waves of [`CONNECT_TIMEOUT`] ≈ 1.2 s,
//! plus [`INFO_TIMEOUT`] against the handful of hosts that answered. It finishes inside the
//! existing window, so the documented worst case does not grow.
//!
//! To be precise about the bound rather than assume the happy path: a permit is held across
//! *both* stages, so a subnet where every address accepts a connection and then stalls on
//! `/info` would want 3 × (400 ms + 2 × 2 s) ≈ 13 s. That case does not extend discovery
//! either, because this runs inside [`super::discover_on`]'s `timeout` — the sweep is simply
//! cut off mid-flight, having reported whatever it found. The deadline, not this module, is
//! what bounds the call.
//!
//! # Two stages, because a blind /info sweep is not affordable
//!
//! Probing `/info` directly would mean up to two HTTPS attempts against 254 mostly-absent
//! hosts, each waiting out a full timeout. Instead:
//!
//! 1. **TCP connect** to `ip:port` with a short timeout. Absent hosts either refuse
//!    immediately or time out at [`CONNECT_TIMEOUT`]; this is the only stage that touches
//!    every address.
//! 2. **`GET /api/localsend/v2/info`** on the handful that accepted, https first, then http —
//!    the same order and reasoning as [`super::probe_protocol`].
//!
//! # Privacy posture
//!
//! This is a connect-only sweep of the interface's own subnet, sending nothing but a TCP SYN
//! until a host proves it speaks LocalSend. It carries no photo and no catalog data, and it
//! never leaves the local link — [`super::local_ipv4_networks`] excludes loopback and
//! point-to-point (VPN/tunnel) interfaces, and [`subnet_hosts`] refuses anything wider than a
//! /24, so a host on a large corporate or VPN-routed network is not swept. It is still a
//! different kind of network behaviour from a multicast announcement; `docs/localsend.md`
//! says so in the user-facing terms.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Semaphore;

use super::{base_url, client_for, Device};

/// The well-known LocalSend port. A blind sweep has no announcement to read a port from, so it
/// probes the default; a peer on a non-default port is still reachable via manual IP.
pub(super) const SCAN_PORT: u16 = 53317;

/// How long a single TCP connect gets before the address is treated as absent. A LAN round
/// trip is single-digit milliseconds (6–49 ms measured against the phone in #58); this is
/// generous by two orders of magnitude while still keeping a full sweep inside the existing
/// discovery window.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(400);

/// How long each `/info` request gets. Longer than [`CONNECT_TIMEOUT`] because this one runs
/// only against hosts that already accepted a connection, and a TLS handshake plus a JSON
/// response is real work rather than a liveness check.
const INFO_TIMEOUT: Duration = Duration::from_millis(2000);

/// Concurrent connects in flight. Bounds both the wall clock (a /24 is three waves) and the
/// burst of sockets: 254 simultaneous connects is the shape that makes a scan look hostile to
/// a network, and gains nothing over pipelining them.
const CONCURRENCY: usize = 96;

/// Narrowest prefix worth sweeping, as a host count. A /24 is 254 usable addresses; a /16 is
/// 65 534, which is neither affordable nor a plausible "my phone is on this LAN" scope.
const MIN_PREFIX: u32 = 24;

/// The addresses to sweep for an interface at `addr` with `netmask`, or `None` when the
/// network is not one worth scanning.
///
/// Excludes the network and broadcast addresses (neither is a host) and `addr` itself (that is
/// us). Returns `None` for:
///
/// - a **non-contiguous** mask — not a real CIDR prefix, so its host range is meaningless;
/// - anything **wider than [`MIN_PREFIX`]** — see that constant;
/// - **/31 and /32**, which have no usable host range between network and broadcast.
///
/// Pure, so the range arithmetic is testable without touching this host's real interfaces —
/// the same reason [`super::interface_flags_qualify`] is split out from the `getifaddrs` walk.
pub(super) fn subnet_hosts(addr: Ipv4Addr, netmask: Ipv4Addr) -> Option<Vec<Ipv4Addr>> {
    let mask = u32::from(netmask);
    let prefix = mask.leading_ones();
    // A CIDR mask is a run of ones followed by a run of zeros. Anything else (0.255.0.255 and
    // friends) is not a prefix, and `network`/`broadcast` below would not describe its range.
    if mask.count_ones() != prefix {
        return None;
    }
    if prefix < MIN_PREFIX || prefix > 30 {
        return None;
    }
    let bits = u32::from(addr);
    let network = bits & mask;
    let broadcast = network | !mask;
    Some(
        (network + 1..broadcast)
            .filter(|host| *host != bits)
            .map(Ipv4Addr::from)
            .collect(),
    )
}

/// Parse a `GET /api/localsend/v2/info` response body into a [`Device`].
///
/// Deliberately **not** [`super::parse_announcement`]: that requires a `port` field, because a
/// UDP announcement carries no address of its own and must name the port to come back on. An
/// `/info` response is served over a connection we opened, so the LocalSend v2 `InfoDto` does
/// not repeat the port — and passing an announcement parser a body without one yields `None`
/// for every scanned host, which is a silent total failure rather than a visible one. `ip`,
/// `port` and `protocol` therefore come from the connection that produced `json`.
///
/// Returns `None` for our own fingerprint — the same self-echo guard the UDP and register
/// paths apply, and not merely theoretical here: a sweep of our own subnet reaches this
/// machine's other addresses.
pub(super) fn parse_info_response(
    json: &str,
    ip: &str,
    port: u16,
    protocol: &str,
    our_fingerprint: &str,
) -> Option<Device> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let fingerprint = v.get("fingerprint")?.as_str()?.to_string();
    if fingerprint == our_fingerprint {
        return None;
    }
    let alias = v.get("alias")?.as_str()?.to_string();
    Some(Device {
        alias,
        device_model: v.get("deviceModel").and_then(|s| s.as_str()).map(String::from),
        device_type: v.get("deviceType").and_then(|s| s.as_str()).map(String::from),
        ip: ip.to_string(),
        port,
        protocol: protocol.to_string(),
        fingerprint,
    })
}

/// Ask one host that already accepted a connection whether it speaks LocalSend, https first
/// (see [`super::probe_protocol`] for why that order). `None` when neither scheme yields a
/// parseable info body — an open port that is not LocalSend, which is the common case for any
/// host running an unrelated service.
async fn identify(ip: Ipv4Addr, port: u16, our_fingerprint: &str) -> Option<Device> {
    let ip = ip.to_string();
    for scheme in ["https", "http"] {
        let probe = Device {
            alias: String::new(),
            device_model: None,
            device_type: None,
            ip: ip.clone(),
            port,
            protocol: scheme.to_string(),
            fingerprint: String::new(),
        };
        let Ok(client) = client_for(&probe) else { continue };
        let url = format!("{}/info", base_url(&probe));
        let Ok(Ok(resp)) = tokio::time::timeout(INFO_TIMEOUT, client.get(&url).send()).await else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        let Ok(Ok(body)) = tokio::time::timeout(INFO_TIMEOUT, resp.text()).await else {
            continue;
        };
        if let Some(device) = parse_info_response(&body, &ip, port, scheme, our_fingerprint) {
            return Some(device);
        }
    }
    None
}

/// Sweep `targets` for LocalSend peers, sending each one found to `tx`.
///
/// Each target carries its own port rather than the sweep reaching for [`SCAN_PORT`] itself,
/// for the same reason [`super::discover_on`] takes its interface list as a parameter: the
/// whole path — including `discover_on`'s wiring of it — is then exercisable against a
/// loopback stub on an ephemeral port, without depending on real network topology or
/// contending with whatever else holds the well-known port on the test host.
///
/// Returns when every target has been probed. Failures are per-target and silent by design: on
/// a /24 the overwhelmingly common outcome is "nothing there", and logging 250 refusals per
/// Refresh would bury the join/announce diagnostics that [`super::discover`] deliberately
/// keeps loud.
pub(super) async fn run(
    targets: &[SocketAddrV4],
    our_fingerprint: &str,
    tx: UnboundedSender<Device>,
) {
    if targets.is_empty() {
        return;
    }
    let permits = Arc::new(Semaphore::new(CONCURRENCY));
    let fingerprint = Arc::new(our_fingerprint.to_string());
    let mut tasks = tokio::task::JoinSet::new();

    for target in targets {
        let target = *target;
        let permits = Arc::clone(&permits);
        let fingerprint = Arc::clone(&fingerprint);
        let tx = tx.clone();
        tasks.spawn(async move {
            // Held for the whole probe, so `CONCURRENCY` bounds sockets in flight rather than
            // just connects started.
            let Ok(_permit) = permits.acquire().await else {
                return;
            };
            let connect = tokio::net::TcpStream::connect(target);
            let Ok(Ok(stream)) = tokio::time::timeout(CONNECT_TIMEOUT, connect).await else {
                return;
            };
            // Stage 2 opens its own connection through `reqwest` (it needs TLS and HTTP);
            // this one has done its job as a liveness check.
            drop(stream);
            if let Some(device) = identify(*target.ip(), target.port(), &fingerprint).await {
                // The collector may already be gone if the outer deadline fired; that is a
                // normal end to a pass, not an error.
                let _ = tx.send(device);
            }
        });
    }

    // Drop our own handle so the only senders left are the tasks'; otherwise the collector
    // would stay open on a sender that will never produce.
    drop(tx);
    while tasks.join_next().await.is_some() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    #[test]
    fn subnet_hosts_covers_a_24_without_network_broadcast_or_ourselves() {
        let hosts = subnet_hosts(mask("192.168.1.90"), mask("255.255.255.0")).unwrap();

        // 254 usable, minus ourselves.
        assert_eq!(hosts.len(), 253);
        assert!(hosts.contains(&mask("192.168.1.1")));
        assert!(hosts.contains(&mask("192.168.1.128")), "the #58 phone must be in range");
        assert!(hosts.contains(&mask("192.168.1.254")));
        assert!(!hosts.contains(&mask("192.168.1.0")), "network address is not a host");
        assert!(!hosts.contains(&mask("192.168.1.255")), "broadcast is not a host");
        assert!(!hosts.contains(&mask("192.168.1.90")), "must not probe ourselves");
    }

    #[test]
    fn subnet_hosts_refuses_networks_wider_than_a_24() {
        // A /16 is 65 534 addresses: not affordable, and not a plausible "my phone is here".
        assert!(subnet_hosts(mask("10.0.0.5"), mask("255.255.0.0")).is_none());
        assert!(subnet_hosts(mask("10.0.0.5"), mask("255.0.0.0")).is_none());
        assert!(subnet_hosts(mask("10.0.0.5"), mask("0.0.0.0")).is_none());
        // A /23 is only 510 addresses but still wider than the documented scope.
        assert!(subnet_hosts(mask("10.0.0.5"), mask("255.255.254.0")).is_none());
    }

    #[test]
    fn subnet_hosts_accepts_prefixes_narrower_than_a_24() {
        // A /25 splits a /24 in half; both halves must scan, and neither may include the
        // other's addresses.
        let low = subnet_hosts(mask("192.168.1.10"), mask("255.255.255.128")).unwrap();
        assert_eq!(low.len(), 125, "126 usable minus ourselves");
        assert!(low.contains(&mask("192.168.1.126")));
        assert!(!low.contains(&mask("192.168.1.127")), "broadcast of the low half");
        assert!(!low.contains(&mask("192.168.1.129")), "belongs to the high half");

        let high = subnet_hosts(mask("192.168.1.200"), mask("255.255.255.128")).unwrap();
        assert!(high.contains(&mask("192.168.1.129")));
        assert!(!high.contains(&mask("192.168.1.128")), "network address of the high half");
    }

    #[test]
    fn subnet_hosts_refuses_masks_with_no_usable_host_range() {
        // /31 (point-to-point) and /32 (single host): network + 1 >= broadcast, so the range
        // is empty or inverted. Rejected explicitly rather than relying on the range being
        // empty, so the reason is legible.
        assert!(subnet_hosts(mask("192.168.1.10"), mask("255.255.255.254")).is_none());
        assert!(subnet_hosts(mask("192.168.1.10"), mask("255.255.255.255")).is_none());
    }

    #[test]
    fn subnet_hosts_refuses_a_non_contiguous_mask() {
        // Not a CIDR prefix, so `network`/`broadcast` would not describe its range at all.
        assert!(subnet_hosts(mask("192.168.1.10"), mask("255.0.255.0")).is_none());
        assert!(subnet_hosts(mask("192.168.1.10"), mask("255.255.255.5")).is_none());
    }

    #[test]
    fn parse_info_response_takes_port_and_scheme_from_the_connection() {
        // The LocalSend v2 `InfoDto` as a real peer serves it: no `port`, because the caller
        // already knows which one it connected on. This is the case `parse_announcement`
        // rejects, and the reason this parser exists.
        let body = r#"{
            "alias": "Kind Carrot",
            "version": "2.0",
            "deviceModel": "iPhone",
            "deviceType": "mobile",
            "fingerprint": "phone-fp",
            "download": false
        }"#;

        let device = parse_info_response(body, "192.168.1.128", 53317, "https", "our-fp")
            .expect("a valid info body must yield a device");

        assert_eq!(device.alias, "Kind Carrot");
        assert_eq!(device.fingerprint, "phone-fp");
        assert_eq!(device.device_model.as_deref(), Some("iPhone"));
        assert_eq!(device.device_type.as_deref(), Some("mobile"));
        assert_eq!(device.ip, "192.168.1.128");
        assert_eq!(device.port, 53317, "port comes from the connection, not the body");
        assert_eq!(device.protocol, "https", "scheme comes from the connection too");

        // The guard that makes this concrete: the same body through the announcement parser
        // yields nothing, so reusing it would have made every scanned host invisible.
        assert!(
            super::super::parse_announcement(body, "192.168.1.128", "our-fp").is_none(),
            "parse_announcement requires `port`; if it ever stops doing so, this module's \
             separate parser needs revisiting rather than silently diverging"
        );
    }

    #[test]
    fn parse_info_response_ignores_our_own_fingerprint() {
        // A sweep of our own subnet reaches this machine's other addresses, so the self-echo
        // guard is load-bearing here, not theoretical.
        let body = r#"{"alias":"ChairPhoto","fingerprint":"our-fp","version":"2.0"}"#;
        assert!(parse_info_response(body, "192.168.1.90", 53317, "http", "our-fp").is_none());
    }

    #[test]
    fn parse_info_response_rejects_bodies_that_are_not_localsend() {
        // An open :53317 that belongs to something else must not become a device.
        assert!(parse_info_response("not json", "10.0.0.1", 53317, "http", "our-fp").is_none());
        assert!(parse_info_response("{}", "10.0.0.1", 53317, "http", "our-fp").is_none());
        // Alias present, fingerprint missing — half a peer is not a peer.
        assert!(
            parse_info_response(r#"{"alias":"X"}"#, "10.0.0.1", 53317, "http", "our-fp").is_none()
        );
        // Fingerprint present, alias missing.
        assert!(
            parse_info_response(r#"{"fingerprint":"x"}"#, "10.0.0.1", 53317, "http", "our-fp")
                .is_none()
        );
    }

    #[tokio::test]
    async fn run_reports_a_peer_that_answers_info_and_ignores_a_dead_address() {
        // A stub peer on loopback, on an ephemeral port so this never contends with a
        // co-resident LocalSend app holding the real one.
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind a loopback stub");
        let port = listener.local_addr().unwrap().port();

        let body = r#"{"alias":"Stub Peer","fingerprint":"stub-fp","version":"2.0"}"#;
        let server = tokio::spawn(async move {
            // Loops rather than accepting once: `identify` tries https first, and that
            // handshake fails against this plain-HTTP stub, so the http attempt that actually
            // succeeds is the *second* connection.
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let body = body.to_string();
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        // 127.0.0.2 is loopback-range but has nothing bound: it exercises the connect-refused
        // path in the same pass, so "found the peer" is not confused with "reported
        // everything".
        run(
            &[
                SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
                SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 2), port),
            ],
            "our-fp",
            tx,
        )
        .await;
        server.abort();

        let found: Vec<Device> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(found.len(), 1, "exactly the stub, not the dead address: {found:?}");
        assert_eq!(found[0].alias, "Stub Peer");
        assert_eq!(found[0].fingerprint, "stub-fp");
        assert_eq!(found[0].port, port, "the port we probed, not the well-known one");
        assert_eq!(found[0].protocol, "http");
    }

    #[tokio::test]
    async fn run_on_no_hosts_closes_the_channel_instead_of_holding_it_open() {
        // `discover_on`'s collector ends when every sender drops. If the empty-host early
        // return kept `tx` alive, a pass with nothing to scan would hold the collector open
        // until the outer deadline instead of finishing with the other sources.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Device>();
        run(&[], "our-fp", tx).await;
        assert!(
            matches!(rx.recv().await, None),
            "the channel must be closed, not merely empty"
        );
    }
}
