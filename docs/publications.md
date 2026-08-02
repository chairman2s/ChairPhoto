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

## Migration

Schema v15 backfills the legacy flat `"instagram"` tag: every photo carrying it gets one
Instagram publication (Original), dated to when the tag was applied. The tag itself is
left in place (no user data is deleted); it's now redundant.
