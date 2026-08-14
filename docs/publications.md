---
title: "Publications — where a photo was posted, and which version"
description: "Where a photo was published and which version went to each platform."
tags:
  - chairphoto/core
  - chairphoto/publishing
aliases:
  - "Published to"
---

# Publications — where a photo was posted, and which version

ChairPhoto records where each photo has been **published** (Instagram, Flickr, SmugMug,
…) and, crucially, **which version** went to each platform. A photo can go to Instagram
and Flickr as the same edit but to SmugMug as a different one; tags can't express that
(they're keyed by photo only), so publications are a small first-class model.

## Data model

Core table `publications` (schema v15):

```sql
publications(
  id, photo_id, version_id, version_name, platform, url, published_at, created_at,
  UNIQUE(photo_id, platform, version_id)
)
```

- `version_id` — the published version, or `NULL` for the **Original** (unedited base),
  matching the rest of the app where the active version `null` = Original. FK is
  `ON DELETE SET NULL`.
- `version_name` — a snapshot of the version's name at publish time, so a record still
  reads correctly ("SmugMug · Punchy crop") after that version is deleted.
- `platform` — the **marker**, e.g. `"instagram"`. See *Markers* below.
- Key = `(photo_id, platform, version_id)`. **Different versions of the same photo can go
  to the same platform** — each is its own record (e.g. Instagram · Original *and*
  Instagram · Punchy crop). Re-marking the *same* (photo, platform, version) upserts
  (updates the date/url), so you don't accumulate duplicates from re-posting the same
  thing. SQLite treats NULLs as distinct in `UNIQUE`, so the Original bucket is deduped in
  `record_publication` (matching a true Original — `version_id IS NULL AND version_name IS
  NULL`), not by the constraint; this also avoids clobbering the orphan record left when a
  published version is deleted (its `version_id` goes NULL but it keeps its snapshot name).
  Tracking *repeats of the identical version over time* is a non-goal.

Catalog API: `record_publication`, `list_publications`, `delete_publication`,
`published_platforms` (`src-tauri/src/catalog/publications.rs`). `record_publication`
rejects an empty `platform`.

## Markers are declared by the publishing module

The backend never invents a platform string. A publishing module declares
`publicationMarker` on its `ChairPhotoModule` (in `src/modules/registry.ts`); if it omits
one, the host falls back to the module `id`.

A module records a publication through its injected `ChairPhotoAPI` — and **never passes
the platform string itself**:

```ts
// inside a module, `api` is the injected ChairPhotoAPI
await api.recordPublication(photoId, versionId /* null = Original */, postUrl);
const pubs = await api.listPublications(photoId);
await api.deletePublication(pubs[0].id);
```

The host wires `recordPublication` to stamp the calling module's marker
(`getPublicationMarker(mod.id)` in `src/modules/host.ts`), so the "module declares it,
host enforces the fallback" rule holds for every module by construction — a module can't
record under the wrong platform. The **Flickr and SmugMug modules** are the reference
implementation of this contract — see [flickr.md](flickr.md) and [smugmug.md](smugmug.md).

Instagram, Flickr, and SmugMug are all modules now and record this way (on a confirmed
post), each via its publish target in the unified Publish dialog. One lower-level escape
hatch remains for core UI: the raw `recordPublication(photoId, versionId, platform, url)`
wrapper in `src/modules/api.ts`, which takes an explicit platform.

## How it's surfaced

- **Auto-record:** a confirmed Instagram post records a publication with the version it
  actually rendered (`post_to_instagram`, `src-tauri/src/commands/instagram.rs`).
- **Manual:** the inspector's **Published to** panel (`src/components/PublishedPanel.tsx`)
  lists publications and lets the user mark a Flickr/SmugMug/other post by picking a
  platform and which version (defaults to the inspector's active version).
- **Filtering:** dynamic facets keyed `published:<platform>` are appended by
  `available_facets()` and reuse the existing FilterBar chips — no dedicated filter UI.

## Progress and cancellation

What each publish path reports while it runs, as of this writing:

| path | progress | cancellation |
|---|---|---|
| Flickr, SmugMug | none — one render, one upload request, and the command returns when it finishes | none |
| Instagram | none — the supervised flow ends by handing you the composer, which *is* the progress report | none |
| LocalSend | `localsend:progress` `{ done, total }` after each file, rendered by `SendToDevicePanel.tsx` | none — the protocol's `POST /cancel?sessionId=` exists but ChairPhoto never calls it |

**Nothing in the publish UI stops a publish once it has started.** That is a real gap for a
multi-photo LocalSend send, where a wrong selection means waiting out every file; it is much
less of one for the single-photo services, where by the time a user reaches for Cancel the
request is usually already in flight and aborting it would leave the service holding a
partial upload it may or may not commit. Instagram cannot be cancelled by us at all in the
supervised case — the post is finished by the user, in a browser ChairPhoto deliberately
does not own; closing that window is the cancel.

## Rendering and upload strategy: render-first by design

Every upload path renders to a JPEG first and reads the whole render into memory before sending:

| Path | How it reads the render | Approx. peak |
|---|---|---|
| Flickr (`flickr/mod.rs:739`) | `fs::read()` into memory | ~5–25 MB |
| SmugMug (`smugmug/mod.rs:208`) | `fs::read()` into memory | ~5–25 MB |
| LocalSend (`localsend/mod.rs:863`) | `tokio::fs::read()` into memory, one file per loop iteration | ~5–25 MB |
| Instagram (`commands/instagram.rs`) | rendered to disk, path passed to Chrome (not uploaded by ChairPhoto) | ~200 KB (1080px cap) |

This design is deliberate. **Peak exposure is roughly one full-resolution JPEG** (~5–25 MB for Flickr,
SmugMug, and LocalSend; ~200 KB for Instagram). LocalSend's batch loop reads one file at a time
(not fifty simultaneously), so a 50-photo send peaks at one JPEG in memory, not fifty. Streaming
would add async-body plumbing and OAuth signing complexity across multiple modules to save memory
that is not scarce at these sizes.

### The tripwire: originals and video

**If any upload path is changed to send an original rather than a render, it must stream the body
first.** This is a condition on future change:

- A RAW original is 25–80 MB.
- A video original runs to gigabytes. `fs::read()` on one is a real problem, not a theoretical
  concern.

Today every path renders to JPEG specifically because RAW decode is slow, so the constraint is not
yet active. It is recorded here against the condition that would trigger it, so that whoever changes
an upload path to send originals meets the requirement before writing the code rather than after.

Temp renders do not depend on any of this. Each job renders into a directory of its own that
is removed when the job ends, whichever way it ends — see `publishing::JobTempDir` in
`src-tauri/src/commands/publishing.rs`, and [instagram.md](instagram.md) for the one flow
whose render outlives the command on purpose.

## Migration

Schema v15 backfills the legacy flat `"instagram"` tag: every photo carrying it gets one
Instagram publication (Original), dated to when the tag was applied. The tag itself is
left in place (no user data is deleted); it's now redundant.
