---
title: Module capabilities
description: "What a third-party module can do today, what stops it, and the changes that remove those limits."
---

# Module capabilities

`docs/plugin-system.md` describes the module contract as it exists. This document covers the
gap between that contract and a module system a third party can actually build against, and
the pieces of work that close it.

Status: step 1 (the rendering ABI, #46) is implemented and is described in the past tense
below. Steps 2–4 — the CSP lockdown, per-module permissions, and host-mediated capabilities —
are still design, and are marked as such.

## What already works

The external-module path is built and functioning, not planned. Verified end to end on
2026-08-06 by installing a hand-written module into `<app_data_dir>/modules/hello/` and
launching the app against an isolated profile: the backend discovered its manifest, the host
checked `minHostVersion`, resolved the entrypoint through `convertFileSrc`, imported it,
validated the default export, registered it, enabled it from persisted settings, and ran
`onLoad`, which wrote through `api.setSetting` into the catalog. The write landed under the
namespaced key `hello.loaded_at`, so per-module setting isolation works too.

So discovery, versioning, requirements, registration, enablement, lifecycle, and the settings
API are all real. What used to stop a third party using them for anything substantial was two
things: a module could not draw, and it could not reach a capability the host lacks. The first
is fixed; the second is not.

## Rendering ABI (was: blocker 1 — a module cannot render)

Every UI contribution point used to return `ReactNode` and nothing else:

| Contribution | Declared in |
|---|---|
| `ModulePanel` | `src/modules/registry.ts` |
| `MainView` | same |
| `ToolbarAction` | same |
| `PublishTarget` / `EditRenderer` | same |
| `registerSettingsPanel()` | same |

A bundled module satisfies that by being compiled with the app and sharing its React
instance. An external module is imported straight from disk and has no route to React:

- `withGlobalTauri` is off, so there is no `window.React` or any other global to reach for;
- there is no import map, so `import React from "react"` cannot resolve — bare specifiers
  have no resolver in a browser;
- Vite has no `external` config, so React is bundled inside the app and unreachable;
- bundling its own copy would not work either, because two React instances break hooks and
  context.

`ReactNode` includes strings, so an external module could return bare text. That was the
entire ceiling.

### Options that were considered

**Expose the host's React on a global.** Around ten lines: publish `React` on a namespaced
global at startup, and modules destructure it. Same instance, so hooks and context work.
Immediate and effective; it also makes React's version part of the module ABI forever.

**Ship an import map** so modules write `import React from "react"` normally. Better to
author against, more plumbing — React has to be served from a stable URL — and it has the
same ABI-coupling problem.

**Make the ABI framework-agnostic.** Contribution points hand the module a DOM element and a
`mount(el)` / `unmount()` pair instead of asking for a `ReactNode`. The module owns what it
renders and how.

### Decision — `mount(el)` / `unmount(el)`

The third option was taken. Both cheaper options put React's version in the ABI: a React
major upgrade would break every third-party module ever written, and their authors — not
this project — would have to fix them. A module system whose stability promise is
"additive-only within a host major" (`docs/plugin-system.md`) cannot honour that promise while
its rendering contract is a re-export of somebody else's framework. `mount(el)` costs more
once, here, instead of costing every module author later. Nothing in the ABI names React, so
the host is free to change or replace its UI framework without touching the contract.

The contract is `ModuleMount` in `src/modules/registry.ts`:

```ts
interface ModuleMount {
  mount?(el: HTMLElement): void;
  unmount?(el: HTMLElement): void;
}
```

`ModulePanel`, `MainView`, `PublishTarget` and `ToolbarAction` each extend it, `render()`
is now optional on all of them, and `registerSettingsPanel` accepts a React thunk *or* a
`ModuleMount`. `ToolbarAction` is the one contribution whose draw call takes an argument, so
its DOM form is `mount(el, close)`, mirroring `render(close)`.

Rules:

- Provide exactly one path. If a contribution has both, **`render` wins**, so a bundled
  module is never routed through the DOM adapter.
- `unmount(el)` releases what lives *outside* `el` (listeners on `document`, timers,
  observers). The host empties `el` itself, which is what makes `mount` safe to call again.
- The pair can run more than once for one contribution: React StrictMode mounts, unmounts and
  remounts every effect in development. `mount` must build from scratch.
- Both calls are wrapped in try/catch by the host, for the same reason `onLoad` is: an
  installed module is untrusted code, and a throw from it must not take down the panel
  rendering it.

### Both paths coexist, and which is which

Bundled modules keep using `render(): ReactNode`. They are compiled with the app, share its
React instance, and rewriting ~20k lines of first-party TSX to DOM calls would buy nothing —
the ABI-coupling argument does not apply to code that ships in the same binary as the
framework. External modules use `mount()`.

Carrying two paths is acceptable because the second one is nine lines of type and one adapter
component (`src/modules/ModuleContent.tsx`), and every slot goes through that single adapter:
`ModuleContent` picks the path, so no call site knows there are two. The React path is a
pass-through that emits no DOM of its own, so a bundled module's output lands exactly where
it did before the adapter existed — asserted in
`src/modules/__tests__/moduleContent.test.tsx`.

This answers the open question below ("Does `ReactNode` stay for bundled modules…"): yes, and
yes.

### Worked example

`examples/modules/hello/` is an external module that renders into the `inspector` panel slot:
plain JavaScript, no React import, no build step, loaded from
`<app_data_dir>/modules/hello/`. It shows the full shape of a stateful external panel —
`mount(el)` builds DOM and remembers the element, `unmount(el)` forgets it, and
`onPhotoSelected` repaints every element still mounted, which is how an external module
reacts to host state without a React render loop.

The last case in `src/modules/__tests__/moduleContent.test.tsx` dynamically imports that same
file — the shipped one, not a copy — registers it through the real host, renders it through
the real adapter, and drives `setSelection()` to prove the repaint.

## Blocker 2 — a module cannot reach a capability the host lacks

A module composes commands that already exist. There are no generic primitives — no HTTP
command, no file-bytes read. A module that wants to talk to a service ChairPhoto has never
heard of has no route to it, and would need a Rust backend compiled into the app.

That is why Flickr and SmugMug are bundled rather than external. Flickr is ~1500 lines of Rust
(OAuth 1.0a signing, multipart upload, REST calls) and ~570 lines of TSX. Its Rust half cannot
be loaded at runtime — `plugin-system.md` states backends are compiled in with no safe
hot-loading — and its TSX half would have had no way to draw before the rendering ABI landed.

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

1. ~~**Blocker 1**, in isolation. Modules can render; nothing about the security model
   changes.~~ Done — see [Rendering ABI](#rendering-abi-was-blocker-1--a-module-cannot-render).
2. **CSP lockdown**, before any capability exists to grant. It is a breaking change for
   anything currently relying on direct `fetch`, so it goes first while nothing does.
3. **Per-module permissions** — manifest field, host enforcement in `api.invoke` and
   `api.fetch`, review surfaced at enable time. *Deferred by owner decision.*
4. **Capabilities**, one at a time, each gated by 3. *Deferred by owner decision.*

Steps 2–4 are one unit in practice: shipping a generic fetch primitive without the permission
gate would widen the hole this is meant to close.

## Open questions

- Do system-installed modules stay per-user toggleable, or does system-wide mean always on?
  `get_modules_dir()` currently returns a single user-scoped path, so a distro package cannot
  ship a module at all today.
- Are permissions reviewed at install time from the manifest, or prompted at first use?
- ~~Does `ReactNode` stay for bundled modules if external modules move to `mount()`, and is
  carrying two contribution paths acceptable?~~ Answered: yes to both, see
  [Both paths coexist](#both-paths-coexist-and-which-is-which).
