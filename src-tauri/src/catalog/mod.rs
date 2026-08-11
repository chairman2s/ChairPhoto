//! The catalog: a single SQLite file that is the source of truth for photos,
//! tags, ratings, and metadata.
//!
//! Photo paths are stored RELATIVE to the catalog root (the `catalog_root`
//! setting). All absolute-path conversion happens at this boundary so the rest
//! of the app never hard-codes machine-specific paths — this is what makes a
//! catalog portable between the laptop and the desktop.

mod albums;
mod autotags;
mod batches;
mod edits;
mod facets;
mod groups;
mod identity;
mod lifecycle;
mod merge;
mod reconcile;
pub mod locations;
mod models;
mod publications;
mod schema;
mod smart_albums;
mod stats;
mod tag_path;
mod terms;

#[cfg(test)]
mod performance_harness;

pub use facets::{Facet, SOFT_THRESHOLD_DEFAULT, SOFT_THRESHOLD_KEY};
pub use identity::{
    bind_sidecar_identity, IdentityRepairPlan, IdentityRepairSummary, PendingIdentity,
    PendingIdentitySummary, SidecarIdentity,
};
pub use locations::PathCandidate;
pub use lifecycle::{copy_and_verify, verify_and_delete_locals, BackupPlan, OffloadPlan, RestorePlan};
pub use merge::MergeSummary;
pub use models::{
    Album, BurstInput, ExportKeywords, ImportBatch, IptcFields, LocationRole, MetadataEntry,
    PendingOperation, Photo, PhotoLocation, PhotoVersion, PickState, PromotedMetadata,
    Publication, StorageStatus, SmartAlbum, Tag, TagGroup, TagTerm, TagWithCount, Volume,
    VolumeKind,
};
pub use smart_albums::rule_to_sql;
pub use reconcile::DrainSummary;

use rusqlite::{params, Connection, ErrorCode, OptionalExtension, Row};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tag_path::{normalize_lookup, normalize_tag_path};

pub type Result<T> = std::result::Result<T, CatalogError>;

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid tag: {0}")]
    Tag(String),
    #[error("invalid input: {0}")]
    Validation(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("path {0} is outside the catalog root")]
    OutsideRoot(String),
    #[error("failed to enable WAL mode; SQLite reported journal_mode={0}")]
    JournalMode(String),
}

// Match the 60-second migration lock wait; catalog opens run on blocking workers.
const WAL_BUSY_RETRY_TIMEOUT: Duration = Duration::from_secs(60);
const WAL_BUSY_RETRY_DELAY: Duration = Duration::from_millis(10);

// SQLite's bind-parameter ceiling (`SQLITE_MAX_VARIABLE_NUMBER`) depends on the build:
// 32766 since 3.32, and 999 in older builds. We bundle 3.45, but the chunk stays below
// the older 999 too, so chunked `IN` queries hold if this ever links a system SQLite.
// A 100k-photo grid refresh passes every returned id, which exceeds both ceilings.
pub(crate) const SQLITE_PARAM_CHUNK: usize = 900;

pub(crate) fn sqlite_param_placeholders(count: usize) -> String {
    vec!["?"; count].join(",")
}

fn enable_wal(conn: &Connection) -> Result<()> {
    let started = Instant::now();
    loop {
        match conn.query_row("PRAGMA journal_mode = WAL;", [], |row| {
            row.get::<_, String>(0)
        }) {
            Ok(mode) if mode.eq_ignore_ascii_case("wal") => return Ok(()),
            Ok(mode) if started.elapsed() >= WAL_BUSY_RETRY_TIMEOUT => {
                return Err(CatalogError::JournalMode(mode));
            }
            Ok(_) => {}
            Err(error) => {
                if error.sqlite_error_code() != Some(ErrorCode::DatabaseBusy)
                    || started.elapsed() >= WAL_BUSY_RETRY_TIMEOUT
                {
                    return Err(error.into());
                }
            }
        }

        // Concurrent WAL activation can form a lock-upgrade deadlock. SQLite
        // deliberately skips the busy handler in that case, so finalize this
        // statement and retry after the competing connection can progress.
        thread::sleep(WAL_BUSY_RETRY_DELAY);
    }
}

pub struct Catalog {
    conn: Connection,
    root: PathBuf,
    path: PathBuf,
}

impl Catalog {
    /// Open (or create) a catalog file. `root` is the folder photo paths are
    /// stored relative to; it is persisted so a reopened catalog keeps the same
    /// root, and can be overridden on import to remap a catalog from another machine.
    pub fn open(catalog_path: &Path, root: &Path) -> Result<Self> {
        if let Some(parent) = catalog_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(catalog_path)?;
        // busy_timeout: with WAL there's one writer at a time across connections; a
        // dedicated scan connection (see `open_secondary`) may hold the write lock, so
        // wait a few seconds for it rather than erroring "database is locked" instantly.
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        enable_wal(&conn)?;
        conn.execute_batch("PRAGMA busy_timeout = 5000;")?;
        let mut catalog = Self {
            conn,
            root: root.to_path_buf(),
            path: catalog_path.to_path_buf(),
        };
        catalog.migrate()?;
        Ok(catalog)
    }

    /// Open an ADDITIONAL connection to an already-open catalog, WITHOUT re-running
    /// migration. Used to give a long scan its own connection so the primary connection
    /// keeps serving reads (grid, thumbnails) concurrently under WAL — the UI stays
    /// responsive instead of blocking on the shared catalog lock for the whole scan.
    pub fn open_secondary(catalog_path: &Path, root: &Path) -> Result<Self> {
        let conn = Connection::open(catalog_path)?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        enable_wal(&conn)?;
        conn.execute_batch("PRAGMA busy_timeout = 5000;")?;
        Ok(Self {
            conn,
            root: root.to_path_buf(),
            path: catalog_path.to_path_buf(),
        })
    }

    /// The catalog database file path (used to open a secondary connection for scans).
    pub fn db_path(&self) -> &Path {
        &self.path
    }

    fn migrate(&mut self) -> Result<()> {
        // Two connections can run this concurrently: the frontend's startup
        // `init_catalog` is double-invoked under React StrictMode in dev, and a catalog
        // switch can race a slow first open. `ensure_column`'s check-then-ALTER is not
        // atomic across connections, so the loser used to die with "duplicate column
        // name" (seen live with `phash`, 2026-07-07). BEGIN EXCLUSIVE serializes the
        // whole migration: the second connection waits on the write lock and then
        // observes the winner's columns, making every ensure_* check race-free.
        // Migrations with version-gated backfills can outlast the normal 5s busy
        // timeout on a large catalog, so widen it for the wait and restore it after.
        self.conn.execute_batch("PRAGMA busy_timeout = 60000; BEGIN EXCLUSIVE;")?;
        let out = self.migrate_locked();
        let end = if out.is_ok() { "COMMIT" } else { "ROLLBACK" };
        let ended = self.conn.execute_batch(end);
        self.conn.execute_batch("PRAGMA busy_timeout = 5000;")?;
        ended?;
        out
    }

    fn migrate_locked(&mut self) -> Result<()> {
        self.conn.execute_batch(schema::SCHEMA_SQL)?;
        // Additive columns for catalogs created before the column existed.
        self.ensure_column("tags", "description", "TEXT NOT NULL DEFAULT ''")?;
        self.ensure_column("tags", "auto_rule", "TEXT")?;
        self.ensure_column("tags", "uuid", "TEXT")?;
        // Per-tag export gate: 0 = organizational, never emitted as a keyword (its
        // descendants still export). Mirrors the darktable "_"-prefix convention.
        self.ensure_column("tags", "exportable", "INTEGER NOT NULL DEFAULT 1")?;
        // Per-tag privacy gate (schema v19): 1 = withheld from external/cloud AI
        // (people's names etc.); the local model still gets it. See tag_private.
        self.ensure_column("tags", "private", "INTEGER NOT NULL DEFAULT 0")?;
        // Stable tag UUIDs: backfill any missing, then enforce uniqueness. The index
        // is created here (not in SCHEMA_SQL) because the column may have just been
        // added by ensure_column on an older catalog.
        let tags_without_uuid: Vec<i64> = {
            let mut stmt = self
                .conn
                .prepare("SELECT id FROM tags WHERE uuid IS NULL OR uuid = ''")?;
            let ids = stmt
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            ids
        };
        for id in tags_without_uuid {
            self.conn.execute(
                "UPDATE tags SET uuid = ?1 WHERE id = ?2",
                params![uuid::Uuid::new_v4().to_string(), id],
            )?;
        }
        self.conn
            .execute_batch("CREATE UNIQUE INDEX IF NOT EXISTS idx_tags_uuid ON tags(uuid);")?;
        self.ensure_column("photos", "gps_latitude", "REAL")?;
        self.ensure_column("photos", "gps_longitude", "REAL")?;
        // Volume kind (local | backup) for the storage lifecycle (schema v8). The
        // CHECK(kind IN (...)) in SCHEMA_SQL can't be carried by ALTER TABLE ADD
        // COLUMN, so upgraded catalogs lack the DB-level constraint; the Rust writer
        // (VolumeKind::as_db_str) is the only writer and always yields a valid value.
        self.ensure_column("volumes", "kind", "TEXT NOT NULL DEFAULT 'local'")?;
        // Import batch membership (schema v9). Column added here (not just SCHEMA_SQL)
        // for existing catalogs; the index follows once the column is guaranteed.
        self.ensure_column("photos", "import_batch_id", "INTEGER")?;
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_photos_import_batch ON photos(import_batch_id);",
        )?;
        // Verified-hash on locations for the backup/offload lifecycle (schema v10).
        self.ensure_column("photo_locations", "verified_hash", "TEXT")?;
        // Pixel-derived B&W flag for the monochrome auto-tag (schema v12).
        self.ensure_column("photos", "is_grayscale", "INTEGER")?;
        // Non-destructive user orientation override (degrees clockwise), schema v17.
        self.ensure_column("photos", "user_rotation", "INTEGER NOT NULL DEFAULT 0")?;
        // RAW+JPEG stacking: a derivative photo points at its master (schema v18).
        self.ensure_column(
            "photos",
            "stack_parent_id",
            "INTEGER REFERENCES photos(id) ON DELETE SET NULL",
        )?;
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_photos_stack ON photos(stack_parent_id);",
        )?;
        // Phase A/B boundary for two-phase live scan (I6a): 0 = awaiting metadata extraction,
        // 1 = metadata ready for display. Existing rows default 1 so grid is unaffected.
        self.ensure_column("photos", "metadata_ready", "INTEGER NOT NULL DEFAULT 1")?;
        // Tiled sharpness score (H16b): the ~90th-percentile Laplacian-variance tile score
        // from the ~1024–2048px preview (NULL = not yet scored). sharpness_method records how
        // the score was computed ('tile' / 'face' / 'afpoint'), so UI thresholds can differ
        // per method. Both are core columns — not a plugin table — because the `soft` facet
        // (H16d) and burst-relative ranking (H16e) depend on them in core queries.
        self.ensure_column("photos", "sharpness", "REAL")?;
        self.ensure_column("photos", "sharpness_method", "TEXT")?;
        // 64-bit perceptual hash (dHash) of the photo, computed once from the cached
        // preview/thumbnail decode (H15a). NULL = not yet hashed. Core infra reused for
        // burst grouping (H15), near-duplicate detection, and auto-stacks — not a plugin
        // table. Stored as INTEGER; SQLite's i64 holds the full 64-bit value (top bit
        // becomes the sign, which is fine — comparisons use Hamming distance, not order).
        self.ensure_column("photos", "phash", "INTEGER")?;
        // Burst-relative sharpness flag (H16e): set by `analyze_burst_sharpness` over an
        // H15 cluster. Values: NULL = not analysed, 'soft-in-burst' = scored below ~60% of
        // the cluster median, 'sharpest-of-burst' = the sharpest frame in the cluster.
        // Recomputed (overwritten) on each explicit analysis run — not magic, not automatic.
        self.ensure_column("photos", "burst_flag", "TEXT")?;
        // Manual-usage timestamp driving the "Recently used" quick-tag group (schema v13).
        self.ensure_column("tags", "last_used_at", "INTEGER")?;
        for col in [
            "iptc_description",
            "iptc_headline",
            "iptc_title",
            "iptc_creator",
            "iptc_copyright",
            "iptc_credit",
            "iptc_source",
            "iptc_city",
            "iptc_state",
            "iptc_country",
            "iptc_country_code",
        ] {
            self.ensure_column("photos", col, "TEXT NOT NULL DEFAULT ''")?;
        }

        // Persist the root the first time; keep any existing value otherwise.
        let root_str = self.root.to_string_lossy().to_string();
        self.conn.execute(
            "INSERT INTO settings(key, value) VALUES('catalog_root', ?1)
             ON CONFLICT(key) DO NOTHING",
            params![root_str],
        )?;
        // Adopt the stored root (relevant when reopening an existing catalog).
        if let Some(stored) = self.get_setting("catalog_root")? {
            self.root = PathBuf::from(stored);
        }

        // Versioned migrations. Read the prior version BEFORE stamping the new one.
        let prior_version: i64 = self
            .get_setting("schema_version")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        self.migrate_sidecar_identity_fields()?;
        if prior_version < 2 {
            // Introduce the physical-location layer: default volume + backfill.
            self.backfill_default_volume()?;
        }
        if prior_version < 15 {
            // Publications replace the old flat "instagram" tag: seed one Instagram
            // publication (Original) per photo that carried that tag, dated to when the
            // tag was applied. The tag itself is left in place (don't delete user data).
            self.backfill_instagram_publications()?;
        }
        if prior_version < 18 {
            // One-time: stack each derivative JPEG under its sibling RAW (same folder +
            // same filename stem). Future imports are paired by the scanner.
            let _ = self.pair_raw_jpeg_stacks();
        }
        // Keep the catalog-root (local) volume pointing at the current root, so
        // re-rooting the catalog moves it too.
        self.sync_default_volume_root()?;

        self.set_setting("schema_version", &schema::SCHEMA_VERSION.to_string())?;
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Raw connection for in-crate plugins that own their own prefix-named tables
    /// (e.g. `ai__suggestions`). Core defines no plugin tables; see docs/plugin-system.md.
    pub(crate) fn conn(&self) -> &rusqlite::Connection {
        &self.conn
    }

    /// Add a column to a table if it doesn't already exist (idempotent migration).
    fn ensure_column(&self, table: &str, column: &str, declaration: &str) -> Result<()> {
        let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let existing: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if !existing.iter().any(|c| c == column) {
            self.conn
                .execute(&format!("ALTER TABLE {table} ADD COLUMN {column} {declaration}"), [])?;
        }
        Ok(())
    }

    /// Schema v21: generalize the sidecar repair queue from one UUID debt per copy to
    /// one sidecar-field debt per copy. SQLite cannot alter a primary key in place, so
    /// v20 catalogs are rebuilt and their existing rows become `field = 'identifier'`.
    fn migrate_sidecar_identity_fields(&self) -> Result<()> {
        let mut stmt = self.conn.prepare("PRAGMA table_info(pending_sidecar_identity)")?;
        let columns: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if columns.iter().any(|column| column == "field") {
            return Ok(());
        }

        self.conn.execute_batch(
            "CREATE TABLE pending_sidecar_identity_v21 (
                photo_id        INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
                field           TEXT NOT NULL DEFAULT 'identifier'
                                CHECK (field IN ('identifier', 'import_batch')),
                volume_id       INTEGER NOT NULL REFERENCES volumes(id) ON DELETE CASCADE,
                relative_path   TEXT NOT NULL,
                attempts        INTEGER NOT NULL DEFAULT 1,
                error           TEXT NOT NULL DEFAULT '',
                queued_at       INTEGER NOT NULL,
                last_attempt_at INTEGER NOT NULL,
                PRIMARY KEY(photo_id, field, volume_id, relative_path)
             );
             INSERT OR IGNORE INTO pending_sidecar_identity_v21
                (photo_id, field, volume_id, relative_path, attempts, error, queued_at, last_attempt_at)
             SELECT photo_id, 'identifier', volume_id, relative_path, attempts, error,
                    queued_at, last_attempt_at
             FROM pending_sidecar_identity;
             DROP TABLE pending_sidecar_identity;
             ALTER TABLE pending_sidecar_identity_v21 RENAME TO pending_sidecar_identity;
             CREATE INDEX IF NOT EXISTS idx_pending_sidecar_identity_photo
                ON pending_sidecar_identity(photo_id);",
        )?;
        Ok(())
    }

    /// One-time migration to schema v15: turn the legacy flat "instagram" tag into
    /// Instagram publications (Original version), dated to when the tag was applied.
    /// Idempotent (INSERT OR IGNORE against the (photo, platform) unique key).
    fn backfill_instagram_publications(&self) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO publications
                 (photo_id, version_id, version_name, platform, url, published_at, created_at)
             SELECT pt.photo_id, NULL, NULL, 'instagram', NULL, pt.created_at, ?1
             FROM photo_tags pt
             JOIN tags t ON t.id = pt.tag_id
             WHERE t.full_path_norm = ?2",
            params![now(), normalize_lookup("instagram")],
        )?;
        Ok(())
    }

    // --- settings -----------------------------------------------------------

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Current on-disk size of the catalog in bytes (page_count × page_size).
    pub fn db_size_bytes(&self) -> Result<i64> {
        let pages: i64 = self.conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let page_size: i64 = self.conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        Ok(pages * page_size)
    }

    /// Rebuild the database file, reclaiming free space left by deletions and
    /// defragmenting. Rewrites the whole file (needs ~2× space transiently) and holds the
    /// connection for the duration — run it off the UI thread. Data is unchanged.
    pub fn vacuum(&self) -> Result<()> {
        self.conn.execute_batch("VACUUM")?;
        Ok(())
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM settings WHERE key = ?1", params![key], |r| {
                r.get::<_, String>(0)
            })
            .optional()?)
    }

    // --- path conversion (the relative/absolute boundary) -------------------

    /// Convert an absolute path under the catalog root to its stored relative form.
    pub fn to_relative(&self, absolute: &Path) -> Result<String> {
        let rel = absolute
            .strip_prefix(&self.root)
            .map_err(|_| CatalogError::OutsideRoot(absolute.display().to_string()))?;
        Ok(rel.to_string_lossy().replace('\\', "/"))
    }

    /// Resolve a stored relative path back to an absolute path on this machine.
    pub fn to_absolute(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    /// Begin a transaction on the shared connection. Callers run a batch of writes
    /// (e.g. a whole scan) and `commit()` once, so the dozens of per-photo writes
    /// share a single commit instead of one fsync each (WAL mode). The returned guard
    /// rolls back if dropped without committing. Safe to interleave with the catalog's
    /// own `&self` write methods — they run on the same connection and so participate
    /// in this transaction.
    pub fn begin(&self) -> Result<rusqlite::Transaction<'_>> {
        Ok(self.conn.unchecked_transaction()?)
    }

    // --- photos -------------------------------------------------------------

    /// Insert a photo if new (assigning a fresh UUID), or update its file stats
    /// if it already exists. Returns the photo id and whether a UUID was created.
    ///
    /// The caller is responsible for writing the UUID to the XMP sidecar on first
    /// import — see the AGENTS.md "Photo UUID" invariant.
    pub fn upsert_photo(
        &self,
        absolute_path: &Path,
        folder_id: Option<i64>,
        mtime_ns: i64,
        size: i64,
    ) -> Result<UpsertResult> {
        self.upsert_photo_with_identity(absolute_path, folder_id, mtime_ns, size, None)
    }

    /// Upsert a scanned photo, matching an existing row by path first, then — when given
    /// the file's own UUID from its sidecar — by that UUID. The UUID match re-homes a
    /// *moved or re-rooted* file onto its existing row (path changed, identity didn't)
    /// instead of creating a duplicate. When there's no match, a new row is created; it
    /// adopts the sidecar UUID if one was supplied, else mints a fresh one.
    pub fn upsert_photo_with_identity(
        &self,
        absolute_path: &Path,
        folder_id: Option<i64>,
        mtime_ns: i64,
        size: i64,
        sidecar_uuid: Option<&str>,
    ) -> Result<UpsertResult> {
        let rel = self.to_relative(absolute_path)?;
        let extension = absolute_path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let ts = now();

        // 1) A file at this exact (relative) path → update it. Also read its stored file
        //    stats so we can tell the caller whether the bytes actually changed (lets a
        //    re-scan skip re-extracting metadata for untouched files).
        let by_path: Option<(i64, String, i64, i64)> = self
            .conn
            .query_row(
                "SELECT id, uuid, mtime_ns, size FROM photos WHERE path = ?1",
                params![rel],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;

        // 2) Otherwise, if the file carries a UUID, an existing row with that UUID is the
        //    same photo that has moved/re-rooted → re-home it (no duplicate).
        let by_uuid: Option<i64> = match (by_path.is_some(), sidecar_uuid) {
            (false, Some(uuid)) => self
                .conn
                .query_row(
                    "SELECT id FROM photos WHERE uuid = ?1",
                    params![uuid],
                    |r| r.get(0),
                )
                .optional()?,
            _ => None,
        };

        let result = if let Some((id, uuid, old_mtime, old_size)) = by_path {
            let unchanged = old_mtime == mtime_ns && old_size == size;
            self.conn.execute(
                "UPDATE photos SET folder_id = ?1, mtime_ns = ?2, size = ?3,
                    extension = ?4, missing = 0, updated_at = ?5 WHERE id = ?6",
                params![folder_id, mtime_ns, size, extension, ts, id],
            )?;
            UpsertResult { id, uuid, created: false, unchanged }
        } else if let Some(id) = by_uuid {
            // Re-home the moved file: point the existing row at the new path.
            self.conn.execute(
                "UPDATE photos SET path = ?1, folder_id = ?2, mtime_ns = ?3, size = ?4,
                    extension = ?5, missing = 0, updated_at = ?6 WHERE id = ?7",
                params![rel, folder_id, mtime_ns, size, extension, ts, id],
            )?;
            UpsertResult {
                id,
                uuid: sidecar_uuid.unwrap_or_default().to_string(),
                created: false,
                unchanged: false,
            }
        } else {
            // New photo. Adopt the file's existing UUID (sidecar) if it has one, so its
            // identity is preserved across machines; otherwise mint a fresh one.
            let uuid = sidecar_uuid
                .map(str::to_string)
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            self.conn.execute(
                "INSERT INTO photos(uuid, path, folder_id, mtime_ns, size, extension,
                    missing, created_at, updated_at)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, ?7)",
                params![uuid, rel, folder_id, mtime_ns, size, extension, ts],
            )?;
            UpsertResult {
                id: self.conn.last_insert_rowid(),
                uuid,
                created: true,
                unchanged: false,
            }
        };

        // Record where the bytes physically are, so the resolver can find them.
        self.set_primary_location(result.id, absolute_path)?;
        Ok(result)
    }

    /// Index a photo that physically lives on a non-root volume (e.g. a pre-existing
    /// archive on the NAS), **in place** — no local copy is made. The volume-relative
    /// path becomes the logical `path`, and the file's location is recorded on that
    /// volume (role Primary). Since there's no local copy, the photo shows as living on
    /// the NAS ("On NAS"). Matches an existing row by an existing location at this
    /// volume+path, then by sidecar UUID, else creates a new row (folder_id null — it's
    /// not under an indexed local folder). See `scanner::scan_external_folder`.
    pub fn upsert_photo_on_volume(
        &self,
        absolute: &Path,
        mtime_ns: i64,
        size: i64,
        sidecar_uuid: Option<&str>,
    ) -> Result<UpsertResult> {
        let (volume_id, rel) = self.volume_for_path(absolute)?;
        let extension = absolute
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let ts = now();

        // 1) A photo already located on this volume at this path → update it.
        let by_loc: Option<(i64, String, i64, i64)> = self
            .conn
            .query_row(
                "SELECT p.id, p.uuid, p.mtime_ns, p.size
                 FROM photo_locations l JOIN photos p ON p.id = l.photo_id
                 WHERE l.volume_id = ?1 AND l.relative_path = ?2",
                params![volume_id, rel],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;

        // 2) Otherwise match the file's own UUID (same photo from another machine/path).
        let by_uuid: Option<(i64, String)> = match (&by_loc, sidecar_uuid) {
            (None, Some(uuid)) => self
                .conn
                .query_row(
                    "SELECT id, uuid FROM photos WHERE uuid = ?1",
                    params![uuid],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?,
            _ => None,
        };

        let result = if let Some((id, uuid, old_mtime, old_size)) = by_loc {
            let unchanged = old_mtime == mtime_ns && old_size == size;
            self.conn.execute(
                "UPDATE photos SET mtime_ns = ?1, size = ?2, extension = ?3, missing = 0,
                    updated_at = ?4 WHERE id = ?5",
                params![mtime_ns, size, extension, ts, id],
            )?;
            UpsertResult { id, uuid, created: false, unchanged }
        } else if let Some((id, uuid)) = by_uuid {
            self.conn.execute(
                "UPDATE photos SET mtime_ns = ?1, size = ?2, extension = ?3, missing = 0,
                    updated_at = ?4 WHERE id = ?5",
                params![mtime_ns, size, extension, ts, id],
            )?;
            UpsertResult { id, uuid, created: false, unchanged: false }
        } else {
            let uuid = sidecar_uuid
                .map(str::to_string)
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            self.conn.execute(
                "INSERT INTO photos(uuid, path, folder_id, mtime_ns, size, extension,
                    missing, created_at, updated_at)
                 VALUES(?1, ?2, NULL, ?3, ?4, ?5, 0, ?6, ?6)",
                params![uuid, rel, mtime_ns, size, extension, ts],
            )?;
            UpsertResult {
                id: self.conn.last_insert_rowid(),
                uuid,
                created: true,
                unchanged: false,
            }
        };

        // Record the file's location on its (NAS) volume so the resolver finds it there.
        self.add_location(result.id, volume_id, &rel, LocationRole::Primary)?;
        Ok(result)
    }

    pub fn get_photo(&self, photo_id: i64) -> Result<Photo> {
        self.conn
            .query_row(
                "SELECT id, uuid, path, rating, color_label, pick_state, capture_time,
                    width, height, camera_model, lens, aperture, shutter_speed, iso,
                    external_editors, thumbnail_path,
                    (SELECT COUNT(*) FROM photos c WHERE c.stack_parent_id = photos.id),
                    stack_parent_id, metadata_ready, sharpness, sharpness_method, burst_flag
                 FROM photos WHERE id = ?1",
                params![photo_id],
                row_to_photo,
            )
            .optional()?
            .ok_or_else(|| CatalogError::NotFound(format!("photo {photo_id}")))
    }

    /// One photo by its stable uuid — the chairphoto:// deep-link target.
    /// Served by `idx_photos_uuid`.
    pub fn get_photo_by_uuid(&self, uuid: &str) -> Result<Photo> {
        self.conn
            .query_row(
                "SELECT id, uuid, path, rating, color_label, pick_state, capture_time,
                    width, height, camera_model, lens, aperture, shutter_speed, iso,
                    external_editors, thumbnail_path,
                    (SELECT COUNT(*) FROM photos c WHERE c.stack_parent_id = photos.id),
                    stack_parent_id, metadata_ready, sharpness, sharpness_method, burst_flag
                 FROM photos WHERE uuid = ?1",
                params![uuid],
                row_to_photo,
            )
            .optional()?
            .ok_or_else(|| CatalogError::NotFound(format!("photo uuid {uuid}")))
    }

    /// Count how many of the given UUIDs already exist in the catalog.
    /// Chunks the input into batches of at most 999 to stay within SQLite's
    /// SQLITE_MAX_VARIABLE_NUMBER limit (default 999).
    pub fn count_existing_uuids(&self, uuids: &[String]) -> Result<usize> {
        if uuids.is_empty() {
            return Ok(0);
        }
        const CHUNK: usize = 999;
        let mut total: usize = 0;
        for chunk in uuids.chunks(CHUNK) {
            // Build the parameterised placeholder list: (?1,?2,…,?N).
            let placeholders: String = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!("SELECT COUNT(*) FROM photos WHERE uuid IN ({placeholders})");
            let params: Vec<&dyn rusqlite::ToSql> =
                chunk.iter().map(|u| u as &dyn rusqlite::ToSql).collect();
            let count: i64 = self
                .conn
                .query_row(&sql, params.as_slice(), |row| row.get(0))?;
            total += count as usize;
        }
        Ok(total)
    }

    /// Replace a photo's metadata: clear and re-insert the generic key-value
    /// entries, and update the promoted columns. Called by the scanner after EXIF
    /// extraction. See `metadata` module + `docs` for the two-tier rationale.
    pub fn set_photo_metadata(
        &self,
        photo_id: i64,
        promoted: &PromotedMetadata,
        entries: &[MetadataEntry],
    ) -> Result<()> {
        self.conn
            .execute("DELETE FROM photo_metadata WHERE photo_id = ?1", params![photo_id])?;
        {
            let mut stmt = self.conn.prepare(
                "INSERT OR IGNORE INTO photo_metadata(photo_id, key, group_name, value, value_norm)
                 VALUES(?1, ?2, ?3, ?4, ?5)",
            )?;
            for e in entries {
                stmt.execute(params![
                    photo_id,
                    e.key,
                    e.group_name,
                    e.value,
                    normalize_lookup(&e.value)
                ])?;
            }
        }
        self.conn.execute(
            "UPDATE photos SET width = ?1, height = ?2, capture_time = ?3, camera_make = ?4,
                camera_model = ?5, lens = ?6, focal_length = ?7, aperture = ?8, shutter_speed = ?9,
                iso = ?10, gps_latitude = ?11, gps_longitude = ?12, updated_at = ?13
             WHERE id = ?14",
            params![
                promoted.width,
                promoted.height,
                promoted.capture_time,
                promoted.camera_make,
                promoted.camera_model,
                promoted.lens,
                promoted.focal_length,
                promoted.aperture,
                promoted.shutter_speed,
                promoted.iso,
                promoted.gps_latitude,
                promoted.gps_longitude,
                now(),
                photo_id
            ],
        )?;
        Ok(())
    }

    /// Record the external editors detected from a photo's sidecars (comma-joined,
    /// e.g. "RawTherapee, darktable"). Empty string means none — drives the "edited"
    /// filter. See `scanner::sidecars`.
    pub fn set_external_editors(&self, photo_id: i64, editors: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE photos SET external_editors = ?1, updated_at = ?2 WHERE id = ?3",
            params![editors, now(), photo_id],
        )?;
        Ok(())
    }

    /// A photo's non-destructive orientation override (degrees clockwise: 0/90/180/270).
    pub fn photo_rotation(&self, photo_id: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT user_rotation FROM photos WHERE id = ?1",
            params![photo_id],
            |r| r.get(0),
        )?)
    }

    /// Set a photo's orientation override, normalized to 0/90/180/270 (degrees clockwise).
    /// Non-destructive: only the catalog changes; the original file is never rewritten.
    pub fn set_photo_rotation(&self, photo_id: i64, degrees: i64) -> Result<i64> {
        let norm = ((degrees % 360) + 360) % 360;
        let norm = (norm / 90) * 90; // snap to a right angle
        self.conn.execute(
            "UPDATE photos SET user_rotation = ?1, updated_at = ?2 WHERE id = ?3",
            params![norm, now(), photo_id],
        )?;
        Ok(norm)
    }

    // --- RAW + JPEG stacking ---------------------------------------------------
    // A derivative photo (e.g. the camera JPEG) is stacked under its master (the RAW)
    // via `photos.stack_parent_id`. Children are hidden from the main grid (see
    // `list_photos`) and surfaced through the master's Stack section.

    /// Stack `child_id` under `parent_id` (the child is hidden from the grid). Rejects
    /// self-stacking; flattens any existing children of the child onto the new parent so
    /// stacks never nest more than one level deep.
    pub fn set_stack_parent(&self, child_id: i64, parent_id: i64) -> Result<()> {
        if child_id == parent_id {
            return Err(CatalogError::Tag("a photo cannot stack under itself".into()));
        }
        // If the child was itself a master, re-home its children onto the new parent.
        self.conn.execute(
            "UPDATE photos SET stack_parent_id = ?1 WHERE stack_parent_id = ?2",
            params![parent_id, child_id],
        )?;
        self.conn.execute(
            "UPDATE photos SET stack_parent_id = ?1, updated_at = ?2 WHERE id = ?3",
            params![parent_id, now(), child_id],
        )?;
        Ok(())
    }

    /// Remove a photo from its stack — it returns to the main grid as a top-level photo.
    pub fn unstack(&self, child_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE photos SET stack_parent_id = NULL, updated_at = ?1 WHERE id = ?2",
            params![now(), child_id],
        )?;
        Ok(())
    }

    /// The photos stacked under `parent_id` (e.g. the camera JPEG under a RAW).
    pub fn list_stack_children(&self, parent_id: i64) -> Result<Vec<Photo>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, uuid, path, rating, color_label, pick_state, capture_time,
                width, height, camera_model, lens, aperture, shutter_speed, iso,
                external_editors, thumbnail_path,
                (SELECT COUNT(*) FROM photos c WHERE c.stack_parent_id = photos.id),
                stack_parent_id, metadata_ready, sharpness, sharpness_method, burst_flag
             FROM photos WHERE stack_parent_id = ?1 ORDER BY path COLLATE NOCASE",
        )?;
        let rows = stmt.query_map(params![parent_id], row_to_photo)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Stack each derivative JPEG under its sibling RAW — same folder + same filename
    /// stem (case-insensitive). Idempotent: only stacks JPEGs that are currently
    /// top-level. Returns the number newly stacked. Used by the v18 migration backfill
    /// and after every scan so new RAW+JPEG imports pair automatically.
    pub fn pair_raw_jpeg_stacks(&self) -> Result<usize> {
        let raw_exts: std::collections::HashSet<&str> = [
            "cr2", "cr3", "crw", "arw", "orf", "dng", "nef", "raf", "rw2", "pef", "srw",
            "raw", "3fr", "iiq", "dcr", "erf", "mrw", "x3f",
        ]
        .into_iter()
        .collect();
        struct R {
            id: i64,
            path: String,
            ext: String,
            parent: Option<i64>,
        }
        let rows: Vec<R> = {
            let mut stmt = self
                .conn
                .prepare("SELECT id, path, lower(extension), stack_parent_id FROM photos")?;
            let v = stmt
                .query_map([], |r| {
                    Ok(R {
                        id: r.get(0)?,
                        path: r.get(1)?,
                        ext: r.get(2)?,
                        parent: r.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            v
        };
        // (dir, stem_lower) -> a RAW photo id in that group.
        let mut raw_by_key: std::collections::HashMap<(String, String), i64> =
            std::collections::HashMap::new();
        for row in &rows {
            if raw_exts.contains(row.ext.as_str()) {
                raw_by_key.entry(dir_stem(&row.path)).or_insert(row.id);
            }
        }
        let mut count = 0;
        for row in &rows {
            if (row.ext == "jpg" || row.ext == "jpeg") && row.parent.is_none() {
                if let Some(&raw_id) = raw_by_key.get(&dir_stem(&row.path)) {
                    if raw_id != row.id {
                        self.conn.execute(
                            "UPDATE photos SET stack_parent_id = ?1, updated_at = ?2
                             WHERE id = ?3 AND stack_parent_id IS NULL",
                            params![raw_id, now(), row.id],
                        )?;
                        count += 1;
                    }
                }
            }
        }
        Ok(count)
    }

    /// Forget a photo: delete its catalog row, cascading (via foreign keys) to its
    /// tags, metadata, locations, versions, publications, album membership, and culling
    /// marks. This removes only the catalog's knowledge of the photo — it never deletes
    /// any file on disk or on the NAS (the "nothing ever leaves home" invariant). Use
    /// when an original is gone for good and the user wants the placeholder cleared.
    pub fn remove_photo(&self, photo_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM photos WHERE id = ?1", params![photo_id])?;
        Ok(())
    }

    /// Re-point a photo at a file the user moved to a new location: update its logical
    /// (catalog-root-relative) path, refresh its file stats, reset its primary location,
    /// and clear the `missing` flag. The new file must live under the catalog root —
    /// returns `OutsideRoot` otherwise. Returns the photo's UUID so the caller can keep
    /// the file's sidecar bound to it. Does not move or copy any file.
    pub fn relocate_photo(&self, photo_id: i64, new_path: &Path) -> Result<String> {
        if !new_path.is_file() {
            return Err(CatalogError::Validation(format!(
                "not a file: {}",
                new_path.display()
            )));
        }
        // Must be under the catalog root so it has a valid root-relative logical path.
        let rel = self.to_relative(new_path)?;
        let md = std::fs::metadata(new_path).map_err(|e| CatalogError::Validation(e.to_string()))?;
        let mtime_ns = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        let size = md.len() as i64;
        let uuid: String = self
            .conn
            .query_row("SELECT uuid FROM photos WHERE id = ?1", params![photo_id], |r| r.get(0))
            .optional()?
            .ok_or_else(|| CatalogError::NotFound(format!("photo {photo_id}")))?;
        self.conn.execute(
            "UPDATE photos SET path = ?1, mtime_ns = ?2, size = ?3, missing = 0, updated_at = ?4
             WHERE id = ?5",
            params![rel, mtime_ns, size, now(), photo_id],
        )?;
        self.set_primary_location(photo_id, new_path)?;
        Ok(uuid)
    }

    /// Read a photo's authored IPTC fields.
    pub fn get_iptc(&self, photo_id: i64) -> Result<IptcFields> {
        self.conn
            .query_row(
                "SELECT iptc_description, iptc_headline, iptc_title, iptc_creator, iptc_copyright,
                    iptc_credit, iptc_source, iptc_city, iptc_state, iptc_country, iptc_country_code
                 FROM photos WHERE id = ?1",
                params![photo_id],
                |r| {
                    Ok(IptcFields {
                        description: r.get(0)?,
                        headline: r.get(1)?,
                        title: r.get(2)?,
                        creator: r.get(3)?,
                        copyright: r.get(4)?,
                        credit: r.get(5)?,
                        source: r.get(6)?,
                        city: r.get(7)?,
                        state: r.get(8)?,
                        country: r.get(9)?,
                        country_code: r.get(10)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| CatalogError::NotFound(format!("photo {photo_id}")))
    }

    /// Write a photo's authored IPTC fields to the catalog. (XMP-sidecar writing is
    /// done by the caller via the `xmp` module so it can target the original file.)
    pub fn set_iptc(&self, photo_id: i64, f: &IptcFields) -> Result<()> {
        self.conn.execute(
            "UPDATE photos SET iptc_description = ?1, iptc_headline = ?2, iptc_title = ?3,
                iptc_creator = ?4, iptc_copyright = ?5, iptc_credit = ?6, iptc_source = ?7,
                iptc_city = ?8, iptc_state = ?9, iptc_country = ?10, iptc_country_code = ?11,
                updated_at = ?12
             WHERE id = ?13",
            params![
                f.description,
                f.headline,
                f.title,
                f.creator,
                f.copyright,
                f.credit,
                f.source,
                f.city,
                f.state,
                f.country,
                f.country_code,
                now(),
                photo_id
            ],
        )?;
        Ok(())
    }

    /// All stored metadata entries for a photo, grouped-friendly (ordered by group).
    pub fn get_photo_metadata(&self, photo_id: i64) -> Result<Vec<MetadataEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT key, group_name, value FROM photo_metadata WHERE photo_id = ?1
             ORDER BY group_name, key COLLATE NOCASE",
        )?;
        let rows = stmt.query_map(params![photo_id], |r| {
            Ok(MetadataEntry {
                key: r.get(0)?,
                group_name: r.get(1)?,
                value: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// List photos, optionally filtered by a tag (including its descendants), an
    /// album, an import batch, derived `facets` (e.g. "has-gps", "mobile", "drone"),
    /// and a culling filter ("all", "unrated", "pick", "reject", "edited"). All
    /// supplied filters are ANDed. Missing photos are excluded.
    /// Distinct non-empty values of a photo column, for the camera/lens filter dropdowns.
    /// `column` is a fixed allowlist (camera_model | lens) — never user input.
    pub fn distinct_photo_values(&self, column: &str) -> Result<Vec<String>> {
        let col = match column {
            "camera" => "camera_model",
            "lens" => "lens",
            other => return Err(CatalogError::Tag(format!("unknown column: {other}"))),
        };
        let sql = format!(
            "SELECT DISTINCT {col} FROM photos \
             WHERE missing = 0 AND {col} IS NOT NULL AND {col} <> '' \
             ORDER BY {col} COLLATE NOCASE"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn list_photos(
        &self,
        tag_id: Option<i64>,
        album_id: Option<i64>,
        batch_id: Option<i64>,
        facets: &[String],
        culling_filter: &str,
        smart_album_id: Option<i64>,
        storage_tier: &str,
        camera: Option<&str>,
        lens: Option<&str>,
        // Colour-label filter, OR-combined: keep photos whose label is any of these
        // (canonical "Red"… strings; "" = the No-label option). Empty slice = no filter.
        labels: &[String],
        // Sort order: None / "date" (oldest-first capture time, default),
        // "sharpness_asc" (least-sharp first — puts suspect frames up front for
        // culling), "sharpness_desc" (sharpest-first). Unscored photos
        // (sharpness IS NULL) sort after scored ones in both sharpness orderings.
        sort: Option<&str>,
    ) -> Result<Vec<Photo>> {
        let mut sql = String::from(
            "SELECT DISTINCT p.id, p.uuid, p.path, p.rating, p.color_label, p.pick_state,
                p.capture_time, p.width, p.height, p.camera_model, p.lens,
                p.aperture, p.shutter_speed, p.iso,
                p.external_editors, p.thumbnail_path,
                (SELECT COUNT(*) FROM photos c WHERE c.stack_parent_id = p.id),
                p.stack_parent_id, p.metadata_ready,
                p.sharpness, p.sharpness_method, p.burst_flag
             FROM photos p",
        );
        // Stacked derivatives (e.g. a camera JPEG under its RAW) are hidden from the
        // main grid; they're reached via the master's Stack section in the inspector.
        let mut wheres = vec![
            "p.missing = 0".to_string(),
            "p.stack_parent_id IS NULL".to_string(),
        ];
        let mut bind: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(tid) = tag_id {
            let ids = self.descendant_tag_ids(tid)?;
            let placeholders = vec!["?"; ids.len()].join(",");
            sql.push_str(" JOIN photo_tags pt ON pt.photo_id = p.id");
            wheres.push(format!("pt.tag_id IN ({placeholders})"));
            for id in ids {
                bind.push(Box::new(id));
            }
        }

        if let Some(aid) = album_id {
            sql.push_str(" JOIN album_photos ap ON ap.photo_id = p.id");
            wheres.push("ap.album_id = ?".to_string());
            bind.push(Box::new(aid));
        }

        if let Some(bid) = batch_id {
            wheres.push("p.import_batch_id = ?".to_string());
            bind.push(Box::new(bid));
        }

        // Smart album: a saved RULE evaluated LIVE. Load its rule_json, translate it to
        // WHERE fragments (over the same `p` alias) + bound parameters, and AND them in.
        // The translator is the single source of truth for the field/op allowlist; every
        // value it yields is already a bound parameter (see catalog/smart_albums.rs).
        if let Some(sid) = smart_album_id {
            let rule_json = self.smart_album_rule_json(sid)?;
            let (rule_wheres, rule_binds) = smart_albums::rule_to_sql(&rule_json)?;
            for w in rule_wheres {
                wheres.push(w);
            }
            for b in rule_binds {
                bind.push(b);
            }
        }

        // Derived facets (internal, never exported): each is a boolean predicate, ANDed
        // in. Most are computed from photos columns (no binds); the dynamic
        // "published:<platform>" facets take the platform as a bound parameter; the `soft`
        // facet uses a dynamic threshold from settings (via facet_predicate_owned).
        for facet in facets {
            if let Some(platform) = facet.strip_prefix("published:") {
                wheres.push(
                    "EXISTS (SELECT 1 FROM publications pub \
                     WHERE pub.photo_id = p.id AND pub.platform = ?)"
                        .to_string(),
                );
                bind.push(Box::new(platform.to_string()));
            } else {
                wheres.push(self.facet_predicate_owned(facet)?);
            }
        }

        match culling_filter {
            "all" => {}
            "unrated" => wheres.push("p.rating = 0".into()),
            "pick" => {
                wheres.push("p.pick_state = 'pick'".into());
            }
            "reject" => {
                wheres.push("p.pick_state = 'reject'".into());
            }
            "edited" => wheres.push("p.external_editors != ''".into()),
            other => return Err(CatalogError::Tag(format!("unknown filter: {other}"))),
        }

        // Storage tier: where the photo's bytes live (by volume kind, matching the grid's
        // status icons). "local" = has a copy on a local volume (recent, on-disk photos);
        // "nas" = NAS-only, i.e. offloaded — no local copy but a backup exists.
        match storage_tier {
            "" | "all" => {}
            "local" => wheres.push(
                "EXISTS (SELECT 1 FROM photo_locations l JOIN volumes v ON v.id = l.volume_id \
                 WHERE l.photo_id = p.id AND v.kind = 'local')"
                    .into(),
            ),
            "nas" => wheres.push(
                "NOT EXISTS (SELECT 1 FROM photo_locations l JOIN volumes v ON v.id = l.volume_id \
                   WHERE l.photo_id = p.id AND v.kind = 'local') \
                 AND EXISTS (SELECT 1 FROM photo_locations l JOIN volumes v ON v.id = l.volume_id \
                   WHERE l.photo_id = p.id AND v.kind = 'backup')"
                    .into(),
            ),
            other => return Err(CatalogError::Tag(format!("unknown storage tier: {other}"))),
        }

        // Camera / lens exact-match filters (values come from distinct_photo_values, bound).
        if let Some(cam) = camera {
            wheres.push("p.camera_model = ?".to_string());
            bind.push(Box::new(cam.to_string()));
        }
        if let Some(l) = lens {
            wheres.push("p.lens = ?".to_string());
            bind.push(Box::new(l.to_string()));
        }

        // Colour labels, OR-combined within the filter (a photo has one label, so a
        // multi-select is a set-membership test). "" is a valid member: the No-label
        // option. NOCASE matches the smart-album text predicate's behaviour.
        if !labels.is_empty() {
            let placeholders = vec!["?"; labels.len()].join(",");
            wheres.push(format!("p.color_label COLLATE NOCASE IN ({placeholders})"));
            for label in labels {
                bind.push(Box::new(label.clone()));
            }
        }

        sql.push_str(" WHERE ");
        sql.push_str(&wheres.join(" AND "));
        // Albums are explicitly ordered collections — honour their member order when
        // viewing one. Otherwise sort by "date taken", oldest first, with path as a
        // deterministic tiebreaker for same-timestamp bursts. Prefer the EXIF capture
        // time; when it's missing (e.g. iPhone HEICs / files stripped of EXIF) fall back
        // to the file's modification time (mtime_ns, always recorded on scan) so those
        // photos slot into the timeline chronologically instead of piling up at the end.
        // capture_time is local-clock ISO ("YYYY-MM-DDTHH:MM:SS"); mtime is true unix
        // seconds — the small tz skew at the EXIF/no-EXIF boundary is acceptable for sort.
        if album_id.is_some() {
            sql.push_str(" ORDER BY ap.position, p.path COLLATE NOCASE");
        } else {
            match sort.unwrap_or("date") {
                // Sharpness sorts: unscored photos (NULL) always sort last so the ordering
                // makes sense in both directions and newly-indexed photos don't jump around.
                "sharpness_asc" => sql.push_str(
                    " ORDER BY CASE WHEN p.sharpness IS NULL THEN 1 ELSE 0 END, \
                     p.sharpness ASC, p.path COLLATE NOCASE",
                ),
                "sharpness_desc" => sql.push_str(
                    " ORDER BY CASE WHEN p.sharpness IS NULL THEN 1 ELSE 0 END, \
                     p.sharpness DESC, p.path COLLATE NOCASE",
                ),
                // Default ("date" or anything unknown): oldest-first by capture time.
                _ => sql.push_str(
                    " ORDER BY COALESCE(CAST(strftime('%s', p.capture_time) AS INTEGER), \
                     p.mtime_ns / 1000000000), p.path COLLATE NOCASE",
                ),
            }
        }

        let params: Vec<&dyn rusqlite::ToSql> = bind.iter().map(|b| b.as_ref()).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params.as_slice(), row_to_photo)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Update any of rating / color label / pick state. `None` leaves it unchanged.
    pub fn set_culling(
        &self,
        photo_id: i64,
        rating: Option<i64>,
        color_label: Option<&str>,
        pick_state: Option<PickState>,
    ) -> Result<Photo> {
        let current = self.get_photo(photo_id)?;
        let rating = rating.unwrap_or(current.rating);
        if !(0..=5).contains(&rating) {
            return Err(CatalogError::Tag("rating must be 0..=5".into()));
        }
        let label = color_label.unwrap_or(&current.label).to_string();
        let pick = pick_state.unwrap_or(current.pick_state);
        self.conn.execute(
            "UPDATE photos SET rating = ?1, color_label = ?2, pick_state = ?3, updated_at = ?4
             WHERE id = ?5",
            params![rating, label, pick.as_db_str(), now(), photo_id],
        )?;
        self.get_photo(photo_id)
    }

    /// Set the metadata_ready flag (phase A/B boundary for two-phase live scan, I6a).
    /// 0 = awaiting metadata extraction; 1 = metadata ready for display.
    pub fn set_metadata_ready(&self, photo_id: i64, ready: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE photos SET metadata_ready = ?1, updated_at = ?2 WHERE id = ?3",
            params![if ready { 1 } else { 0 }, now(), photo_id],
        )?;
        Ok(())
    }

    // --- burst-relative sharpness flags (H16e) ------------------------------

    /// Persist burst-relative sharpness flags for a batch of photos. Called by the
    /// `analyze_burst_sharpness` command after clustering. Each entry is `(photo_id,
    /// flag)` where `flag` is `"soft-in-burst"`, `"sharpest-of-burst"`, or `""` (clear).
    /// An empty flag string NULLs the column (resets to unanalysed). Runs in a single
    /// transaction for atomicity — the whole batch is committed or nothing is.
    pub fn set_burst_flags(&self, flags: Vec<(i64, String)>) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for (photo_id, flag) in &flags {
            if flag.is_empty() {
                tx.execute(
                    "UPDATE photos SET burst_flag = NULL WHERE id = ?1",
                    params![photo_id],
                )?;
            } else {
                tx.execute(
                    "UPDATE photos SET burst_flag = ?1 WHERE id = ?2",
                    params![flag, photo_id],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Fetch the (photo_id, capture_time, phash, rating, sharpness) rows for the given IDs.
    /// Used by the burst analysis command to build [`BurstPhoto`] inputs without re-scanning.
    /// Rows are returned in `photo_ids` order; IDs missing from the catalog are silently skipped.
    pub fn burst_inputs(&self, photo_ids: &[i64]) -> Result<Vec<BurstInput>> {
        if photo_ids.is_empty() {
            return Ok(Vec::new());
        }
        // Build the IN-list (chunked to respect SQLite's variable limit = 999).
        const CHUNK: usize = 999;
        let mut out: Vec<BurstInput> = Vec::with_capacity(photo_ids.len());
        for chunk in photo_ids.chunks(CHUNK) {
            let placeholders =
                (1..=chunk.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT id, capture_time, phash, rating, sharpness
                 FROM photos WHERE id IN ({placeholders}) ORDER BY id"
            );
            let params: Vec<&dyn rusqlite::ToSql> =
                chunk.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(params.as_slice(), |r| {
                Ok(BurstInput {
                    id: r.get(0)?,
                    capture_time: r.get(1)?,
                    phash: r
                        .get::<_, Option<i64>>(2)?
                        .map(|v| v as u64),
                    rating: r.get(3)?,
                    sharpness: r.get(4)?,
                })
            })?;
            for row in rows {
                out.push(row?);
            }
        }
        Ok(out)
    }

    // --- Flickr import helper ------------------------------------------------

    /// Return (id, path, capture_time) for all non-missing photos.  Used by the Flickr
    /// import command to build the candidate list for datetime + title matching without
    /// exposing `conn` outside the catalog module.
    #[cfg(feature = "flickr")]
    pub fn photos_for_flickr_match(
        &self,
    ) -> Result<Vec<crate::flickr::CatalogPhotoRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, capture_time FROM photos WHERE missing = 0",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(crate::flickr::CatalogPhotoRow {
                id: r.get(0)?,
                path: r.get(1)?,
                capture_time: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // --- pending enrichment queue (I6d) -------------------------------------

    /// Enqueue a photo for Phase B enrichment (idempotent: INSERT OR IGNORE on photo_id PK).
    /// Called by Phase A for every new/changed file. Survives a crash so Phase B can
    /// auto-resume on the next startup.
    pub fn enqueue_pending_enrichment(&self, photo_id: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO pending_enrichment(photo_id, queued_at) VALUES(?1, ?2)
             ON CONFLICT(photo_id) DO NOTHING",
            params![photo_id, now()],
        )?;
        Ok(())
    }

    /// Remove a photo from the pending-enrichment queue after Phase B completes it.
    pub fn dequeue_pending_enrichment(&self, photo_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM pending_enrichment WHERE photo_id = ?1",
            params![photo_id],
        )?;
        Ok(())
    }

    /// Number of photos still waiting for Phase B enrichment.
    pub fn pending_enrichment_count(&self) -> Result<usize> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM pending_enrichment",
            [],
            |r| r.get::<_, i64>(0),
        )? as usize)
    }

    /// Load the pending-enrichment queue as a `(path, photo_id, needs_extract=true)` list
    /// suitable for feeding directly into [`phase_b_enrich`]. Paths are resolved via the
    /// multi-volume resolver (preferring local-cache over primary over backup), which
    /// honours the AGENTS.md invariant that photo bytes must never be read via
    /// `photos.path` directly. Photos that have no reachable copy (resolver returns `None`)
    /// or were deleted since enqueue (CASCADE removed the queue row) are silently skipped.
    pub fn load_pending_enrichment(&self) -> Result<Vec<(std::path::PathBuf, i64, bool)>> {
        let photo_ids: Vec<i64> = {
            let mut stmt = self.conn.prepare(
                "SELECT pe.photo_id
                 FROM pending_enrichment pe
                 JOIN photos p ON p.id = pe.photo_id
                 ORDER BY pe.queued_at, pe.photo_id",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut out = Vec::with_capacity(photo_ids.len());
        for photo_id in photo_ids {
            // Use the resolver: it picks the best available copy across all volumes
            // (local-cache > primary > backup), so sidecar detection in Phase B targets
            // the canonical directory, not a potentially-wrong cache location.
            if let Some(path) = self.resolve_photo_path(photo_id)? {
                out.push((path, photo_id, true));
            }
        }
        Ok(out)
    }

    // --- folders ------------------------------------------------------------

    pub fn add_folder(&self, absolute_path: &Path) -> Result<i64> {
        let rel = self.to_relative(absolute_path)?;
        self.conn.execute(
            "INSERT OR IGNORE INTO indexed_folders(path) VALUES(?1)",
            params![rel],
        )?;
        Ok(self.conn.query_row(
            "SELECT id FROM indexed_folders WHERE path = ?1",
            params![rel],
            |r| r.get(0),
        )?)
    }

    /// Mark all photos in a folder as missing before a rescan; the scan clears
    /// the flag for files it finds, leaving deleted files flagged missing.
    pub fn mark_folder_scan_started(&self, folder_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE photos SET missing = 1, updated_at = ?1 WHERE folder_id = ?2",
            params![now(), folder_id],
        )?;
        Ok(())
    }

    pub fn mark_folder_scan_finished(&self, folder_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE indexed_folders SET last_scan_at = ?1 WHERE id = ?2",
            params![now(), folder_id],
        )?;
        Ok(())
    }

    // --- tags ---------------------------------------------------------------

    /// Create a tag path, creating any missing ancestors. Idempotent — returns
    /// the leaf tag id whether or not it already existed.
    pub fn create_tag(&self, path: &str) -> Result<i64> {
        let normalized = normalize_tag_path(path).map_err(CatalogError::Tag)?;
        let mut parent_id: Option<i64> = None;
        let mut prefix: Vec<String> = Vec::new();

        for component in &normalized.components {
            prefix.push(component.clone());
            let full_path = prefix.join("/");
            let full_path_norm = normalize_lookup(&full_path);

            let existing: Option<i64> = self
                .conn
                .query_row(
                    "SELECT id FROM tags WHERE full_path_norm = ?1",
                    params![full_path_norm],
                    |r| r.get(0),
                )
                .optional()?;

            parent_id = Some(match existing {
                Some(id) => id,
                None => {
                    let ts = now();
                    self.conn.execute(
                        "INSERT INTO tags(uuid, name, name_norm, parent_id, full_path,
                            full_path_norm, created_at, updated_at)
                         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                        params![
                            uuid::Uuid::new_v4().to_string(),
                            component,
                            normalize_lookup(component),
                            parent_id,
                            full_path,
                            full_path_norm,
                            ts
                        ],
                    )?;
                    self.conn.last_insert_rowid()
                }
            });
        }

        parent_id.ok_or_else(|| CatalogError::Tag("empty tag path".into()))
    }

    /// Resolve a tag path (e.g. "Animals/Birds/Owl") to its id, if it exists.
    /// Used to classify AI suggestions as existing vs new.
    pub fn find_tag_id_by_path(&self, path: &str) -> Result<Option<i64>> {
        let normalized = normalize_tag_path(path).map_err(CatalogError::Tag)?;
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM tags WHERE full_path_norm = ?1",
                params![normalized.key()],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Suggest existing tags that photos taken near this one (within
    /// `window_seconds` of its capture time) already have — session tags like
    /// Events/Places naturally rank highest by neighbour frequency. Excludes tags
    /// already on this photo. Returns empty if the photo has no capture time.
    /// `photo_count` carries the neighbour frequency.
    pub fn suggest_tags_by_time(
        &self,
        photo_id: i64,
        window_seconds: i64,
    ) -> Result<Vec<TagWithCount>> {
        let capture: Option<String> = self
            .conn
            .query_row(
                "SELECT capture_time FROM photos WHERE id = ?1",
                params![photo_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let Some(capture) = capture.filter(|s| !s.is_empty()) else {
            return Ok(Vec::new());
        };
        let Ok(dt) = chrono::NaiveDateTime::parse_from_str(&capture, "%Y-%m-%dT%H:%M:%S") else {
            return Ok(Vec::new());
        };
        // capture_time is stored as fixed-format ISO, so string BETWEEN == time range.
        let fmt = "%Y-%m-%dT%H:%M:%S";
        let lo = (dt - chrono::Duration::seconds(window_seconds)).format(fmt).to_string();
        let hi = (dt + chrono::Duration::seconds(window_seconds)).format(fmt).to_string();

        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.name, t.full_path, t.parent_id, t.description, t.auto_rule,
                    t.uuid, t.private, COUNT(*) AS freq
             FROM photos p
             JOIN photo_tags pt ON pt.photo_id = p.id
             JOIN tags t ON t.id = pt.tag_id
             WHERE p.capture_time BETWEEN ?1 AND ?2 AND p.id != ?3 AND p.missing = 0
               AND t.id NOT IN (SELECT tag_id FROM photo_tags WHERE photo_id = ?3)
             GROUP BY t.id
             ORDER BY freq DESC, t.full_path_norm",
        )?;
        let rows = stmt.query_map(params![lo, hi, photo_id], |r| {
            Ok(TagWithCount {
                tag: row_to_tag(r)?,
                photo_count: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn get_tag(&self, tag_id: i64) -> Result<Tag> {
        self.conn
            .query_row(
                "SELECT id, name, full_path, parent_id, description, auto_rule, uuid, private
                 FROM tags WHERE id = ?1",
                params![tag_id],
                row_to_tag,
            )
            .optional()?
            .ok_or_else(|| CatalogError::NotFound(format!("tag {tag_id}")))
    }

    /// Set a tag's description (internal metadata; not exported to image sidecars).
    pub fn set_tag_description(&self, tag_id: i64, description: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE tags SET description = ?1, updated_at = ?2 WHERE id = ?3",
            params![description, now(), tag_id],
        )?;
        Ok(())
    }

    /// Whether a tag is emitted on export. `false` = organizational: the tag itself is
    /// never written as a keyword (nor as a segment of the hierarchical path), but its
    /// descendant tags still export. Missing/unknown → `true`.
    pub fn tag_exportable(&self, tag_id: i64) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT exportable FROM tags WHERE id = ?1",
                params![tag_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .map(|v| v != 0)
            .unwrap_or(true))
    }

    /// Mark a tag as exported or organizational (see [`tag_exportable`]).
    pub fn set_tag_exportable(&self, tag_id: i64, exportable: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE tags SET exportable = ?1, updated_at = ?2 WHERE id = ?3",
            params![exportable as i64, now(), tag_id],
        )?;
        Ok(())
    }

    /// Whether a tag is marked private: withheld from EXTERNAL/cloud AI providers
    /// (Claude/OpenAI/Gemini) so sensitive labels (e.g. people's names) never leave the
    /// machine. The LOCAL model (Ollama) still receives it. Does not affect export,
    /// filtering, or display. Missing/unknown → `false`.
    pub fn tag_private(&self, tag_id: i64) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT private FROM tags WHERE id = ?1",
                params![tag_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .map(|v| v != 0)
            .unwrap_or(false))
    }

    /// Set a tag's private flag (see [`tag_private`]). With `recursive`, applies to the
    /// tag AND every descendant in one pass (e.g. "People" + all names under it). Returns
    /// the number of tags changed.
    pub fn set_tag_private(&self, tag_id: i64, private: bool, recursive: bool) -> Result<usize> {
        let now = now();
        let ids = if recursive {
            self.descendant_tag_ids(tag_id)?
        } else {
            vec![tag_id]
        };
        let mut changed = 0;
        for id in ids {
            changed += self.conn.execute(
                "UPDATE tags SET private = ?1, updated_at = ?2 WHERE id = ?3",
                params![private as i64, now, id],
            )?;
        }
        Ok(changed)
    }

    /// All tags with a recursive photo count (counts include descendant tags).
    pub fn list_tags_with_counts(&self) -> Result<Vec<TagWithCount>> {
        let mut stmt = self.conn.prepare(
            "WITH RECURSIVE tag_tree(root_id, tag_id) AS (
                 SELECT id, id FROM tags
                 UNION ALL
                 SELECT tag_tree.root_id, tags.id
                 FROM tags JOIN tag_tree ON tags.parent_id = tag_tree.tag_id
             )
             SELECT t.id, t.name, t.full_path, t.parent_id, t.description, t.auto_rule,
                    t.uuid, t.private, COUNT(DISTINCT p.id) AS photo_count
             FROM tags t
             LEFT JOIN tag_tree ON tag_tree.root_id = t.id
             LEFT JOIN photo_tags pt ON pt.tag_id = tag_tree.tag_id
             -- Count only photos that are actually present (the grid hides missing/removed
             -- ones), so a tag's count matches what selecting it shows. Counting NULL p.id
             -- (a missing photo) is skipped by COUNT(DISTINCT …).
             LEFT JOIN photos p ON p.id = pt.photo_id AND p.missing = 0
             GROUP BY t.id
             ORDER BY t.full_path_norm",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(TagWithCount {
                tag: row_to_tag(r)?,
                photo_count: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Rename a tag's canonical name. Rewrites this tag's `full_path` and every
    /// descendant's path (paths embed ancestor names). Translations/synonyms/terms
    /// are unaffected. Errors if the rename would collide with an existing path.
    pub fn rename_tag(&self, tag_id: i64, new_name: &str) -> Result<()> {
        let new_name = new_name.split_whitespace().collect::<Vec<_>>().join(" ");
        if new_name.is_empty() || new_name.contains('/') || new_name.contains('|') {
            return Err(CatalogError::Tag(
                "tag name cannot be empty or contain '/' or '|'".into(),
            ));
        }
        let tag = self.get_tag(tag_id)?;
        let new_full_path = match tag.parent_id {
            Some(parent_id) => format!("{}/{}", self.get_tag(parent_id)?.full_path, new_name),
            None => new_name.clone(),
        };
        let old_prefix = tag.full_path.clone();

        // (id, old_full_path) for this tag and all descendants.
        let subtree = self.descendant_paths(tag_id)?;
        let moving: std::collections::HashSet<i64> = subtree.iter().map(|(id, _)| *id).collect();

        // Compute new paths and check none collides with a tag outside the subtree.
        let mut updates: Vec<(i64, String, String)> = Vec::new();
        for (id, old_path) in &subtree {
            let suffix = &old_path[old_prefix.len()..];
            let new_path = format!("{new_full_path}{suffix}");
            let new_norm = normalize_lookup(&new_path);
            let conflict: Option<i64> = self
                .conn
                .query_row(
                    "SELECT id FROM tags WHERE full_path_norm = ?1",
                    params![new_norm],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(other) = conflict {
                if !moving.contains(&other) {
                    return Err(CatalogError::Tag(
                        "that name would create a duplicate tag path".into(),
                    ));
                }
            }
            updates.push((*id, new_path, new_norm));
        }

        let ts = now();
        self.conn.execute(
            "UPDATE tags SET name = ?1, name_norm = ?2, updated_at = ?3 WHERE id = ?4",
            params![new_name, normalize_lookup(&new_name), ts, tag_id],
        )?;
        for (id, new_path, new_norm) in updates {
            self.conn.execute(
                "UPDATE tags SET full_path = ?1, full_path_norm = ?2, updated_at = ?3 WHERE id = ?4",
                params![new_path, new_norm, ts, id],
            )?;
        }
        Ok(())
    }

    /// Reparent a tag (drag-and-drop in the tree). `new_parent_id = None` moves it to
    /// the top level. Rewrites this tag's and all descendants' `full_path`; rejects
    /// moving a tag into itself/a descendant, or a move that would duplicate a path.
    pub fn move_tag(&self, tag_id: i64, new_parent_id: Option<i64>) -> Result<()> {
        let tag = self.get_tag(tag_id)?;
        if let Some(parent) = new_parent_id {
            if self.descendant_tag_ids(tag_id)?.contains(&parent) {
                return Err(CatalogError::Tag(
                    "cannot move a tag into itself or one of its descendants".into(),
                ));
            }
        }
        let new_full_path = match new_parent_id {
            Some(parent) => format!("{}/{}", self.get_tag(parent)?.full_path, tag.name),
            None => tag.name.clone(),
        };
        let old_prefix = tag.full_path.clone();
        if new_full_path == old_prefix {
            return Ok(()); // already there
        }

        let subtree = self.descendant_paths(tag_id)?;
        let moving: std::collections::HashSet<i64> = subtree.iter().map(|(id, _)| *id).collect();
        let mut updates: Vec<(i64, String, String)> = Vec::new();
        for (id, old_path) in &subtree {
            let suffix = &old_path[old_prefix.len()..];
            let new_path = format!("{new_full_path}{suffix}");
            let new_norm = normalize_lookup(&new_path);
            let conflict: Option<i64> = self
                .conn
                .query_row(
                    "SELECT id FROM tags WHERE full_path_norm = ?1",
                    params![new_norm],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(other) = conflict {
                if !moving.contains(&other) {
                    return Err(CatalogError::Tag(
                        "that move would create a duplicate tag path".into(),
                    ));
                }
            }
            updates.push((*id, new_path, new_norm));
        }

        let ts = now();
        self.conn.execute(
            "UPDATE tags SET parent_id = ?1, updated_at = ?2 WHERE id = ?3",
            params![new_parent_id, ts, tag_id],
        )?;
        for (id, new_path, new_norm) in updates {
            self.conn.execute(
                "UPDATE tags SET full_path = ?1, full_path_norm = ?2, updated_at = ?3 WHERE id = ?4",
                params![new_path, new_norm, ts, id],
            )?;
        }
        Ok(())
    }

    /// Delete a tag and its whole subtree. Foreign-key cascades remove descendant
    /// tags, their photo assignments (`photo_tags`), and their terms (`tag_terms`).
    pub fn delete_tag(&self, tag_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM tags WHERE id = ?1", params![tag_id])?;
        Ok(())
    }

    /// (id, full_path) for a tag and all its descendants.
    fn descendant_paths(&self, tag_id: i64) -> Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(
            "WITH RECURSIVE d(id) AS (
                 SELECT id FROM tags WHERE id = ?1
                 UNION ALL
                 SELECT tags.id FROM tags JOIN d ON tags.parent_id = d.id
             )
             SELECT tags.id, tags.full_path FROM tags JOIN d ON d.id = tags.id",
        )?;
        let rows = stmt.query_map(params![tag_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// A tag plus all of its descendants, by id.
    pub fn descendant_tag_ids(&self, tag_id: i64) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare(
            "WITH RECURSIVE descendants(id) AS (
                 SELECT id FROM tags WHERE id = ?1
                 UNION ALL
                 SELECT tags.id FROM tags JOIN descendants ON tags.parent_id = descendants.id
             )
             SELECT id FROM descendants",
        )?;
        let rows = stmt.query_map(params![tag_id], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn assign_tag(&self, photo_id: i64, tag_id: i64) -> Result<()> {
        let now = now();
        self.conn.execute(
            "INSERT OR IGNORE INTO photo_tags(photo_id, tag_id, created_at) VALUES(?1, ?2, ?3)",
            params![photo_id, tag_id, now],
        )?;
        // Record manual usage (even on re-apply, where the INSERT above is ignored) so the
        // "Recently used" group reflects what you actually tag with. The auto-tag engine
        // bypasses this path, so machine tagging never bumps last_used_at.
        self.conn.execute(
            "UPDATE tags SET last_used_at = ?1 WHERE id = ?2",
            params![now, tag_id],
        )?;
        // Keep the photo's tags to leaves only: a more specific assigned tag implies its
        // ancestors everywhere (filter, count, export), so drop any now-redundant ones.
        self.prune_redundant_tags(photo_id)?;
        Ok(())
    }

    /// Remove assigned tags that are a strict ancestor of another assigned tag on the
    /// same photo (the descendant already implies them). Returns the number removed.
    /// Ancestry is by stored `full_path` prefix (`A/B` is an ancestor of `A/B/C`).
    pub fn prune_redundant_tags(&self, photo_id: i64) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM photo_tags
             WHERE photo_id = ?1 AND tag_id IN (
                 SELECT a.id FROM tags a
                 JOIN photo_tags pa ON pa.tag_id = a.id AND pa.photo_id = ?1
                 WHERE EXISTS (
                     SELECT 1 FROM tags d
                     JOIN photo_tags pd ON pd.tag_id = d.id AND pd.photo_id = ?1
                     WHERE d.id != a.id
                       AND substr(d.full_path, 1, length(a.full_path) + 1) = a.full_path || '/'
                 )
             )",
            params![photo_id],
        )?)
    }

    /// Tag co-occurrence graph: every tag that's on at least one (present) photo, with its
    /// photo count, plus, for each pair of tags sharing photos, how many they share.
    /// Returns `(nodes: (id, full_path, photo_count), edges: (tag_a, tag_b, shared))`.
    #[allow(clippy::type_complexity)]
    pub fn tag_cooccurrence(&self) -> Result<(Vec<(i64, String, i64)>, Vec<(i64, i64, i64)>)> {
        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.full_path, COUNT(DISTINCT pt.photo_id) AS c
             FROM tags t
             JOIN photo_tags pt ON pt.tag_id = t.id
             JOIN photos p ON p.id = pt.photo_id AND p.missing = 0
             GROUP BY t.id HAVING c > 0 ORDER BY c DESC",
        )?;
        let nodes = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut stmt = self.conn.prepare(
            "SELECT a.tag_id, b.tag_id, COUNT(*) AS w
             FROM photo_tags a
             JOIN photo_tags b ON a.photo_id = b.photo_id AND a.tag_id < b.tag_id
             JOIN photos p ON p.id = a.photo_id AND p.missing = 0
             GROUP BY a.tag_id, b.tag_id",
        )?;
        let edges = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((nodes, edges))
    }

    /// Library graph: all tags, cameras, and their relationships.
    /// Returns:
    /// - tags: `Vec<(tag_id, full_path, photo_count)>`
    /// - cameras: `Vec<(camera_model, photo_count)>`
    /// - cooc_edges: `Vec<(tag_a, tag_b, shared_photos)>`
    /// - hierarchy_edges: `Vec<(parent_tag_id, child_tag_id)>`
    /// - camera_edges: `Vec<(camera_model, tag_id, shared_photos)>`
    #[allow(clippy::type_complexity)]
    pub fn library_graph(
        &self,
    ) -> Result<(
        Vec<(i64, String, i64)>,
        Vec<(String, i64)>,
        Vec<(i64, i64, i64)>,
        Vec<(i64, i64)>,
        Vec<(String, i64, i64)>,
    )> {
        // Tag nodes
        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.full_path, COUNT(DISTINCT pt.photo_id) AS c
             FROM tags t
             JOIN photo_tags pt ON pt.tag_id = t.id
             JOIN photos p ON p.id = pt.photo_id AND p.missing = 0
             GROUP BY t.id HAVING c > 0 ORDER BY c DESC",
        )?;
        let tags = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        // Camera nodes
        let mut stmt = self.conn.prepare(
            "SELECT camera_model, COUNT(*) FROM photos WHERE missing = 0 AND camera_model IS NOT NULL AND camera_model != '' GROUP BY camera_model ORDER BY COUNT(*) DESC",
        )?;
        let cameras = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        // Tag co-occurrence edges
        let mut stmt = self.conn.prepare(
            "SELECT a.tag_id, b.tag_id, COUNT(*) AS w
             FROM photo_tags a
             JOIN photo_tags b ON a.photo_id = b.photo_id AND a.tag_id < b.tag_id
             JOIN photos p ON p.id = a.photo_id AND p.missing = 0
             GROUP BY a.tag_id, b.tag_id",
        )?;
        let cooc_edges = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        // Tag hierarchy edges
        let mut stmt = self.conn.prepare("SELECT parent_id, id FROM tags WHERE parent_id IS NOT NULL")?;
        let hierarchy_edges = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        // Camera-tag edges
        let mut stmt = self.conn.prepare(
            "SELECT p.camera_model, pt.tag_id, COUNT(*) FROM photos p
             JOIN photo_tags pt ON pt.photo_id = p.id
             WHERE p.missing = 0 AND p.camera_model IS NOT NULL AND p.camera_model != ''
             GROUP BY p.camera_model, pt.tag_id",
        )?;
        let camera_edges = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok((tags, cameras, cooc_edges, hierarchy_edges, camera_edges))
    }

    /// Photo↔tag bipartite graph: tagged (present) photos with their tag count, the tags
    /// in play with their photo count, and the assignment edges `(photo_id, tag_id)`.
    #[allow(clippy::type_complexity)]
    pub fn photo_tag_graph(
        &self,
    ) -> Result<(Vec<(i64, String, i64)>, Vec<(i64, String, i64)>, Vec<(i64, i64)>)> {
        let mut stmt = self.conn.prepare(
            "SELECT p.id, p.path, COUNT(pt.tag_id) AS c
             FROM photos p JOIN photo_tags pt ON pt.photo_id = p.id
             WHERE p.missing = 0 GROUP BY p.id",
        )?;
        let photos = stmt
            .query_map([], |r| {
                let path: String = r.get(1)?;
                let name = path.rsplit('/').next().unwrap_or(&path).to_string();
                Ok((r.get::<_, i64>(0)?, name, r.get::<_, i64>(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.full_path, COUNT(DISTINCT pt.photo_id) AS c
             FROM tags t
             JOIN photo_tags pt ON pt.tag_id = t.id
             JOIN photos p ON p.id = pt.photo_id AND p.missing = 0
             GROUP BY t.id HAVING c > 0",
        )?;
        let tags = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut stmt = self.conn.prepare(
            "SELECT pt.photo_id, pt.tag_id FROM photo_tags pt
             JOIN photos p ON p.id = pt.photo_id AND p.missing = 0",
        )?;
        let edges = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((photos, tags, edges))
    }

    /// Library-wide tidy: prune redundant ancestor tags from every photo at once. Returns
    /// the total number of assignments removed.
    pub fn tidy_redundant_tags(&self) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM photo_tags
             WHERE (photo_id, tag_id) IN (
                 SELECT pa.photo_id, a.id FROM tags a
                 JOIN photo_tags pa ON pa.tag_id = a.id
                 WHERE EXISTS (
                     SELECT 1 FROM tags d
                     JOIN photo_tags pd ON pd.tag_id = d.id AND pd.photo_id = pa.photo_id
                     WHERE d.id != a.id
                       AND substr(d.full_path, 1, length(a.full_path) + 1) = a.full_path || '/'
                 )
             )",
            [],
        )?)
    }

    pub fn remove_tag(&self, photo_id: i64, tag_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM photo_tags WHERE photo_id = ?1 AND tag_id = ?2",
            params![photo_id, tag_id],
        )?;
        Ok(())
    }

    pub fn get_photo_tags(&self, photo_id: i64) -> Result<Vec<Tag>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.name, t.full_path, t.parent_id, t.description, t.auto_rule, t.uuid, t.private
             FROM tags t JOIN photo_tags pt ON pt.tag_id = t.id
             WHERE pt.photo_id = ?1
             ORDER BY t.full_path_norm",
        )?;
        let rows = stmt.query_map(params![photo_id], row_to_tag)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

/// Result of [`Catalog::upsert_photo`].
pub struct UpsertResult {
    pub id: i64,
    pub uuid: String,
    /// True when this call created the row (and thus the UUID) for the first time.
    pub created: bool,
    /// True when an existing row matched by path and its file stats (mtime + size)
    /// were already identical — i.e. the file's bytes haven't changed since the last
    /// scan. Lets the scanner skip re-extracting metadata on a re-scan. Always false
    /// for newly-created or re-homed (moved) rows.
    pub unchanged: bool,
}

/// Split a catalog-relative path into (directory, lowercased filename stem) for
/// RAW+JPEG pairing — e.g. "2025/02/02/DSC06647.JPG" -> ("2025/02/02", "dsc06647").
fn dir_stem(path: &str) -> (String, String) {
    let (dir, file) = match path.rfind('/') {
        Some(i) => (path[..i].to_string(), &path[i + 1..]),
        None => (String::new(), path),
    };
    let stem = match file.rfind('.') {
        Some(i) => &file[..i],
        None => file,
    };
    (dir, stem.to_lowercase())
}

fn row_to_photo(r: &Row) -> rusqlite::Result<Photo> {
    Ok(Photo {
        id: r.get(0)?,
        uuid: r.get(1)?,
        path: r.get(2)?,
        rating: r.get(3)?,
        label: r.get(4)?,
        pick_state: PickState::from_db_str(&r.get::<_, String>(5)?),
        capture_time: r.get(6)?,
        width: r.get(7)?,
        height: r.get(8)?,
        camera_model: r.get(9)?,
        lens: r.get(10)?,
        aperture: r.get(11)?,
        shutter_speed: r.get(12)?,
        iso: r.get(13)?,
        external_editors: r.get(14)?,
        thumbnail_path: r.get(15)?,
        stack_count: r.get(16)?,
        stack_parent_id: r.get(17)?,
        metadata_ready: r.get(18)?,
        sharpness: r.get(19)?,
        sharpness_method: r.get(20)?,
        burst_flag: r.get(21)?,
    })
}

/// Build a `Tag` from a row selecting (id, name, full_path, parent_id, description,
/// auto_rule, uuid, private) in that column order.
fn row_to_tag(r: &Row) -> rusqlite::Result<Tag> {
    Ok(Tag {
        id: r.get(0)?,
        name: r.get(1)?,
        full_path: r.get(2)?,
        parent_id: r.get(3)?,
        description: r.get(4)?,
        auto_rule: r.get(5)?,
        uuid: r.get(6)?,
        private: r.get::<_, i64>(7)? != 0,
    })
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
