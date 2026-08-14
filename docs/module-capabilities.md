---
title: Module capabilities
description: "What a third-party module can do today, what stops it, and the changes that remove those limits."
---

# Module capabilities

`docs/plugin-system.md` describes the module contract as it exists. This document covers the
gap between that contract and a module system a third party can actually build against, and
the pieces of work that close it.

Status: all four steps are implemented and are described in the past tense below — the
rendering ABI (#46), the CSP lockdown (#47), per-module permissions (#48), and the first
host-mediated capability, `api.fetch` (#49). The capabilities beside `api.fetch` in
[The capability set](#the-capability-set) — reading photo bytes in particular — are not.

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

A module composed commands that already existed. There were no generic primitives — no HTTP
command, no file-bytes read — so a module that wanted to talk to a service ChairPhoto had
never heard of had no route to it, and would have needed a Rust backend compiled into the app.
The HTTP half of that is now closed by [`api.fetch`](#host-mediated-apifetch-49); the
file-bytes half is not.

That is why Flickr and SmugMug are bundled rather than external. Flickr is ~1500 lines of Rust
(OAuth 1.0a signing, multipart upload, REST calls) and ~570 lines of TSX. Its Rust half cannot
be loaded at runtime — `plugin-system.md` states backends are compiled in with no safe
hot-loading — and its TSX half would have had no way to draw before the rendering ABI landed.

### The security shape this has to fit

Two facts constrain any answer.

**The host API is already a real chokepoint.** Because `withGlobalTauri` is off and there is
no import map, an external module cannot reach Tauri directly. The `api` object passed to
`onLoad` is the only route to the backend. The "unrestricted `api.invoke`" described in
`plugin-system.md` was a policy choice in `host.ts`, not a technical necessity — the host
could gate it per module whenever it decided to, and now does; see
[Per-module permissions](#per-module-permissions). Most plugin systems never get this
boundary; here it exists by construction.

**Network used not to be gated at all.** `tauri.conf.json` set `"csp": null`, so there was no
Content-Security-Policy and an installed module could `fetch()` any CORS-permitting endpoint
directly, never touching the host API. Measured against the binding invariant that nothing
leaves the machine without an explicit, feature-specific opt-in, that was a live gap, not a
theoretical one. The trust model in `plugin-system.md` acknowledges that installed modules are
fully trusted, but it describes the risk as modules "calling network-capable backend
commands" — the actual exposure needed no backend command at all. That is now closed; see
[The policy](#the-policy).

Together the two facts supply the design. Close the direct route, then mediate it:

```
CSP: connect-src 'self' ipc:      direct fetch() is blocked          — done
manifest "permissions": {...}     declared per module                — done
host gates api.invoke per module  least privilege, enforceable because api is the only route
api.fetch(url, init)              proxied through Rust, so CORS does not apply either — done
host gates api.fetch per module   the same gate, extended to the network capability — done
```

Locking the door is what makes selling keys meaningful. Without the CSP the permission list is
decoration. The lock went on first, deliberately, while nothing depended on the door being
open: tightening `connect-src` after `api.fetch` exists would break modules written against
the looser policy.

### The policy

`src-tauri/tauri.conf.json` → `app.security.csp`:

| Directive | Sources | Why |
|---|---|---|
| `default-src` | `'self'` | Baseline for anything not listed below. |
| `connect-src` | `'self' ipc: http://ipc.localhost` | **The lockdown.** `fetch`/`XHR`/`WebSocket`/`EventSource` may reach the app's own origin and the Tauri IPC bridge, nothing else. Tauri's bridge is literally `fetch(convertFileSrc(cmd, "ipc"))` — `ipc://localhost/<cmd>` on Linux/macOS, `http://ipc.localhost/<cmd>` on Windows — so both forms are required for commands to work at all. |
| `script-src` | `'self' asset: http://asset.localhost` | The app bundle, plus external module entrypoints, which `host.ts` imports through `convertFileSrc` (the asset protocol, scoped to `$DATA/chairphoto/modules/**`). No `'unsafe-eval'`. |
| `style-src` | `'self' 'unsafe-inline'` | The bundled stylesheet, plus the pre-paint `<style>` in `index.html`. Tauri rewrites that tag with a nonce at build time and appends the nonce here, which makes browsers ignore `'unsafe-inline'`; it is kept as the fallback for a build where no nonce is injected. React sets styles through CSSOM (`node.style`), which CSP does not govern either way. |
| `img-src` | `'self' data: blob: asset: thumb: preview: zoom:` + the `http://<scheme>.localhost` forms + `https:` + loopback | The three native media protocols (`src-tauri/src/lib.rs`), edit renderer output (data/blob URLs), and map tiles. Tile URLs are a user setting (`map.tileUrl`), so this cannot be pinned to one host. |
| `media-src` | `'self' data: blob: http://127.0.0.1:* http://localhost:*` | `<video>` is served by the loopback HTTP server in `src-tauri/src/protocol.rs` — WebKitGTK decodes video through GStreamer, which cannot read custom URI schemes. The port is chosen at bind time, hence the wildcard. |
| `font-src` | `'self' data:` | Bundled Inter woff2. |
| `object-src` | `'none'` | No `<object>`/`<embed>` anywhere. |

`src-tauri/tests/csp.rs` pins the properties (not the exact text): a CSP exists, `connect-src`
carries the IPC bridge and nothing remote, the media protocols and the module asset path stay
loadable, and the asset-protocol scope stays on the modules directory.

### What it stops, and what it does not

Stops:

- A module calling `fetch`, `XMLHttpRequest`, `WebSocket` or `EventSource` against any
  origin other than the app itself. It has to go through `api.invoke` — i.e. through Rust,
  where the host can see and (later) gate it.
- Loading and running remote script.

Does **not** stop:

- **Egress through `img-src`.** A module can encode data in a URL and set it as an image
  source on any `https:` host; the response is discarded but the request is made. Closing
  that would break user-configured map tiles, and `connect-src` is where the issue's stated
  gap lives. This is a known residual channel, not an oversight.
- **Anything a backend command already does.** The CSP itself does not care which command a
  module invokes, so a module could reach the network by asking a network-capable command
  to. That is what step 3 addressed — `api.invoke` is now gated per module, so a module can
  only reach the commands it declared and the user approved; see
  [Per-module permissions](#per-module-permissions). The CSP remains the half that needed no
  new machinery.
- **Anything outside the webview.** The CSP governs the page, not the process.
- **Anything in `tauri dev`.** On desktop, `tauri dev` points the webview at the Vite dev
  server (`build.devUrl`), and Tauri only attaches the CSP header to assets it serves itself
  over `tauri://localhost` (`get_asset` in `tauri`'s `manager/mod.rs`). So HMR is unaffected
  *because the policy is not in force there at all*. It applies to `tauri build` and
  `tauri build --debug`, which is where installed modules are a real concern.

## Per-module permissions

Step 3 (#48). `api.invoke` was `(command, args) => invoke(command, args)` — a raw
command-string pass-through, so any enabled module could call any Tauri command the app
registers, related to it or not. It is now gated on two things at once.

**Declared** comes from the module. A bundled module sets `permissions` on its
`ChairPhotoModule`; an external one sets it in `chairphoto-module.json`:

```jsonc
"permissions": {
  "commands": ["faces_for_photo", "faces_accept"],
  "origins": ["https://api.flickr.com"]
}
```

A struct rather than the flat string list this document originally sketched, which is what
let step 4 add `origins` beside `commands` without breaking manifests already on disk: a
pre-#49 manifest parses unchanged and declares no origins, i.e. reaches nothing. Matching in
both lists is exact string equality — there are no wildcards, so `"*"` is permission to
invoke a command literally named `*`. `origins` is documented under
[Host-mediated `api.fetch`](#host-mediated-apifetch-49); everything below about declaring,
granting, intersection and review applies to both fields at once.

**Granted** comes from the user, at the moment they enable the module, and is persisted
under the `modules.permissions` setting.

Enforcement is the **intersection**, `granted ∩ declared`. Trusting the grant alone would
leave a stale entry live after a module stopped asking for it; intersecting means a module
that narrows its declaration is held to the narrower set with no re-review, and one that
widens it is refused until reviewed again.

### Decision — reviewed at enable time, from the manifest

The open question this section replaces asked whether permissions are reviewed *at install
time from the manifest* or *prompted at first use*. There are really three moments, and the
first one does not exist:

1. **Install time** is not an event. A module is installed by copying a folder into
   `<app_data_dir>/modules/` — no installer, no marketplace, and discovery happens at the
   next startup. The app is never notified that an install happened.
2. **Enable time** is the first moment the app can address the user about a specific module,
   and is already a deliberate act: a toggle in Preferences → Modules.
3. **First use** is when the module calls `api.invoke("…")` for the first time.

**Enable time is the choice.** It is the manifest-review option the question named —
"install time" was shorthand for "before it runs", and in an app with no install step that
lands on enablement.

Against first use, on its merits rather than around them:

- **The prompt would arrive when "no" is not an available answer.** It fires in the middle of
  an operation the user just started: they clicked *Index faces*, and a dialog asks whether
  Faces may call `faces_index_photos`. Declining fails the action they asked for, halfway.
  That is the structure that trains click-through — a formality dressed as a decision.
- **Nothing is written to survive a denial mid-flow.** A refusal at an arbitrary `await`
  leaves a half-finished operation with no defined recovery. Deciding at enable time keeps a
  module either fully working or not running, which is the only state anyone tests.
- **The framing would be the module's, at a moment the module chose.** Enable-time review is
  the host's framing at a moment the *user* chose. For code this project explicitly calls
  untrusted, that asymmetry settles it.
- **It costs standing machinery.** A first-use gate is a permission dialog reachable from any
  `await` point inside untrusted code, with re-entrancy, queueing, and "don't ask again"
  state. The enable-time gate is a set-membership test against a set that is fixed for the
  module's whole session.

The real cost of choosing enable time, and what answers each part:

- **The user has no context yet for what the module does.** True, and the strongest argument
  for the alternative. `faces_index_photos` means little before you have used Faces. But
  timing does not supply context; wording does. What a first-use prompt adds is *situational*
  context — "you just clicked something" — and that is precisely what makes it a rubber
  stamp, because the user has already decided they want the action. Situational context
  raises the quality of consent only where "no" is a live option. The fix for "I don't know
  what this means" is to show the list beside the module's own description and keep it short
  and exact, not to move the decision to a worse moment.
- **A single decision point goes stale.** The classic failure of install-time review is a
  module quietly widening its permissions when it is updated. Closed by the intersection
  rule: a module that grows its declaration is refused and flagged for review again.
- **The decision disappears after it is made.** Answered by keeping the declared set visible
  in the module's row in Preferences → Modules — granted or not — and by
  `revokePermissions()`, which puts a module back to needing review.

**Grants are all-or-nothing per module.** There is no per-command checkbox: a module given
three of the five commands it needs would fail in ways nobody wrote or tested for.

This is the owner-overrulable decision in #48. Reversing it means keeping the manifest field
and the intersection rule and moving only *when* the grant is collected.

### How it is enforced

- **Identity is the closure's, not the caller's.** `apiFor(reg)` builds one API object per
  module and the gate reads the id from the registry entry — the same shape
  `recordPublication` uses one field up, where the host stamps the publication marker so a
  module never handles the platform string. A module never handles its own id either, so
  there is nothing for it to forge. Registering under another module's id does not help
  either: `register()` keeps the first entry for an id, and bundled modules are registered
  before external ones are loaded.
- **The id is read once, at registration.** `Registered.id` is a snapshot, and everything
  that decides what a module may do keys on it: the permission lookup, the settings
  namespace, the publication marker, and the id the Modules panel feeds back into
  `grantPermissions()`. This is not hypothetical tidiness. `module.id` is a property on an
  object a third party wrote, so it can be an accessor returning one value while the loader
  validates it against the manifest and another afterwards; resolving identity per call let
  a module that declared nothing invoke `post_to_flickr` under a module that had been
  granted it. Pinned by "cannot be re-pointed by a module whose `id` is an accessor" in
  `src/modules/__tests__/permissions.test.ts`.
- **The manifest wins for external modules.** `host.ts` takes an external module's
  permissions from the manifest and ignores `permissions` on the imported object. The
  manifest is what the backend parsed *without executing the module* and what the user
  reviewed, so a module cannot ship a modest manifest and widen itself from code. The
  declaration is snapshotted at registration, so mutating it afterwards changes nothing.
- **Refusals are visible to both audiences.** The module gets a rejected promise carrying a
  `ModulePermissionError` (module id + command); the user gets a toast naming both. The toast
  is deduped per module+command so a module retrying in a loop cannot bury the UI; the
  console line is not deduped.
- **Enablement fails closed.** `enableModule` refuses a module whose declaration is not fully
  granted. Ungranted permissions are deliberately *not* a `blockedReason` — a `blockedReason`
  disables the toggle in the Modules panel, and this is the one thing the toggle is supposed
  to let the user resolve.
- **Upgrades keep working.** An **absent** `modules.permissions` row means a pre-permissions
  install: modules already listed in `modules.enabled` are grandfathered to their declared
  set. That is not a widening — before the gate their `api.invoke` reached any command at
  all. A row that is *present but unreadable* is not treated the same way, or corrupting one
  setting would become a way to re-grant a module that had grown since it was last reviewed.

### Bundled modules are not exempt

All fourteen bundled modules that call `api.invoke` declare their command sets — 77 commands
in total, from `statistics` with one (`catalog_stats`) to `faces` with twenty-one.
`basic-editor` declares nothing because it invokes nothing: it renders through `renderEdit`,
a core wrapper.

Exempting them would have left the enforcement path untested by the code that actually
ships. It did not stay theoretical: the first run of the suite after the gate landed failed
on `statistics`, which is the gate working.

`src/modules/__tests__/bundledPermissions.test.ts` cross-checks every `api.invoke` call site
in `src/modules/plugins/` against what `BUNDLED_MODULES` declares, in both directions — an
undeclared call is a module that breaks in the user's hands, and a declared-but-uncalled
command is a permission the user was asked to approve for nothing.

### What this does not cover

- **Bundled modules are inside the trust boundary.** They are compiled into the app and can
  `import` from `modules/api.ts` directly, so for them the declaration documents their
  backend surface and exercises the gate; it does not confine them. The confinement is real
  for external modules, which reach the backend only through the injected `ChairPhotoAPI`.
- **The rest of the curated host API is not gated.** `listTags`, `assignTag`, `getSetting`,
  `getEditRecord`, `renderEdit` and the rest of `ChairPhotoAPI` are the host's own surface,
  deliberately available to every module. The permission list covers the *module-owned*
  command surface — the part that was a raw pass-through — plus `api.fetch`, which is gated by
  `permissions.origins` because it is a route off the machine rather than a curated operation
  on the user's own catalog.
- **`api.onEvent` is not gated.** A module can subscribe to any backend event. Events carry
  payloads but take no arguments and cause no action, so this is a read channel, not a
  capability. Gating it would want a second manifest field and is not part of #48.
- **Anything outside `api`.** The gate is a host-side policy in the webview, like the CSP; it
  governs what module code can ask the backend for, not what the process can do.

### The capability set

Enough for the modules that exist to have been written as JavaScript:

| Capability | Needed by | Notes |
|---|---|---|
| `api.fetch` | flickr, smugmug, map | **Exists** (#49) — host-proxied, so CORS does not apply. See [Host-mediated `api.fetch`](#host-mediated-apifetch-49) |
| read photo/export bytes | flickr, smugmug | Not built. Would be scoped to catalog roots and export outputs; never arbitrary paths |
| plugin storage | all | Exists — namespaced settings plus `<id>__*` tables |
| crypto | flickr, smugmug | Exists — WebCrypto does HMAC-SHA1 |

`api.fetch` was the dangerous one, and it is scoped by origin rather than granted wholesale.
Without "read photo bytes" the set is still short of what a JavaScript Flickr would need: a
module can talk to Flickr's API but cannot obtain a photo's bytes to upload, and `api.fetch`
takes a text body only. That is a deliberate stopping point, not an oversight — see
[What #49 does not do](#what-49-does-not-do).

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

## Host-mediated `api.fetch` (#49)

Step 4, and the first capability granted through the machinery step 3 built. A module that
needs to talk to a service ChairPhoto has never heard of now can:

```ts
if (!api.fetch) return;                       // optional member; absent on older hosts
const res = await api.fetch("https://api.flickr.com/services/rest?method=…", {
  method: "POST",
  headers: { Authorization: "OAuth …" },
  body: "…",
});
```

The request is made **by Rust** (`src-tauri/src/commands/net.rs`), which is what makes it
possible at all: `connect-src` (see [The policy](#the-policy)) stops module code opening its
own socket, and going through the host also means CORS never applies, because the request no
longer originates in a browsing context. That was the specific thing that forced Flickr's
upload into Rust in the first place.

### Destinations are per module, and the unit is the origin

`permissions.origins` is a list of full origins — `https://host` or `https://host:port` —
matched exactly after normalisation (lower-cased scheme and host, IDN punycoded, default port
dropped). It is not a "network: yes" switch, and it is not a host list either.

**Why the origin and not the host.** A host-granularity grant for `api.flickr.com` would also
be spendable on `http://api.flickr.com`: the same destination with the transport security
removed, so the data the user meant for Flickr also reaches everyone on the network path. The
scheme has to be part of the grant. The port comes along for the same reason it does in every
other same-origin decision on the web — `https://host:8443` is a different service.

**Why not a path prefix, which sounds tighter.** It is not a boundary. The path space is the
server's to define, any API worth granting has enough surface under one prefix to do whatever
the origin allows, a prefix breaks the moment the service adds an endpoint, and the pressure
that creates is to write `/` and be done. What actually decides whether data leaves for
somewhere the user did not agree to is *which machine receives it* — which is exactly what an
origin names, and nothing more. This document previously asked for scoping "by host"; an
origin is that plus the scheme and port qualifiers, so it is strictly tighter than what was
asked for.

**No wildcards.** `https://*.example.com` covers hosts that do not exist yet and can be
created at will by whoever runs the domain. Note that `new URL` parses that string quite
happily — `*` is not a forbidden host code point — so a declared origin also has to pass a
host-shape check, or a wildcard would read as a wildcard grant in the review dialog while
actually naming a host that cannot exist. A declaration carrying a path, query, fragment,
credentials, a trailing dot, or a non-`https` scheme is dropped on the fail-closed principle
step 3 established: junk narrows a declaration, it never widens one.

### What the grant authorises — say it plainly

**A granted origin is permission to send that origin arbitrary data.** `api.fetch` takes a
method, headers and a body; a module that can read photos or catalog rows through its other
granted commands can put them in that body. This is not a leak in the proxy, it is what the
capability *is*, and pretending otherwise would be the actual privacy failure. So:

- The permission is the opt-in the binding invariant requires, and it is
  feature-specific in the way that matters — it names the destination.
- The review dialog says the module "can **send data to**" each destination, and spells out
  that whatever it can read it can send there. A dialog reading "can connect to" would
  describe half the decision.
- The declared origins stay visible in the module's row in Preferences → Modules, granted or
  not, so the decision remains inspectable long after it was made.
- Network origins are **never grandfathered**. Command grandfathering is safe because before
  the gate an enabled module's `api.invoke` reached any command at all, so recording its
  declaration narrows it. The reverse holds here: before #49 no module could reach the network
  through the host, so writing origins into a grandfathered grant would manufacture consent to
  send data off the machine. A module upgraded across both changes keeps its commands and is
  sent through review for its origins.

### The invariants Rust holds regardless

The per-module allowlist lives in `src/modules/host.ts`, because the frontend host is the only
layer that knows *which* module is calling — the same boundary, with the same limits, as the
`api.invoke` gate. `commands/net.rs` separately enforces what any request from this app may
do, and no grant lifts these:

| Invariant | What it stops |
|---|---|
| `https:` only | A cleartext route to a granted host. |
| **No redirects followed** | The obvious bypass: a permitted host answering `302` to a non-permitted one. |
| Public unicast addresses only | A grant for `example.com` becoming a route to `127.0.0.1`, the LAN, or `169.254.169.254`. |
| The vetted addresses are the ones dialled | A second DNS answer rebinding the connection after the check. |
| No proxies | The vetting describing a host that is not the one contacted. |
| No cookie jar | Ambient authority — the thing that turns a fetch proxy into a confused deputy. |
| 8 MiB each way, 30 s | A module wedging the process or exhausting memory. |
| Method + header allowlists | `CONNECT`/`TRACE`, and request smuggling via module-set framing headers. |

Two of those are worth their own paragraph.

**Redirects.** They are not followed at all. A `3xx` comes back to the module as an ordinary
result with its `location` header intact, and the module may call `api.fetch` again with that
URL — which re-enters the origin gate like any other call. Following-with-a-recheck was the
alternative, and it is more machinery for a worse guarantee: every hop needs the origin check
*and* re-resolution *and* a hop limit, and it still has to decide what happens to the request
body and `Authorization` header on a cross-origin `307`, where the browser's answer is to
replay them. Making the module ask again keeps every hop inside its grant, and keeps it
visible.

**Local and private addresses.** The host is resolved before the request and *every* answer
must be a public unicast address — one loopback, private, carrier-NAT, link-local,
documentation, benchmarking, multicast or reserved answer refuses the whole request rather
than picking the good one, because a name resolving to both is either broken or attacking.
IPv6 forms that wrap an IPv4 address (v4-mapped, 6to4, NAT64) are judged on the address
inside, or `::ffff:127.0.0.1` walks straight through; the same unwrapping keeps DNS64 networks
working, since refusing `64:ff9b::/96` outright would break IPv6-only hosts reaching ordinary
sites. The connection is then pinned to the vetted addresses, so the resolver cannot answer
differently the second time. Certificate validation still uses the hostname, so pinning
weakens nothing about TLS.

### What the module sees of the response

Status, canonical status text, `ok`, the requested URL, the headers, and the body as text.

- **`set-cookie` is withheld.** The proxy keeps no cookie jar — `reqwest` is built without the
  `cookies` feature at all — so a cookie cannot be honoured by the host; handing the raw
  credential to module code, which runs unsandboxed in the app's page, would widen things for
  nothing. A module that needs authenticated calls sends its own `Authorization` header, whose
  value it already had. Cookie-session APIs are therefore not usable through `api.fetch`; that
  is a refusal, not an omission.
- **The body is text.** A response that is not valid UTF-8 is an error rather than lossily
  mangled text. This proxy is for JSON/XML/form-encoded protocols; binary responses are not
  supported.

### The one place the two halves meet

The host passes the origin it approved alongside the WHATWG-normalised URL, and `net.rs`
re-derives the origin with its own parser and refuses a mismatch. That is not circular
reasoning about a caller-supplied value: it catches a URL that the browser's parser and Rust's
read differently, which is otherwise a way to be granted one origin and reach another. What it
is *not* is a second trust boundary — see below.

### What #49 does not do

- **It is not a sandbox, and the origin gate is not enforced against a module that forges
  IPC.** Module code shares the page with the app; `docs/plugin-system.md` § "Trust model" has
  always said so. The origin allowlist is a host-side policy in the webview, exactly like the
  `api.invoke` gate and exactly like the CSP, and all three are least-privilege boundaries over
  the *designed* path rather than isolation. Passing the module's allowlist down to Rust and
  calling that enforcement would be theatre — the caller would be supplying the list it is
  checked against. The honest split is the one implemented: identity policy where identity is
  known, transport invariants where they can be held unconditionally.
- **No binary request bodies, so no photo upload.** `body` is text. Combined with the fact
  that "read photo bytes" is a separate capability that does not exist, a JavaScript Flickr is
  still not writable: a module can drive Flickr's REST API but cannot obtain or send a photo
  file. Both halves would have to land together to be worth anything, and the second one wants
  its own scoping design (catalog roots and export outputs, never arbitrary paths).
- **No proxy support.** A user behind a corporate HTTP proxy cannot use `api.fetch` at all,
  because honouring the proxy would mean the address vetting above describes a host that is
  never contacted. Refused rather than half-supported.
- **`img-src` egress is still open**, unchanged from #47: a module can encode data in a URL
  and set it as an image source on any `https:` host. That residual channel is why `api.fetch`
  being tightly scoped is a real improvement and not a complete one.
- **No streaming, no WebSocket, no `EventSource`.** One request, one buffered response.

## Order

1. ~~**Blocker 1**, in isolation. Modules can render; nothing about the security model
   changes.~~ Done — see [Rendering ABI](#rendering-abi-was-blocker-1--a-module-cannot-render).
2. ~~**CSP lockdown**, before any capability exists to grant. It is a breaking change for
   anything currently relying on direct `fetch`, so it goes first while nothing does.~~
   Done — see [The policy](#the-policy).
3. ~~**Per-module permissions** — manifest field, host enforcement in `api.invoke`, review
   surfaced at enable time.~~ Done — see
   [Per-module permissions](#per-module-permissions).
4. ~~**Capabilities**, one at a time, each gated by 3. `api.fetch` is #49.~~ `api.fetch` is
   done — see [Host-mediated `api.fetch`](#host-mediated-apifetch-49). The next capability in
   [the set](#the-capability-set), reading photo/export bytes, is not filed.

Steps 2–4 were one unit in practice: shipping a generic fetch primitive without the permission
gate would have widened the hole this is meant to close. Step 2 shipped on its own because it
is only ever harder later, and because it stands alone: it needs no new machinery and grants
nothing.

## Open questions

- Do system-installed modules stay per-user toggleable, or does system-wide mean always on?
  `get_modules_dir()` currently returns a single user-scoped path, so a distro package cannot
  ship a module at all today.
- Should `api.onEvent` be gated too? It is a read channel — no arguments, no action — so #48
  left it open, but a module subscribing to another module's job events is still a leak of
  sorts.
- ~~Are permissions reviewed at install time from the manifest, or prompted at first use?~~
  Answered: at enable time, from the manifest, with the reasoning in
  [Decision — reviewed at enable time](#decision--reviewed-at-enable-time-from-the-manifest).
- ~~Does `ReactNode` stay for bundled modules if external modules move to `mount()`, and is
  carrying two contribution paths acceptable?~~ Answered: yes to both, see
  [Both paths coexist](#both-paths-coexist-and-which-is-which).
