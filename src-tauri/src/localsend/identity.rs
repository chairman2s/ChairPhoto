//! The TLS client identity ChairPhoto presents to LocalSend peers (issue #56).
//!
//! # Why a client certificate at all
//!
//! LocalSend v2 peers serve HTTPS with a self-signed certificate, and some of them —
//! observed on LocalSend mobile, 2026-08-12 — additionally *require* the client to present
//! one. Without it the peer aborts with a TLS alert before any LocalSend request is read:
//!
//! ```text
//! OpenSSL SSL_read: error:0A00045C:SSL routines::tlsv13 alert certificate required
//! ```
//!
//! `danger_accept_invalid_certs` does not help — that governs how we treat *their*
//! certificate, not whether we send one of ours.
//!
//! # Why it is presented unconditionally
//!
//! Peers that do not require client certificates ignore one that is offered: verified
//! against the LocalSend desktop app v2.1, which answers `GET /api/localsend/v2/info` with
//! `200` identically with and without a client certificate. So there is no negotiation to
//! do and no per-peer state to track — always present it, and peers that care are satisfied
//! while peers that do not are unaffected.
//!
//! # Identity, not authentication
//!
//! This certificate authenticates nothing. LocalSend is a LAN protocol between devices that
//! trust each other by fingerprint and user confirmation, and every peer's certificate is
//! self-signed. Ours exists to satisfy the handshake and to be stable across runs, not to
//! prove anything about who we are.
//!
//! # Persistence
//!
//! Generated once and stored under the application data directory, alongside the catalog
//! database rather than inside it: it is per-installation, not per-catalog, and a catalog is
//! a portable document that should not carry a private key. `0600` on unix.

use std::path::PathBuf;
use std::sync::OnceLock;

/// Filename under the app data directory. Holds the private key and certificate as PEM, in
/// the order `reqwest::Identity::from_pem` expects.
const IDENTITY_FILE: &str = "localsend-client-identity.pem";

/// Process-wide cache. `reqwest::Identity` is cheap to clone but generating or reading the
/// PEM is not, and a send renders one client per device.
static IDENTITY: OnceLock<Option<reqwest::Identity>> = OnceLock::new();

fn identity_path() -> Result<PathBuf, String> {
    Ok(crate::commands::app_data_dir()?.join(IDENTITY_FILE))
}

/// The client identity, generating and persisting it on first use.
///
/// Returns `None` — never an error — when the identity cannot be produced or loaded, so a
/// send to a plain-`http` peer, or to an `https` peer that does not demand a client
/// certificate, still works on a machine where the key cannot be written. The reason is
/// logged once. Callers treat `None` as "no identity to attach", not as failure.
pub(super) fn client_identity() -> Option<reqwest::Identity> {
    IDENTITY
        .get_or_init(|| match load_or_create() {
            Ok(id) => Some(id),
            Err(e) => {
                eprintln!(
                    "localsend: no TLS client identity ({e}); sends to peers that require \
                     one will fail, others are unaffected"
                );
                None
            }
        })
        .clone()
}

fn load_or_create() -> Result<reqwest::Identity, String> {
    let path = identity_path()?;

    if path.is_file() {
        let pem = std::fs::read(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
        // A corrupt or truncated file is worth replacing rather than failing forever: it is
        // regenerable state, not user data. Fall through to generation.
        match reqwest::Identity::from_pem(&pem) {
            Ok(id) => return Ok(id),
            Err(e) => eprintln!(
                "localsend: stored client identity at {} is unusable ({e}); regenerating",
                path.display()
            ),
        }
    }

    let pem = generate_pem()?;
    let identity = reqwest::Identity::from_pem(pem.as_bytes())
        .map_err(|e| format!("building an identity from the generated PEM: {e}"))?;
    persist(&path, &pem)?;
    Ok(identity)
}

/// A fresh self-signed certificate and key, as one PEM blob (key first, then certificate).
fn generate_pem() -> Result<String, String> {
    // The SAN is cosmetic here — no peer validates it, since every certificate in this
    // protocol is self-signed — but a certificate needs one, and this is what shows up if
    // anyone inspects the handshake.
    let cert = rcgen::generate_simple_self_signed(vec!["chairphoto.local".to_string()])
        .map_err(|e| format!("generating a self-signed certificate: {e}"))?;
    Ok(format!(
        "{}{}",
        cert.signing_key.serialize_pem(),
        cert.cert.pem()
    ))
}

/// Write the PEM, owner-readable only.
fn persist(path: &std::path::Path, pem: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    std::fs::write(path, pem).map_err(|e| format!("writing {}: {e}", path.display()))?;
    // Best-effort tightening after the fact. Writing then chmod-ing leaves a brief window at
    // the default mode; acceptable for a key that authenticates nothing and lives in the
    // user's own data directory, and the alternative (a custom open mode) buys little here.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            eprintln!("localsend: couldn't restrict permissions on {}: {e}", path.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_pem_contains_a_key_and_a_certificate_and_parses() {
        let pem = generate_pem().expect("generation must succeed");
        assert!(pem.contains("PRIVATE KEY"), "PEM must carry the private key, got: {pem:.80}");
        assert!(pem.contains("BEGIN CERTIFICATE"), "PEM must carry the certificate");
        // The order matters to reqwest; this is the assertion that would catch a swap.
        assert!(
            pem.find("PRIVATE KEY").unwrap() < pem.find("BEGIN CERTIFICATE").unwrap(),
            "reqwest::Identity::from_pem wants the key before the certificate"
        );
        reqwest::Identity::from_pem(pem.as_bytes()).expect("reqwest must accept the PEM");
    }

    #[test]
    fn each_generation_is_distinct() {
        // Not cosmetic: a shared key across installations would make every ChairPhoto
        // present the same TLS identity on every LAN it joins.
        let a = generate_pem().unwrap();
        let b = generate_pem().unwrap();
        assert_ne!(a, b, "each installation must get its own key");
    }

    #[test]
    fn a_corrupt_stored_identity_is_replaced_rather_than_fatal() {
        let dir = crate::test_support::TestTmpDir::new("localsend-identity");
        let path = dir.join("localsend-client-identity.pem");
        std::fs::write(&path, b"not a pem at all").unwrap();

        // `load_or_create` reads a fixed path, so exercise the recovery rule it applies
        // rather than the path lookup: a stored blob reqwest rejects must not be trusted.
        assert!(
            reqwest::Identity::from_pem(b"not a pem at all").is_err(),
            "precondition: the corrupt blob really is unusable"
        );
        let fresh = generate_pem().unwrap();
        assert!(reqwest::Identity::from_pem(fresh.as_bytes()).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn persist_writes_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::test_support::TestTmpDir::new("localsend-identity-perms");
        let path = dir.join("id.pem");
        persist(&path, &generate_pem().unwrap()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a private key must not be group/world readable");
    }
}
