//! Catalog schema. Translated from the old Python `catalog.py` (schema v3) with
//! two changes required by the AGENTS.md invariants:
//!
//!  1. `photos.uuid` — stable identity for cross-machine catalog merge.
//!  2. Photo `path` is stored RELATIVE to the catalog root (see the
//!     `catalog_root` setting), so a catalog can be remapped on import.

pub const SCHEMA_VERSION: i64 = 22;

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

CREATE TABLE IF NOT EXISTS photo_tags (
    photo_id   INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
    tag_id     INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (photo_id, tag_id)
);

CREATE TABLE IF NOT EXISTS photo_metadata (
    photo_id   INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
    key        TEXT NOT NULL,
    group_name TEXT NOT NULL,
    value      TEXT NOT NULL,
    value_norm TEXT NOT NULL,
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
CREATE TABLE IF NOT EXISTS photo_versions (
    id         INTEGER PRIMARY KEY,
    photo_id   INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    edit_json  TEXT NOT NULL DEFAULT '{}',
    position   INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_photo_versions_photo ON photo_versions(photo_id);

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
CREATE INDEX IF NOT EXISTS idx_photos_folder         ON photos(folder_id);
CREATE INDEX IF NOT EXISTS idx_photos_missing        ON photos(missing);
CREATE INDEX IF NOT EXISTS idx_photos_capture_time   ON photos(capture_time);
CREATE INDEX IF NOT EXISTS idx_photos_uuid           ON photos(uuid);
CREATE INDEX IF NOT EXISTS idx_tags_parent           ON tags(parent_id);
CREATE INDEX IF NOT EXISTS idx_photo_tags_tag        ON photo_tags(tag_id);
CREATE INDEX IF NOT EXISTS idx_photo_metadata_lookup ON photo_metadata(key, value_norm, photo_id);
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
"#;
