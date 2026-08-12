//! Inbound `POST /api/localsend/v2/register` receiver (issue #55).
//!
//! # Why this exists
//!
//! LocalSend discovery is two-sided. We announce over UDP multicast; a peer that hears us
//! replies *over HTTP* to the `port` our announcement advertised, and only falls back to a
//! UDP `announce:false` datagram when that HTTP register fails. Before this module,
//! ChairPhoto advertised a port it ran no TCP listener on, and relied entirely on that
//! fallback.
//!
//! That works right up until another LocalSend-speaking process on this machine holds the
//! advertised port. Then the peer's register POST *succeeds* — against the wrong process —
//! so the peer never sends the UDP fallback and ChairPhoto never learns it exists. Observed
//! live on 2026-08-12 with the LocalSend desktop app running (issue #55).
//!
//! # Why an ephemeral port
//!
//! The obvious fix — bind the well-known 53317 — cannot work, because in the failure case
//! another process already holds it. It also isn't necessary: the announcement carries its
//! own `port` field and peers POST to whatever it names. So we bind an ephemeral port and
//! advertise *that*. The collision disappears; both processes coexist. See
//! `docs/adr/0001-localsend-register-listener.md`.
//!
//! # Deliberately not a receiver
//!
//! LocalSend's protocol is symmetric — the same HTTP surface that accepts `register` also
//! accepts `prepare-upload` and `upload`. Implementing those by accident would turn a
//! discovery fix into "any device on the LAN can push files into the library". This server
//! answers `register` and refuses every other route, with the upload routes refused
//! explicitly rather than by falling through to a 404, so the refusal is legible as a
//! decision rather than an omission. Nothing here is ever auto-accepted.
//!
//! # Lifetime
//!
//! Bound only for the duration of one discovery pass. `serve` never returns on its own; the
//! caller drops it (and with it the listener and every in-flight handler) when the discovery
//! deadline fires. See `discover_on`.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinSet;

use super::{our_info_with_port, parse_announcement, Device};

/// Caps on what one unauthenticated LAN peer can make us buffer. A register body is a small
/// JSON info object; these are three orders of magnitude of headroom, not a tight fit.
const MAX_HEADER_BYTES: usize = 8 * 1024;
const MAX_BODY_BYTES: usize = 64 * 1024;
/// How long one connection may take to deliver a complete request before we drop it. Bounds
/// what a peer that opens a socket and then stalls can hold.
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(3);
/// Ceiling on concurrently-handled connections, so a burst can't spawn unboundedly.
const MAX_CONCURRENT_CONNECTIONS: usize = 16;

const REGISTER_PATH: &str = "/api/localsend/v2/register";
/// Refused explicitly (see the module doc) rather than left to the 404 fallthrough.
const UPLOAD_PATHS: &[&str] = &[
    "/api/localsend/v2/prepare-upload",
    "/api/localsend/v2/upload",
    "/api/localsend/v1/send-request",
    "/api/localsend/v1/send",
];

/// Bind an ephemeral TCP port on all interfaces for one discovery pass.
///
/// `0.0.0.0:0` on purpose: LAN-reachable (a peer on another host must be able to POST here,
/// so loopback would defeat the point) and OS-assigned (the whole reason this fix works
/// alongside a co-resident LocalSend app). Returns the listener and the port it actually
/// got, which is what the caller must advertise.
pub(super) async fn bind() -> std::io::Result<(TcpListener, u16)> {
    let listener = TcpListener::bind(("0.0.0.0", 0)).await?;
    let port = listener.local_addr()?.port();
    Ok((listener, port))
}

/// Accept and answer register POSTs until dropped, forwarding each discovered peer to `tx`.
///
/// Never returns: the caller bounds it. Accept errors are logged and the loop continues —
/// one refused connection must not end discovery's ability to hear the next peer.
pub(super) async fn serve(
    listener: TcpListener,
    our_fingerprint: String,
    advertised_port: u16,
    tx: UnboundedSender<Device>,
) {
    let mut handlers = JoinSet::new();
    loop {
        // Reap finished handlers before accepting more, so `handlers` tracks live work
        // rather than growing for the life of the pass.
        while handlers.len() >= MAX_CONCURRENT_CONNECTIONS {
            if handlers.join_next().await.is_none() {
                break;
            }
        }
        match listener.accept().await {
            Ok((stream, peer)) => {
                let fp = our_fingerprint.clone();
                let tx = tx.clone();
                handlers.spawn(async move {
                    // A stalled or malformed connection is bounded here, not by the caller's
                    // discovery deadline — otherwise one bad peer could hold a handler slot
                    // for the whole pass.
                    let _ = tokio::time::timeout(
                        CONNECTION_TIMEOUT,
                        handle_connection(stream, peer, &fp, advertised_port, &tx),
                    )
                    .await;
                });
            }
            Err(e) => {
                eprintln!("localsend: register listener accept failed: {e}");
            }
        }
    }
}

/// Read one request, answer it, and — if it was a valid register — forward the peer.
async fn handle_connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    our_fingerprint: &str,
    advertised_port: u16,
    tx: &UnboundedSender<Device>,
) {
    let Some(request) = read_request(&mut stream).await else {
        // Unparseable or oversized. Say so and close; don't leave the peer waiting.
        let _ = write_response(&mut stream, 400, "Bad Request", "{}").await;
        return;
    };

    let response = route(&request, peer, our_fingerprint, advertised_port, tx);
    let _ = write_response(&mut stream, response.status, response.reason, &response.body).await;
}

struct Response {
    status: u16,
    reason: &'static str,
    body: String,
}

/// Decide what one parsed request gets, and forward the peer on a successful register.
///
/// Split out from the socket handling so the routing decisions — which paths are answered,
/// which are refused, what a register yields — are testable without a live connection.
fn route(
    request: &Request,
    peer: SocketAddr,
    our_fingerprint: &str,
    advertised_port: u16,
    tx: &UnboundedSender<Device>,
) -> Response {
    if UPLOAD_PATHS.contains(&request.path.as_str()) {
        // ChairPhoto is send-only. Announced as `download: false`, and enforced here so the
        // claim is more than documentation.
        return Response {
            status: 403,
            reason: "Forbidden",
            body: r#"{"message":"ChairPhoto is send-only and does not accept uploads."}"#
                .to_string(),
        };
    }

    if request.path != REGISTER_PATH {
        return Response { status: 404, reason: "Not Found", body: "{}".to_string() };
    }

    if request.method != "POST" {
        return Response {
            status: 405,
            reason: "Method Not Allowed",
            body: "{}".to_string(),
        };
    }

    // The register body is the peer's own info object — the same shape as a UDP
    // announcement, so the same parser applies. The IP comes from the connection, not the
    // body: a peer's own idea of its address is not authoritative, and announcements don't
    // carry one at all.
    if let Some(device) = parse_announcement(&request.body, &peer.ip().to_string(), our_fingerprint)
    {
        // A closed receiver means the discovery pass has ended and this response no longer
        // matters to anyone; still answer the peer politely rather than dropping mid-exchange.
        let _ = tx.send(device);
    }

    // The spec's response to a register is the receiver's own info object, with
    // `announce: false` so the peer does not treat it as a fresh announcement to answer.
    Response {
        status: 200,
        reason: "OK",
        body: our_info_with_port(our_fingerprint, false, advertised_port).to_string(),
    }
}

struct Request {
    method: String,
    path: String,
    body: String,
}

/// Read one HTTP/1.1 request off `stream`.
///
/// Deliberately minimal rather than a dependency: this speaks exactly one request shape (a
/// small JSON POST from a LocalSend peer) and refuses everything else, so a full HTTP server
/// crate would be a large amount of unused surface — and unused surface on a LAN-reachable
/// port is precisely what this module is trying not to have. Returns `None` on anything
/// malformed or oversized; the caller answers 400.
async fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];

    // Headers first, up to the blank line.
    let header_end = loop {
        if let Some(i) = find_subslice(&buf, b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEADER_BYTES {
            return None;
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None; // peer closed before finishing its headers
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head = std::str::from_utf8(&buf[..header_end]).ok()?;
    let mut lines = head.split("\r\n");

    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_string();
    let target = request_line.next()?;
    // Ignore any query string; none of our routes take one.
    let path = target.split('?').next().unwrap_or(target).to_string();

    let content_length = lines
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    if content_length > MAX_BODY_BYTES {
        return None;
    }

    // Then the body, some of which the header reads may already have pulled in.
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None; // peer closed mid-body
        }
        body.extend_from_slice(&chunk[..n]);
        if body.len() > MAX_BODY_BYTES {
            return None;
        }
    }
    body.truncate(content_length);

    Some(Request {
        method,
        path,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    body: &str,
) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    fn request(method: &str, path: &str, body: &str) -> Request {
        Request { method: method.to_string(), path: path.to_string(), body: body.to_string() }
    }

    fn peer() -> SocketAddr {
        "192.168.1.128:41234".parse().unwrap()
    }

    fn peer_info(fingerprint: &str) -> String {
        // Written out by hand rather than built from `our_info_with_port`, so this exercises
        // the shape a real peer sends instead of testing our serializer against itself.
        serde_json::json!({
            "alias": "Fake Peer Phone",
            "version": "2.0",
            "deviceModel": "Test",
            "deviceType": "mobile",
            "fingerprint": fingerprint,
            "port": 53317,
            "protocol": "http",
            "download": false,
        })
        .to_string()
    }

    #[test]
    fn register_post_yields_a_device_and_our_info() {
        let (tx, mut rx) = unbounded_channel();
        let response = route(
            &request("POST", REGISTER_PATH, &peer_info("peer-fp")),
            peer(),
            "our-fp",
            51586,
            &tx,
        );

        assert_eq!(response.status, 200);
        let device = rx.try_recv().expect("a register POST must yield a Device");
        assert_eq!(device.alias, "Fake Peer Phone");
        assert_eq!(device.fingerprint, "peer-fp");
        // The IP is the connection's, not anything the body claimed.
        assert_eq!(device.ip, "192.168.1.128");

        let body: serde_json::Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(body["port"], 51586, "we must answer with the port we actually bound");
        assert_eq!(body["announce"], false, "a register reply must not re-trigger the peer");
    }

    #[test]
    fn upload_routes_are_refused_explicitly() {
        // The acceptance criterion for #55: answering `register` must not have made
        // ChairPhoto a LocalSend *receiver*. Every upload entry point is refused, and none
        // of them may produce a Device or any other side effect.
        let (tx, mut rx) = unbounded_channel();
        for path in UPLOAD_PATHS {
            let response = route(&request("POST", path, "{}"), peer(), "our-fp", 51586, &tx);
            assert_eq!(response.status, 403, "{path} must be refused, got {}", response.status);
        }
        assert!(rx.try_recv().is_err(), "a refused upload must not register anything");
    }

    #[test]
    fn unknown_paths_are_404_and_wrong_method_is_405() {
        let (tx, _rx) = unbounded_channel();
        assert_eq!(route(&request("POST", "/", "{}"), peer(), "f", 1, &tx).status, 404);
        assert_eq!(
            route(&request("GET", REGISTER_PATH, ""), peer(), "f", 1, &tx).status,
            405
        );
    }

    #[test]
    fn our_own_register_echo_is_ignored() {
        // Same guard `parse_announcement` gives the UDP path: our own fingerprint coming back
        // at us is not a peer.
        let (tx, mut rx) = unbounded_channel();
        let response = route(
            &request("POST", REGISTER_PATH, &peer_info("our-fp")),
            peer(),
            "our-fp",
            51586,
            &tx,
        );
        assert_eq!(response.status, 200, "still a well-formed exchange");
        assert!(rx.try_recv().is_err(), "our own fingerprint must not become a Device");
    }

    #[test]
    fn malformed_register_body_still_answers_without_registering() {
        let (tx, mut rx) = unbounded_channel();
        let response = route(
            &request("POST", REGISTER_PATH, "not json at all"),
            peer(),
            "our-fp",
            51586,
            &tx,
        );
        assert_eq!(response.status, 200);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn bind_gets_an_ephemeral_port_and_releases_it_on_drop() {
        // Both halves of the lifetime criterion: the port is real (so the advertisement is
        // honest) and it is gone once the pass ends (so nothing stays LAN-reachable).
        let (listener, port) = bind().await.expect("must bind an ephemeral port");
        assert_ne!(port, 0, "the OS must have assigned a real port");
        assert_ne!(port, super::super::OUR_PORT, "must not be the contended well-known port");
        assert!(
            TcpListener::bind(("0.0.0.0", port)).await.is_err(),
            "the port must be held while the listener lives"
        );

        drop(listener);
        TcpListener::bind(("0.0.0.0", port))
            .await
            .expect("the port must be free once the listener is dropped");
    }

    #[tokio::test]
    async fn a_real_register_post_round_trips_over_tcp() {
        // End-to-end over a real socket: the HTTP parsing, routing, and response writing
        // together, against a client that speaks the wire format rather than calling `route`.
        let (listener, port) = bind().await.expect("must bind");
        let (tx, mut rx) = unbounded_channel();
        let server = tokio::spawn(serve(listener, "our-fp".to_string(), port, tx));

        let body = peer_info("wire-peer-fp");
        let mut client = TcpStream::connect(("127.0.0.1", port)).await.expect("must connect");
        let request = format!(
            "POST {REGISTER_PATH} HTTP/1.1\r\nHost: localhost\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        client.write_all(request.as_bytes()).await.expect("must send");

        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), client.read_to_end(&mut response))
            .await
            .expect("the server must answer within the timeout")
            .expect("must read a response");
        let response = String::from_utf8_lossy(&response);

        assert!(response.starts_with("HTTP/1.1 200 OK"), "got: {response}");
        let device = rx.try_recv().expect("the POST must have produced a Device");
        assert_eq!(device.fingerprint, "wire-peer-fp");
        assert_eq!(device.ip, "127.0.0.1");

        server.abort();
    }

    #[tokio::test]
    async fn an_oversized_body_is_rejected_not_buffered() {
        // A hostile or broken peer must not be able to make us allocate without bound.
        let (listener, port) = bind().await.expect("must bind");
        let (tx, _rx) = unbounded_channel();
        let server = tokio::spawn(serve(listener, "our-fp".to_string(), port, tx));

        let mut client = TcpStream::connect(("127.0.0.1", port)).await.expect("must connect");
        let request = format!(
            "POST {REGISTER_PATH} HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        client.write_all(request.as_bytes()).await.expect("must send");

        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), client.read_to_end(&mut response))
            .await
            .expect("the server must answer within the timeout")
            .expect("must read a response");
        assert!(
            String::from_utf8_lossy(&response).starts_with("HTTP/1.1 400"),
            "an over-long Content-Length must be refused before the body is read"
        );

        server.abort();
    }
}
