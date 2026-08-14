//! The shipped Content-Security-Policy, pinned.
//!
//! `tauri.conf.json`'s `app.security.csp` is what stops an installed module reaching an
//! arbitrary origin with `fetch`/`XHR`/`WebSocket` (docs/module-capabilities.md § "The
//! policy"). It is one JSON edit away from being widened back, and the failure is silent:
//! the app keeps working, tests stay green, and only the guarantee is gone.
//!
//! These tests read the real config file and assert the properties the policy is *for*,
//! not its exact text — a new directive or an added image host must not fail them, but
//! putting a remote origin back into `connect-src` must.
//!
//! Scope, honestly: this proves what the app ships, not what the webview enforces. The
//! browser-side half — that `connect-src` really does block a module's `fetch` while the
//! app's own protocols keep working — is a manual check against a `custom-protocol` build,
//! because `tauri dev` serves the page from Vite and injects no CSP at all on desktop.

use serde_json::Value;

const CONFIG: &str = include_str!("../tauri.conf.json");

fn csp() -> Value {
    let config: Value = serde_json::from_str(CONFIG).expect("tauri.conf.json is not valid JSON");
    config["app"]["security"]["csp"].clone()
}

/// The sources of one directive, split on whitespace.
fn sources(directive: &str) -> Vec<String> {
    let csp = csp();
    let raw = csp
        .get(directive)
        .unwrap_or_else(|| panic!("csp has no `{directive}` directive"));
    match raw {
        Value::String(s) => s.split_whitespace().map(str::to_string).collect(),
        Value::Array(a) => a
            .iter()
            .map(|v| v.as_str().expect("csp source is not a string").to_string())
            .collect(),
        other => panic!("csp `{directive}` is neither a string nor an array: {other}"),
    }
}

#[test]
fn a_csp_is_set_at_all() {
    assert!(
        !csp().is_null(),
        "app.security.csp is null — with no policy an installed module can fetch() any \
         CORS-permitting endpoint directly, never touching the host API"
    );
}

#[test]
fn connect_src_reaches_the_ipc_bridge_and_nothing_remote() {
    let sources = sources("connect-src");

    // The app's own IPC. Tauri's bridge is `fetch(convertFileSrc(cmd, "ipc"))`, which is
    // `ipc://localhost/<cmd>` on Linux/macOS and `http://ipc.localhost/<cmd>` on Windows.
    assert!(
        sources.iter().any(|s| s == "ipc:"),
        "connect-src must allow `ipc:` or every Tauri command fails on Linux/macOS: {sources:?}"
    );
    assert!(
        sources.iter().any(|s| s == "http://ipc.localhost"),
        "connect-src must allow `http://ipc.localhost` or every Tauri command fails on \
         Windows: {sources:?}"
    );

    // Nothing else. `'self'` is the app's own origin; the two IPC forms above are the
    // bridge. Any other host or scheme here is a route out of the machine that does not
    // pass through Rust — which is the hole this policy exists to close.
    let allowed = ["'self'", "ipc:", "http://ipc.localhost"];
    let extra: Vec<_> = sources.iter().filter(|s| !allowed.contains(&s.as_str())).collect();
    assert!(
        extra.is_empty(),
        "connect-src allows more than the IPC bridge: {extra:?}. fetch/XHR/WebSocket from \
         module code would reach those origins directly, bypassing the host API."
    );

    // Wildcards would defeat the check above even while listing nothing suspicious.
    for s in &sources {
        assert!(
            s != "*" && !s.starts_with("http:*") && !s.starts_with("https:"),
            "connect-src source `{s}` is a wildcard over remote origins"
        );
    }
}

/// Adding the host-mediated proxy (#49) must not have widened the policy that made it
/// necessary.
///
/// The temptation this guards against is concrete: `api.fetch` gives modules a route to a
/// declared origin, and the cheapest-looking way to make some future thing work would be to
/// put that origin into `connect-src` — at which point every module can reach it directly,
/// with no declaration, no grant, and no Rust in the path. `connect-src` staying at
/// `'self' ipc: http://ipc.localhost` is what keeps the proxy the *only* route, and therefore
/// what makes the per-module gate meaningful rather than advisory.
///
/// `default-src` is checked too: a `connect-src` deleted rather than narrowed would silently
/// fall back to it.
#[test]
fn the_fetch_proxy_did_not_widen_connect_src() {
    let connect = sources("connect-src");
    let allowed = ["'self'", "ipc:", "http://ipc.localhost"];
    for source in &connect {
        assert!(
            allowed.contains(&source.as_str()),
            "connect-src gained `{source}` — module code could reach it without going through \
             api.fetch, so the per-module origin grant would no longer bound anything"
        );
    }
    // The fallback for any fetch-directive that is absent.
    let default = sources("default-src");
    assert_eq!(
        default,
        vec!["'self'"],
        "default-src must stay 'self' — it is what connect-src falls back to if it is ever \
         removed instead of narrowed: {default:?}"
    );
}

#[test]
fn the_native_media_protocols_stay_loadable() {
    let img = sources("img-src");
    // src-tauri/src/lib.rs registers these three; src/modules/previewCache.ts and
    // src/components/Thumbnail.tsx build `<scheme>://localhost/<photo_id>` URLs from them.
    for scheme in ["thumb:", "preview:", "zoom:"] {
        assert!(
            img.iter().any(|s| s == scheme),
            "img-src must allow `{scheme}` or grid thumbnails and loupe previews go blank: {img:?}"
        );
    }
    // Windows/Android form of the same three protocols.
    for host in [
        "http://thumb.localhost",
        "http://preview.localhost",
        "http://zoom.localhost",
    ] {
        assert!(
            img.iter().any(|s| s == host),
            "img-src must allow `{host}` for the Windows protocol form: {img:?}"
        );
    }

    // Videos are served by the loopback HTTP server in src-tauri/src/protocol.rs, because
    // WebKitGTK decodes <video> through GStreamer, which cannot read custom URI schemes.
    // The port is chosen at bind time, hence the wildcard.
    let media = sources("media-src");
    assert!(
        media.iter().any(|s| s == "http://127.0.0.1:*"),
        "media-src must allow the loopback video server or <video> stops playing: {media:?}"
    );
}

#[test]
fn external_module_entrypoints_stay_importable() {
    // host.ts imports an external module's entrypoint through `convertFileSrc`, i.e. the
    // asset protocol, whose scope is `$DATA/chairphoto/modules/**` (see assetProtocol
    // below). A module script blocked here would break external modules entirely.
    let script = sources("script-src");
    assert!(
        script.iter().any(|s| s == "asset:"),
        "script-src must allow `asset:` or no external module can be imported: {script:?}"
    );
    assert!(
        script.iter().any(|s| s == "http://asset.localhost"),
        "script-src must allow `http://asset.localhost` for the Windows asset form: {script:?}"
    );
    assert!(
        !script.iter().any(|s| s == "'unsafe-eval'"),
        "script-src must not allow 'unsafe-eval': {script:?}"
    );

    // The asset protocol is what makes that import resolvable, and its scope is what keeps
    // it to the modules directory.
    let config: Value = serde_json::from_str(CONFIG).unwrap();
    let asset = &config["app"]["security"]["assetProtocol"];
    assert_eq!(asset["enable"], Value::Bool(true));
    assert_eq!(
        asset["scope"],
        serde_json::json!(["$DATA/chairphoto/modules/**"]),
        "the asset protocol scope must stay pinned to the modules directory"
    );
}
