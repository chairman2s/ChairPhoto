---
title: "Storage, Import & Sync"
description: "Volumes, import, backup, catalog merge and the reconcile queue."
tags:
  - chairphoto/core
  - chairphoto/library
aliases:
  - "Storage"
  - "Import"
  - "Sync"
  - "Merge"
---

# Storage, Import & Sync

ChairPhoto manages a RAW library that physically lives in more than one place — a NAS
archive, a fast local disk, a memory card, a travel laptop — while presenting it as one
catalog. This document covers how photos are located, how they move between tiers, how
machines stay in sync, and how data is exported.

## Purpose

chairphoto must manage a large RAW library that physically lives in more than one
place — a NAS archive, a fast local working disk, a memory card, a travel laptop —
while presenting it as one coherent catalog. This doc defines how photos are
located, how they move between tiers, how machines stay in sync, and how data is
exported. The overriding goal is **never lose an original**, while keeping culling
and tagging fast regardless of where the files are (or whether the NAS is even
reachable).

## Core idea: logical identity vs physical location

A photo's **identity** is its UUID (already implemented: assigned at first import,
written to the catalog and the XMP sidecar). A photo's **locations** are physical
instances of that identity. One photo (one UUID) may exist simultaneously as:

- the original on the **NAS** (canonical archive),
- a fast **local** working copy,
- (transiently) the file on a **memory card**,
- an outbound **export** copy (laptop / hand-off).

So the model is **"a photo has a set of locations, each with a role,"** not "a photo
has a path." All path resolution goes through a **resolver** that returns the best
*available* copy. This is core, because everything — the image protocol, editor
launch, export — asks "where is this file?"

### Identity that fails to reach the disk

The catalog half of the identity cannot fail; the disk half can. Storage is mounted
read-only, an existing sidecar does not parse, the volume goes offline mid-scan, or
the sidecar already carries a *different* `xmp:Identifier` that must not be
overwritten (XMP safety: when uncertain, preserve). A row whose sidecar never
received the UUID has lost its portable identity — a merge or a re-import can no
longer recognise the file, and the catalog is then the only copy of that fact.

So the operation is **upsert (or relocate) the row AND bind the sidecar identity, or
record a retryable repair** — never "log it and continue". `catalog/identity.rs`
owns it: `bind_sidecar_identity` is pure filesystem work (safe off the catalog lock),
`record_sidecar_identity` writes the outcome, and everything that creates or re-points
a row — the local scan, the NAS in-place scan, card ingest, the bundle indexer,
`relocate_photo` — goes through `ensure_sidecar_identity`. Failures land in
`pending_sidecar_identity` with the reason and an attempt count.

`repair_pending_identity` retries the queue (planned under the lock, sidecar IO off
it, recorded under it) and clears what succeeds. Unreachable files stay queued — an
unmounted volume is a normal state. So does an identity **conflict**: the file keeps
the identifier it has, and the divergence stays visible for a human rather than being
resolved by clobbering somebody else's identity. A sidecar failure never aborts a
scan: one unwritable file must not cost the user the other 99,999 rows.

## Volumes (named storage locations)

Locations never store absolute paths. They reference a **named volume** + a relative
path. A volume (e.g. `NAS-Photos`, `LocalScratch`) has:

- a base path **per machine** (the NAS mounts at different points on desktop vs laptop),
- a runtime **reachability** state (mounted? online?).

Remapping the NAS to a new mount point updates one volume record, not every photo.
This is also what makes catalogs portable between machines.

## Storage lifecycle (Pattern C)

Photos move through states; transitions are **gated on NAS reachability**:

```
card → [LOCAL ONLY, not backed up]   ← only copy is on this disk; AT RISK
            │  (NAS reachable + verified copy made)
            ▼
       [BACKED UP]   = local original + verified NAS original
            │  (need local space; only allowed from BACKED UP)
            ▼
       [ARCHIVED]    = NAS original only; browsable via local cached preview
```

Existing NAS-resident photos (the current library) simply start at **ARCHIVED**.
A laptop with no NAS lives in the top state: imports land local, are fully usable,
and the backup transition is **deferred** until the NAS reappears.

### Per-photo status (derived from its locations)

- **Local only — not backed up** → at-risk; show a visible indicator
- **Backed up** → safe (local + verified NAS)
- **Archived** → NAS-only, browse via cached preview
- **Offline** → NAS-only *and* NAS unreachable → browse/cull/tag still work; edit/export blocked
- **Missing** → no known copy anywhere

`Catalog::photo_storage_status` (+ a batch `photo_storage_statuses` for the grid)
derives this from the photo's locations' volume kinds and reachability. A backup
*record* counts as backed-up even while the NAS is unmounted (reachability only
chooses Archived vs Offline when there's no local copy). The grid shows two icons
per tile — local (▣) and NAS (☁, dimmed when offline).

### The key performance principle

**Cull / browse / tag run entirely off the local preview cache** (already built).
They never touch the original, so they are fast and work even when the NAS is
offline. **Only editing and export need the original**, fetched on demand. This is
what makes NAS slowness/absence largely invisible in daily use.

## Reconcile queue

Because actions can't run while the NAS is away, pending operations are queued and
drained when the NAS volume is detected:

- `backup(photo)` — copy local → NAS, hash-verify, mark Backed up
- `offload(photo)` — only after verified backup; frees local space
- `restore(photo)` — pull an archived original back to local (e.g. to edit it)

**The ops and verification**: `catalog/lifecycle.rs` + async `backup_photo` /
`offload_photo` / `restore_photo` commands. SHA-256 (`photo_locations.verified_hash`,
schema v10); each op is plan-under-lock → pure file IO off-thread → record-under-lock,
so a NAS copy never blocks the UI. The backup target is the single backup volume and
restore lands on the single local volume (multi-volume selection is future). The `pending_operations`
queue and automatic draining on NAS reappearance are not implemented; the ops are
invoked directly per photo.

Reconcile is **prompt-then-go**: surface "240 photos from Trip 2026-06 aren't backed
up — back up now?" rather than acting silently. (Decision.)

`reconcile_now` drains the queue op-by-op via the plan→IO→record
split (off the UI thread). Trigger (owner decision): **on app launch + window focus** —
when a backup volume is reachable and ops are pending, the drain runs **in the
background** (non-blocking; a status line shows "Backing up N to NAS…", a topbar
"Back up (N)" indicator shows the count and triggers it manually). The earlier blocking
"prompt-then-go" dialog was dropped — it looped on the focus that dismissing it caused,
and the owner asked for silent background backup. Backup entry (owner decision):
**both** — every imported photo is auto-enqueued, and a manual per-photo "Back up"
(run-or-queue) sits in the inspector alongside Offload / Restore (shown by status).

## Safety invariants (non-negotiable)

1. **Never delete the last verified copy** of a photo.
2. **Never offload** anything not verified-backed-up.
3. **Hash-verify** the NAS copy before marking safe or deleting anything local.
4. On a NAS-less machine, offload of un-backed-up photos is **unavailable**; they
   stay local and flagged at-risk.
5. If local fills up with **no NAS**, chairphoto may auto-evict only the
   **regenerable preview/thumbnail cache** — never an original. It warns instead.

### Trust hierarchy

- **Home (NAS + desktop catalog)** — canonical, permanent, **only grows**.
- **Local working disk** — fast, disposable cache; evictable only once home has a
  verified copy.
- **Laptop / hand-off exports** — outbound copies; creating or deleting them never
  affects home.

**Nothing ever leaves home.** The only delete operation (offload) removes a *local
cache* copy after the NAS (= home) holds a verified original, so the original never
actually leaves home.

## Import

Two modes over the same core location model:

- **Import in place (reference)** — e.g. existing NAS images. Create the photo with a
  single location on the NAS volume, role = primary. Files are **not moved**. (This is
  essentially today's scan-in-place.)
- **Import from card (ingest)** — `scanner::ingest_from_card` copies
  each supported image from the card into `<dest>/YYYY/MM/DD/` (date from EXIF capture
  time, falling back to file mtime), **keeping camera filenames**, then indexes the
  copies, groups them in one import batch, and auto-enqueues NAS backup. Destination
  default `~/Pictures/Raw` (must be under the catalog root). Collisions: same byte-size →
  skipped as already-imported; different size → ` (n)` rename (never overwrites). Owner
  decisions: Year/Month/Day tree, keep filenames. UI: topbar "Import card" dialog with an
  optional **Import name** that labels the batch (defaults to the source folder); the batch
  keeps its stable UUID underneath. (Cross-volume "import once" by UUID is handled by bundle merge.)

### Import batches ("negative film roll")

Every ingest creates an **import batch** with a permanent unique ID — think of it as
a negative film roll. All photos from that ingest belong to it forever.

- Batches are **auto-created, immutable**, and shown in their **own list**, separate
  from user albums.
- The batch ID **should be written to each photo's XMP** (like the UUID) for
  permanence and portability. (Decision: yes.) Still **deferred** — but the scan-time
  **UUID write now ships**: the scanner writes `xmp:Identifier` into each file's sidecar
  (merge-safe, only when absent) and matches a moved/re-rooted file to its existing row
  by that UUID instead of duplicating (`Catalog::upsert_photo_with_identity`). The batch
  ID can ride the same path next.
- The batch is the natural unit for: culling on a trip, "not backed up" status,
  NAS reconcile, and merging home.

`import_batches` table + `photos.import_batch_id` (schema v9);
`catalog/batches.rs` (create / assign-immutably / list with counts); each scan that
imports new photos creates one batch (source = scanned folder) and assigns only the
new photos; `list_photos` takes a `batch_id` filter; a read-only Batches sidebar
section + a filter-bar chip. **Batch UUID in XMP sidecar** — the scanner
and bundle importer both write `chairphoto:ImportBatch` (merge-safe, beside the
`chairphoto:LastWrite` field) so the batch survives catalog loss and merge.

## Organizational axes (sidebar)

Four independent axes:

1. **Tags** — hierarchical, describe *content* (`Birds/Owls`). Already implemented.
2. **Import batches** — auto, immutable, per-ingest, separate list.
3. **Albums** — manual curated collections; photos from anywhere. (`albums` +
   `album_photos` junction.) Ordered membership, sidebar section,
   add-from-selection; album viewing composes with the culling filter via
   `list_photos(.., album_id, ..)`. Deleting an album never deletes photos.
4. **Smart albums** — rule-based, auto-populated from metadata. currently a list of AND
   conditions over fields (camera, lens, ISO, date, rating, label, pick, tag, batch).
   Nested AND/OR is not implemented.

Import batches and albums share the same "show me this set of photos" plumbing but
are distinct concepts (honoring "don't mix them in the album list").

## Catalog topology & merge

**Decision: (b) desktop-main + laptop-satellite.** Catalogs are **per-machine and
local** (required, since the laptop must work with no NAS — the catalog can't live on
the NAS). The desktop holds the permanent main catalog; the laptop is a satellite.

A **merge** is two independent halves:

1. **Metadata merge** — bring the laptop's records into the desktop catalog
   (photos by UUID, batches, tags, ratings, labels, picks, edits, albums). Fast,
   pure database rows.
2. **Physical reconcile** — copy the trip's originals from the laptop's local disk to
   the NAS, hash-verify, mark Backed up. This is the lifecycle queue with the
   laptop's files as source.

### Additive-only

The laptop only **adds new import batches**; it does not check out existing library
photos. Therefore merge is **additive** — the desktop gains photos it has never seen,
and **no edit conflicts are possible**. The one shared structure is the **tag
taxonomy**, resolved by matching tags on normalized full path (assignments union);
albums merge by name. All non-destructive.

Transport: **bundle file** (a `.chairphoto` export the desktop imports), unit =
import batch. Live-network sync is not implemented.

### Bundle format

A bundle is a single zip archive (`<name>.chairphoto`):

```
<name>.chairphoto  (zip)
├── manifest.json              ← BundleManifest (batch, photos, taxonomy)
├── originals/<relative_path>  ← RAW/JPEG originals, keyed by catalog-relative path
├── originals/<path>.xmp       ← each original's XMP sidecar (carries UUID + ImportBatch)
└── previews/<uuid>.jpg        ← cached JPEG previews (best-effort, optional)
```

**Identity is always UUID, never a path**: photos are keyed by `photos.uuid`, tags by
`tags.uuid`, the batch by `import_batches.uuid`. Paths are hints only.

The merge engine (`catalog/merge.rs`) is **pure-DB, no file I/O**:

| Operation | Strategy |
|-----------|----------|
| Batch | Insert idempotently by uuid; re-merge of the same bundle is a no-op |
| Tag taxonomy | Resolve by tag uuid first, then normalized full_path; create missing ancestors; never overwrite existing uuid / exportable flag |
| Tag terms | `INSERT … ON CONFLICT DO NOTHING` — adds missing terms, never modifies existing |
| Photo (new) | Insert with full state (rating, label, pick, IPTC, edit record, versions) |
| Photo (existing) | **Never touched** — existing rating/label/pick/IPTC/edits/versions are preserved |
| Tag assignments | `INSERT OR IGNORE` union — new assignments added, none removed |

The importer (`bundle/importer.rs`) runs in three phases:
1. **Parse** — open the zip, validate `format_version`.
2. **Copy** (off the catalog lock) — extract `originals/` into `<root>/YYYY/MM/DD/`;
   same-size collision → skip; different-size → rename with ` (n)` suffix; never overwrite.
   Writes a UUID sidecar beside each original so the index phase can match by identity.
3. **Index** (secondary connection, off the main lock) — `upsert_photo_with_identity` for
   each extracted file; run `merge_bundle`; assign newly-created photos to the batch;
   write the batch UUID sidecar; apply auto-tags; reconcile missing.

**Batch UUID in XMP sidecar** (`chairphoto:ImportBatch`): every imported photo's
XMP sidecar carries the batch UUID alongside the photo UUID. This makes the batch
membership survive catalog loss or a catalog merge on a second machine — a re-scan can
reconstruct which batch each photo belongs to from the sidecar alone.

## Export (one-way)

`export::export_photos` resolves each photo's best original via the resolver
and writes to a destination folder by preset — **Hand-off** (RAW + its XMP sidecar) and
**Show off** (JPEG, currently the embedded full-size preview; edited-JPEG rendering
awaits the editor module). Unreachable originals are counted and reported ("N skipped —
connect the NAS"), never silently dropped. Destination collisions get a " (n)" suffix
(the sidecar stays paired). File I/O runs off the UI thread. *Full bundle* (RAW +
previews + catalog metadata) is the merge format; per-language /
hierarchical keyword assembly is covered in docs/taxonomy.md.

**Export is one-way** ("show off" / hand RAWs to someone). Exported
albums on the laptop are **read-only reference**; only new import batches merge back.
Editing an exported photo on the laptop and merging those edits back is the
checkout/two-way-sync case — **deferred**.

Two flavors:

- **Interchange export** (other people / other software): RAW files + **XMP
  sidecars**. Metadata travels inside the sidecars (`dc:subject`,
  `lr:hierarchicalSubject`, `xmp:Rating`, `xmp:Label` — already written, Lightroom/
  darktable read them natively). No chairphoto catalog needed on the far end.
- **chairphoto bundle** (your own laptop): RAW + previews + catalog metadata in a zip;
  the desktop imports with `import_bundle` and everything — photos, ratings, tags,
  versions, IPTC — lands instantly. **Shipped.**

Selectable contents → presets: *Hand-off to editor* (RAW + XMP), *Show off* (JPEG, or
RAW + previews), *Full bundle* (everything).

**Availability dependency**: exporting RAW originals requires them to be reachable.
If the NAS is offline, only previews/JPEGs can be exported — chairphoto must say so
up front ("18 of 50 originals are offline — connect the NAS to include RAWs") rather
than silently exporting a partial set.

## Catalog switching

chairphoto supports multiple named catalogs (one open at a time). Each catalog is an
independent `.chairphoto` SQLite file; switching replaces the in-memory catalog handle
with a different one and resets all frontend state.

### Commands

| Command | Description |
|---------|-------------|
| `create_catalog(name, catalog_path, root)` | Create a new catalog file at `catalog_path` rooted at `root`. Returns an error if the file already exists. Records the new catalog in the recent list. |
| `open_catalog(catalog_path, root)` | Open an existing catalog file. Returns an error if the file does not exist. Records in the recent list. |
| `switch_catalog(catalog_path, root, create, name)` | Safe teardown + reinit: abort any in-flight scan, close the current catalog (WAL flushed), open (or create) the new one, emit `catalog:switched`. |
| `list_recent_catalogs` | Return up to 20 recently-opened catalogs ordered by last-opened (most recent first). |

### Recent-catalog registry

A `recent_catalogs.json` file under `app_data_dir` (`$XDG_DATA_HOME/chairphoto/`)
tracks up to 20 recently-opened catalogs in JSON:

```json
[
  {
    "name": "Main",
    "catalogPath": "/home/user/.local/share/chairphoto/default.chairphoto",
    "root": "/home/user/Pictures/Raw",
    "lastOpened": 1751500000
  }
]
```

Recording the same `catalogPath` again deduplicates the entry and promotes it to the
front of the list (most-recent-first). The list is capped at 20 entries.

### Stored root vs supplied root

A catalog's library root is **persisted in the `settings` table** on first open
(`catalog_root` key). When the same file is opened again, it **adopts the stored root**
regardless of the `root` argument supplied to `Catalog::open`. To change the root after
the fact, call `set_library_root` (which runs an explicit `UPDATE`).

This behavior is intentional: the catalog stays self-contained and portable — moving the
catalog file to a different machine and opening it automatically recovers the original
root setting rather than silently using whatever the caller passed.

### Switch lifecycle

`switch_catalog` performs a safe handoff in four steps:

1. **Abort any in-flight scan** — sets `AppState::scan_abort` (`Arc<AtomicBool>`) so the
   scan's per-file loop exits at its next cancellation point. The scan runs on its own
   `open_secondary` connection, so the mutex is not held and the swap is not blocked by a
   running scan.
2. **Close the current catalog** — drops the `Option<Catalog>` under the mutex. This
   releases the WAL write lock and flushes pending writes before the new catalog is opened.
3. **Open (or create) the new catalog** — off the async executor, so the UI thread is
   never stalled.
4. **Emit `catalog:switched`** — the frontend resets all React state (selection, filters,
   albums, scan progress) and re-queries the new catalog.

The abort flag is cleared after the new catalog is open so subsequent scans on the new
catalog start un-aborted.

### Invariants

- **No cross-catalog writes**: an aborted scan stops at the first cancellation point; any
  writes already committed are durable in the *old* catalog only and never appear in the
  new one (they are separate SQLite files).
- **No dangling state**: between step 2 (close) and step 4 (emit `catalog:switched`) the
  `Option<Catalog>` holds `None`. Any command that calls `with_catalog` during this window
  returns `"No catalog is open"` rather than touching a stale connection.
- **Root follows the catalog**: each catalog remembers its own library root; opening a
  different catalog automatically switches the effective root. Volume health caches are
  invalidated on switch.

## Core vs module split

- **Core**: the location model (photos have N locations on named volumes, with roles
  + availability), the **resolver**, import-batch identity, and the catalog/merge
  primitives. Path resolution is fundamental, so this must be core.
- **Module**: policy and packaging — ingest-from-card rules, auto-backup scheduling,
  verification, offload/eviction, and the export presets/packaging (zip, folder, JPEG
  rendering). JPEG rendering for export leans on the editing module
  (exposure/crop → JPEG).

## Data model

- `volumes` — id, name, per-machine base path, (runtime reachability not stored).
- `photo_locations` — photo_id, volume_id, relative_path, role
  (`primary`/`local_cache`/`backup`/`export`), verified_hash, state.
- `import_batches` — id, uuid, imported_at, source_label, note, count.
  Photos gain `import_batch_id` (and the batch id is mirrored to XMP).
- `albums` — id, name; `album_photos` — album_id, photo_id (manual M:N).
- `smart_albums` — id, name, rule definition (AND conditions).
- `pending_operations` — kind (backup/offload/restore), photo_id, target, status.
- `pending_sidecar_identity` — photo_id, attempts, error, timestamps: photos whose
  UUID is in SQLite but not yet in their sidecar. No uuid column — `photos.uuid` is
  the single source of truth for identity.

## Storage model

- **Volume kinds.** Each volume is **`local`** (fast working disk, e.g.
  `~/Pictures/Raw`) or **`backup`** (NAS/remote, e.g. the ZimaCube at
  `~/ZimaCube/Gallery`). The default catalog-root volume is `local`. Multiple `backup`
  volumes are allowed (a photo is "backed up" if any backup holds a verified copy).
  Implemented.
- **Local-cache eviction = rolling time window.** Local keeps the most recent **X
  months** (default **12**, configurable); the NAS keeps that window **plus everything
  older, forever**. Photos older than the window are offloaded from local (verified NAS
  copy retained) → **Archived**, still browsable via the cached preview. New imports
  land local and are backed up to the NAS. (Implements the Pattern C lifecycle; the
  `Year/month/day` folder layout makes the cutoff a simple date compare.) Offload itself
  is the backup / offload / restore lifecycle.
- **One photo, shown once, prefers local.** A photo is a single catalog row (identity =
  UUID) with N locations; the resolver already prefers a local copy over the NAS copy,
  so a photo present on both shows **once** and reads from local — no duplicates. Making
  this automatic across the two folders (whose relative paths differ) means the
  multi-volume scan/ingest matches the same photo across volumes **by its XMP UUID**,
  not by path (card ingest; underpins bundle merge).
- **Restore (temporary local copy).** Pulling older NAS folders back to local for fast
  editing (e.g. a 2015 shoot) is the `restore` lifecycle op, the inverse of
  offload.

