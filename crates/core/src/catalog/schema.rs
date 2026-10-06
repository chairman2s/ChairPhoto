//! Catalog schema. Translated from the old Python `catalog.py` (schema v3) with
//! two changes required by the AGENTS.md invariants:
//!
//!  1. `photos.uuid` — stable identity for cross-machine catalog merge.
//!  2. Photo `path` is stored RELATIVE to the catalog root (see the
//!     `catalog_root` setting), so a catalog can be remapped on import.

pub const SCHEMA_VERSION: i64 = 27;

pub const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS indexed_folders (
    id           INTEGER PRIMARY KEY,
    path         TEXT NOT NULL UNIQUE,   -- relative to catalog root
    last_scan_at INTEGER
);

CREATE TABLE IF NOT EXISTS photos (
    id                     INTEGER PRIMARY KEY,
    uuid                   TEXT NOT NULL UNIQUE,
    path                   TEXT NOT NULL UNIQUE,   -- relative to catalog root
    folder_id              INTEGER REFERENCES indexed_folders(id) ON DELETE SET NULL,
    mtime_ns               INTEGER NOT NULL,
    size                   INTEGER NOT NULL,
    extension              TEXT NOT NULL,
    mime_type              TEXT,
    width                  INTEGER,
    height                 INTEGER,
    capture_time           TEXT,
    camera_make            TEXT,
    camera_model           TEXT,
    lens                   TEXT,
    focal_length           REAL,
    aperture               REAL,
    shutter_speed          TEXT,
    iso                    INTEGER,
    gps_latitude           REAL,
    gps_longitude          REAL,
    -- The import batch this photo was first ingested in (immutable). See import_batches.
    import_batch_id        INTEGER REFERENCES import_batches(id) ON DELETE SET NULL,
    -- Pixel-derived B&W flag (1/0), computed from the preview during caching; NULL =
    -- not yet computed. Drives the monochrome auto-tag (camera metadata is unreliable).
    is_grayscale           INTEGER,
    -- Non-destructive user orientation correction, in degrees clockwise (0/90/180/270),
    -- applied ON TOP of the file's EXIF orientation when rendering. The original is never
    -- rewritten; survives rescans. See protocol.rs (render) and commands::rotate_photo.
    user_rotation          INTEGER NOT NULL DEFAULT 0,
    -- The original's EXIF Orientation code (1-8) as exiftool read it at scan/import, or
    -- NULL when unknown (never extracted, or the file carries none). Face boxes are
    -- measured on the EXIF-oriented preview, while MWG face regions refer to the stored,
    -- un-oriented image; this is what converts between the two (#136, schema v26).
    exif_orientation       INTEGER,
    -- Trash: when the user hid this photo, or NULL. A timestamp rather than a boolean so
    -- the trash view can order by it, "empty trash older than N days" is expressible, and
    -- restoring a stack can find the frames that were trashed *together* (cluster B, D8).
    -- Catalog-local, like every other mutable per-photo state — see CONTEXT.md.
    trashed_at             INTEGER,
    -- RAW+JPEG stacking: a derivative (e.g. the camera JPEG) points at its master photo
    -- (the RAW). Children are hidden from the main grid and grouped under the master.
    -- ON DELETE SET NULL so removing the master un-stacks the child (never deletes it).
    stack_parent_id        INTEGER REFERENCES photos(id) ON DELETE SET NULL,
    -- User-authored IPTC Core fields (survive rescans; written to XMP sidecar).
    iptc_description       TEXT NOT NULL DEFAULT '',
    iptc_headline          TEXT NOT NULL DEFAULT '',
    iptc_title             TEXT NOT NULL DEFAULT '',
    iptc_creator           TEXT NOT NULL DEFAULT '',
    iptc_copyright         TEXT NOT NULL DEFAULT '',
    iptc_credit            TEXT NOT NULL DEFAULT '',
    iptc_source            TEXT NOT NULL DEFAULT '',
    iptc_city              TEXT NOT NULL DEFAULT '',
    iptc_state             TEXT NOT NULL DEFAULT '',
    iptc_country           TEXT NOT NULL DEFAULT '',
    iptc_country_code      TEXT NOT NULL DEFAULT '',
    thumbnail_path         TEXT,
    rating                 INTEGER NOT NULL DEFAULT 0 CHECK (rating BETWEEN 0 AND 5),
    color_label            TEXT NOT NULL DEFAULT '',
    pick_state             TEXT NOT NULL DEFAULT 'none' CHECK (pick_state IN ('none','pick','reject')),
    external_editors       TEXT NOT NULL DEFAULT '',
    external_edit_mtime_ns INTEGER,
    missing                INTEGER NOT NULL DEFAULT 0,
    created_at             INTEGER NOT NULL,
    updated_at             INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS tags (
    id             INTEGER PRIMARY KEY,
    -- Stable identity so shared taxonomies merge by id, not by name (see taxonomy.md).
    uuid           TEXT,
    name           TEXT NOT NULL,
    name_norm      TEXT NOT NULL,
    parent_id      INTEGER REFERENCES tags(id) ON DELETE CASCADE,
    full_path      TEXT NOT NULL,
    full_path_norm TEXT NOT NULL UNIQUE,
    -- Description of the tag's meaning. Internal metadata, NOT exported to image
    -- sidecars; travels with shared taxonomies and gives LLM tagging semantic
    -- context. Added via ALTER for existing catalogs (see Catalog::ensure_column).
    description    TEXT NOT NULL DEFAULT '',
    -- Non-null = an AUTO-TAG: membership is computed by this named rule (e.g.
    -- 'monochrome'), applied/maintained by the system, not assigned by hand.
    auto_rule      TEXT,
    -- Last time this tag was applied by hand via assign_tag (quick-tag, inspector,
    -- AI-accept). NOT touched by the auto-tag engine, so it reflects user usage —
    -- drives the "Recently used" quick-tag group. Null = never manually applied.
    last_used_at   INTEGER,
    -- 0 = organizational: never emitted as an export keyword or hierarchical-path
    -- segment (descendants still export). Mirrors the darktable "_"-prefix convention.
    exportable     INTEGER NOT NULL DEFAULT 1,
    -- 1 = private/sensitive (e.g. a person's name): withheld from EXTERNAL/cloud AI
    -- providers (Claude/OpenAI/Gemini) so it never leaves the machine. The LOCAL model
    -- (Ollama) still receives it. Does not affect export, filtering, or display.
    private        INTEGER NOT NULL DEFAULT 0,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS tag_synonyms (
    id           INTEGER PRIMARY KEY,
    tag_id       INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    synonym      TEXT NOT NULL,
    synonym_norm TEXT NOT NULL UNIQUE,
    created_at   INTEGER NOT NULL
);

-- Tag terms: per-tag labels for display, translation, and export. A tag's
-- name/full_path remain the language-neutral internal identity; terms are the
-- human/interop labels. language NULL = neutral; is_primary marks a language's
-- canonical name (a translation); export gates emission. See the taxonomy design.
CREATE TABLE IF NOT EXISTS tag_terms (
    id         INTEGER PRIMARY KEY,
    tag_id     INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    text       TEXT NOT NULL,
    text_norm  TEXT NOT NULL,
    language   TEXT,
    is_primary INTEGER NOT NULL DEFAULT 0,
    export     INTEGER NOT NULL DEFAULT 1,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tag_terms_tag ON tag_terms(tag_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_tag_terms_unique
    ON tag_terms(tag_id, text_norm, coalesce(language, ''));

-- Tombstones for tags removed by a merge (A5). `tags.uuid` exists so shared taxonomies
-- merge by identity rather than by name, which cuts both ways: `catalog/merge.rs` matches
-- an incoming bundle tag by uuid first, so importing a bundle that still carries a merged-
-- away tag would re-create it and quietly undo the reorg. A row here says "this uuid is now
-- that tag", and the bundle importer consults it before creating anything.
--
-- Kept forever and deliberately: it is small (one row per merged tag), and its whole job is
-- to answer a bundle that may be imported years later. A merge that repoints an earlier
-- merge's target rewrites the older rows too, so a chain of merges never strands a uuid.
CREATE TABLE IF NOT EXISTS tag_aliases (
    dead_uuid     TEXT PRIMARY KEY,
    target_tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    -- The path the tag had when it was merged away, for the audit trail: the uuid alone
    -- says nothing to a human reading this table later.
    dead_path     TEXT NOT NULL,
    merged_at     INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tag_aliases_target ON tag_aliases(target_tag_id);

CREATE TABLE IF NOT EXISTS photo_tags (
    photo_id   INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
    tag_id     INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (photo_id, tag_id)
);

-- Every EXIF/IPTC/XMP field the app does not promote to a `photos` column, as EAV. Large:
-- ~274 rows per photo, so a six-figure library holds tens of millions of rows and this is
-- the biggest object in the catalog by a wide margin.
--
-- It carried a `value_norm` column and an `idx_photo_metadata_lookup(key, value_norm,
-- photo_id)` index, staged for a "filter by EXIF key/value" feature that was never built.
-- Measured on the owner's 165,093-photo catalog (A7, 2026-08-17): the index was 1,844 MB —
-- 22% of the whole catalog — the column was written by one INSERT and read by nothing, and
-- together they made metadata inserts ~2.8x slower on every scan of every photo. Both are
-- retired here; an existing catalog drops the index on its next open, and sheds the column
-- when the user compacts (see `Catalog::vacuum`). Rebuilding either is one statement if
-- that feature is ever built.
--
-- Both reads lead with photo_id (`get_photo_metadata`, and the sharpness indexer's AF-point
-- lookup), which the PK autoindex and `idx_photo_metadata_photo` serve.
CREATE TABLE IF NOT EXISTS photo_metadata (
    photo_id   INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
    key        TEXT NOT NULL,
    group_name TEXT NOT NULL,
    value      TEXT NOT NULL,
    PRIMARY KEY (photo_id, key, value)
);

-- Physical-location layer (see docs/storage-and-import.md). A volume is a named
-- storage location with a per-machine base path; a photo may have several
-- locations (e.g. local cache + NAS primary). photos.path remains the catalog-
-- root-relative logical path; locations are where bytes physically live.
CREATE TABLE IF NOT EXISTS volumes (
    id        INTEGER PRIMARY KEY,
    uuid      TEXT NOT NULL UNIQUE,
    name      TEXT NOT NULL UNIQUE,
    base_path TEXT NOT NULL,
    -- 'local' = fast working disk; 'backup' = NAS/remote. Drives per-photo status
    -- and the backup/offload lifecycle. See docs/storage-and-import.md.
    kind      TEXT NOT NULL DEFAULT 'local' CHECK (kind IN ('local','backup'))
);

CREATE TABLE IF NOT EXISTS photo_locations (
    id            INTEGER PRIMARY KEY,
    photo_id      INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
    volume_id     INTEGER NOT NULL REFERENCES volumes(id) ON DELETE CASCADE,
    relative_path TEXT NOT NULL,
    role          TEXT NOT NULL DEFAULT 'primary'
                  CHECK (role IN ('primary','local_cache','backup','export')),
    -- SHA-256 of the copy, set when a backup/restore is hash-verified (lifecycle E3).
    verified_hash TEXT,
    created_at    INTEGER NOT NULL,
    UNIQUE (photo_id, volume_id, role)
);

-- User-defined tag groups for fast tagging (a named set of tags shown as quick
-- buttons). Core feature; groups are ordered, members are ordered within a group.
CREATE TABLE IF NOT EXISTS tag_groups (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL,
    position   INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS tag_group_members (
    group_id INTEGER NOT NULL REFERENCES tag_groups(id) ON DELETE CASCADE,
    tag_id   INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    position INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (group_id, tag_id)
);

-- Reconcile queue (E4): storage ops deferred until the NAS is reachable, drained
-- then. One pending op per (kind, photo); cleared on success, kept with an error on
-- failure. See docs/storage-and-import.md ("Reconcile queue").
CREATE TABLE IF NOT EXISTS pending_operations (
    id         INTEGER PRIMARY KEY,
    kind       TEXT NOT NULL CHECK (kind IN ('backup','offload','restore')),
    photo_id   INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
    status     TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','failed')),
    error      TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    UNIQUE (kind, photo_id)
);

-- Import batches ("negative film roll"): every ingest creates one, auto + immutable.
-- A photo belongs to the batch it was first imported in, forever (photos.import_batch_id).
-- See docs/storage-and-import.md. The batch uuid is also mirrored into each photo's XMP
-- sidecar as chairphoto:ImportBatch (xmp::write_import_batch, called from the scanner and
-- the bundle importer), so the batch survives catalog loss and cross-machine merge.
CREATE TABLE IF NOT EXISTS import_batches (
    id           INTEGER PRIMARY KEY,
    uuid         TEXT NOT NULL UNIQUE,
    source_label TEXT NOT NULL DEFAULT '',
    note         TEXT NOT NULL DEFAULT '',
    created_at   INTEGER NOT NULL
);

-- Manual albums: user-curated collections of photos from anywhere (distinct from
-- import batches and tags — see docs/storage-and-import.md, "Organizational axes").
-- Membership is an explicit, ordered junction; deleting an album drops membership
-- only (never the photos).
CREATE TABLE IF NOT EXISTS albums (
    id         INTEGER PRIMARY KEY,
    uuid       TEXT NOT NULL UNIQUE,
    name       TEXT NOT NULL,
    note       TEXT NOT NULL DEFAULT '',
    position   INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS album_photos (
    album_id   INTEGER NOT NULL REFERENCES albums(id) ON DELETE CASCADE,
    photo_id   INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
    position   INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (album_id, photo_id)
);
CREATE INDEX IF NOT EXISTS idx_album_photos_photo   ON album_photos(photo_id);

-- Smart albums: a saved, named RULE that resolves to a photo set — the dynamic
-- counterpart to manual albums (see docs/smart-albums.md). Membership is evaluated
-- LIVE: rule_json is translated to a SQL WHERE clause ANDed into list_photos on every
-- view, so there is no membership table and no staleness. rule_json is opaque to the
-- schema (the rule_to_sql translator interprets it), same spirit as photo_edits.edit_json.
CREATE TABLE IF NOT EXISTS smart_albums (
    id         INTEGER PRIMARY KEY,
    uuid       TEXT NOT NULL UNIQUE,
    name       TEXT NOT NULL,
    rule_json  TEXT NOT NULL,
    position   INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

-- Non-destructive edit record (see docs/plugin-system.md, "Editing / Develop").
-- Core owns this row but is editing-agnostic: edit_json is an opaque JSON document
-- that editing MODULES namespace and interpret. Core never decodes its meaning; it
-- only stores/serves it and exposes a render hook. Deleted with the photo.
CREATE TABLE IF NOT EXISTS photo_edits (
    photo_id   INTEGER PRIMARY KEY REFERENCES photos(id) ON DELETE CASCADE,
    edit_json  TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

-- Named, non-destructive versions of a photo (different crops/exposures). Each holds an
-- opaque edit record (crop + tone), interpreted by the editing module, not core. The
-- original photo is the implicit unedited base; these are derivatives. See docs/editing.md.
-- Companion files carried alongside a photo's image at one location (cluster B, D2/D5).
-- A copy is the image *plus* its declared companions; this records which ones were
-- actually placed at that location and what the source looked like when they were, so a
-- later pass can tell "carried and current" from "the local one has moved on since".
-- `carried_mtime` is the SOURCE file's mtime at carry time, not the destination's: the
-- question being answered is whether the local file has changed since we copied it.
-- The photos a user should see. Everything that *lists* or *counts* photos for the user
-- reads this instead of `photos`, so the visibility rule lives in one place rather than
-- being re-spelled at every call site (it was spelled 36 times before this view existed).
--
-- Deliberately NOT used by: single-row lookups by id (the caller already has the row and
-- wants it whatever its state), the background indexer queues, and maintenance/purge paths.
-- Those must see hidden photos, and routing them here would silently change behaviour.
--
-- `SELECT *` on purpose: columns are added to `photos` by migration, and the view is
-- re-resolved on use, so it keeps up without a second column list to maintain.
DROP VIEW IF EXISTS photos_visible;
CREATE VIEW photos_visible AS SELECT * FROM photos WHERE missing = 0 AND trashed_at IS NULL;

CREATE TABLE IF NOT EXISTS photo_location_companions (
    location_id   INTEGER NOT NULL REFERENCES photo_locations(id) ON DELETE CASCADE,
    -- File name of the companion at that location (e.g. `DSC1.ARW.xmp`).
    name          TEXT    NOT NULL,
    -- The source companion's mtime, in whole seconds, when it was carried here.
    -- **NULL means never carried**: the scanner found this companion beside a local copy
    -- and there is no copy of it at this location. That is a distinct state from "carried
    -- and possibly out of date", and collapsing the two is what would let a photo whose
    -- edit state exists in exactly one place report as safe (cluster B, D5).
    carried_mtime INTEGER,
    -- The source file's mtime as the scanner last saw it. NULL = not looked at since the
    -- carry. Newer than `carried_mtime` — or present when `carried_mtime` is NULL — means
    -- home is missing an edit this machine has. Recorded rather than computed on read so
    -- the safety summary stays pure SQL and an unreachable NAS cannot slow it down.
    source_mtime_seen INTEGER,
    -- When it was carried. NULL alongside a NULL `carried_mtime`.
    carried_at    INTEGER,
    -- SHA-256 of the bytes the carry confirmed identical on both sides (#257): what home
    -- held then, so an automatic re-backup can tell a copy changed at home since. NULL for
    -- a row recorded before the column existed (added by `ensure_column` there).
    carried_hash  TEXT,
    PRIMARY KEY (location_id, name)
);

-- `changed_seq` orders a photo's versions by when their settings were last written (a save,
-- a commit, a history step, a new or duplicated version) — not a rename or a reorder, which
-- bump `updated_at`. Each write sets it one past the photo's highest, so the most recently
-- changed version has the largest; the automatic Library face is that version (#252,
-- `photo_cover`). A counter, not a time: two writes in one second still order. 0 = never
-- written in this catalog: a version a bundle or catalog merge added to an existing photo,
-- which the automatic face passes over until it is edited here (#252). Added to older
-- catalogs by `ensure_column` and backfilled from `updated_at` there (from 1).
CREATE TABLE IF NOT EXISTS photo_versions (
    id         INTEGER PRIMARY KEY,
    photo_id   INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    edit_json  TEXT NOT NULL DEFAULT '{}',
    position   INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    changed_seq INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_photo_versions_photo ON photo_versions(photo_id);

-- Edit history per version (the Darkroom autosaves every change; docs/editing.md). One row
-- per step: a snapshot of the version's settings after that change, with a short label
-- ("Exposure +0.50"). `seq` counts up from 0 — step 0 is "Before", the settings the
-- version had when its history began, so the first change can always be undone. The head
-- table says which step is current: stepping back moves the head without deleting
-- anything, and the next change drops the steps after it (a list, not a tree). Settings
-- only, never pixels. Local to this catalog: catalog merge and bundle export do not carry
-- it (a version arriving from elsewhere starts its history fresh). Additive and
-- idempotent, so no SCHEMA_VERSION bump: SCHEMA_SQL runs on every open.
CREATE TABLE IF NOT EXISTS photo_version_history (
    id         INTEGER PRIMARY KEY,
    version_id INTEGER NOT NULL REFERENCES photo_versions(id) ON DELETE CASCADE,
    seq        INTEGER NOT NULL,
    label      TEXT NOT NULL,
    edit_json  TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE(version_id, seq)
);
-- A photo's face: the look the Library grid, the Bench and the filmstrip show for it
-- (#252). `pin` says how it is chosen: 0 = automatically — the version whose settings were
-- written last (`photo_versions.changed_seq`), else the original; 1 = pinned to the
-- version in `version_id` ("Use as cover"); 2 = pinned to the original. `version_id` is
-- the face itself, kept current by `catalog::edits` in the transaction of every write
-- that can move it (NULL = the original). `rev` rises on every change of the face — it
-- moving to another version, the face version's settings changing, a pin or unpin, a
-- deletion — so the thumbnail look `"version:rev"` the rows carry is never one shown
-- before for another look. One per photo; gone with the photo. Local to this catalog,
-- like the history. Rows from before #252 were all set by hand: migrated as pinned.
CREATE TABLE IF NOT EXISTS photo_cover (
    photo_id   INTEGER PRIMARY KEY REFERENCES photos(id) ON DELETE CASCADE,
    -- NULL = the original is the face. The row outlives a pinned face's version so `rev`
    -- keeps counting and a later face never reuses a token the views have cached.
    version_id INTEGER REFERENCES photo_versions(id) ON DELETE SET NULL,
    rev        INTEGER NOT NULL DEFAULT 0,
    pin        INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS photo_version_history_head (
    version_id INTEGER PRIMARY KEY REFERENCES photo_versions(id) ON DELETE CASCADE,
    seq        INTEGER NOT NULL
);

-- Where a photo has been published (Instagram, Flickr, SmugMug, …) and WHICH version
-- went out. version_id NULL = the Original (unedited base); version_name is a snapshot
-- so the record still reads correctly after a version is deleted (ON DELETE SET NULL).
-- One row per (photo, platform, version): DIFFERENT versions of the same photo can go to
-- the same platform (each is its own record), while re-posting the SAME version to the
-- same platform upserts. (SQLite treats NULLs as distinct in UNIQUE, so the Original
-- bucket is deduped in `record_publication`, not by the constraint.) The `platform`
-- marker is supplied by the publishing module, never invented by core. See
-- docs/publications.md.
CREATE TABLE IF NOT EXISTS publications (
    id           INTEGER PRIMARY KEY,
    photo_id     INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
    version_id   INTEGER REFERENCES photo_versions(id) ON DELETE SET NULL,
    version_name TEXT,
    platform     TEXT NOT NULL,
    url          TEXT,
    published_at INTEGER NOT NULL,
    created_at   INTEGER NOT NULL,
    UNIQUE (photo_id, platform, version_id)
);
CREATE INDEX IF NOT EXISTS idx_publications_photo ON publications(photo_id);

-- Pending Phase B enrichment queue (I6d): one row per photo that has been
-- indexed by Phase A (metadata_ready = 0) but not yet enriched by Phase B.
-- Cleared row-by-row as Phase B sets metadata_ready = 1. Survives a crash or
-- quit mid-scan so Phase B can auto-resume on the next startup. The
-- photo_id CASCADE means the queue entry is cleaned up automatically if the
-- photo row itself is deleted (e.g. rescan removes it as missing).
CREATE TABLE IF NOT EXISTS pending_enrichment (
    photo_id   INTEGER PRIMARY KEY REFERENCES photos(id) ON DELETE CASCADE,
    queued_at  INTEGER NOT NULL
);

-- Sidecar identity that is in SQLite but NOT yet on disk (schema v21). The binding
-- invariant is that portable identity fields live in both SQLite and the sidecar:
-- `photos.uuid` -> `xmp:Identifier`, and immutable import batch UUID ->
-- `chairphoto:ImportBatch`. When the sidecar write cannot complete (read-only storage,
-- an unparseable existing sidecar, an offline volume, or an identifier conflict we must
-- not clobber), the debt is recorded here instead of being logged and forgotten, and
-- `repair_pending_identity` retries it. One row per photo copy per field; CASCADE
-- clears it with the photo or volume.
--
-- There is deliberately no uuid column: photos.uuid is the single source of truth
-- for identity, and import batch UUID is derived from photos.import_batch_id ->
-- import_batches.uuid. A copy here could disagree with either. The target is the same
-- volume-relative location model as photo_locations; `error` is the last failure reason,
-- kept for the UI and for diagnosing storage that never becomes writable.
CREATE TABLE IF NOT EXISTS pending_sidecar_identity (
    photo_id        INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
    field           TEXT NOT NULL DEFAULT 'identifier'
                    CHECK (field IN ('identifier', 'import_batch')),
    volume_id       INTEGER NOT NULL REFERENCES volumes(id) ON DELETE CASCADE,
    relative_path   TEXT NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 1,
    error           TEXT NOT NULL DEFAULT '',
    queued_at       INTEGER NOT NULL,
    last_attempt_at INTEGER NOT NULL,
    -- Schema v22 (#33). When non-zero, a human decided to stop retrying this (copy, field):
    -- the row is kept for the record, the repair pass skips it, and it stops counting as
    -- debt (CONTEXT.md § Identity, "Dismiss"). Only conflicts can be dismissed today, and
    -- the decision is reversible (Restore sets it back to 0). Existing catalogs get the
    -- column from `Catalog::ensure_column`, defaulting every queued row to "not dismissed".
    dismissed_at    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(photo_id, field, volume_id, relative_path)
);

CREATE INDEX IF NOT EXISTS idx_pending_sidecar_identity_photo ON pending_sidecar_identity(photo_id);
-- Covers Catalog::list_pending_identity_page's copy-grouped paging query: GROUP BY +
-- ORDER BY photo_id, volume_id, relative_path, LIMIT/OFFSET. `EXPLAIN QUERY PLAN` confirms
-- this index lets SQLite drive that whole query as an ordered index scan with no
-- `USE TEMP B-TREE FOR ORDER BY` — without it, every page turn sorts the full matching set
-- while `with_catalog_blocking` holds the shared catalog mutex. It does not make deep
-- offsets cheap (SQLite's `OFFSET` still walks the skipped rows off an index), only the
-- sort: measured end to end on a 74,488-row table (50,000 distinct copies, 24,488 owing
-- both fields), `LIMIT 500` — ~5ms at offset 0, ~45ms at offset 25,000, ~73ms at offset
-- 49,500 (see Catalog::list_pending_identity_page's doc for the full measurement).
-- No SCHEMA_VERSION bump needed: this file (SCHEMA_SQL) runs unconditionally via
-- `execute_batch` on every catalog open (see `Catalog::migrate_locked`), and
-- `CREATE INDEX IF NOT EXISTS` is naturally idempotent, so an existing catalog picks this
-- up the next time it opens. On a catalog still at schema v20 (no `field` column on
-- `pending_sidecar_identity`), `Catalog::migrate_sidecar_identity_fields` drops and
-- rebuilds this table AFTER this file runs, which would otherwise drop this index right
-- back out for that one session — it explicitly recreates both this index and
-- `idx_pending_sidecar_identity_photo` above once the rebuild is done, so a v20 catalog
-- has both on its very first open, not just "eventually, next time".
CREATE INDEX IF NOT EXISTS idx_pending_sidecar_identity_copy ON pending_sidecar_identity(photo_id, volume_id, relative_path);
CREATE INDEX IF NOT EXISTS idx_photo_locations_photo  ON photo_locations(photo_id);
-- The names a folder's locations hold on a volume (#247, `Catalog::names_held_in`): an
-- import reads them once per date folder, a range on `relative_path` under one volume.
CREATE INDEX IF NOT EXISTS idx_photo_locations_volume_path ON photo_locations(volume_id, relative_path);
CREATE INDEX IF NOT EXISTS idx_photos_folder         ON photos(folder_id);
CREATE INDEX IF NOT EXISTS idx_photos_missing        ON photos(missing);
CREATE INDEX IF NOT EXISTS idx_photos_capture_time   ON photos(capture_time);
CREATE INDEX IF NOT EXISTS idx_photos_uuid           ON photos(uuid);
CREATE INDEX IF NOT EXISTS idx_tags_parent           ON tags(parent_id);
CREATE INDEX IF NOT EXISTS idx_photo_tags_tag        ON photo_tags(tag_id);
CREATE INDEX IF NOT EXISTS idx_photo_metadata_photo  ON photo_metadata(photo_id);

-- The library grid's default ordering, as an INDEX ON AN EXPRESSION.
--
-- `catalog::query::order_by`'s Date sort is not a column — it is
-- `COALESCE(CAST(strftime('%s', capture_time) AS INTEGER), mtime_ns / 1000000000)`,
-- because photos with no EXIF capture time fall back to file mtime so they still slot
-- into the timeline. `idx_photos_capture_time` indexes the bare column and therefore
-- cannot satisfy it, so before this index every library listing sorted the WHOLE matching
-- set in a temp B-tree just to return one 200-row window: measured 176 ms over 144,242
-- rows on a 165k-photo catalog, versus 0.59 ms with this index (agent measurement,
-- 2026-08-17). Deep windows improved 410 ms -> 110 ms.
--
-- The three expressions must stay BYTE-IDENTICAL to `order_by`'s Date arm (modulo the
-- `p.` alias, which SQLite resolves away). SQLite matches an expression index by
-- comparing the parsed expression: change the ORDER BY without changing this and the
-- index silently stops being used — the query still returns correct rows, just slowly.
-- `tests` asserts the plan has no temp B-tree, which is what catches that drift.
--
-- `strftime` is deterministic (no 'now' argument), which is what makes it legal in an
-- index expression at all.
--
-- This index is INERT WITHOUT PLANNER STATISTICS. Measured on a 165k-photo catalog: with
-- the index present and no `sqlite_stat1`, SQLite still chose the temp-B-tree sort and
-- the query still took 167 ms; after `ANALYZE photos` it took 3 ms. Creating the index is
-- therefore only half the change — see `Catalog::optimize` and its two call sites.
CREATE INDEX IF NOT EXISTS idx_photos_sort_date ON photos(
    COALESCE(CAST(strftime('%s', capture_time) AS INTEGER), mtime_ns / 1000000000),
    path COLLATE NOCASE,
    id
);

-- Schema v23 (#146). The identifier a photo held as `photos.uuid` before it was re-minted
-- because it was not a UUID: a DAM asset id that a scan adopted from a sidecar before #141.
-- That sidecar still carries it (it is somebody else's, so only a person may overwrite it),
-- so it is the one link between the file and its row until the conflict is resolved: a scan
-- re-homes a moved file onto the row holding its legacy identifier, but only when that is
-- the only such row and every primary copy it records is gone (`Catalog::scan_identity`).
-- Never a merge key and never written to a sidecar; one row per re-minted photo.
CREATE TABLE IF NOT EXISTS photo_legacy_identifiers (
    photo_id   INTEGER PRIMARY KEY REFERENCES photos(id) ON DELETE CASCADE,
    identifier TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_photo_legacy_identifiers ON photo_legacy_identifiers(identifier);

-- Schema v25 (#148). Authored IPTC that is in SQLite but NOT yet in the photo's sidecar.
-- A sidecar write runs after the catalog commit and writes only the fields it owes, so a
-- write that fails after the store (read-only storage, an unparseable sidecar, a volume
-- unmounting mid-save) would otherwise leave a field the catalog holds and no later save
-- writes. `owed` is a bitmask of `IptcMask` fields — its bit numbering is persisted here
-- and must never change. `Catalog::set_iptc` ORs the fields it changed into `owed` and
-- bumps `generation` in the same transaction as the store; a successful write clears
-- `owed` only while `generation` is still the one it wrote (a compare-and-set), so a newer
-- save's fields are never cleared by an older write. Rows are kept at `owed = 0` rather
-- than deleted, so `generation` only ever grows for a photo. `attempts`/`error` describe
-- the last failed write, for diagnosis. Retried by the identity repair pass.
CREATE TABLE IF NOT EXISTS pending_sidecar_iptc (
    photo_id        INTEGER PRIMARY KEY REFERENCES photos(id) ON DELETE CASCADE,
    owed            INTEGER NOT NULL DEFAULT 0 CHECK (owed >= 0 AND owed < 2048),
    generation      INTEGER NOT NULL DEFAULT 1,
    attempts        INTEGER NOT NULL DEFAULT 0,
    error           TEXT NOT NULL DEFAULT '',
    queued_at       INTEGER NOT NULL,
    last_attempt_at INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_pending_sidecar_iptc_owed ON pending_sidecar_iptc(photo_id) WHERE owed != 0;

-- Schema v27 (#248). Which bundle has already been merged into which photo. A bundle is its
-- import batch (`import_batches.uuid`, as the manifest carries it) exported at a time
-- (`BundleManifest::created_at`, Unix seconds): a later export of the same batch carries the
-- work done since and is another bundle. A bundle fills only an existing photo's blanks, and
-- cannot tell "never set" from "the user cleared it": without this, importing the same bundle
-- again would bring back a rating, label, pick, IPTC value or tag the user removed, and a
-- version the user deleted. `Catalog::merge_bundle_into` records every photo it merges a
-- bundle's photo into — inserted, created by the importer for the bundle, or filled in — and
-- applies nothing of that bundle to a photo already recorded (no fill, no version, no tag).
-- Local to this catalog: never exported or merged. No backfill: which bundles earlier
-- imports merged is not recorded anywhere. (An unreleased first shape keyed on the batch
-- alone is dropped and recreated by `Catalog::migrate_locked`.)
CREATE TABLE IF NOT EXISTS bundle_merges (
    photo_id          INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
    batch_uuid        TEXT NOT NULL,
    bundle_created_at INTEGER NOT NULL,
    merged_at         INTEGER NOT NULL,
    PRIMARY KEY (photo_id, batch_uuid, bundle_created_at)
) WITHOUT ROWID;
"#;
