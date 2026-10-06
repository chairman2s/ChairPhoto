//! Minimal OAuth 1.0a (HMAC-SHA1) request signing, shared by the Flickr and SmugMug
//! modules (the `flickr`/`smugmug` Cargo features). Both services require OAuth 1.0a for
//! uploads; this module is the signing core and the one piece verifiable offline — the
//! `signature` test below checks it against the canonical Twitter OAuth example (the
//! reference vector used to validate HMAC-SHA1 OAuth implementations).
//!
//! Everything here is pure: no network, no global state. Callers assemble request params,
//! call [`signed_params`], then send the oauth_* protocol values in an `Authorization: OAuth`
//! header ([`auth_header`], RFC 5849 §3.5.1) and only the request params in the URL or body
//! ([`request_url`], [`request_params`]). Protocol params — the access token and signature
//! among them — never go in a URL (#190): a URL lands in proxy and request logs where a
//! header usually does not. The one exception is the authorize URL the user opens in a
//! browser, which carries the short-lived request token by protocol (RFC 5849 §2.2).

use base64::Engine;
use hmac::{Hmac, Mac};
use sha1::Sha1;
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// Percent-encode per RFC 3986: only unreserved characters pass through unescaped.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Compute the OAuth 1.0a HMAC-SHA1 signature (base64) for a request. `params` must be every
/// protocol (oauth_*) and request (query/body) parameter except `oauth_signature` and any
/// file part. Sorting/encoding follow RFC 5849 §3.4.1.
pub fn signature(
    method: &str,
    base_url: &str,
    params: &BTreeMap<String, String>,
    consumer_secret: &str,
    token_secret: &str,
) -> String {
    // Normalized parameter string: encode each key & value, sort by the encoded pair.
    let mut pairs: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| (percent_encode(k), percent_encode(v)))
        .collect();
    pairs.sort();
    let normalized = pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");

    let base = format!(
        "{}&{}&{}",
        method.to_uppercase(),
        percent_encode(base_url),
        percent_encode(&normalized)
    );
    let key = format!(
        "{}&{}",
        percent_encode(consumer_secret),
        percent_encode(token_secret)
    );
    let mut mac = Hmac::<Sha1>::new_from_slice(key.as_bytes()).expect("HMAC accepts any key length");
    mac.update(base.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

/// Build the complete signed parameter set for a request: the standard oauth_* protocol
/// params (consumer key, nonce, signature method, timestamp, version, and `oauth_token` when
/// present) merged with `extra` request params, plus the computed `oauth_signature`.
pub fn signed_params(
    method: &str,
    url: &str,
    consumer_key: &str,
    consumer_secret: &str,
    token: Option<&str>,
    token_secret: &str,
    extra: &[(&str, &str)],
) -> BTreeMap<String, String> {
    signed_params_with(
        method,
        url,
        consumer_key,
        consumer_secret,
        token,
        token_secret,
        extra,
        &nonce(),
        &timestamp(),
    )
}

/// [`signed_params`] with a fixed nonce and timestamp (tests pin them to a known vector).
#[allow(clippy::too_many_arguments)]
fn signed_params_with(
    method: &str,
    url: &str,
    consumer_key: &str,
    consumer_secret: &str,
    token: Option<&str>,
    token_secret: &str,
    extra: &[(&str, &str)],
    nonce: &str,
    timestamp: &str,
) -> BTreeMap<String, String> {
    let mut params: BTreeMap<String, String> = BTreeMap::new();
    params.insert("oauth_consumer_key".into(), consumer_key.into());
    params.insert("oauth_nonce".into(), nonce.into());
    params.insert("oauth_signature_method".into(), "HMAC-SHA1".into());
    params.insert("oauth_timestamp".into(), timestamp.into());
    params.insert("oauth_version".into(), "1.0".into());
    if let Some(t) = token {
        params.insert("oauth_token".into(), t.into());
    }
    for (k, v) in extra {
        params.insert((*k).into(), (*v).into());
    }
    let sig = signature(method, url, &params, consumer_secret, token_secret);
    params.insert("oauth_signature".into(), sig);
    params
}

/// An `Authorization: OAuth …` header value from the oauth_* entries of a signed param set
/// (RFC 5849 §3.5.1). Request params are sent in the URL/body, not the header; the protocol
/// params (`oauth_callback` and `oauth_verifier` included) go only here.
pub fn auth_header(params: &BTreeMap<String, String>) -> String {
    let inner = params
        .iter()
        .filter(|(k, _)| k.starts_with("oauth_"))
        .map(|(k, v)| format!("{}=\"{}\"", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("OAuth {inner}")
}

/// The request (non-`oauth_`) entries of a signed param set: what goes in the URL query or
/// the body. Never carries a protocol param, so never the token or signature.
pub fn request_params(params: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    params
        .iter()
        .filter(|(k, _)| !k.starts_with("oauth_"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// `base_url` with the request params of a signed param set as its query (oauth_* left out;
/// they belong in [`auth_header`]). The bare `base_url` when there are none.
pub fn request_url(base_url: &str, params: &BTreeMap<String, String>) -> String {
    let query = request_params(params);
    if query.is_empty() {
        base_url.to_string()
    } else {
        format!("{base_url}?{}", query_string(&query))
    }
}

/// A `key=value&…` query/body string (percent-encoded) for the given params.
pub fn query_string(params: &BTreeMap<String, String>) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// `text` with every occurrence of each non-empty `secret` replaced by `[redacted]` — raw,
/// percent-encoded, and double-encoded (as a signature base string quotes it). For error
/// text that echoes a service reply: a signature-failure reply can quote the signed request.
pub fn redact(text: &str, secrets: &[&str]) -> String {
    let mut out = text.to_string();
    for s in secrets.iter().filter(|s| !s.is_empty()) {
        let once = percent_encode(s);
        let twice = percent_encode(&once);
        for form in [twice, once, s.to_string()] {
            out = out.replace(&form, "[redacted]");
        }
    }
    out
}

/// Parse an `oauth_token=…&oauth_token_secret=…` form-encoded token response into a map.
pub fn parse_kv(body: &str) -> BTreeMap<String, String> {
    body.split('&')
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (decode(k), decode(v)))
        .collect()
}

fn decode(s: &str) -> String {
    // Minimal application/x-www-form-urlencoded decode (tokens are ASCII-safe in practice).
    let bytes = s.replace('+', " ");
    let mut out = Vec::with_capacity(bytes.len());
    let mut it = bytes.bytes();
    while let Some(b) = it.next() {
        if b == b'%' {
            let h = it.next();
            let l = it.next();
            if let (Some(h), Some(l)) = (h, l) {
                if let (Some(h), Some(l)) = ((h as char).to_digit(16), (l as char).to_digit(16)) {
                    out.push((h * 16 + l) as u8);
                    continue;
                }
            }
        } else {
            out.push(b);
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn nonce() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string()
}

#[cfg(test)]
pub(crate) mod stub {
    //! Test-only HTTP/1.1 stub for the Flickr and SmugMug transports: an ephemeral loopback
    //! port that records every request (method, target, headers, body) and answers each path
    //! with a scripted body. Never the network. [`Captured::verify_signature`] re-derives the
    //! OAuth signature from what actually went on the wire.

    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// One request the stub received.
    #[derive(Debug, Clone)]
    pub(crate) struct Captured {
        pub method: String,
        /// Path and query, e.g. `/services/rest/?method=…`.
        pub target: String,
        /// Header names lowercased.
        pub headers: BTreeMap<String, String>,
        pub body: Vec<u8>,
    }

    impl Captured {
        pub fn path(&self) -> &str {
            self.target.split('?').next().unwrap_or("")
        }

        /// The decoded URL query params.
        pub fn query(&self) -> BTreeMap<String, String> {
            match self.target.split_once('?') {
                Some((_, q)) => parse_kv(q),
                None => BTreeMap::new(),
            }
        }

        /// The decoded `Authorization: OAuth k="v", …` params (empty when absent).
        pub fn oauth_header(&self) -> BTreeMap<String, String> {
            let Some(h) = self.headers.get("authorization") else { return BTreeMap::new() };
            let inner = h.strip_prefix("OAuth ").expect("an OAuth authorization header");
            inner
                .split(", ")
                .map(|kv| {
                    let (k, v) = kv.split_once('=').expect("k=\"v\"");
                    let v = v
                        .strip_prefix('"')
                        .and_then(|v| v.strip_suffix('"'))
                        .expect("a quoted value");
                    (decode(k), decode(v))
                })
                .collect()
        }

        /// Recompute the HMAC-SHA1 signature from the header's protocol params plus the URL
        /// query and the extra signed `body_params` (form fields), and check it against the
        /// header's `oauth_signature`. `base_url` is the scheme/host/path the client signed.
        pub fn verify_signature(
            &self,
            base_url: &str,
            body_params: &BTreeMap<String, String>,
            consumer_secret: &str,
            token_secret: &str,
        ) -> bool {
            let mut params = self.oauth_header();
            let Some(sig) = params.remove("oauth_signature") else { return false };
            params.extend(self.query());
            params.extend(body_params.clone());
            signature(&self.method, base_url, &params, consumer_secret, token_secret) == sig
        }
    }

    pub(crate) struct Server {
        pub port: u16,
        log: Arc<Mutex<Vec<Captured>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Server {
        /// Bind `127.0.0.1:0`; answer a request whose path is a key of `routes` with `200`
        /// and that body, anything else with `404`.
        pub async fn start(routes: Vec<(String, String)>) -> Server {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind a loopback port");
            let port = listener.local_addr().unwrap().port();
            let log: Arc<Mutex<Vec<Captured>>> = Arc::default();
            let routes = Arc::new(routes);
            let task = {
                let log = log.clone();
                tokio::spawn(async move {
                    while let Ok((stream, _)) = listener.accept().await {
                        tokio::spawn(serve(stream, routes.clone(), log.clone()));
                    }
                })
            };
            Server { port, log, task }
        }

        pub fn url(&self, path: &str) -> String {
            format!("http://127.0.0.1:{}{path}", self.port)
        }

        pub fn log(&self) -> Vec<Captured> {
            self.log.lock().unwrap().clone()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn serve(
        mut stream: TcpStream,
        routes: Arc<Vec<(String, String)>>,
        log: Arc<Mutex<Vec<Captured>>>,
    ) {
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let head_end = loop {
                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break i;
                }
                let mut chunk = [0u8; 8192];
                match stream.read(&mut chunk).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            };
            let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
            let mut lines = head.split("\r\n");
            let mut start = lines.next().unwrap_or("").split(' ');
            let method = start.next().unwrap_or("").to_string();
            let target = start.next().unwrap_or("").to_string();
            let headers: BTreeMap<String, String> = lines
                .filter_map(|l| l.split_once(':'))
                .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                .collect();
            let len: usize = headers
                .get("content-length")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let body_start = head_end + 4;
            while buf.len() < body_start + len {
                let mut chunk = [0u8; 65536];
                match stream.read(&mut chunk).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            let body = buf[body_start..body_start + len].to_vec();
            buf.drain(..body_start + len);
            let path = target.split('?').next().unwrap_or("").to_string();
            log.lock().unwrap().push(Captured { method, target, headers, body });

            let (status, reply) = match routes.iter().find(|(p, _)| *p == path) {
                Some((_, b)) => ("200 OK", b.clone()),
                None => ("404 Not Found", String::new()),
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\r\n{reply}",
                reply.len()
            );
            if stream.write_all(resp.as_bytes()).await.is_err() {
                return;
            }
        }
    }

    /// Assert the #190 contract on one captured request: no protocol param anywhere in the
    /// URL, and the `Authorization` header carries the signature and, for a token-bearing
    /// call, exactly `token`.
    pub(crate) fn assert_oauth_in_header_only(req: &Captured, token: Option<&str>) {
        assert!(
            !req.target.contains("oauth_"),
            "a protocol param leaked into the URL: {}",
            req.target
        );
        if let Some(t) = token {
            assert!(!req.target.contains(t), "the token leaked into the URL: {}", req.target);
        }
        let h = req.oauth_header();
        assert!(
            h.contains_key("oauth_signature"),
            "no signature in the Authorization header: {:?}",
            req.headers
        );
        assert!(h.contains_key("oauth_consumer_key"));
        assert_eq!(h.get("oauth_token").map(String::as_str), token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The Twitter OAuth example's parameters/secrets (a widely-used HMAC-SHA1 OAuth 1.0a
    // fixture). The base string and signing key this produces are verified byte-for-byte
    // against Twitter's published example; the expected signature is the value an
    // independent reference HMAC-SHA1 (Python's `hmac`/`hashlib`) computes for that exact
    // base string + key. If this passes, our base-string construction, percent-encoding,
    // parameter sorting, signing key, and HMAC-SHA1+base64 are all correct.
    #[test]
    fn signature_matches_reference_hmac_sha1() {
        let mut p = BTreeMap::new();
        p.insert(
            "status".into(),
            "Hello Ladies + Gentlemen, a signed OAuth request!".into(),
        );
        p.insert("include_entities".into(), "true".into());
        p.insert("oauth_consumer_key".into(), "xvz1evFS4wEEPTGEFPHBog".into());
        p.insert(
            "oauth_nonce".into(),
            "kYjzVBB8Y0ZFabxSWbWovY3uYSQ2pTgmZeNu2VS4cg".into(),
        );
        p.insert("oauth_signature_method".into(), "HMAC-SHA1".into());
        p.insert("oauth_timestamp".into(), "1318622958".into());
        p.insert(
            "oauth_token".into(),
            "370773112-GmHxMAgYyLbNEtIKZeRNFsMKPR9EyMZeS9weJAEb".into(),
        );
        p.insert("oauth_version".into(), "1.0".into());

        let sig = signature(
            "POST",
            "https://api.twitter.com/1/statuses/update.json",
            &p,
            "kAcSOqF21Fu85e7zjz7ZN2U4ZRhfV3WpwPAoE3Y7Q",
            "LswwdoUaIVS3lIvBJ8u8Yuf6vnjbW7Q1d3hRzqJ8XQ",
        );
        assert_eq!(sig, "SWuLigt7Lt7fOv3HyQS8HZdEeFg=");
    }

    // ── RFC 5849 vectors and the header/URL split (#190) ──────────────────────

    // RFC 5849 §1.2 (the photos.example.net request): its `oauth_signature`
    // "MdpQcU8iPSUjWoN/UDMsK2sui9I=" is the RFC's own; Python's hmac/hashlib computes the
    // same value for that base string and key.
    #[test]
    fn signature_matches_rfc5849_example() {
        let mut p = BTreeMap::new();
        p.insert("file".into(), "vacation.jpg".into());
        p.insert("size".into(), "original".into());
        p.insert("oauth_consumer_key".into(), "dpf43f3p2l4k3l03".into());
        p.insert("oauth_token".into(), "nnch734d00sl2jdk".into());
        p.insert("oauth_signature_method".into(), "HMAC-SHA1".into());
        p.insert("oauth_timestamp".into(), "137131202".into());
        p.insert("oauth_nonce".into(), "chapoH".into());
        let sig = signature(
            "GET",
            "http://photos.example.net/photos",
            &p,
            "kd94hf93k423kf44",
            "pfkkdhi9sl3r4s00",
        );
        assert_eq!(sig, "MdpQcU8iPSUjWoN/UDMsK2sui9I=");
    }

    // The same request through `signed_params_with`, split for the wire: the header carries
    // every protocol param (the token and signature included), the URL only the request
    // params. With oauth_version=1.0 (which `signed_params` always adds) and nonce/timestamp
    // "kllo9940pd9333jh"/"1191242096" this is the OAuth Core 1.0 Appendix A.5 request, whose
    // published signature is "tR3+Ty81lMeYAr/Fid0kMTYa/WM=" (also cross-checked with Python).
    #[test]
    fn signed_request_splits_protocol_params_into_the_header() {
        let p = signed_params_with(
            "GET",
            "http://photos.example.net/photos",
            "dpf43f3p2l4k3l03",
            "kd94hf93k423kf44",
            Some("nnch734d00sl2jdk"),
            "pfkkdhi9sl3r4s00",
            &[("file", "vacation.jpg"), ("size", "original")],
            "kllo9940pd9333jh",
            "1191242096",
        );
        assert_eq!(p["oauth_signature"], "tR3+Ty81lMeYAr/Fid0kMTYa/WM=");

        assert_eq!(
            auth_header(&p),
            "OAuth oauth_consumer_key=\"dpf43f3p2l4k3l03\", oauth_nonce=\"kllo9940pd9333jh\", \
             oauth_signature=\"tR3%2BTy81lMeYAr%2FFid0kMTYa%2FWM%3D\", \
             oauth_signature_method=\"HMAC-SHA1\", oauth_timestamp=\"1191242096\", \
             oauth_token=\"nnch734d00sl2jdk\", oauth_version=\"1.0\""
        );
        assert_eq!(
            request_url("http://photos.example.net/photos", &p),
            "http://photos.example.net/photos?file=vacation.jpg&size=original"
        );

        // A token step: oauth_callback is a protocol param, so the URL stays bare.
        let step = signed_params_with(
            "GET",
            "http://x/",
            "k",
            "s",
            None,
            "",
            &[("oauth_callback", "oob")],
            "n",
            "1",
        );
        assert_eq!(request_url("http://x/", &step), "http://x/");
        assert!(auth_header(&step).contains("oauth_callback=\"oob\""));
    }

    #[test]
    fn redact_hides_raw_and_encoded_secrets() {
        let t = "72157-ab/c+d";
        let text = format!(
            "oauth_problem=signature_invalid&debug_sbs=GET&x&oauth_token%253D{}%26 raw={t} once={}",
            percent_encode(&percent_encode(t)),
            percent_encode(t)
        );
        let r = redact(&text, &[t, ""]);
        assert!(!r.contains(t) && !r.contains(&percent_encode(t)), "{r}");
        assert!(!r.contains(&percent_encode(&percent_encode(t))), "{r}");
        assert_eq!(r.matches("[redacted]").count(), 3, "{r}");
        assert_eq!(redact("no secrets here", &[""]), "no secrets here");
    }

    #[test]
    fn percent_encode_escapes_reserved() {
        assert_eq!(percent_encode("Ladies + Gentlemen"), "Ladies%20%2B%20Gentlemen");
        assert_eq!(percent_encode("aA1-._~"), "aA1-._~");
    }

    #[test]
    fn parse_kv_round_trips_token_response() {
        let m = parse_kv("oauth_token=abc&oauth_token_secret=xyz&oauth_callback_confirmed=true");
        assert_eq!(m.get("oauth_token").unwrap(), "abc");
        assert_eq!(m.get("oauth_token_secret").unwrap(), "xyz");
    }
}
