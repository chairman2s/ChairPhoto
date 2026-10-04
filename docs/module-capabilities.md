---
title: Module capabilities
description: "What a module can reach today, now that modules are compiled-in Rust."
---

# Module capabilities

`docs/plugin-system.md` describes the module contract as it exists: first-party Rust,
compiled into the app through the `Module` trait. There is no third-party or externally
loaded module today, so there is no capability gap between "what the host exposes" and "what
a module could do" of the kind this document used to track — a module's Rust code can call
anything the rest of `crates/app`/`crates/core` can, because it is the rest of the program.

## History

Before the GPUI cutover (#165), ChairPhoto's React/Tauri front end supported a true runtime
plugin tier: external modules were plain JavaScript loaded from disk at startup. This
document used to track the four pieces of work that made that tier capable of doing anything
substantial — a rendering ABI so a module could draw without the host's React instance
(`mount(el)`/`unmount(el)`), a Content-Security-Policy lockdown (`connect-src` pinned to the
app's own origin and the Tauri IPC bridge), per-module command/origin permissions reviewed at
enable time, and a host-mediated `api.fetch` as the first capability granted through that
gate. All four had shipped and were described here in the past tense.

That whole tier — the external loader, the manifest (`chairphoto-module.json`), the install
directory, the CSP, `api.invoke`/`api.fetch` gating, and the security reasoning behind each
(origin-vs-host scoping, redirect handling, private-address blocking, and so on) — existed
specifically to let **untrusted** JavaScript, never compiled by this project, run inside a
webview safely. The `Module` trait that replaced the JS host (#104, at the GPUI cutover #165)
is for Rust **this project compiles**; see `docs/plugin-system.md` § "Why no third-party
modules" for what was dropped and why none of that machinery carries over.

The detailed design reasoning this document used to hold — why an origin is the right grant
unit, why redirects are never followed, why permissions are reviewed at enable time rather
than first use — applied to a CSP and an `api.fetch` proxy that no longer exist, so it is not
reproduced here. It is recoverable from version control (this file, before the #166 rewrite)
if a future sandboxed extension host is ever designed and needs the history.

## If a sandboxed extension host returns

The `Module` trait is deliberately object-safe and metadata-first — "nothing assumes the
implementor is compiled in" (`crates/app/src/modules/mod.rs`) — so a future host for untrusted
code is not foreclosed. It would need its own trust model for whatever it hosts (most likely
not JavaScript in a webview, since GPUI has none), and this document would be the place to
design and track it again.
