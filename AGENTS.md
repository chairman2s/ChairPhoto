# ChairPhoto Agent Guide

ChairPhoto is a native photo organizer built with Tauri (Rust) and React/TypeScript.
It is catalog-first, non-destructive, local-first, and optimized for culling large RAW
collections.

## Source Of Truth

Use the narrowest authoritative source for the question:

1. Current code, schema, and tests describe implemented behavior.
2. The matching domain document describes design intent and binding invariants.

Read only the documents triggered by the task:

| Work area | Read first |
|---|---|
| Storage, catalog roots, import, backup, merge | `docs/storage-and-import.md` |
| Tags, vocabulary, XMP keywords | `docs/taxonomy.md` |
| Modules and host API | `docs/plugin-system.md` |
| What third-party modules can/cannot do, and why | `docs/module-capabilities.md` |
| Editing and versions | `docs/editing.md` |
| Publications and platform integrations | `docs/publications.md`, then the platform doc |
| AI, Smart Tagging, faces, sharpness | `docs/ai-tagging.md`, `docs/face-tagging.md`, `docs/sharpness-culling.md` |
| Map/geotagging | `docs/map-and-geotagging.md` |
| Collage, slideshow, LocalSend | `docs/collage.md`, `docs/slideshow.md`, `docs/localsend.md` |


## Binding Invariants

### Privacy and originals

- Nothing leaves the user's machine or home storage by default. Any cloud/network transfer of
  photos requires an explicit, feature-specific opt-in.
- Originals are never modified. Edits, metadata, and interoperability state live in the catalog,
  versions, exports, or merge-safe sidecars.

### Photo identity

- Assign every photo a UUID v4 on first import.
- Persist it in both SQLite and `xmp:Identifier`; never skip the sidecar write.
- Catalog merge matches photos by UUID, not path.

### Paths and storage

- `photos.path` is catalog-root-relative and logical; never store an absolute photo path.
- To read bytes, call `catalog.resolve_photo_path(id)` or `require_photo_path(id)`.
  Never construct a physical path from `photos.path`.
- A photo can have multiple locations. Missing/unmounted storage is a normal state, not
  evidence that the catalog row is invalid.
- The catalog root, library folder, and local catalog-volume base are one concept, stored as
  `settings.catalog_root` and changed through `set_library_root`. It defaults to
  `~/Pictures/Raw`; the `.chairphoto` database lives separately under application data.

### XMP safety

- Read-modify-write the existing sidecar. Each writer touches only what it owns:
  `write_iptc` through `MANAGED`, `write_keywords` through its local set, identifier/import/GPS
  through their named elements, and face regions by Name + Area match. Preserve every other
  element/attribute and foreign namespace.
- Sidecars are `<original_filename>.xmp`, alongside the original.
- Before ChairPhoto's first in-library write to an existing sidecar, back it up if it lacks
  `chairphoto:LastWrite`. Export-only destination copies are not subject to this rule.
- Face-region writes replace only matching ChairPhoto regions and preserve foreign regions.
  MWG areas use normalized center coordinates and oriented pixel dimensions. When uncertain,
  preserve.

### Background work and ownership

- Commands that perform blocking disk/model/external-process/image work or long SQLite work
  are async or moved to a blocking worker; they must not block the UI thread.
- A newer job start or catalog switch must make older workers abortable and unreachable as
  owners. Job status/progress/terminal mutation is scoped to the current job id.
- Where a subsystem exposes a queryable job-status slot (currently Faces and Smart Tagging),
  clear it before the terminal event and only if that job still owns it.
- Acquire every lock needed for an ownership transition before the first mutation. Keep a
  documented global lock order; do not repair one side of a start/switch protocol in isolation.
- Event listeners are resources: give each async registration an owner, stop late registrations,
  and release only the listener owned by that attempt.

### Performance

- Thumbnail/grid work never decodes on the UI thread.
- Serve image bytes through the native media protocols/cache, never as base64 IPC payloads.
- Navigation loads the requested photo first, then preloads N-1 and N+1. Target display latency
  is under 50 ms when preloaded and under 500 ms cold.
- Scans, indexing, export, and model work expose progress without making progress events a
  substitute for the command's terminal result.

### Plugin isolation

- Core tables live in `catalog/schema.rs`; plugins use prefixed tables such as `faces__*` and
  `smarttags__*` and never alter core tables.
- Modules reach host/backend services through `ChairPhotoAPI`, declared core wrappers, and host
  hooks; never through `window.__TAURI__` or arbitrary app internals.
- Core command wrappers stay in `modules/api.ts`; module-owned wrappers stay with the module
  and call `ChairPhotoAPI.invoke` / optional host capabilities.
- Missing optional host capabilities degrade only the cosmetic/optional behavior. A required
  terminal signal must fail closed with an explicit state.

## Architecture

```
src/                    React/TypeScript UI and host/module contracts
src-tauri/src/          Rust I/O, catalog, image processing, and Tauri commands
```

Frontend/backend communication is asynchronous Tauri IPC. Rust owns file access,
catalog queries, image decoding, XMP, and external processes. TypeScript invokes typed
commands and renders their results; it never reads photo files directly.

### Backend map

| Path | Responsibility |
|---|---|
| `commands/` | Flat Tauri command surface; one submodule per domain. `mod.rs` holds `AppState` and genuinely shared helpers only. |
| `catalog/` | SQLite schema, migrations, lifecycle, locations/resolver, vocabulary, albums, and merge. |
| `scanner/`, `thumbnails/`, `image_pool/`, `protocol/` | Import/index, preview generation/cache, bounded decode work, and native media protocols. |
| `xmp/` | Merge-safe sidecar reads/writes. |
| `raw/`, `export/`, `bundle/` | Full RAW decode, one-way export, and portable catalogs. |
| `burst*`, `phash*`, `sharpness*` | Derived culling signals and grouping. |
| `plugins/*` | Feature-gated module backends and their prefixed tables. |
| `flickr/`, `smugmug/`, `instagram/`, `localsend/`, `oauth1/` | Publishing, web automation, LAN send, and shared OAuth. |

Add commands to their domain submodule, not `commands/mod.rs`.

### Frontend map

| Path | Responsibility |
|---|---|
| `App.tsx` | Shell state and panel wiring. |
| `components/CatalogGrid*`, `Thumbnail*` | Virtualized library/culling hot path. |
| `components/PhotoInspector*`, `Tag*`, `Editor*`, `Preferences*` | Core user workflows. |
| `modules/registry.ts` | Pure module and `ChairPhotoAPI` types. |
| `modules/host.ts` | Capability adaptation, enablement, requirements, and slots. |
| `modules/api.ts` | Typed wrappers for core commands only. |
| `modules/plugins/*` | First-party modules; each owns its DTOs and command/event wrappers. |

## Working Agreements

### Understand before changing

- Start with `git status`, the relevant domain doc, neighboring code, and the full call path.
- Prefer existing module boundaries and helpers. Add an abstraction only when it removes real
  duplication or protects a binding invariant.
- Keep the change scoped to the stated behavior. Record adjacent defects separately unless
  leaving them unfixed would make the requested change incorrect.

### Build and test

Run the full suite for every package touched, not a scoped test that can hide breakage:

```bash
# frontend, from repository root
npx tsc --noEmit
npm test
npm run build

# backend, from src-tauri/
cargo test
cargo check --all-features --all-targets
cargo check --no-default-features
```

`cargo check --all-features --all-targets` includes `#[cfg(test)]` code; plain
`cargo check` does not. Keep every feature combination warning-clean.

Tests that cannot run on a given machine — no ONNX Runtime, no `ffmpeg`, no model behind
`SMARTTAGS_TEST_MODEL`, no loopback multicast — skip rather than fail, and announce it as
`SKIPPED: <test_name> — <why>`. `cargo test` captures that line for a *passing* test, so run
`cargo test -- --nocapture` when you need to know what actually executed. A plain green run
does not distinguish "passed" from "never ran".

### Verify, then report

Commit messages, reviews, and status reports are part of the engineering record.

- Immediately before reporting a check, confirm `git rev-parse HEAD` is the commit tested and
  `git status --short` is understood. A moving worktree invalidates hash-specific claims.
- Tests prove only the behavior they exercise. For concurrency or lifecycle work, force the
  relevant interleaving or state why the gap remains untested.
- Never explain away a failure because a rerun passes. Capture the failing test/output and
  reproduce or report it unresolved.
- **Cite or qualify.** Attach factual claims to command output or `file:line`; label hypotheses
  and inferences. Scope negative claims to the exact paths/searches checked.
- **Separate evidence from inference.** "I observed X" and "therefore Y" are different claims.
- **Open the source before describing it.** Do not infer Rust behavior from TypeScript types,
  current behavior from a design doc, or parent behavior from the current tree.
- **Attribution requires comparison.** "Introduced here" and "pre-existing" require checking
  the parent or another fixed point.
- **A convenient explanation is still a hypothesis.** Unresolved is more accurate than an
  untested diagnosis.
- **Wrong twice means rewrite from sources.** Do not keep patching a sentence or implementation
  whose premise was wrong.
- **Verify completion by construction.** Re-run the grep, diff, count, or test that proves the
  completion claim; do not report it from memory.

### Git discipline

- Run `git diff --check` and inspect the final diff.
- One logical, verified change per commit. Separate unrelated fixes while the work is in progress,
  not after they become tangled.
- Work on a feature/WIP branch, never directly on the default branch. Commit completed units
  without asking; never push without permission.
- Do not add work on top of unrelated uncommitted changes; surface them first.
- Before committing, confirm repository-local identity and review the staged diff and trailers.
- Working notes — review findings, hand-offs between agents — go in `agent-notes/`, which is
  ignored except for its README. Anything a future reader would need belongs in `docs/`, a
  comment, or a commit message instead.
- `git add -A` stages whatever is lying around. Check `git status --short` before it, and
  `git ls-files <path>` rather than memory when asserting a file is untracked.

### Versioning

Calendar versioning: `YEAR.MONTH.RELEASE` — e.g. `2026.8.0`, then `2026.8.1` for the next
release that month, `2026.9.0` in September. `RELEASE` counts releases within the month and
restarts at `0`, so it is not a patch/minor distinction and carries no compatibility promise.

- **Never zero-pad the month.** `2026.08.0` is not valid semver ("invalid leading zero in
  minor version number") and Cargo refuses to build. Write `2026.8.0`.
- One version, three files, always in step: `package.json`, `src-tauri/Cargo.toml`, and
  `src-tauri/tauri.conf.json`. Bumping one alone ships a build that disagrees with itself.
- Tag a release `v2026.8.0`, matching the manifests exactly.

## Runtime Notes

Missing runtime tools degrade only their feature; they must not crash the app:

| Tool | Used for |
|---|---|
| `exiftool`, `exiv2` | Metadata and embedded RAW previews. |
| `ffmpeg` | Video posters and slideshow rendering. |
| ImageMagick with libheif | HEIF/HEIC decoding. |
| darktable / RawTherapee / ART and CLIs | Optional external develop and rendered re-import. |

LibRaw is a link-time dependency for full-resolution decode under the `raw` feature, not a
runtime fallback.

On NVIDIA/Wayland, WebKitGTK may crash without
`WEBKIT_DISABLE_DMABUF_RENDERER=1`. `src-tauri/src/lib.rs::run` sets it on Linux while
respecting an existing value. Do not remove it without a tested replacement.

Optional `faces-cuda` accelerates face inference and must fall back to CPU without crashing.
See `docs/face-tagging.md` before changing model execution or `indexing.speed`.
