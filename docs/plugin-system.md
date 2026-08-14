---
title: "Plugin / Module System"
description: "The module and host-API contract: slots, stability rules, and external modules."
tags:
  - chairphoto/core
  - chairphoto/platform
aliases:
  - "Modules"
  - "Host API"
---

# Plugin / Module System

ChairPhoto's features ship as modules over a small, stable host API. First-party modules
(AI Tagging, Faces, Map, Editing and others) use the same contract as external ones, so
the API is exercised by real consumers rather than designed in the abstract. External,
user-installed modules load from disk under the trust model described below.

## Goals

- Optional features live as **modules**, separable from a lean core.
- A small, stable **host API** is the only surface a module touches (AGENTS.md:
  "modules call the host API only").
- LLM- and human-friendly: a module is one explicit object; the contract fits on a
  screen; a working reference module (AI tagging) shows how.

## Host-API stability contract

> **TL;DR for module authors:** `ChairPhotoAPI` and `ChairPhotoModule` are stable within
> a host major version. Set your `minHostVersion` to the lowest ChairPhoto release whose
> API you use; we guarantee nothing will break until the next major.

The `ChairPhotoAPI` / `ChairPhotoModule` contract follows **additive-only stability within a
major version**:

- **Within a `MAJOR` version** (e.g. `0.x`, `1.x`): we only *add* new optional
  methods or slot types to the API. We never remove, rename, or change the semantics of
  an existing member. A module that compiled against `0.1.0` keeps working unchanged
  against `0.9.0`; a module against `1.0.0` keeps working against `1.y.z` for any `y` and `z`.
- **Across a `MAJOR` bump** (e.g. `0.x → 1.0`): the contract may change in any way.
  Modules must be updated; the `minHostVersion` floor in the manifest ensures the old
  module refuses to load against an incompatible host rather than misbehaving silently.

**Enforcement:** `hostSatisfies(appVersion, minHostVersion)` in `host.ts` checks the
floor. If the running host is older than `minHostVersion`, the module is refused — its code
is never executed, and a `blockedReason` appears in the Modules panel ("requires host
≥ X.Y.Z, this is A.B.C"). An unknown host version (empty string) conservatively treats any
non-empty floor as unmet. Unit tests in `src/modules/__tests__/host.test.ts`.

**Practical guidance for module authors:**
- Use `minHostVersion` equal to the oldest host release whose API member(s) you call.
- Do not rely on API members beyond `ChairPhotoAPI` / `ChairPhotoModule` (i.e. never
  reach into app internals or call `window.__TAURI__` directly — the host API is the
  only stable surface).
- Optional API members added in later hosts will simply be `undefined` on older hosts;
  check before calling if you want to support a range of host versions.

### Backend events — `onEvent`

A module that needs to observe a backend event (typically its own progress stream,
`"<id>:progress"`) uses `ChairPhotoAPI.onEvent`. It must **not** import Tauri's `listen`:
that reaches past the contract, and no Tauri type may appear in the module surface.

```ts
export type Unsubscribe =  => void;

onEvent?<T>(event: string, handler: (payload: T) => void): Promise<Unsubscribe>;
```

The host adapts Tauri's `listen` in `host.ts`: it unwraps `event.payload` before calling
the handler, so a module receives the payload itself rather than the `{ payload }`
envelope, and it returns a contract-owned `Unsubscribe` rather than Tauri's `UnlistenFn`.

`onEvent` is **optional**, per the additive-stability rule — it is `undefined` on hosts
older than the release that introduced it. Check before calling, or set `minHostVersion`
to that release. Typical use:

```ts
useEffect( => {
  if (!api.onEvent) return;              // older host: no event support
  const sub = api.onEvent<Progress>("mymodule:progress", setProgress);
 return  => void sub.then((stop) => stop).catch( => {});
}, [api]);
```

Covered by the `ChairPhotoAPI.onEvent` tests in `src/modules/__tests__/host.test.ts`.

## Two layers, two kinds of "optional"

- **Frontend (TS/React)** — a true runtime plugin. A module registers UI
  contributions (panels, actions, settings) and can be **enabled/disabled at runtime**.
- **Backend (Rust)** — compiled in (no safe hot-loading), made optional via a
  **Cargo feature** per module (compile it out entirely) **and** a runtime toggle.

The host loads **bundled** modules (shipped in the app, enabled/disabled by the user).
The same contract covers third-party/external modules; that is an extension,
not a redesign.

## Frontend host

A module implements `ChairPhotoModule`:

```ts
interface ChairPhotoModule {
  id: string; name: string; version: string;
  // declares the Cargo feature its backend needs, if any (e.g. "ai").
  backendFeature?: string;
  onLoad(api: ChairPhotoAPI): void;     // register contributions here
 onUnload?: void;
  onPhotoSelected?(photos: Photo[], api: ChairPhotoAPI): void;
}
```

The **host API** is the only thing a module may use:

```ts
interface ChairPhotoAPI {
  // selection & data
 getSelectedPhotos: Photo[];
 getActivePhotoId: number | null;
 listTags: Promise<TagWithCount[]>;
  assignTag(photoId, tagId): Promise<void>;
  // backend
  invoke<T>(command: string, args?): Promise<T>;     // gated to the module's feature
  // module-scoped settings (namespaced in the settings table)
  getSetting(key): Promise<string | null>;
  setSetting(key, value): Promise<void>;
  // UI contributions
  registerPanel(panel: ModulePanel): void;           // slot: 'inspector' | 'sidebar' | 'loupe'
  registerAction(action: ToolbarAction): void;
  registerSettingsPanel(panel: SettingsPanel): void; // React thunk or ModuleMount
  showToast(message): void;
}
```

**Mount points (slots).** The app renders enabled modules' contributions at fixed
slots — initially an `inspector` section list (below the built-in inspector) and a
`settings` area (the Modules panel). `sidebar`/`loupe` slots come as needed. A module
can also contribute a **full-surface main view** via `registerMainView`:
the topbar shows a view switcher (built-in *Library* + each module view), and the
selected view replaces the central grid/loupe. The Map module uses this.

**How a contribution draws — `render()` or `mount()`.** Every contribution point accepts two
forms, and the host picks between them in one adapter (`src/modules/ModuleContent.tsx`) that
every slot renders through:

```ts
render?(): ReactNode;                 // bundled modules — they share the app's React
mount?(el: HTMLElement): void;        // external modules — a DOM handle, no framework
unmount?(el: HTMLElement): void;
```

- **Bundled** modules use `render()`. They are compiled with the app and share its React
  instance, so a `ReactNode` is the natural thing for them to produce.
- **External** modules use `mount()`. They are imported straight from disk with no route to
  the host's React instance, and nothing in `ModuleMount` names React — so a host-side React
  upgrade cannot break a module already written. That is why the framework-agnostic form
  exists; `docs/module-capabilities.md` § "Rendering ABI" records the decision and its trade.
- Supply **exactly one**. If a contribution has both, `render` wins.
- `unmount(el)` releases what lives outside `el` — listeners on `document`, timers,
  observers. The host empties `el` itself, so DOM inside it needs no cleanup, and `mount` can
  therefore be called again safely (React StrictMode does exactly that in development).
- Both calls run inside the host's try/catch, like `onLoad`: a throw from an installed module
  is logged and contained, never propagated into the slot rendering it.

`ToolbarAction` is the one contribution whose draw call takes an argument, so its DOM form is
`mount(el, close)`, mirroring `render(close)`. `examples/modules/hello/` is a worked external
module using this path; `src/modules/__tests__/moduleContent.test.tsx` covers both paths,
including that the React path adds no DOM of its own.

**Lifecycle.** At startup the app imports all bundled modules and registers them in a
registry. For each **enabled** module it calls `onLoad(api)`; the module registers its
panels/actions. Disabling calls `onUnload` and drops its contributions. The enabled
set is persisted in the `settings` table under `modules.enabled`.

**Registry runtime** (`src/modules/host.ts`): holds registered modules, the enabled
set, and the collected contributions; exposes `enable(id)`, `disable(id)`,
`getInspectorPanels`, etc. `App` subscribes and re-renders contributions.

## Backend host

- Each backend-bearing module gets a **Cargo feature** (e.g. `ai`). Its Rust lives in
  its own module (`src-tauri/src/plugins/<name>/`), and its commands are registered
  conditionally: `#[cfg(feature = "ai")] commands::ai_suggest, …`.
- Default feature set decides what ships; a build can omit a feature to exclude it.
- The frontend asks the backend which features are compiled in (a `plugin_features`
  command returning the list); a module whose `backendFeature` is absent shows
  "backend not included in this build" and stays inert.
- Runtime toggle: even when compiled in, a module does nothing until enabled.

## A module's settings

Stored in the `settings` table, namespaced by module id (e.g. `ai.provider`,
`ai.ollama_url`). The host API's `getSetting`/`setSetting` auto-namespace so a module
can't read another's keys. Requires exposing `get_setting`/`set_setting` Tauri
commands (backed by the existing `Catalog` settings KV).

## Plugin data storage (tables)

Plugins that need more than small settings own their **own tables**, kept out of the
core schema so core stays feature-agnostic and removing a plugin never touches core
data:

- Each plugin table is named `<plugin_id>__<name>` (double-underscore prefix), so two
  plugins can't collide (e.g. `ai__suggestions`).
- The plugin creates its tables lazily with `CREATE TABLE IF NOT EXISTS` the first
  time it needs them — no core migration involved.
- In-crate (bundled) plugins access the database through a `pub(crate)` accessor on
  `Catalog` (the connection). Core defines no plugin tables itself.
- When external/third-party plugins arrive (deferred), they'll get a sandboxed DB API
  instead of the raw connection; the prefix convention carries over.

This is how AI tagging persists: an `ai__suggestions` table it owns and migrates.

Note the distinction from **core-owned** tables: the non-destructive edit record
(`photo_edits`) lives in the core schema, like `photo_metadata`/`photo_locations`. It
is *not* a plugin table — it's a neutral, one-to-one-with-photos core row that core
stores opaquely (see "Editing / Develop"). The "core defines no plugin tables" rule is
about *plugin-owned* `<id>__*` tables, not about core's own data.

## Enable/disable UI

A **Modules** settings panel lists bundled modules with: name, version, description,
whether its backend feature is compiled in, and an enable toggle. Toggling persists to
`modules.enabled` and loads/unloads the module live.

## How AI tagging maps on

AI tagging becomes the first module: `id: "ai-tagging"`, `backendFeature: "ai"`.
- `onLoad`: registers an inspector panel ("Suggest tags") and a settings panel
  (provider/model/key), reads config via the namespaced `getSetting`.
- Backend: `src-tauri/src/plugins/ai/` behind the `ai` Cargo feature — the provider
  abstraction + `ai_suggest_tags` / accept / reject commands + the `ai__suggestions` table.
- Fully optional: omit the `ai` feature to compile it out; or leave it off in Modules.

## Editing is modular

Photo editing is **not** in core. Rationale: it's heavy (image processing/GPU),
optional (many users — including the project owner — edit in darktable/DaVinci), and
divergent/fast-evolving. Decision (with the user): **all editing lives in modules**,
over a small neutral core contract:

- **Core** owns a non-destructive **edit record** (per-photo edit settings, stored as a
  sidecar/edit row) + a **render hook** (apply edits → preview/export). Core defines no
  editing UI or processing.
- **Modules** provide the editor(s): even *basic* exposure/crop/B&W is a first-party
  module; advanced tools (curves, masks, etc.) are further modules — all writing the
  same core edit record. (Mirrors AI tagging: core is editing-agnostic; the plugin fills it.)
- Derived properties consumed elsewhere: e.g. a B&W edit makes the **monochrome facet**
  true (see docs/taxonomy.md), which the filter bar / smart albums read. RAW is colour,
  so "make monochrome" is purely a develop function.

Core provides `photo_edits` (an opaque JSON edit record), `get/set_edit_record` and
`photo_has_edits`, plus the frontend render-hook contract (`EditRecord` / `EditRenderer`,
`registerEditRenderer`, `activeEditRenderer`). The bundled Basic Editor module supplies the
renderer; with no editor module enabled the loupe simply shows the original. See
`docs/editing.md`.

## Module dependencies (`requires`)

A module may declare dependencies on other modules:

```ts
requires?: { id: string; version?: string }[];   // version = semver range; omit = any
// e.g. snapchat → requires: [{ id: "localsend", version: "^0.1.0" }]
```

The host (`host.ts`) enforces this:
- `enableModule(id)` first validates the module's own requirements (the required module
  must exist, its `backendFeature` must be compiled in, and its `version` must satisfy the
  range), then **recursively enables each required module first** (deps before dependents,
  so `onLoad` order is correct). If any requirement can't be met it refuses with a toast +
  console warning and leaves the module disabled.
- `disableModule(id)` **cascade-disables** any enabled module that requires it (with a
  toast), so a dependent is never orphaned.
- The enabled set is persisted (and re-loaded by `initHost`) in **dependency order**.

Version matching uses a small in-repo `satisfies(version, range)` (no new dependency)
covering simple `MAJOR.MINOR.PATCH` module versions: omitted/`*`/`x` = any, exact `X.Y.Z`,
`^X.Y.Z` (same major and ≥), and `>=X.Y.Z`. The **Modules** panel shows "Requires: <name>
<range>" and disables the enable toggle (with a hint) when a dependency is unavailable or
version-incompatible. This matters most once external modules (deferred) load against a
host/modules of a given version.

## External / third-party modules

> The trust model below was approved by the owner
> (2026-07-23). The manifest spec, install convention, and host-version compatibility
> rules are in effect. `hostSatisfies` enforces the `minHostVersion` floor;
> failures surface as a `blockedReason` in the Modules panel. Unit-tested in
> `src/modules/__tests__/host.test.ts`.

Everything above (the `ChairPhotoModule` contract, the host API, `requires`/version
matching, plugin-owned tables) applies unchanged to external modules — an external module
is just a `ChairPhotoModule` that ships **outside** the app bundle and is loaded from disk
at startup. The additions here are the parts a bundled module doesn't need: a **trust
model**, an on-disk **manifest**, an **install directory**, and **host-version compatibility**.

### Trust model

**Explicit user install ⇒ implicit trust** — the browser-extension model. When a user
places a module in the install directory (below), they have granted it trust to run.
There is no sandbox and no per-command permission prompt: **an installed module runs
with the same full access as a bundled one**, including unrestricted `api.invoke(command,
args)` to *any* backend Tauri command whose feature is compiled in — not only its own
`backendFeature`. It executes in the app's WebView with the app's privileges; a malicious
or buggy module can read/modify the catalog and drive file I/O through backend commands.

**One exception, added later:** it can no longer reach the network *directly*. A
Content-Security-Policy pins `connect-src` to the app's own origin and the Tauri IPC bridge,
so `fetch`/`XHR`/`WebSocket`/`EventSource` from module code cannot reach an arbitrary origin.
A module can still get to the network by invoking a network-capable backend command — the
CSP closes the route that needed no backend command at all. See
`docs/module-capabilities.md` § "The policy" for the exact directives and for what the
policy does *not* stop (notably `img-src`).

This is a deliberate trade-off (get external modules working against the *existing*
stable contract without first building a capability system), not a claim that it is safe.
It is acceptable only because installation is an explicit, deliberate act by the machine's
owner — the same reasoning browsers use for unpacked extensions.

**Security caveats (must be surfaced to the user and honoured by us):**

- Installed modules are **fully trusted code**. There is no isolation between a module and
  the rest of the app. Treat installing a module as equivalent to running a program on your
  machine.
- **Recommend reading the source before installing.** External modules are plain JS/TS; the
  install UI should tell the user to review a module's code (and its author/origin)
  before dropping it in the modules directory, and warn that a module can invoke any
  backend command.
- **No auto-install, no auto-update, no live FS watching.** Modules are discovered only at
  startup; adding one requires an explicit restart. We never fetch or update
  module code on the user's behalf. There is no marketplace.
- "Nothing ever leaves home" still binds the *app's* behaviour. A fully-trusted module can no
  longer open its own socket to an arbitrary origin (the CSP above), but it can still call
  network-capable backend commands — another reason review-before-install matters.

**A stronger mitigation, not implemented:** a **per-module invoke allowlist** — each
module declares the backend commands (or capability groups) it needs, the host enforces
that `api.invoke` only reaches allowlisted commands, and the user reviews/grants that set at
install time (again browser-extension-style: "this module wants to: read the catalog, access
the network"). This is the intended path to real least-privilege; it is not implemented.
Were it added, the manifest would gain a `permissions` field and `api.invoke` would be gated
per-module rather than per-compiled-feature.

### Manifest — `chairphoto-module.json`

Each external module ships a manifest file named exactly `chairphoto-module.json` at the
root of its install directory. It is JSON with these fields:

```jsonc
{
  "id": "my-module",                 // required. stable unique id, matches ChairPhotoModule.id
  "name": "My Module",               // required. human-readable display name
  "version": "1.2.0",                // required. MAJOR.MINOR.PATCH (matches `requires`/satisfies)
  "description": "What it does.",     // required. one line shown in the Modules panel
  "entrypoint": "index.js",          // required. path (relative to the module dir) to the JS
                                     //   entry module that default-exports a ChairPhotoModule
  "backendFeature": "ai",            // optional. the Cargo feature its backend needs, if any;
                                     //   host shows "backend not included" if it's absent
  "requires": [                      // optional. inter-module deps (same shape as ChairPhotoModule.requires)
    { "id": "localsend", "version": "^0.1.0" }
  ],
  "minHostVersion": "0.1.0"          // required. lowest host (app) version this module supports
}
```

- `id`, `version`, `backendFeature`, and `requires` mirror the in-code `ChairPhotoModule`
  fields exactly and MUST agree with the loaded module object (the loader validates the
  shape; a mismatched `id` is a load error). The manifest exists so the host can read
  identity/compat **without executing** the module (discovery) and so the Modules
  panel can list a module even when its `entrypoint` fails to load.
- `entrypoint` is resolved relative to the module directory and loaded via Tauri's
 `convertFileSrc` + dynamic `import`. Its default export must be a valid
  `ChairPhotoModule`.
- Unknown/extra fields are ignored (forward-compatible), and malformed manifests are
  **skipped with a logged warning** — one bad module never breaks discovery of the others
  and never crashes the app.

### Install directory

External modules live under the OS app-data directory, one directory per module keyed by id:

```
<app_data_dir>/modules/<id>/
    chairphoto-module.json      # the manifest above
    index.js                    # the entrypoint (name is whatever the manifest says)
    …                           # any other assets the module bundles
```

- `<app_data_dir>` is Tauri's resolved `app_data_dir` for ChairPhoto (platform-specific,
  e.g. `~/.local/share/chairphoto/` on Linux — the same base that holds the catalog DB).
- The `<id>` directory name SHOULD match the manifest `id`; discovery keys on the manifest
  `id`, and a mismatch is logged (the manifest is authoritative).
- The directory is **not** created or watched automatically. The Modules panel shows
  the absolute path and instructs the user to drop a module there and **restart** to
  discover it (intentional — no live filesystem watching).

### Host-version compatibility

The **host** is the ChairPhoto app; its version is the app version
(`src-tauri/tauri.conf.json` / `package.json`, currently `0.1.0`). Compatibility has two
independent axes:

1. **`minHostVersion` (manifest → host).** The host refuses to *enable* a module whose
   `minHostVersion` is **newer** than the running app, surfacing a clear `blockedReason`
   in the Modules panel ("requires host ≥ X.Y.Z, this is A.B.C"). Enforced by a
   `hostSatisfies(appVersion, minHostVersion)` helper alongside the existing
   `satisfies(version, range)` used for `requires`.

2. **Host-API stability (the contract → module).** The **`ChairPhotoAPI` / `ChairPhotoModule`
   contract is stable within a host major version.** Within a given `MAJOR`, we only add to
   the API (new optional methods/slots) and never remove or change the meaning of an existing
   member — a module that worked against `1.x` keeps working against any later `1.y`. A
   **breaking** change to the contract bumps the host MAJOR, and modules built for the old
   major must be updated. Practically: set your module's `minHostVersion` to the lowest host
   whose API you rely on; the host guarantees API additivity up to the next major.

`requires` between modules keeps using the same `satisfies` semantics documented under
[Module dependencies](#module-dependencies-requires) (omitted/`*`/`x` = any, exact,
`^X.Y.Z`, `>=X.Y.Z`). `minHostVersion` is deliberately a single floor (not a range): a
module states the oldest host it supports, and the "stable within a major" promise covers
the upper bound implicitly until the next major.

## Deliberate limits

- Dynamic Rust plugin loading (`.so`) — not planned; Cargo features instead. External
  modules are **frontend-only** JS loaded from disk; their backend, if any, must already
  be a compiled-in Cargo feature (`backendFeature`).
- A **per-module invoke allowlist / capability system** (the long-term trust mitigation
  above) — external modules run fully trusted.
- A module marketplace, auto-update, and live filesystem watching of the install dir
  (discovery happens at startup only; the `requires` mechanism covers basic inter-module
  dependencies).
