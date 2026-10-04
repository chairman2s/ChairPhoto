---
title: "Plugin / Module System"
description: "The Module trait: first-party Rust modules compiled into the GPUI app."
tags:
  - chairphoto/core
  - chairphoto/platform
aliases:
  - "Modules"
  - "Host API"
---

# Plugin / Module System

ChairPhoto's optional features ship as **modules**: first-party Rust, compiled into the
`chairphoto-app` binary, each gated by its own Cargo feature. A module implements the
[`Module`] trait (`crates/app/src/modules/mod.rs`) and registers in [`modules::bundled()`]
(`crates/app/src/modules/mod.rs`); the [`ModuleRegistry`] (`crates/app/src/modules/registry.rs`)
is the runtime that enables, disables and routes to them.

There is no third-party or externally loaded module today. See
[Why no third-party modules](#why-no-third-party-modules) for what used to exist and why it
was dropped.

## Goals

- Optional features live as **modules**, separable from a lean core.
- A module is one explicit object, object-safe and small enough to read in one sitting.
- First-party modules (AI Tagging, Faces, Map, and others) are the only consumers of the
  contract, so it is exercised by real code rather than designed in the abstract.

## The `Module` trait

```rust
pub trait Module: 'static {
    fn meta(&self) -> ModuleMeta;
    fn load(&self, host: ModuleHost, cx: &mut App) -> Result<Box<dyn ModuleInstance>, String>;
}

pub trait ModuleInstance: 'static {
    fn contributions(&self) -> Contributions;
    fn on_event(&mut self, _event: &CoreEvent, _cx: &mut App) {}
    fn on_unload(&mut self, _cx: &mut App) {}
}
```

- **`meta()`** is read once, at registration: the module's id, name, description, the other
  module ids it [`requires`](#module-dependencies-requires), and the Cargo
  [`backend_feature`](#backend-features) it needs, if any.
- **`load()`** runs on enable, after every required module has loaded. It builds the module's
  live state and returns what it contributes ([`Contributions`]: panels, actions, publish
  targets, settings panels, main views — see [Contributions](#contributions)). An `Err(why)`
  leaves the module disabled and shows `why` on the status line; this is the only place a
  module refuses to start.
- **`on_event`** reaches every *enabled* module's instance, in registration order, for every
  [`CoreEvent`] the app model routes.
- **`on_unload`** runs on disable, before the instance and its contributed views are dropped.

**Panics are bugs.** A module is first-party Rust, not sandboxed third-party code: a
recoverable failure to load is `Err` from `load`; a panic fails like any other panic in the
app, with no try/catch wrapper absorbing it.

**Re-entrancy.** The registry calls `load`, `on_event`, `on_unload` and every view factory
while holding no lease on itself, so a module may read the registry from them. An enable or
disable asked for from inside one of those callbacks is logged and deferred until it returns,
rather than run while the module's instance is in use.

## Contributions

A module hands back data plus view factories, never a reference into the shell's own views:

- **Panels** at one of four [`PanelSlot`]s: `Inspector` (the inspector's tags tab), `Sidebar`
  (the collection browser), `Loupe` (absolutely positioned over the loupe — the face overlay
  uses this), and `TagEditor` (inside the tag editor).
- **Actions**, listed under the module in More ⋯ → Modules: fire-and-forget, or a modal view.
- **Publish targets**, a destination in the Publish dialog.
- **Settings panels**, shown on the module's own Preferences tab.
- **Main views**, a full-surface view on the icon rail (the Map module uses this).

The shell renders every contribution through a [`ViewFactory`] it calls once per window,
keeping the view alive while the module stays enabled (modal actions and publish targets get
a fresh view each time their dialog opens).

## Services (`ModuleHost`)

`load()` receives a [`ModuleHost`], the module's route to the rest of the app:

- **`settings()`** — a [`ModuleSettings`] handle: the catalog's `settings` table, namespaced
  `<module id>.<key>`, so a module cannot read or write another module's keys or the host's
  own (see [Settings](#a-modules-settings)). Bound to the catalog opening it was taken from;
  take a fresh handle after a catalog switch.
- **`model()`** / **`shell()`** — the app's core state and the Library session (selection,
  active photo, scope).
- **`images()`** — the image layer (thumbnails/previews), when the app has one.
- **`show_in_loupe()`** / **`open_loupe()`** — put a card in the pop-out loupe in place of the
  selected photo, or open/focus that window.

## Backend features

Each backend-bearing module's Rust lives in its own module under
`crates/core/src/plugins/<name>/`, behind a Cargo feature (e.g. `faces`). A build can omit the
feature to compile the backend out entirely; [`modules::compiled_features()`] lists what this
build has. Most modules are themselves gated behind the matching app feature
(`crates/app/src/modules/mod.rs::bundled()`), so omitting it drops the module from the registry
entirely — it never registers, metadata or otherwise. Instagram, Flickr and SmugMug are the
exception: they always register (their publishing feature is opt-in upstream), and when their
`backend_feature` is absent they register metadata-only — the Modules panel shows "backend not
included in this build" and refuses to enable it. Default feature set decides what ships.
Runtime toggle is independent: even compiled in, a module does nothing until enabled.

## A module's settings

Stored in the catalog's `settings` table, namespaced by module id (e.g. `ai.provider`,
`faces.model`). [`ModuleSettings::get`]/`set` auto-namespace so a module can't read another's
keys. A fixed id list — [`registry::RESERVED_NAMESPACES`] — belongs to the host itself
(`modules`, `indexing`, `sharpness`, `editor`, `develop`, `basic-editor`, `metrics`, `geocode`,
`cull`) and cannot be taken as a module id ([`registry::validate_id`]). A second list,
[`registry::BACKEND_NAMESPACES`] (`ai`, `faces`, `smarttags`, `flickr`, `smugmug`), lets a
module deliberately share its namespace with its own Rust backend, which reads the same keys.

Settings calls are **blocking** (the catalog lock and SQLite): call them from a background
task, never from a render or an event handler.

## Plugin data storage (tables)

Plugins that need more than small settings own their own tables, kept out of the core schema
so core stays feature-agnostic and removing a plugin never touches core data:

- Each plugin table is named `<plugin_id>__<name>` (double-underscore prefix), so two plugins
  can't collide (e.g. `faces__people`, `smarttags__suggestions`).
- The plugin creates its tables lazily with `CREATE TABLE IF NOT EXISTS` the first time it
  needs them — no core migration involved.
- A plugin accesses its tables through its own accessor on `Catalog` (the connection). Core
  defines no plugin tables itself.

Note the distinction from **core-owned** tables: the non-destructive edit record
(`photo_edits`) lives in the core schema, like `photo_metadata`/`photo_locations`. It is *not*
a plugin table — it's a neutral, one-to-one-with-photos core row that core stores opaquely
(see [Editing is modular](#editing-is-modular)). The "core defines no plugin tables" rule is
about *plugin-owned* `<id>__*` tables, not about core's own data.

## Enable/disable

A **Modules** settings panel ([`panel::ModulesPanel`]) lists first-party modules with: name,
description, whether its backend feature is compiled in, and an enable toggle. Toggling
persists to the catalog setting `modules.enabled` (comma-separated, in dependency order) and
loads/unloads the module live. See the lock/ordering details in
`crates/app/src/modules/registry.rs`'s module doc comment for exactly when a toggle made
before a catalog has finished restoring is queued rather than applied.

## How AI tagging maps on

AI tagging is a module: `id: "ai"`, `backend_feature: "ai"`.
- `load`: registers an inspector panel ("AI tags") and a settings panel
  (provider/model/key), reading config via the namespaced settings handle.
- Backend: `crates/core/src/plugins/ai/` behind the `ai` Cargo feature.
- Fully optional: omit the `ai` feature to compile it out; or leave it off in Modules.

## Editing is modular

Photo editing is **not** in core. Rationale: it's heavy (image processing/GPU), optional (many
users — including the project owner — edit in darktable/DaVinci), and divergent/fast-evolving.
Decision (with the user): **all editing lives in modules**, over a small neutral core
contract:

- **Core** owns a non-destructive **edit record** (per-photo edit settings, stored as a
  sidecar/edit row) + a **render hook** (apply edits → preview/export). Core defines no
  editing UI or processing.
- **Modules** provide the editor(s): even *basic* exposure/crop/B&W is a first-party module
  (the Darkroom); advanced tools are further modules, all writing the same core edit record.
- Derived properties consumed elsewhere: e.g. a B&W edit makes the **monochrome facet** true
  (see `docs/taxonomy.md`), which the filter bar / smart albums read. RAW is colour, so "make
  monochrome" is purely a develop function.

Core provides `photo_edits` (an opaque JSON edit record), `get/set_edit_record` and
`photo_has_edits`, plus the render-hook contract the Darkroom's render path uses. See
`docs/editing.md`.

## Module dependencies (`requires`)

A module may declare other module ids it needs in `ModuleMeta::requires`. The registry
enforces this:

- **Enable** first validates the module's own requirement (the required module must exist and
  its backend feature, if any, must be compiled in), then **enables each required module
  first** (dependencies before dependents, so `load` order is correct). If a requirement
  can't be met it refuses, with the reason on the status line, and leaves the module disabled.
- **Disable** cascade-disables any enabled module that requires it, so a dependent is never
  orphaned.
- The enabled set is persisted (and restored) in dependency order.

There is no version range on `requires` today — a compiled-in module ships with the app, so
there is nothing to version-match against, unlike the removed external-module contract (see
below).

## Why no third-party modules

Before the GPUI cutover (#165), ChairPhoto's React/Tauri front end had a true runtime plugin
tier: external modules were plain JavaScript, discovered from an on-disk manifest
(`chairphoto-module.json`), loaded from the app-data directory at startup, and constrained by
a purpose-built trust model — a Content-Security-Policy lockdown, per-module command/origin
permissions reviewed at enable time, and a `mount(el)`/`unmount(el)` rendering ABI so a module
could draw without sharing the host's React instance. That whole design — the host API
(`ChairPhotoAPI`), the manifest, the install directory, the CSP, `api.invoke`/`api.fetch`
gating — existed to let **untrusted** code, loaded from disk and never compiled by this
project, run inside the app safely.

The `Module` trait that replaced it (decided in "Module trait: contribution points for
first-party Rust modules", #104) is for modules **this project compiles**. It explicitly
dropped what only made sense for untrusted JS: the external loader, semver ranges on
`requires`, permission grants, `api.fetch`, and the try/catch isolation every callback used to
run under. None of that protects anything when the "module" is Rust that was reviewed, built
and shipped with the rest of the binary — and Rust has no safe hot-loading story to begin with
(`.so` plugin loading was considered and rejected; Cargo features are the substitute).

The trait is still deliberately object-safe and metadata-first ("nothing assumes the
implementor is compiled in"), so a future **sandboxed** extension host remains possible if
the project ever wants one again — but it would need its own trust model designed from
scratch for whatever untrusted code it hosts, not a revival of the CSP/manifest machinery
above, which was specific to a webview that no longer exists.

[`Module`]: ../crates/app/src/modules/mod.rs
[`ModuleRegistry`]: ../crates/app/src/modules/registry.rs
[`modules::bundled()`]: ../crates/app/src/modules/mod.rs
[`Contributions`]: ../crates/app/src/modules/mod.rs
[`ViewFactory`]: ../crates/app/src/modules/mod.rs
[`ModuleHost`]: ../crates/app/src/modules/mod.rs
[`ModuleSettings`]: ../crates/app/src/modules/mod.rs
[`ModuleSettings::get`]: ../crates/app/src/modules/mod.rs
[`modules::compiled_features()`]: ../crates/app/src/modules/mod.rs
[`registry::RESERVED_NAMESPACES`]: ../crates/app/src/modules/registry.rs
[`registry::BACKEND_NAMESPACES`]: ../crates/app/src/modules/registry.rs
[`registry::validate_id`]: ../crates/app/src/modules/registry.rs
[`panel::ModulesPanel`]: ../crates/app/src/modules/panel.rs
