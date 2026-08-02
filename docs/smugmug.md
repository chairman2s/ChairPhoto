---
title: "SmugMug publishing module"
description: "Publish to SmugMug through the official API, into a chosen album."
tags:
  - chairphoto/module
  - chairphoto/publishing
---

# SmugMug publishing module

Publishes a photo to SmugMug through the **official API v2** — not browser automation — and
records the result via the [`publications`](publications.md) model.

Together with [Flickr](flickr.md) it is the reference implementation of a publishing module:
it declares its `publicationMarker` and records through `api.recordPublication`, so the
marker stays module-owned and posts appear in the inspector's "Published to" panel and the
`published:smugmug` filter facet automatically.

## Setup (one-time)

1. Register a developer app at <https://api.smugmug.com/api/developer/apply> to get an
   **API key + secret**.
2. Enable the module in **Preferences → Modules**. It needs the `smugmug` Cargo feature
   compiled in, which is **not** in the default build — build with
   `--features smugmug`. Once enabled, the module gets its **own tab in Preferences**, where
   the key/secret and Connect live, and an entry in the header **Publish** dialog.
3. In that tab, paste the key and secret and click **Connect**. The authorize page opens in
   your browser (OAuth 1.0a, out-of-band flow); approve it, then paste the **verifier code**
   back to finish. Tokens are stored in the local catalog `settings` table.

## Publishing

`post_to_smugmug` reads the module-namespaced settings (`smugmug.api_key` / `.api_secret` /
`.access_token` / `.access_secret`), renders the **selected version** to a temp JPEG through
the export path (`resolve_originals` + `write_item_jpeg`) at **full resolution**, and uploads
off the UI thread into the chosen album. An edited version is the full-res RAW decode with
the crop applied; an unedited one is the embedded full-size preview. The original's EXIF and
GPS are copied in.

## Albums

The publish form caches the album list in the module's settings (`smugmug.albums_cache`) and
shows it immediately on open, so the form is usable before any network round-trip.
**Refresh** re-fetches from SmugMug via `smugmug_list_albums`. **+ New** creates an album
under your root folder via `smugmug_create_album` and selects it. The last-used album is
remembered as `smugmug.last_album` and pre-selected next time.

## Implementation

All network I/O is in Rust. OAuth 1.0a signing lives in `src-tauri/src/oauth1.rs` — RFC 5849
HMAC-SHA1, pure and unit-tested against a reference vector, and shared with the Flickr
module.

`src-tauri/src/smugmug/mod.rs` handles the request/access token exchange, listing the user's
albums against `api.smugmug.com` (API v2), and the raw-binary upload to
`upload.smugmug.com`.

Commands, all gated on the `smugmug` feature: `smugmug_begin_auth`,
`smugmug_complete_auth`, `smugmug_connected`, `post_to_smugmug`, `smugmug_list_albums`,
`smugmug_create_album`.

Frontend: `src/modules/plugins/smugmug.tsx` is a thin module definition over shared UI in
`plugins/publishing.tsx` — the OAuth settings panel and the per-photo publish panel, whose
version picker defaults to the active version via `api.getActiveVersionId()`. The module
registers a **publish target** (`api.registerPublishTarget`) shown in the unified
`PublishDialog`, and a **settings panel** (`api.registerSettingsPanel`) shown as its
Preferences tab.

## Limits

- **Live OAuth and upload can only be exercised with your own API key and account.** The
  signing is verified against a reference HMAC-SHA1 vector, but the end-to-end flow has not
  been run in development.
- No deeper library management: folders and sub-albums beyond a top-level album create are
  not supported.
- Tokens live in the catalog settings, a local file, rather than an OS keychain.
