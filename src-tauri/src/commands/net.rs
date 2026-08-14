//! Host-mediated network access for modules (#49) — the Rust half of `api.fetch`.
//!
//! `docs/module-capabilities.md` § "The policy" closed the direct route: `connect-src` pins
//! the webview to the app's own origin and the IPC bridge, so module code cannot open a
//! socket to an arbitrary host. This command is the mediated route back — one HTTP request,
//! made by Rust on the module's behalf, which is also why CORS never enters the picture: the
//! request no longer originates in the webview at all.
//!
//! ## The division of labour, stated plainly
//!
//! **Which module may talk to which origin is decided in `src/modules/host.ts`**, against the
//! origins that module declared and the user granted, keyed on the identity snapshot taken at
//! registration (#48). It has to live there: the frontend host is the only layer that knows
//! which module is calling. That is the same boundary `api.invoke` has, with the same
//! documented limit — a module shares the page with the app, so this is least privilege over
//! the host API, not a sandbox (`docs/plugin-system.md` § "Trust model").
//!
//! **What any request from this app may do at all is decided here**, and cannot be argued out
//! of by the caller:
//!
//! | Invariant | Why |
//! |---|---|
//! | `https:` only | A cleartext grant means the user consented to one host and got everyone on the path. |
//! | No redirects followed | A permitted host redirecting to a non-permitted one is the bypass. See [`ModuleFetchResponse`]. |
//! | Public unicast addresses only | A grant for `example.com` must not become a route to `127.0.0.1`, the LAN, or `169.254.169.254`. |
//! | Address pinning | The vetted addresses are the ones dialled, so a second DNS answer cannot rebind under us. |
//! | No proxies | Through a proxy the host we vetted is not the host contacted, and the vetting is theatre. |
//! | No cookie jar | Ambient authority is what turns a fetch proxy into a confused deputy. `reqwest` is built without the `cookies` feature at all. |
//! | Size and time caps | A module must not be able to hold a connection open forever or exhaust memory. |
//!
//! The one place the two halves meet is `origin`: the frontend passes the origin it checked
//! alongside the URL, and this command re-derives the origin with its own parser and refuses
//! a mismatch. That is not circular — it catches a URL that the two parsers read differently,
//! which is otherwise a way to be granted one origin and reach another.

use super::*;
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

/// The largest request body a module may send in one call. Not a privacy boundary — a
/// module that has been granted an origin may send it whatever it likes (see the module
/// doc-comment on `ChairPhotoAPI.fetch`) — but an unbounded body is a way to wedge the
/// process, and a cap makes the IPC payload size a bounded quantity.
const MAX_REQUEST_BODY: usize = 8 * 1024 * 1024;

/// The largest response body this command will buffer. Exceeded is an error, never a silent
/// truncation: a module parsing half a JSON document would fail somewhere far from here.
const MAX_RESPONSE_BODY: usize = 8 * 1024 * 1024;

/// Whole-request deadline, including reading the response body.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Methods a module may use. An allowlist rather than a denylist: `CONNECT` would ask the
/// client to open a tunnel, `TRACE` reflects the request back, and an arbitrary token method
/// is a way to probe a server for behaviour no module needs.
const ALLOWED_METHODS: &[&str] = &["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"];

/// Request headers a module may not set.
///
/// The framing headers (`host`, `content-length`, `transfer-encoding`, `connection`, …) are
/// the client's to compute; letting a module dictate them is request smuggling. `expect` and
/// `te` change the wire protocol. `accept-encoding` is refused for a different reason: this
/// build of `reqwest` has no decompression features, so a module asking for `gzip` would get
/// bytes it cannot decode and a confusing UTF-8 error from [`ModuleFetchResponse::body`].
const FORBIDDEN_REQUEST_HEADERS: &[&str] = &[
    "accept-encoding",
    "connection",
    "content-length",
    "expect",
    "host",
    "keep-alive",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Response headers withheld from the module.
///
/// This command keeps no cookie jar, so a `Set-Cookie` cannot be honoured by the host — it
/// would only be a credential handed to module code, which lives unsandboxed in the app's
/// page. A module that needs authenticated calls sends its own `Authorization` header, whose
/// value it already had. Recorded as a deliberate refusal in `docs/module-capabilities.md`.
const WITHHELD_RESPONSE_HEADERS: &[&str] = &["set-cookie", "set-cookie2"];

/// One HTTP response, as much of it as a module gets to see.
///
/// **Redirects are not followed**, so `url` is always the URL that was requested. A `3xx`
/// arrives here as an ordinary result with its `location` header intact, and the module may
/// choose to call `api.fetch` again with that URL — which goes through the origin gate like
/// any other call. That is the whole reason for not following: a permitted host redirecting
/// to a non-permitted one is the obvious bypass, and following-with-a-recheck would also have
/// to decide what happens to the request body and `Authorization` header on a `307` to
/// another host. Making the module ask again keeps every hop inside its grant, visibly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModuleFetchResponse {
    pub status: u16,
    /// The canonical reason phrase for `status` (HTTP/2 carries none on the wire).
    pub status_text: String,
    /// `true` for 2xx, mirroring `Response.ok` so the shape is familiar.
    pub ok: bool,
    /// The URL that was requested. Equal to the input, because no redirect is followed.
    pub url: String,
    /// Response headers, lower-cased, with repeats joined by `", "` (RFC 9110 §5.3) — minus
    /// [`WITHHELD_RESPONSE_HEADERS`]. A `BTreeMap` so the order is stable for tests and logs.
    pub headers: BTreeMap<String, String>,
    /// The response body as text. Not valid UTF-8 is an error, not lossy replacement: this
    /// proxy is for text protocols (JSON, XML, form-encoded), and silently corrupting bytes
    /// would be worse than refusing them.
    pub body: String,
}

/// Is `ip` a public unicast address a module may be routed to?
///
/// A denylist of everything that is *not* somewhere else on the internet: loopback, the
/// private ranges, carrier NAT, link-local (which is where cloud metadata services live, at
/// `169.254.169.254`), documentation and benchmarking ranges, multicast, and the reserved
/// space. IPv6 forms that embed an IPv4 address — v4-mapped, 6to4, NAT64 — are unwrapped and
/// judged on the address inside, because that is what the packet ends up reaching.
fn is_public_unicast(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()             // 127/8
        || ip.is_private()              // 10/8, 172.16/12, 192.168/16
        || ip.is_link_local()           // 169.254/16 — incl. the cloud metadata address
        || ip.is_multicast()            // 224/4
        || ip.is_broadcast()            // 255.255.255.255
        || ip.is_documentation()        // 192.0.2/24, 198.51.100/24, 203.0.113/24
        || a == 0                       // 0/8 "this network"
        || (a == 100 && (64..128).contains(&b))   // 100.64/10 carrier-grade NAT
        || (a == 192 && b == 0 && ip.octets()[2] == 0) // 192.0.0/24 IETF protocol assignments
        || (a == 198 && (b == 18 || b == 19))     // 198.18/15 benchmarking
        || a >= 240)                    // 240/4 reserved
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    // An address that carries an IPv4 address inside it reaches that IPv4 address. Judge the
    // payload, not the wrapper, or `::ffff:127.0.0.1` walks straight through.
    if let Some(v4) = embedded_v4(ip) {
        return is_public_v4(v4);
    }
    let seg = ip.segments();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()                       // ff00::/8
        || (seg[0] & 0xfe00) == 0xfc00             // fc00::/7 unique local
        || (seg[0] & 0xffc0) == 0xfe80             // fe80::/10 link-local unicast
        || (seg[0] == 0x2001 && seg[1] == 0x0db8)  // 2001:db8::/32 documentation
        || (seg[0] == 0x2001 && seg[1] == 0x0000)  // 2001::/32 Teredo (tunnels to a v4 peer)
        || (seg[0] == 0x0100 && seg[1] == 0 && seg[2] == 0 && seg[3] == 0)) // 100::/64 discard
}

/// The IPv4 address an IPv6 address forwards to, if it is one of the transition forms.
fn embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return Some(v4); // ::ffff:0:0/96
    }
    let seg = ip.segments();
    // 2002::/16 (6to4): the next 32 bits are the v4 endpoint.
    if seg[0] == 0x2002 {
        return Some(Ipv4Addr::from(((seg[1] as u32) << 16) | seg[2] as u32));
    }
    // 64:ff9b::/96 and 64:ff9b:1::/48 (NAT64/DNS64): the low 32 bits are the v4 address. A
    // DNS64 resolver hands these back for ordinary hosts on an IPv6-only network, so they
    // cannot simply be refused — but the v4 inside still has to be public.
    if seg[0] == 0x0064 && seg[1] == 0xff9b && seg[2] == 0 && seg[3] == 0 && seg[4] == 0 && seg[5] == 0
    {
        return Some(Ipv4Addr::from(((seg[6] as u32) << 16) | seg[7] as u32));
    }
    // ::a.b.c.d — the deprecated IPv4-compatible form. `to_ipv4_mapped` does not cover it.
    if seg[..6].iter().all(|s| *s == 0) && !(seg[6] == 0 && seg[7] <= 1) {
        return Some(Ipv4Addr::from(((seg[6] as u32) << 16) | seg[7] as u32));
    }
    None
}

/// The origin of `url`, in the form a module declares in its manifest: scheme, host, and the
/// port only when it is not the scheme's default. `Url` has already lower-cased the scheme
/// and host and punycoded an IDN, so two spellings of one origin normalise together.
fn origin_of(url: &reqwest::Url) -> Result<String, String> {
    let host = url.host_str().ok_or("URL has no host")?;
    Ok(match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    })
}

/// Parse and vet the request URL against the origin the host says it approved.
///
/// `claimed_origin` comes from `src/modules/host.ts`, which checked it against the module's
/// granted set using the *browser's* URL parser. Re-deriving it here with Rust's parser and
/// insisting the two agree closes a parser differential — a URL that reads as
/// `https://granted.example` to one parser and something else to the other.
fn vet_url(raw: &str, claimed_origin: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(raw).map_err(|e| format!("invalid URL: {e}"))?;
    if url.scheme() != "https" {
        return Err(format!(
            "api.fetch is https-only; \"{}\" was requested over {}",
            raw,
            url.scheme()
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("api.fetch refuses URLs carrying credentials in the authority".into());
    }
    let origin = origin_of(&url)?;
    if origin != claimed_origin {
        return Err(format!(
            "URL origin {origin} does not match the approved origin {claimed_origin}"
        ));
    }
    Ok(url)
}

/// Resolve `host` and refuse unless every answer is a public unicast address.
///
/// Every answer, not merely the one that would be dialled: a name that resolves to both a
/// public address and `127.0.0.1` is either broken or attacking, and picking the good one
/// would make the outcome depend on resolver ordering. Returning the vetted list is what lets
/// the caller pin the connection to it.
async fn resolve_public_addrs(host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !is_public_unicast(ip) {
            return Err(refusal_for(host, ip));
        }
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let resolved: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("could not resolve {host}: {e}"))?
        .collect();
    if resolved.is_empty() {
        return Err(format!("{host} resolved to no addresses"));
    }
    for addr in &resolved {
        if !is_public_unicast(addr.ip()) {
            return Err(refusal_for(host, addr.ip()));
        }
    }
    Ok(resolved)
}

fn refusal_for(host: &str, ip: IpAddr) -> String {
    format!(
        "{host} resolves to {ip}, which is not a public address. api.fetch does not reach \
         loopback, private, carrier-NAT or link-local addresses, so a grant for a public host \
         cannot become a route to this machine, the LAN, or a cloud metadata service."
    )
}

/// Build the header map a module asked for, refusing anything it must not control.
fn build_headers(
    headers: &BTreeMap<String, String>,
) -> Result<reqwest::header::HeaderMap, String> {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        let lower = name.trim().to_ascii_lowercase();
        if FORBIDDEN_REQUEST_HEADERS.contains(&lower.as_str()) {
            return Err(format!("api.fetch does not let a module set the `{lower}` header"));
        }
        let header = HeaderName::from_bytes(lower.as_bytes())
            .map_err(|_| format!("`{name}` is not a valid header name"))?;
        // `from_str` rejects CR, LF and NUL, which is what would otherwise let a header value
        // inject a second header (or a whole second request) into the stream.
        let header_value = HeaderValue::from_str(value)
            .map_err(|_| format!("`{name}` has a value that is not a valid header value"))?;
        out.append(header, header_value);
    }
    Ok(out)
}

/// Perform one HTTP request on behalf of a module.
///
/// `origin` is the origin `src/modules/host.ts` matched against the calling module's granted
/// set; it is re-derived from `url` here and a mismatch is refused (see [`vet_url`]). Every
/// other guarantee in this file's header holds regardless of what the caller passes.
#[tauri::command]
pub async fn module_fetch(
    url: String,
    origin: String,
    method: Option<String>,
    headers: Option<BTreeMap<String, String>>,
    body: Option<String>,
) -> Result<ModuleFetchResponse, String> {
    let parsed = vet_url(&url, &origin)?;

    let method = method.unwrap_or_else(|| "GET".into()).to_ascii_uppercase();
    if !ALLOWED_METHODS.contains(&method.as_str()) {
        return Err(format!(
            "api.fetch does not support the {method} method (allowed: {})",
            ALLOWED_METHODS.join(", ")
        ));
    }
    let body = body.unwrap_or_default();
    if !body.is_empty() {
        if matches!(method.as_str(), "GET" | "HEAD") {
            return Err(format!("a {method} request cannot carry a body"));
        }
        if body.len() > MAX_REQUEST_BODY {
            return Err(format!(
                "request body is {} bytes; api.fetch caps it at {MAX_REQUEST_BODY}",
                body.len()
            ));
        }
    }
    let header_map = build_headers(&headers.unwrap_or_default())?;

    let host = parsed.host_str().ok_or("URL has no host")?.to_owned();
    let port = parsed.port_or_known_default().unwrap_or(443);
    let addrs = resolve_public_addrs(&host, port).await?;

    let mut builder = reqwest::Client::builder()
        // The bypass this closes: a permitted host answering 302 to a non-permitted one.
        .redirect(reqwest::redirect::Policy::none())
        // Through a proxy, the address vetting above describes a host we never contact.
        .no_proxy()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("ChairPhoto/", env!("CARGO_PKG_VERSION")));
    // Pin the connection to the addresses just vetted, so the resolver cannot answer
    // differently the second time (DNS rebinding). Certificate validation still uses the
    // hostname, so pinning weakens nothing about TLS. An IP literal needs no override.
    if host.parse::<IpAddr>().is_err() {
        builder = builder.resolve_to_addrs(&host, &addrs);
    }
    let client = builder.build().map_err(|e| format!("HTTP client: {e}"))?;

    let request_method = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|_| format!("invalid method {method}"))?;
    let mut request = client.request(request_method, parsed.clone()).headers(header_map);
    if !body.is_empty() {
        request = request.body(body);
    }

    let mut response = request.send().await.map_err(|e| format!("request failed: {e}"))?;

    let status = response.status();
    let mut out_headers: BTreeMap<String, String> = BTreeMap::new();
    for (name, value) in response.headers() {
        let lower = name.as_str().to_ascii_lowercase();
        if WITHHELD_RESPONSE_HEADERS.contains(&lower.as_str()) {
            continue;
        }
        // A header whose bytes are not visible ASCII cannot be represented as a JS string
        // faithfully; skip it rather than mangle it.
        let Ok(text) = value.to_str() else { continue };
        out_headers
            .entry(lower)
            .and_modify(|existing| {
                existing.push_str(", ");
                existing.push_str(text);
            })
            .or_insert_with(|| text.to_owned());
    }

    if let Some(len) = response.content_length() {
        if len > MAX_RESPONSE_BODY as u64 {
            return Err(format!(
                "response is {len} bytes; api.fetch caps it at {MAX_RESPONSE_BODY}"
            ));
        }
    }
    // Streamed rather than `bytes()`, so a server that lies about (or omits) Content-Length
    // still cannot make this buffer more than the cap.
    let mut buffer: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| format!("reading response: {e}"))? {
        if buffer.len() + chunk.len() > MAX_RESPONSE_BODY {
            return Err(format!(
                "response body exceeds the {MAX_RESPONSE_BODY}-byte cap api.fetch allows"
            ));
        }
        buffer.extend_from_slice(&chunk);
    }
    let text = String::from_utf8(buffer)
        .map_err(|_| "response body is not valid UTF-8; api.fetch returns text only".to_string())?;

    Ok(ModuleFetchResponse {
        status: status.as_u16(),
        status_text: status.canonical_reason().unwrap_or("").to_string(),
        ok: status.is_success(),
        url: parsed.to_string(),
        headers: out_headers,
        body: text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("test address parses")
    }

    #[test]
    fn loopback_private_and_link_local_are_not_public() {
        // The three the issue names explicitly, plus the ranges that reach the same places.
        for addr in [
            "127.0.0.1",
            "127.1.2.3",
            "0.0.0.0",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.1.1",
            "169.254.169.254", // the cloud metadata address
            "100.64.0.1",      // carrier-grade NAT
            "192.0.0.8",
            "198.18.0.1",
            "203.0.113.9",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
        ] {
            assert!(!is_public_unicast(ip(addr)), "{addr} must not be reachable");
        }
    }

    #[test]
    fn ipv6_local_forms_are_not_public() {
        for addr in [
            "::",
            "::1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
            "2001::1", // Teredo
            "100::1",  // discard prefix
        ] {
            assert!(!is_public_unicast(ip(addr)), "{addr} must not be reachable");
        }
    }

    #[test]
    fn ipv6_forms_that_wrap_a_private_ipv4_are_judged_on_the_ipv4() {
        // The whole point of unwrapping: each of these delivers packets to 127.0.0.1 or the
        // LAN while looking like an ordinary IPv6 address.
        for addr in [
            "::ffff:127.0.0.1", // v4-mapped
            "::ffff:10.0.0.1",
            "2002:7f00:1::1",    // 6to4 over 127.0.0.1
            "64:ff9b::7f00:1",   // NAT64 over 127.0.0.1
            "::127.0.0.1",       // deprecated v4-compatible
        ] {
            assert!(!is_public_unicast(ip(addr)), "{addr} must not be reachable");
        }
        // …and the same wrappers around a public address stay usable, or an IPv6-only network
        // behind DNS64 could not reach anything at all.
        assert!(is_public_unicast(ip("::ffff:93.184.216.34")));
        assert!(is_public_unicast(ip("64:ff9b::5db8:d822")));
    }

    #[test]
    fn ordinary_public_addresses_are_reachable() {
        for addr in ["93.184.216.34", "8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(is_public_unicast(ip(addr)), "{addr} must be reachable");
        }
    }

    #[test]
    fn origin_drops_the_default_port_and_keeps_a_custom_one() {
        let o = |s: &str| origin_of(&reqwest::Url::parse(s).unwrap()).unwrap();
        assert_eq!(o("https://api.flickr.com/services/rest?x=1"), "https://api.flickr.com");
        assert_eq!(o("https://api.flickr.com:443/"), "https://api.flickr.com");
        assert_eq!(o("https://api.flickr.com:8443/"), "https://api.flickr.com:8443");
        // Case and IDN normalise, so one origin has one spelling.
        assert_eq!(o("https://API.Flickr.COM/"), "https://api.flickr.com");
    }

    #[test]
    fn only_https_is_accepted() {
        for raw in [
            "http://api.flickr.com/",
            "ftp://api.flickr.com/",
            "file:///etc/passwd",
            "data:text/plain,hello",
        ] {
            let err = vet_url(raw, "https://api.flickr.com").unwrap_err();
            assert!(
                err.contains("https-only") || err.contains("invalid URL") || err.contains("no host"),
                "{raw} was refused with an unexpected message: {err}"
            );
        }
    }

    #[test]
    fn a_url_whose_origin_is_not_the_approved_one_is_refused() {
        // The parser-differential guard: the host says it approved Flickr, the URL is not
        // Flickr, and this side is the one that notices.
        let err = vet_url("https://evil.example/x", "https://api.flickr.com").unwrap_err();
        assert!(err.contains("does not match the approved origin"), "{err}");
        // A path on the approved origin is fine — the grant is per origin, not per path.
        assert!(vet_url("https://api.flickr.com/services/rest", "https://api.flickr.com").is_ok());
    }

    #[test]
    fn credentials_in_the_authority_are_refused() {
        // `https://api.flickr.com@evil.example/` reads as host `evil.example` to a correct
        // parser and as Flickr to a careless human. Refusing the form removes the ambiguity.
        let err = vet_url("https://user:pw@api.flickr.com/", "https://api.flickr.com").unwrap_err();
        assert!(err.contains("credentials"), "{err}");
    }

    #[test]
    fn framing_headers_are_refused_rather_than_dropped() {
        for name in ["Host", "content-length", "Transfer-Encoding", "Connection", "accept-encoding"]
        {
            let mut h = BTreeMap::new();
            h.insert(name.to_string(), "x".to_string());
            let err = build_headers(&h).unwrap_err();
            assert!(err.contains("does not let a module set"), "{name}: {err}");
        }
    }

    #[test]
    fn a_header_value_cannot_smuggle_a_second_header() {
        let mut h = BTreeMap::new();
        h.insert("x-thing".into(), "a\r\nX-Injected: b".into());
        assert!(build_headers(&h).is_err());
    }

    #[test]
    fn the_headers_a_module_legitimately_needs_survive() {
        let mut h = BTreeMap::new();
        h.insert("Authorization".into(), "OAuth oauth_consumer_key=\"k\"".into());
        h.insert("Content-Type".into(), "application/json".into());
        let built = build_headers(&h).expect("ordinary headers are allowed");
        assert_eq!(built.get("authorization").unwrap(), "OAuth oauth_consumer_key=\"k\"");
        assert_eq!(built.get("content-type").unwrap(), "application/json");
    }

    #[tokio::test]
    async fn a_hostname_pointing_at_loopback_is_refused_by_name() {
        // `localhost` is the shortest expression of "a name that resolves inside the
        // machine"; the refusal has to happen on the resolved address, not on the string.
        let err = resolve_public_addrs("localhost", 443).await.unwrap_err();
        assert!(err.contains("not a public address"), "{err}");
    }

    #[tokio::test]
    async fn an_ip_literal_is_vetted_without_a_lookup() {
        assert!(resolve_public_addrs("127.0.0.1", 443).await.is_err());
        assert!(resolve_public_addrs("169.254.169.254", 80).await.is_err());
        let ok = resolve_public_addrs("93.184.216.34", 443).await.unwrap();
        assert_eq!(ok, vec![SocketAddr::new(ip("93.184.216.34"), 443)]);
    }

    #[tokio::test]
    async fn a_refused_request_never_reaches_the_network() {
        // Every guard above runs before a client is even built. Asserting on the error text
        // for the loopback case is the closest a unit test gets to "no packet was sent";
        // the ordering is what the assertion is really about.
        let err = module_fetch(
            "https://localhost/x".into(),
            "https://localhost".into(),
            Some("GET".into()),
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.contains("not a public address"), "{err}");
    }

    #[tokio::test]
    async fn unsupported_methods_and_oversized_bodies_are_refused_up_front() {
        let call = |method: &str, body: Option<String>| {
            let method = method.to_string();
            async move {
                module_fetch(
                    "https://example.com/".into(),
                    "https://example.com".into(),
                    Some(method),
                    None,
                    body,
                )
                .await
            }
        };
        assert!(call("CONNECT", None).await.unwrap_err().contains("does not support"));
        assert!(call("TRACE", None).await.unwrap_err().contains("does not support"));
        assert!(call("GET", Some("x".into())).await.unwrap_err().contains("cannot carry a body"));
        let huge = "x".repeat(MAX_REQUEST_BODY + 1);
        assert!(call("POST", Some(huge)).await.unwrap_err().contains("caps it at"));
    }
}
