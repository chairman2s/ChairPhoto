//! Signing in to an OAuth 1.0a service — Flickr or SmugMug (docs/flickr.md, docs/smugmug.md):
//! the out-of-band flow the Tauri commands and the GPUI modules share.
//!
//! 1. [`begin_auth`] asks the service for a request token (callback `oob`), keeps the token and
//!    its secret in the service's settings, and returns the authorize URL the user opens.
//! 2. The user approves in the browser and pastes back the verifier code.
//! 3. [`complete_auth`] exchanges the stored request token + verifier for the access token,
//!    stores it, and clears the request token — so a verifier is accepted only after this
//!    install's own Connect, and only once.
//!
//! There is no loopback callback: the verifier is pasted, so nothing listens on a port. The
//! request token is the flow's state (a verifier for any other token fails at the service).
//!
//! **Secrets.** The API secret and the tokens live in the catalog `settings` table, as before
//! (`<service>.api_key`, `.api_secret`, `.access_token`, `.access_secret`; Flickr adds
//! `.user_nsid`). Nothing here logs or formats one; [`Credentials`] redacts itself in `Debug`.
//!
//! The network is the service's [`OAuthApi`]: the production one calls `crate::flickr` /
//! `crate::smugmug`; tests hand in a fake and never reach a real service. Every function is
//! **blocking** (network, catalog): run it on a worker.

use super::uploads::{setting, ServiceSettings};

pub const API_KEY: &str = "api_key";
pub const API_SECRET: &str = "api_secret";
pub const REQUEST_TOKEN: &str = "request_token";
pub const REQUEST_SECRET: &str = "request_secret";
pub const ACCESS_TOKEN: &str = "access_token";
pub const ACCESS_SECRET: &str = "access_secret";
/// Flickr's account id, from the access-token answer: builds canonical photo page URLs.
pub const USER_NSID: &str = "user_nsid";

/// What [`complete_auth`] answers when there is no Connect to finish.
pub const CONNECT_FIRST: &str = "Start with Connect before entering the verifier.";

/// A request token and the URL the user authorizes it at.
pub struct RequestToken {
    pub token: String,
    pub secret: String,
    pub authorize_url: String,
}

/// The long-lived access token.
pub struct AccessToken {
    pub token: String,
    pub secret: String,
    /// Flickr's `user_nsid`; `None` for a service without one (or when Flickr omits it).
    pub user_nsid: Option<String>,
}

/// A signed-in service's keys and tokens, for one request. `Debug` shows only the key.
#[derive(Clone)]
pub struct Credentials {
    pub key: String,
    pub secret: String,
    pub token: String,
    pub token_secret: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials").field("key", &self.key).finish_non_exhaustive()
    }
}

/// A service's OAuth 1.0a token endpoints. Blocking.
pub trait OAuthApi: Send + Sync {
    /// Step 1: a request token (callback `oob`) and its authorize URL.
    fn request_token(&self, key: &str, secret: &str) -> Result<RequestToken, String>;
    /// Step 3: the access token for the authorized request token and the pasted verifier.
    fn access_token(&self, key: &str, secret: &str, token: &str, token_secret: &str, verifier: &str) -> Result<AccessToken, String>;
}

/// The user's app key and secret, or a hint to enter them. `service` is the settings
/// namespace (`flickr`, `smugmug`), as the hint has always named it.
pub fn app_keys(settings: &dyn ServiceSettings, service: &str) -> Result<(String, String), String> {
    let key = setting(settings, API_KEY)?;
    let secret = setting(settings, API_SECRET)?;
    if key.is_empty() || secret.is_empty() {
        return Err(format!("Enter your {service} API key and secret in the module settings first."));
    }
    Ok((key, secret))
}

/// The keys and access token of a connected service, or a hint to connect.
pub fn credentials(settings: &dyn ServiceSettings, service: &str) -> Result<Credentials, String> {
    let (key, secret) = app_keys(settings, service)?;
    let token = setting(settings, ACCESS_TOKEN)?;
    let token_secret = setting(settings, ACCESS_SECRET)?;
    if token.is_empty() || token_secret.is_empty() {
        return Err(format!("Connect {service} in the module settings first."));
    }
    Ok(Credentials { key, secret, token, token_secret })
}

/// Whether the service has an access token.
pub fn connected(settings: &dyn ServiceSettings) -> Result<bool, String> {
    Ok(!setting(settings, ACCESS_TOKEN)?.is_empty())
}

/// Connect: get a request token, keep it for [`complete_auth`], return the authorize URL.
pub fn begin_auth(api: &dyn OAuthApi, settings: &dyn ServiceSettings, service: &str) -> Result<String, String> {
    let (key, secret) = app_keys(settings, service)?;
    let rt = api.request_token(&key, &secret)?;
    settings.set(REQUEST_TOKEN, &rt.token)?;
    settings.set(REQUEST_SECRET, &rt.secret)?;
    Ok(rt.authorize_url)
}

/// Finish: exchange the stored request token and `verifier` (trimmed) for the access token,
/// store it, and forget the request token.
pub fn complete_auth(api: &dyn OAuthApi, settings: &dyn ServiceSettings, service: &str, verifier: &str) -> Result<(), String> {
    let (key, secret) = app_keys(settings, service)?;
    let token = setting(settings, REQUEST_TOKEN)?;
    let token_secret = setting(settings, REQUEST_SECRET)?;
    if token.is_empty() {
        return Err(CONNECT_FIRST.into());
    }
    let at = api.access_token(&key, &secret, &token, &token_secret, verifier.trim())?;
    settings.set(ACCESS_TOKEN, &at.token)?;
    settings.set(ACCESS_SECRET, &at.secret)?;
    if let Some(nsid) = at.user_nsid.as_deref() {
        settings.set(USER_NSID, nsid)?;
    } else if settings.get(USER_NSID)?.is_some() {
        // A reconnect to an account whose answer has no NSID must not keep the old one.
        settings.set(USER_NSID, "")?;
    }
    settings.set(REQUEST_TOKEN, "")?;
    settings.set(REQUEST_SECRET, "")?;
    Ok(())
}

/// In-memory settings and a fake token service, for the flows' tests here and in the
/// service modules.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct MemSettings(pub Mutex<BTreeMap<String, String>>);

    impl MemSettings {
        pub fn with(pairs: &[(&str, &str)]) -> Self {
            let s = MemSettings::default();
            for (k, v) in pairs {
                s.0.lock().unwrap().insert((*k).into(), (*v).into());
            }
            s
        }
        pub fn value(&self, key: &str) -> Option<String> {
            self.0.lock().unwrap().get(key).cloned()
        }
    }

    impl ServiceSettings for MemSettings {
        fn get(&self, key: &str) -> Result<Option<String>, String> {
            Ok(self.0.lock().unwrap().get(key).cloned())
        }
        fn set(&self, key: &str, value: &str) -> Result<(), String> {
            self.0.lock().unwrap().insert(key.into(), value.into());
            Ok(())
        }
    }

    /// Issues request token `rt-<n>`; grants access only for the request token it issued
    /// last and the verifier `ok-verifier`.
    #[derive(Default)]
    pub struct FakeTokens {
        pub issued: Mutex<Vec<String>>,
        pub nsid: Option<String>,
    }

    impl OAuthApi for FakeTokens {
        fn request_token(&self, key: &str, _: &str) -> Result<RequestToken, String> {
            let mut issued = self.issued.lock().unwrap();
            let token = format!("rt-{}", issued.len() + 1);
            issued.push(token.clone());
            Ok(RequestToken { authorize_url: format!("https://auth.example/authorize?oauth_token={token}&k={key}"), token, secret: "rs".into() })
        }
        fn access_token(&self, _: &str, _: &str, token: &str, token_secret: &str, verifier: &str) -> Result<AccessToken, String> {
            let last = self.issued.lock().unwrap().last().cloned();
            if last.as_deref() != Some(token) || token_secret != "rs" || verifier != "ok-verifier" {
                return Err("The service refused the verifier".into());
            }
            Ok(AccessToken { token: "access-tok".into(), secret: "access-sec".into(), user_nsid: self.nsid.clone() })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{FakeTokens, MemSettings};
    use super::*;

    /// Connect keeps the request token; Finish exchanges it with the (trimmed) verifier,
    /// stores the access token and the NSID, and forgets the request token — a second Finish
    /// with the same verifier is refused.
    #[test]
    fn connect_then_finish_stores_the_access_token_and_forgets_the_request_token() {
        let api = FakeTokens { nsid: Some("123@N01".into()), ..Default::default() };
        let s = MemSettings::with(&[(API_KEY, "k"), (API_SECRET, "s")]);
        assert!(!connected(&s).unwrap());
        let url = begin_auth(&api, &s, "flickr").unwrap();
        assert!(url.contains("oauth_token=rt-1"));
        assert_eq!(s.value(REQUEST_TOKEN).as_deref(), Some("rt-1"));
        complete_auth(&api, &s, "flickr", "  ok-verifier \n").unwrap();
        assert!(connected(&s).unwrap());
        assert_eq!((s.value(ACCESS_TOKEN).unwrap(), s.value(ACCESS_SECRET).unwrap()), ("access-tok".into(), "access-sec".into()));
        assert_eq!(s.value(USER_NSID).as_deref(), Some("123@N01"));
        assert_eq!(s.value(REQUEST_TOKEN).as_deref(), Some(""), "the request token outlived the exchange");
        assert_eq!(complete_auth(&api, &s, "flickr", "ok-verifier").unwrap_err(), CONNECT_FIRST);
        let c = credentials(&s, "flickr").unwrap();
        let shown = format!("{c:?}");
        assert!(!shown.contains("access-") && !shown.contains("\"s\""), "Debug shows a secret: {shown}");
    }

    /// Finish without Connect, keys missing, and a refused verifier: each says what to do and
    /// stores nothing.
    #[test]
    fn the_flow_refuses_out_of_order_or_unconfigured() {
        let api = FakeTokens::default();
        let s = MemSettings::default();
        assert!(begin_auth(&api, &s, "smugmug").unwrap_err().contains("Enter your smugmug API key"));
        let s = MemSettings::with(&[(API_KEY, "k"), (API_SECRET, "s")]);
        assert_eq!(complete_auth(&api, &s, "smugmug", "ok-verifier").unwrap_err(), CONNECT_FIRST);
        begin_auth(&api, &s, "smugmug").unwrap();
        assert!(complete_auth(&api, &s, "smugmug", "wrong").is_err());
        assert!(!connected(&s).unwrap());
        assert_eq!(s.value(REQUEST_TOKEN).as_deref(), Some("rt-1"), "a refused verifier keeps the pending Connect");
        // A second Connect replaces the pending one: the first token's verifier is stale.
        begin_auth(&api, &s, "smugmug").unwrap();
        assert_eq!(s.value(REQUEST_TOKEN).as_deref(), Some("rt-2"));
        assert!(credentials(&s, "smugmug").unwrap_err().contains("Connect smugmug"));
    }
}
