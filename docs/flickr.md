---
title: "Flickr publishing module"
description: "Publish to Flickr through the official API, and backfill years of past posts."
tags:
  - chairphoto/module
  - chairphoto/publishing
---

# Flickr publishing module

Publishes a photo to Flickr through the **official API** — not browser automation — and
records the result via the [`publications`](publications.md) model.

Together with [SmugMug](smugmug.md) it is the reference implementation of a publishing
module: it declares its `publicationMarker` and records through `api.recordPublication`, so
the marker stays module-owned and posts appear in the inspector's "Published to" panel and
the `published:flickr` filter facet automatically.

## Setup (one-time)

1. Register a developer app at <https://www.flickr.com/services/apps/create> to get an
   **API key + secret**.
2. Enable the module in **Preferences → Modules**. It needs the `flickr` Cargo feature
   compiled in, which is **not** in the default build — build with
   `--features flickr`. Once enabled, the module gets its **own tab in Preferences**, where
   the key/secret and Connect live, and an entry in the header **Publish** dialog.
3. In that tab, paste the key and secret and click **Connect**. The authorize page opens in
   your browser (OAuth 1.0a, out-of-band flow); approve it, then paste the **verifier code**
   back to finish. Tokens are stored in the local catalog `settings` table.

## Publishing

`post_to_flickr` reads the module-namespaced settings (`flickr.api_key` / `.api_secret` /
`.access_token` / `.access_secret`), renders the **selected version** to a temp JPEG through
the export path (`resolve_originals` + `write_item_jpeg`) at **full resolution**, and
uploads off the UI thread. An edited version is the full-res RAW decode with the crop
applied; an unedited one is the embedded full-size preview. The original's EXIF and GPS are
copied in.

Uploads go to your **photostream**. Albums and photosets are not supported.

`flickr_suggest_tags(photoId)` prefills the publish panel's Tags field with the photo's
**export keywords** — the same set ChairPhoto writes to the XMP sidecar as `dc:subject`, so
each assigned tag is expanded to include its ancestors and any export synonyms (see
[taxonomy.md](taxonomy.md)). They arrive in Flickr's `tags` format: space-separated, with
multi-word tags quoted, and you edit them before uploading.

## Backfilling publications from your photostream

If you have been posting to Flickr for years, `flickr_import_published` reconciles that
history with the catalog. It fetches the authenticated user's full photostream
(`flickr::fetch_photostream`), matches each Flickr photo against local photos, and returns a
**dry-run plan**. It is strictly read-only toward Flickr and writes nothing until you apply
it.

Matching uses three signals:

| signal | basis |
|--------|-------|
| **P** | An existing publication URL already names that Flickr photo id — so photos ChairPhoto uploaded itself, or already imported, never land in the ambiguous pile. |
| **A** | Capture datetime matches at second precision. |
| **B** | The Flickr title matches the filename stem or full filename, case-insensitively. A trailing image extension in the title (e.g. `_81A8352-2.jpg`) is stripped before comparing. A title-only match is additionally constrained by a plausibility window. |

The result separates **matched**, **ambiguous** (too many or conflicting candidates, each
carrying up to 10 candidates and a small 240 px Flickr thumbnail for manual resolution), and
**unmatched** counts. Matches and ambiguous items are capped at 50 for display, while the
full uncapped `plan` is returned so the frontend can hand it straight back to
`flickr_import_apply` without re-fetching the photostream. Applying upserts the
publications.

## Implementation

All network I/O is in Rust. OAuth 1.0a signing lives in `src-tauri/src/oauth1.rs` — RFC 5849
HMAC-SHA1, pure and unit-tested against a reference vector, and shared with the SmugMug
module.

`src-tauri/src/flickr/mod.rs` handles the request/access token exchange, the photostream
fetch and matching, and the `up.flickr.com` upload. The multipart body is built by hand to
avoid an extra dependency.

Commands, all gated on the `flickr` feature: `flickr_begin_auth`,
`flickr_complete_auth`, `flickr_connected`, `post_to_flickr`, `flickr_suggest_tags`,
`flickr_import_published`, `flickr_import_apply`.

Frontend: `src/modules/plugins/flickr.tsx` is a thin module definition over shared UI in
`plugins/publishing.tsx` — the OAuth settings panel and the per-photo publish panel, whose
version picker defaults to the active version via `api.getActiveVersionId()`. The module
registers a **publish target** (`api.registerPublishTarget`) shown in the unified
`PublishDialog`, and a **settings panel** (`api.registerSettingsPanel`) shown as its
Preferences tab.

## Limits

- **Live OAuth and upload can only be exercised with your own API key and account.** The
  signing is verified against a reference HMAC-SHA1 vector, but the end-to-end flow has not
  been run in development.
- Uploads go to the photostream; no album or photoset management.
- Tokens live in the catalog settings, a local file, rather than an OS keychain.
