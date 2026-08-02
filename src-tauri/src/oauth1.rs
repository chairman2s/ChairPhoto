//! Minimal OAuth 1.0a (HMAC-SHA1) request signing, shared by the Flickr and SmugMug
//! modules (the `flickr`/`smugmug` Cargo features). Both services require OAuth 1.0a for
//! uploads; this module is the signing core and the one piece verifiable offline — the
//! `signature` test below checks it against the canonical Twitter OAuth example (the
//! reference vector used to validate HMAC-SHA1 OAuth implementations).
//!
//! Everything here is pure: no network, no global state. Callers assemble request params,
//! call [`signed_params`], then send the oauth_* values as an `Authorization` header
//! ([`auth_header`]) or as a query string ([`query_string`]).

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
    let mut params: BTreeMap<String, String> = BTreeMap::new();
    params.insert("oauth_consumer_key".into(), consumer_key.into());
    params.insert("oauth_nonce".into(), nonce());
    params.insert("oauth_signature_method".into(), "HMAC-SHA1".into());
    params.insert("oauth_timestamp".into(), timestamp());
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
/// (request params are sent in the URL/body, not the header).
pub fn auth_header(params: &BTreeMap<String, String>) -> String {
    let inner = params
        .iter()
        .filter(|(k, _)| k.starts_with("oauth_"))
        .map(|(k, v)| format!("{}=\"{}\"", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("OAuth {inner}")
}

/// A `key=value&…` query/body string (percent-encoded) for the given params.
pub fn query_string(params: &BTreeMap<String, String>) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
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
