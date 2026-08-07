---
title: Module capabilities
description: "What a third-party module can do today, what stops it, and the two changes that would remove those limits."
---

# Module capabilities

`docs/plugin-system.md` describes the module contract as it exists. This document covers the
gap between that contract and a module system a third party can actually build against, and
the two pieces of work that close it.

It is a design document. Nothing here is implemented.

## What already works

The external-module path is built and functioning, not planned. Verified end to end on
2026-08-06 by installing a hand-written module into `<app_data_dir>/modules/hello/` and
launching the app against an isolated profile: the backend discovered its manifest, the host
checked `minHostVersion`, resolved the entrypoint through `convertFileSrc`, imported it,
validated the default export, registered it, enabled it from persisted settings, and ran
`onLoad`, which wrote through `api.setSetting` into the catalog. The write landed under the
namespaced key `hello.loaded_at`, so per-module setting isolation works too.

So discovery, versioning, requirements, registration, enablement, lifecycle, and the settings
API are all real. A third party cannot use them for anything substantial, for two reasons.

## Blocker 1 — a module cannot render

Every UI contribution point returns `ReactNode`:

| Contribution | Declared in |
|---|---|
| `ModulePanel.render()` | `src/modules/registry.ts:88` |
| `MainView.icon` / `.render()` | `:124`, `:125` |
| `ToolbarAction.render()` | `:141` |
| publish target / edit renderer | `:153` |
| `registerSettingsPanel()` | `:205` |

A bundled module satisfies that by being compiled with the app and sharing its React
instance. An external module is imported straight from disk and has no route to React:

- `withGlobalTauri` is off, so there is no `window.React` or any other global to reach for;
- there is no import map, so `import React from "react"` cannot resolve — bare specifiers
  have no resolver in a browser;
- Vite has no `external` config, so React is bundled inside the app and unreachable;
- bundling its own copy would not work either, because two React instances break hooks and
  context.

`ReactNode` includes strings, so an external module can return bare text. That is the entire
ceiling. The example module in `examples/modules/hello/` returns a string for exactly this
reason, not as a simplification.

### Options

**Expose the host's React on a global.** Around ten lines: publish `React` on a namespaced
global at startup, and modules destructure it. Same instance, so hooks and context work.
Immediate and effective; it also makes React's version part of the module ABI forever.

**Ship an import map** so modules write `import React from "react"` normally. Better to
author against, more plumbing — React has to be served from a stable URL — and it has the
same ABI-coupling problem.

**Make the ABI framework-agnostic.** Contribution points hand the module a DOM element and a
`mount(el)` / `unmount()` pair instead of asking for a `ReactNode`. The module owns what it
renders and how. This is the option to take if third-party modules are a long-term goal: a
plugin ABI coupled to the host's UI framework means a React major upgrade breaks every
third-party module ever written, and their authors — not you — have to fix them. It is also
the largest change, and bundled modules must keep working across it, which probably means
both paths coexist: `ReactNode` for bundled, `mount()` for external.

Nothing here is decided.

## Blocker 2 — a module cannot reach a capability the host lacks

A module composes commands that already exist. There are no generic primitives — no HTTP
command, no file-bytes read. A module that wants to talk to a service ChairPhoto has never
heard of has no route to it, and would need a Rust backend compiled into the app.

That is why Flickr and SmugMug are bundled rather than external. Flickr is ~1500 lines of Rust
(OAuth 1.0a signing, multipart upload, REST calls) and ~570 lines of TSX. Its Rust half cannot
be loaded at runtime — `plugin-system.md` states backends are compiled in with no safe
hot-loading — and its TSX half would hit Blocker 1 anyway.

### The security shape this has to fit

Two facts constrain any answer.

**The host API is already a real chokepoint.** Because `withGlobalTauri` is off and there is
no import map, an external module cannot reach Tauri directly. The `api` object passed to
`onLoad` is the only route to the backend. The "unrestricted `api.invoke`" described in
`plugin-system.md` is a policy choice in `host.ts`, not a technical necessity — the host can
gate it per module whenever it decides to. Most plugin systems never get this boundary; here
it exists by construction.

**Network, however, is not gated at all.** `tauri.conf.json` sets `"csp": null`, so there is
no Content-Security-Policy and an installed module can `fetch()` any CORS-permitting endpoint
directly, never touching the host API. Measured against the binding invariant that nothing
leaves the machine without an explicit, feature-specific opt-in, that is a live gap, not a
theoretical one. The trust model in `plugin-system.md` acknowledges that installed modules are
fully trusted, but it describes the risk as modules "calling network-capable backend
commands" — the actual exposure needs no backend command at all.

That second fact also supplies the design. Close the direct route, then mediate it:

```
CSP: connect-src 'self' ipc:      direct fetch() is blocked
api.fetch(url, init)              proxied through Rust, so CORS does not apply either
manifest "permissions": [...]     declared per module
host gates api.fetch per module   least privilege, enforceable because api is the only route
```

Locking the door is what makes selling keys meaningful. Without the CSP the permission list is
decoration.

### The capability set

Enough for the modules that exist to have been written as JavaScript:

| Capability | Needed by | Notes |
|---|---|---|
| `api.fetch` | flickr, smugmug, map | Host-proxied; sidesteps CORS, which is why Flickr's upload had to be Rust |
| read photo/export bytes | flickr, smugmug | Scoped to catalog roots and export outputs; never arbitrary paths |
| plugin storage | all | Exists — namespaced settings plus `<id>__*` tables |
| crypto | flickr, smugmug | Exists — WebCrypto does HMAC-SHA1 |

`api.fetch` is the dangerous one and should be scoped by host, not granted wholesale.

### What this does not reach

Roughly half the current modules cannot become JavaScript at any point on this path:

| Module | Blocker |
|---|---|
| localsend | UDP multicast — impossible from a webview |
| instagram | drives Chrome over the DevTools Protocol |
| faces, smarttags | ONNX inference |
| edit, raw | full-resolution RAW decode through LibRaw |
| slideshow | spawns `ffmpeg` |

Making *those* loadable needs a third tier — modules shipping native helper binaries the host
speaks to over a stable protocol — which is a much larger undertaking and is not covered here.
The alternative is that the core keeps those capabilities and exposes them, so modules stay
JavaScript and orchestrate rather than implement. That is the VS Code shape, and it is
cheaper, but it means the core stays capability-rich even though its user-facing surface is
just the grid and tagging.

## Order

1. **Blocker 1**, in isolation. Modules can render; nothing about the security model changes.
2. **CSP lockdown**, before any capability exists to grant. It is a breaking change for
   anything currently relying on direct `fetch`, so it goes first while nothing does.
3. **Per-module permissions** — manifest field, host enforcement in `api.invoke` and
   `api.fetch`, review surfaced at enable time.
4. **Capabilities**, one at a time, each gated by 3.

Steps 2–4 are one unit in practice: shipping a generic fetch primitive without the permission
gate would widen the hole this is meant to close.

## Open questions

- Do system-installed modules stay per-user toggleable, or does system-wide mean always on?
  `get_modules_dir()` currently returns a single user-scoped path, so a distro package cannot
  ship a module at all today.
- Are permissions reviewed at install time from the manifest, or prompted at first use?
- Does `ReactNode` stay for bundled modules if external modules move to `mount()`, and is
  carrying two contribution paths acceptable?
