//! The catalog: a single SQLite file that is the source of truth for photos,
//! tags, ratings, and metadata.
//!
//! Photo paths are stored RELATIVE to the catalog root (the `catalog_root`
//! setting). All absolute-path conversion happens at this boundary so the rest
//! of the app never hard-codes machine-specific paths — this is what makes a
//! catalog portable between the laptop and the desktop.

mod albums;
mod autotags;
pub use autotags::{AutoTagRefusal, BlockedAutoTagRule, TagBatchOutcome};
mod batches;
mod busy;
pub mod culling;
mod edits;
pub use edits::{NewVersion, HISTORY_BASELINE_LABEL, HISTORY_CAP};
mod facets;
mod groups;
mod identity;
mod iptc_owed;
mod lifecycle;
mod merge;
mod reconcile;
mod safety;
pub mod locations;
mod models;
mod publications;
mod query;
mod schema;
mod smart_albums;
mod stats;
mod tag_path;
pub mod tag_maintenance;
mod terms;
mod trash;
mod visibility;
pub(crate) mod working_files;

#[cfg(test)]
mod performance_harness;

pub use facets::{Facet, SOFT_THRESHOLD_DEFAULT, SOFT_THRESHOLD_KEY};
pub use identity::{
    bind_sidecar_identity, canonical_photo_identity, is_photo_identity, legacy_photo_identity,
    photo_identity_for, ForeignConflictAction, ForeignConflictSummary, IdentityConflictAction, LEGACY_IDENTITY_NAMESPACE, IdentityConflictOutcome, IdentityRepairCursor,
    IdentityRepairPlan, IdentityRepairSummary, PendingIdentity, PendingIdentityField,
    PendingIdentityRow, PendingIdentitySummary, SidecarIdentity,
};
pub use iptc_owed::{IptcMask, IptcSettled, IptcSidecarState, IptcSidecarWrite, OwedDismissal, OwedIptc};
pub use locations::{NameHolder, PathCandidate, ResolveMode};
pub use lifecycle::{
    any_backup_present, carry_companions, copy_and_verify, copy_with_companions, filter_offload_eligible,
    resolve_backup_plan, resolve_offload_plan, resolve_restore_plan, sha256_file, verify_and_delete_locals,
    verify_and_delete_locals_abortable, BackupCandidates, BackupPlan, BackupReport, CarriedCompanion,
    CompanionCarry, CopyOutcome, FreedPhoto, OffloadCandidates, OffloadCarry, OffloadEligibility, OffloadPlan,
    OffloadReport, PhotoBackup, PhotoOffload, PhotoRestore, RestoreCandidates, RestorePlan, RestoreReport,
    user_reason, SkipKind, SkippedPhoto, IN_PROGRESS_REASON, LOCAL_CHANGED_REASON, SUPERSEDED_REASON,
};
pub(crate) use lifecycle::verify_and_delete_locals_until;
#[cfg(test)]
pub(crate) use lifecycle::offload_hook;
pub use merge::{MergeOutcome, MergeSummary, IMPORTED_EDIT_VERSION};
pub use models::{
    Album, BurstInput, CoverPin, ExportKeywords, HistoryStep, ImportBatch, IptcFields, LocationRole, MetadataEntry,
    PendingOperation, Photo, PhotoLocation, PhotoVersion, PickState, PromotedMetadata,
    Publication, StorageStatus, SmartAlbum, Tag, TagGroup, TagTerm, TagWithCount, VersionHistory,
    Volume, VolumeKind,
};
pub use query::{CullingFilter, PhotoPage, PhotoQuery, PhotoSort, PhotoWindow, StorageTier};
pub use smart_albums::rule_to_sql;
pub use stats::{CatalogStatsRaw, CullCross};
pub use reconcile::DrainSummary;
pub use trash::TrashSummary;
pub use safety::{SafetyStatus, SafetySummary};

use rusqlite::{params, Connection, ErrorCode, OptionalExtension, Row};
use std::cell::Cell;
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
    /// A hand assignment or removal of an auto-tag, which the auto-tag engine owns
    /// (`autotags.rs`): refused, because its next pass would silently undo it.
    #[error("{0}")]
    AutoTag(AutoTagRefusal),
    #[error("invalid input: {0}")]
    Validation(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("path {0} is outside the catalog root")]
    OutsideRoot(String),
    #[error("failed to enable WAL mode; SQLite reported journal_mode={0}")]
    JournalMode(String),
    /// The catalog was last opened by a newer ChairPhoto, whose schema this build does not
    /// know (`schema::SCHEMA_VERSION`): refused, nothing written.
    #[error(
        "this catalog was last opened by a newer version of ChairPhoto (catalog schema {found}; \
         this version knows up to {known}) — open it with that version or a newer one"
    )]
    NewerSchema { found: i64, known: i64 },
}

// Match the 60-second migration lock wait; catalog opens run on blocking workers.
const WAL_BUSY_RETRY_TIMEOUT: Duration = Duration::from_secs(60);
const WAL_BUSY_RETRY_DELAY: Duration = Duration::from_millis(10);

/// The schema version whose migration fills `photos.exif_orientation` (#136). Its one use is
/// the `prior_version` gate in `migrate_locked`; renumbering the migration is changing this
/// and [`schema::SCHEMA_VERSION`] together.
pub(crate) const EXIF_ORIENTATION_SINCE: i64 = 26;

/// The schema version of the automatic Library face's heal (#252 review L1): a catalog
/// stamped below it once the face columns exist was opened by an older build since this one
/// last opened it (`Catalog::heal_faces`).
pub(crate) const AUTO_FACES_SINCE: i64 = 28;

/// The `settings` key holding the catalog's own identity: a UUID v4 minted once, the first
/// time a catalog is opened by a build that knows it, and never changed. It survives reopening,
/// moving the file and restoring a backup of it; a merge or bundle import never copies it.
/// Its one use today is scoping ChairPhoto's face-region marker in sidecars (#135, see
/// `docs/face-tagging.md`), so a region one catalog wrote is foreign to every other.
pub const CATALOG_UUID_KEY: &str = "catalog_uuid";

/// The catalog's identity ([`CATALOG_UUID_KEY`]), minted on first use. Race-free: two
/// connections minting at once both read back the one row `INSERT OR IGNORE` let in.
pub fn catalog_uuid(conn: &Connection) -> rusqlite::Result<String> {
    conn.execute(
        "INSERT OR IGNORE INTO settings (key, value) VALUES (?1, ?2)",
        params![CATALOG_UUID_KEY, uuid::Uuid::new_v4().to_string()],
    )?;
    conn.query_row("SELECT value FROM settings WHERE key = ?1", [CATALOG_UUID_KEY], |r| r.get(0))
}

/// The catalog's identity as stored, `None` before it is minted — a read only, never a write,
/// for code outside the core (a plugin's sidecar writer) that must not write core tables.
/// Every catalog opened through [`Catalog::open`] has one: the migration mints it.
pub fn read_catalog_uuid(conn: &Connection) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT value FROM settings WHERE key = ?1", [CATALOG_UUID_KEY], |r| r.get(0))
        .optional()
}

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
    /// True when this catalog still carries the retired `photo_metadata.value_norm` column
    /// (A7). Nothing reads it, but SQLite's `DROP COLUMN` rewrites all ~45 M rows, which is
    /// far too slow to do silently at startup — so the column is shed by an explicit
    /// compaction instead, and until then every INSERT still has to name it.
    ///
    /// Per connection, not per catalog: `open_secondary` skips migration, and scans write
    /// metadata through exactly that connection.
    legacy_value_norm: Cell<bool>,
    /// Process-unique id of this handle — what `app::CatalogIdentity` compares. Every
    /// `open`/`open_secondary` gets a new one, so a reopened catalog is a different instance.
    instance: u64,
}

/// The next process-unique catalog handle id (never 0).
fn next_instance() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl Catalog {
    /// This handle's process-unique id. Two handles to the same file have different ids; a
    /// handle keeps its id for life. See `app::CatalogIdentity`.
    pub fn instance_id(&self) -> u64 {
        self.instance
    }

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
            legacy_value_norm: Cell::new(false),
            instance: next_instance(),
        };
        catalog.migrate()?;
        // After migration: the table shape is settled by this point.
        catalog
            .legacy_value_norm
            .set(has_column(&catalog.conn, "photo_metadata", "value_norm")?);
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
        let legacy_value_norm = has_column(&conn, "photo_metadata", "value_norm")?;
        Ok(Self {
            conn,
            root: root.to_path_buf(),
            path: catalog_path.to_path_buf(),
            legacy_value_norm: Cell::new(legacy_value_norm),
            instance: next_instance(),
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
        // The schema the catalog was last stamped with, read BEFORE this open stamps its own.
        // Every build stamps its own on open — an older one included, downwards — so a value
        // below ours also says an older build has opened the catalog since we last did.
        let prior_version: i64 = self
            .get_setting("schema_version")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if prior_version > schema::SCHEMA_VERSION {
            // A newer build's catalog: its schema may hold what this build would misread or
            // corrupt. Refused before anything is written (the transaction rolls back).
            return Err(CatalogError::NewerSchema { found: prior_version, known: schema::SCHEMA_VERSION });
        }
        // Mint the catalog's identity once (a no-op on every later open).
        catalog_uuid(&self.conn)?;
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
        // A7: retire `idx_photo_metadata_lookup(key, value_norm, photo_id)`. It was staged
        // for a "filter by EXIF key/value" feature that was never built — no code in the
        // crate issues the `WHERE key = ?` probe it exists for — and measured 1,844 MB (22%
        // of the catalog) while making metadata inserts ~2.8x slower on every scan. Dropping
        // an index is instant and needs no table rewrite, so every catalog gets this on its
        // next open; the `value_norm` column it indexed goes on the next compaction, which
        // does rewrite. Rebuilding it is one CREATE INDEX if the feature ever lands.
        self.conn
            .execute_batch("DROP INDEX IF EXISTS idx_photo_metadata_lookup;")?;
        // Verified-hash on locations for the backup/offload lifecycle (schema v10).
        self.ensure_column("photo_locations", "verified_hash", "TEXT")?;
        // Trash (cluster B, B2). Must precede the view below, which selects on it.
        self.ensure_column("photos", "trashed_at", "INTEGER")?;
        // The user-visible photo view: everything that lists or counts photos for the
        // user reads this, so the visibility rule lives in one place instead of being
        // re-spelled per call site. Dropped and recreated on every open rather than
        // `IF NOT EXISTS`, because its definition grows (trash joins the predicate in B2)
        // and a view left at an older definition would silently apply an older rule.
        self.conn.execute_batch(
            "DROP VIEW IF EXISTS photos_visible;
             CREATE VIEW photos_visible AS
                 SELECT * FROM photos WHERE missing = 0 AND trashed_at IS NULL;",
        )?;
        // Companions carried alongside the image at a location (cluster B). A copy is the
        // image plus its declared companions; before this, backup carried only the image
        // and left darktable/RapidRAW edit state behind (#80).
        // A NULL `carried_mtime` means "the scanner found this companion locally and it was
        // never carried here" — a state the first shape of this table could not express,
        // because both mtime columns were NOT NULL. The table has never shipped and holds
        // nothing a scan plus a backup cannot re-derive, so an older shape is replaced
        // outright rather than migrated in place.
        let stale_shape: bool = self
            .conn
            .prepare("PRAGMA table_info(photo_location_companions)")?
            .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(3)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .any(|(name, notnull)| name == "carried_mtime" && *notnull == 1);
        if stale_shape {
            self.conn.execute_batch("DROP TABLE photo_location_companions;")?;
        }
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS photo_location_companions (
                 location_id       INTEGER NOT NULL REFERENCES photo_locations(id) ON DELETE CASCADE,
                 name              TEXT    NOT NULL,
                 carried_mtime     INTEGER,
                 source_mtime_seen INTEGER,
                 carried_at        INTEGER,
                 PRIMARY KEY (location_id, name)
             );",
        )?;
        // Pixel-derived B&W flag for the monochrome auto-tag (schema v12).
        self.ensure_column("photos", "is_grayscale", "INTEGER")?;
        // Non-destructive user orientation override (degrees clockwise), schema v17.
        self.ensure_column("photos", "user_rotation", "INTEGER NOT NULL DEFAULT 0")?;
        // The original's EXIF Orientation (1-8, NULL = unknown), for face regions (#136).
        // Backfilled below from what earlier scans already stored in `photo_metadata`.
        self.ensure_column("photos", "exif_orientation", "INTEGER")?;
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
        self.migrate_auto_faces(prior_version)?;

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

        // Versioned migrations, gated on `prior_version` (read above, before the stamp).
        self.migrate_sidecar_identity_fields()?;
        // Schema v22 (#33): a dismissed conflict stops being retried and stops counting as
        // debt. Must run AFTER `migrate_sidecar_identity_fields`, whose v20→v21 rebuild
        // recreates the table without this column.
        self.ensure_column(
            "pending_sidecar_identity",
            "dismissed_at",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
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
        if prior_version < 23 {
            // #146: before #141 a scan adopted a sidecar's non-UUID `xmp:Identifier` as
            // `photos.uuid`. Give those rows a minted UUID and queue each copy's sidecar
            // as the conflict it now is. Needs the locations (v2) and the queue's
            // `dismissed_at` column, both established above.
            self.remint_non_identity_photos()?;
        }
        if prior_version < 24 {
            // #146 L5: an identity is stored lowercase. After v23, so every row it sees
            // that is not a UUID has already been re-minted.
            self.canonicalise_photo_identities()?;
        }
        // Schema v25 (#148): `pending_sidecar_iptc`, created by SCHEMA_SQL above. No backfill:
        // which earlier writes failed is unknown, and owing every photo's IPTC would rewrite
        // every sidecar in the library on the next repair pass.
        if prior_version < EXIF_ORIENTATION_SINCE {
            // #136: a rescan extracts only new or changed files, so a catalog scanned before
            // the column existed would never get it. Every earlier scan stored exiftool's
            // `EXIF:Orientation` as a generic entry; promote that.
            self.backfill_exif_orientation()?;
        }
        // Keep the catalog-root (local) volume pointing at the current root, so
        // re-rooting the catalog moves it too.
        self.sync_default_volume_root()?;

        self.set_setting("schema_version", &schema::SCHEMA_VERSION.to_string())?;
        // Only on a catalog that has never been analysed — the one-time cost of catching
        // up an existing library. Re-analysing on every open would put ~0.15 s of SQLite
        // work in front of every catalog switch to re-derive statistics that did not
        // change; a finished scan is what actually invalidates them, and `phase_b_enrich`
        // calls `optimize` there.
        if self.lacks_planner_statistics() {
            self.optimize();
        }
        Ok(())
    }

    /// Refresh the query planner's statistics (`sqlite_stat1`).
    ///
    /// Without this SQLite plans from heuristics alone. Measured consequence on a real
    /// 165k-photo catalog (agent measurement, 2026-08-17): `sqlite_stat1` did not exist,
    /// and the planner drove the library query through `idx_photos_missing` — an index on
    /// a column with two distinct values, 144,242 of 165,093 rows matching — because
    /// nothing told it the column was not selective. With statistics it correctly prefers
    /// a scan.
    ///
    /// Analysed per table rather than catalog-wide, and unbounded rather than sampled.
    /// Both choices are measurements, not taste (agent measurements, 2026-08-17):
    ///
    /// - **Not a bare `ANALYZE`.** That would also analyse `photo_metadata`, which on a
    ///   real library holds ~274 rows per photo — 45 million rows and three indexes,
    ///   ~82% of the catalog on disk. Measured at 0.8 s per 2 M rows, a full pass costs
    ///   roughly 18 s. Nothing joins that table (every access is by `photo_id` through its
    ///   primary key), so the statistics would buy nothing for the time.
    /// - **Not `PRAGMA analysis_limit`.** Bounding the sample to 400 rows per index makes
    ///   `ANALYZE` cheap but its output too coarse: with it, the planner still chose a
    ///   full temp-B-tree sort over `idx_photos_sort_date` at both 5 k and 165 k rows.
    ///   Sampled statistics are worse than useless here — they cost time and keep the bad
    ///   plan. Unbounded `ANALYZE photos` measures 0.144 s at 165 k rows and gets it right.
    /// - **`PRAGMA optimize` was tried first and rejected.** It returns `Ok` while
    ///   declining to analyse anything, leaving `sqlite_stat1` empty.
    ///
    /// The listed tables are the ones real queries join or filter through; they are core
    /// (`catalog/schema.rs`) so they always exist.
    ///
    /// **Best-effort by design.** Statistics are an optimization, never a correctness
    /// requirement: every query returns the same rows with or without them. Failing an
    /// open because another connection held the write lock for a moment would trade a
    /// working catalog for a slightly better plan, so errors are swallowed here rather
    /// than propagated.
    pub(crate) fn optimize(&self) {
        let _ = self.conn.execute_batch(
            "ANALYZE photos; ANALYZE photo_tags; ANALYZE photo_locations; ANALYZE tags;",
        );
    }

    /// True when the planner has no statistics at all — the state every catalog created
    /// before `optimize` existed is in, and the one that produced the measured bad plan.
    fn lacks_planner_statistics(&self) -> bool {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'sqlite_stat1'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n == 0)
            .unwrap_or(false)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Raw connection for in-crate plugins that own their own prefix-named tables
    /// (e.g. `ai__suggestions`). Core defines no plugin tables; see docs/plugin-system.md.
    ///
    /// `pub` because `crates/app` is a separate crate and its tests query through it (as the
    /// Tauri shell's commands, also a separate crate, did until #165); it is not an
    /// invitation for new front-end SQL.
    pub fn conn(&self) -> &rusqlite::Connection {
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

    /// #252: the Library face follows the most recently changed version unless pinned.
    /// Additive, gated on the columns rather than `schema_version` (like the history tables):
    /// - `photo_versions.changed_seq`, backfilled per photo in `updated_at` order (then id) —
    ///   the best record of the last change there is, though a rename or reorder bumped it too;
    /// - `photo_cover.pin`: every existing cover was chosen by hand, so it is pinned; a
    ///   cleared one (NULL) becomes automatic;
    /// - then every face is brought up to date, so a photo edited before this shows its
    ///   latest version at once.
    ///
    /// On every open after that, the faces are healed from what an older build wrote
    /// meanwhile (schema v28, review of #252 L1): the trigger keeps `changed_seq` true for
    /// its settings changes ([`Catalog::ensure_face_trigger`]), and [`Catalog::heal_faces`]
    /// orders the versions it created and moves each face it left behind. `prior_version` is
    /// the schema the catalog was stamped with before this open.
    fn migrate_auto_faces(&self, prior_version: i64) -> Result<()> {
        let had_seq = has_column(&self.conn, "photo_versions", "changed_seq")?;
        let had_pin = has_column(&self.conn, "photo_cover", "pin")?;
        if had_seq && had_pin {
            self.ensure_face_trigger()?;
            // Below v28 with the columns already there: an older build has opened the
            // catalog since this one last did (every build stamps its own on open).
            return self.heal_faces(prior_version < AUTO_FACES_SINCE);
        }
        self.ensure_column("photo_versions", "changed_seq", "INTEGER NOT NULL DEFAULT 0")?;
        self.ensure_column("photo_cover", "pin", "INTEGER NOT NULL DEFAULT 0")?;
        if !had_seq {
            self.conn.execute_batch(
                "UPDATE photo_versions SET changed_seq = (
                     SELECT COUNT(*) FROM photo_versions o
                      WHERE o.photo_id = photo_versions.photo_id
                        AND (o.updated_at < photo_versions.updated_at
                             OR (o.updated_at = photo_versions.updated_at AND o.id <= photo_versions.id)));",
            )?;
        }
        if !had_pin {
            self.conn
                .execute_batch("UPDATE photo_cover SET pin = 1 WHERE version_id IS NOT NULL;")?;
        }
        self.ensure_face_trigger()?;
        self.refresh_all_faces()
    }

    /// Schema v26 (#136): fill `photos.exif_orientation` from the `EXIF:Orientation` entry a
    /// scan stored in `photo_metadata` (exiftool's phrase, "Rotate 90 CW"), or failing that
    /// the numeric code the AF-point pass stored. A row with neither stays NULL: unknown,
    /// never guessed. Rows that already hold a value are left alone.
    fn backfill_exif_orientation(&self) -> Result<()> {
        let rows: Vec<(i64, String)> = {
            let mut stmt = self.conn.prepare(
                "SELECT m.photo_id, m.value FROM photo_metadata m
                   JOIN photos p ON p.id = m.photo_id AND p.exif_orientation IS NULL
                  WHERE (m.group_name = 'EXIF' AND m.key = 'Orientation')
                     OR (m.group_name = ?1 AND m.key = ?2)
                  ORDER BY m.photo_id, m.group_name = 'EXIF'",
            )?;
            let rows = stmt
                .query_map(
                    params![crate::metadata::AF_GROUP, crate::metadata::AF_ORIENTATION_KEY],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        // Ordered so a photo's `EXIF:Orientation` comes after its AF-pass code and wins.
        let mut update = self
            .conn
            .prepare("UPDATE photos SET exif_orientation = ?1 WHERE id = ?2")?;
        for (photo_id, value) in rows {
            if let Some(code) = crate::metadata::parse_exif_orientation(&value) {
                update.execute(params![code, photo_id])?;
            }
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
                ON pending_sidecar_identity(photo_id);
             CREATE INDEX IF NOT EXISTS idx_pending_sidecar_identity_copy
                ON pending_sidecar_identity(photo_id, volume_id, relative_path);",
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

    /// Set several settings in one transaction: all of them or none (a pair that must not be
    /// seen half-written, such as an OAuth token and its secret).
    pub fn set_settings(&self, pairs: &[(&str, &str)]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for (key, value) in pairs {
            self.set_setting(key, value)?;
        }
        tx.commit()?;
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
    ///
    /// Also sheds retired columns first (currently `photo_metadata.value_norm`). That is a
    /// table rewrite of tens of millions of rows, which is exactly why it lives here rather
    /// than in the startup migration: this operation is already explicit, already slow, and
    /// already about reclaiming space. No behaviour changes — the column had no readers.
    pub fn vacuum(&mut self) -> Result<()> {
        self.drop_retired_columns()?;
        self.conn.execute_batch("VACUUM")?;
        Ok(())
    }

    /// Drop columns the schema has retired but existing catalogs still carry, when the user
    /// asks for a compaction. Idempotent: a catalog that has already shed them does nothing.
    ///
    /// `DROP COLUMN` is refused while an index references the column, so this depends on
    /// migration having dropped `idx_photo_metadata_lookup` first — it runs on every open,
    /// so by the time a user can press Compact it has happened.
    fn drop_retired_columns(&mut self) -> Result<()> {
        if has_column(&self.conn, "photo_metadata", "value_norm")? {
            self.conn
                .execute_batch("ALTER TABLE photo_metadata DROP COLUMN value_norm;")?;
            self.legacy_value_norm.set(false);
        }
        Ok(())
    }

    /// This catalog's identity: see [`CATALOG_UUID_KEY`].
    pub fn catalog_uuid(&self) -> Result<String> {
        Ok(catalog_uuid(&self.conn)?)
    }

    /// What `photo_id`'s offline thumbnail is kept under (#258): this catalog's identity and
    /// the photo's UUID, read together. A read only — it never mints the catalog's identity,
    /// so it costs a grid tile no write. `None` when the photo is not in the catalog or either
    /// value is not a UUID ([`crate::thumbnails::OfflineThumbKey::new`]).
    pub fn offline_thumb_key(&self, photo_id: i64) -> Result<Option<crate::thumbnails::OfflineThumbKey>> {
        let row: Option<(String, Option<String>)> = self
            .conn
            .query_row(
                "SELECT p.uuid, (SELECT value FROM settings WHERE key = ?2) FROM photos p WHERE p.id = ?1",
                params![photo_id, CATALOG_UUID_KEY],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(row.and_then(|(photo, catalog)| crate::thumbnails::OfflineThumbKey::new(catalog.as_deref()?, &photo)))
    }

    /// [`Self::offline_thumb_key`] for each of `ids` this catalog has (one chunked read): the
    /// keys the pre-#258 thumbnails are migrated to (`thumbnails::adopt_id_keyed_thumbs`).
    pub fn offline_thumb_keys(
        &self,
        ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, crate::thumbnails::OfflineThumbKey>> {
        let mut out = std::collections::HashMap::new();
        let Some(catalog) = read_catalog_uuid(&self.conn)? else { return Ok(out) };
        for chunk in ids.chunks(SQLITE_PARAM_CHUNK) {
            let marks = vec!["?"; chunk.len()].join(",");
            let mut stmt = self.conn.prepare(&format!("SELECT id, uuid FROM photos WHERE id IN ({marks})"))?;
            let rows = stmt.query_map(rusqlite::params_from_iter(chunk), |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows {
                let (id, photo) = row?;
                if let Some(key) = crate::thumbnails::OfflineThumbKey::new(&catalog, &photo) {
                    out.insert(id, key);
                }
            }
        }
        Ok(out)
    }

    /// This catalog's offline thumbnails as the store names them — its UUID and every photo's
    /// (trashed ones included) — for [`crate::thumbnails::prune_offline_thumbs`]. `None` with
    /// no catalog UUID minted. One read of every row.
    pub fn offline_thumb_owner(&self) -> Result<Option<crate::thumbnails::OfflineCatalog>> {
        let Some(catalog) = read_catalog_uuid(&self.conn)? else { return Ok(None) };
        let mut stmt = self.conn.prepare("SELECT uuid FROM photos")?;
        let photos = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(crate::thumbnails::OfflineCatalog::new(&catalog, photos))
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
    ///
    /// `sidecar_uuid` is trusted as an identity: a caller that read it from a sidecar passes
    /// it only if [`is_photo_identity`] accepts it (#141). A UUID is matched and stored in
    /// its canonical lowercase spelling (#146), whatever case the sidecar wrote it in. A
    /// trusted value that is not a UUID — an old bundle's manifest id — is the identity an
    /// older catalog gave the photo, so it is matched and stored as its
    /// [`legacy_photo_identity`] and recorded as the row's legacy identifier, as schema v23
    /// does: `photos.uuid` never holds a non-UUID.
    pub fn upsert_photo_with_identity(
        &self,
        absolute_path: &Path,
        folder_id: Option<i64>,
        mtime_ns: i64,
        size: i64,
        sidecar_uuid: Option<&str>,
    ) -> Result<UpsertResult> {
        let source = IdentitySource::Trusted(sidecar_uuid);
        self.upsert_photo_from(absolute_path, folder_id, mtime_ns, size, source)
    }

    /// [`Self::upsert_photo_with_identity`] for a scanned file whose sidecar's
    /// `xmp:Identifier` holds `found`, read as [`Self::scan_identity`] reads it. That reading
    /// can mean a legacy-identifier lookup and a stat of each recorded copy, so it is done
    /// only when no row is at this path already — a rescan of an unchanged file pays nothing
    /// for it (#146 review F7).
    pub fn upsert_scanned_photo(
        &self,
        absolute_path: &Path,
        folder_id: Option<i64>,
        mtime_ns: i64,
        size: i64,
        found: Option<&str>,
    ) -> Result<UpsertResult> {
        let source = IdentitySource::Sidecar(found);
        self.upsert_photo_from(absolute_path, folder_id, mtime_ns, size, source)
    }

    fn upsert_photo_from(
        &self,
        absolute_path: &Path,
        folder_id: Option<i64>,
        mtime_ns: i64,
        size: i64,
        source: IdentitySource<'_>,
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
        //    same photo that has moved/re-rooted → re-home it (no duplicate). The identity is
        //    only worked out when the path did not match.
        let identity = match by_path {
            Some(_) => None,
            None => self.identity_from(&source, size, absolute_path)?,
        };
        let sidecar_uuid = identity.as_deref();
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
        // A row whose own file is still in place is not re-homed (#150): this file is
        // another copy carrying its identity, so it gets a row and a minted UUID of its own,
        // and the binding reports the identity in its sidecar as a conflict.
        let held_in_place = match by_uuid {
            Some(id) => {
                let (volume_id, _) = self.volume_for_path(absolute_path)?;
                self.primary_copy_left_in_place(id, volume_id, true, absolute_path)?
            }
            None => false,
        };
        let (by_uuid, sidecar_uuid) =
            if held_in_place { (None, None) } else { (by_uuid, sidecar_uuid) };

        let matched_by_path = by_path.is_some();
        let result = if let Some((id, uuid, old_mtime, old_size)) = by_path {
            let unchanged = old_mtime == mtime_ns && old_size == size;
            self.conn.execute(
                "UPDATE photos SET folder_id = ?1, mtime_ns = ?2, size = ?3,
                    extension = ?4, missing = 0, updated_at = ?5 WHERE id = ?6",
                params![folder_id, mtime_ns, size, extension, ts, id],
            )?;
            UpsertResult { id, uuid, created: false, unchanged }
        } else if let Some(id) = by_uuid {
            // Re-home the moved file: point the existing row at the new path. Debt queued
            // for the path it left would name a file that is no longer there.
            self.forget_identity_debt_left_behind(id, absolute_path)?;
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
        // A copy kept apart from the row holding its identity does not take its legacy value.
        if !held_in_place {
            self.record_legacy_identifier(result.id, source.legacy_value(matched_by_path))?;
        }
        Ok(result)
    }

    /// Index a photo that physically lives on a non-root volume (e.g. a pre-existing
    /// archive on the NAS), **in place** — no local copy is made. The volume-relative
    /// path becomes the logical `path`, and the file's location is recorded on that
    /// volume (role Primary). Since there's no local copy, the photo shows as living on
    /// the NAS ("On NAS"). Matches an existing row by an existing location at this
    /// volume+path, then by sidecar UUID, else creates a new row (folder_id null — it's
    /// not under an indexed local folder). See `scanner::scan_external_folder`.
    ///
    /// `sidecar_uuid` is trusted as an identity, and canonicalised (or mapped from a legacy
    /// value), as in [`Self::upsert_photo_with_identity`].
    pub fn upsert_photo_on_volume(
        &self,
        absolute: &Path,
        mtime_ns: i64,
        size: i64,
        sidecar_uuid: Option<&str>,
    ) -> Result<UpsertResult> {
        self.upsert_photo_on_volume_from(absolute, mtime_ns, size, IdentitySource::Trusted(sidecar_uuid))
    }

    /// [`Self::upsert_photo_on_volume`] for a scanned file whose sidecar holds `found`; see
    /// [`Self::upsert_scanned_photo`] for why the identity is read only after the location
    /// misses.
    pub fn upsert_scanned_photo_on_volume(
        &self,
        absolute: &Path,
        mtime_ns: i64,
        size: i64,
        found: Option<&str>,
    ) -> Result<UpsertResult> {
        self.upsert_photo_on_volume_from(absolute, mtime_ns, size, IdentitySource::Sidecar(found))
    }

    fn upsert_photo_on_volume_from(
        &self,
        absolute: &Path,
        mtime_ns: i64,
        size: i64,
        source: IdentitySource<'_>,
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

        // 2) Otherwise match the file's own UUID (same photo from another machine/path),
        //    worked out only now that the location did not match.
        let identity = match by_loc {
            Some(_) => None,
            None => self.identity_from(&source, size, absolute)?,
        };
        let sidecar_uuid = identity.as_deref();
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
        // As in `upsert_photo_from` (#150): a row whose primary copy on this volume is still
        // in place is not moved off it. One whose primary copies are on other volumes gains
        // this file as another location; that moves nothing.
        let held_in_place = match &by_uuid {
            Some((id, _)) => self.primary_copy_left_in_place(*id, volume_id, false, absolute)?,
            None => false,
        };
        let (by_uuid, sidecar_uuid) =
            if held_in_place { (None, None) } else { (by_uuid, sidecar_uuid) };

        let matched_by_path = by_loc.is_some();
        let result = if let Some((id, uuid, old_mtime, old_size)) = by_loc {
            let unchanged = old_mtime == mtime_ns && old_size == size;
            self.conn.execute(
                "UPDATE photos SET mtime_ns = ?1, size = ?2, extension = ?3, missing = 0,
                    updated_at = ?4 WHERE id = ?5",
                params![mtime_ns, size, extension, ts, id],
            )?;
            UpsertResult { id, uuid, created: false, unchanged }
        } else if let Some((id, uuid)) = by_uuid {
            // A re-home within this volume leaves its old path: debt queued for that path
            // names a file that is no longer there (#150, as for a root re-home).
            self.forget_identity_debt_left_behind(id, absolute)?;
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
        if !held_in_place {
            self.record_legacy_identifier(result.id, source.legacy_value(matched_by_path))?;
        }
        Ok(result)
    }

    /// The identity an upsert of a `size`-byte file matches and adopts, from where `source`
    /// says it comes. `scanned` is the file being upserted, passed on to
    /// [`Catalog::scan_identity`] for its legacy-identifier re-home guard (#224 L1).
    fn identity_from(&self, source: &IdentitySource<'_>, size: i64, scanned: &Path) -> Result<Option<String>> {
        match *source {
            IdentitySource::Trusted(value) => Ok(value.and_then(photo_identity_for)),
            IdentitySource::Sidecar(found) => self.scan_identity(found, size, scanned),
        }
    }

    pub fn get_photo(&self, photo_id: i64) -> Result<Photo> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {cols} FROM photos {face} WHERE photos.id = ?1",
                    cols = query::photo_columns("photos"),
                    face = query::face_join("photos")
                ),
                params![photo_id],
                row_to_photo,
            )
            .optional()?
            .ok_or_else(|| CatalogError::NotFound(format!("photo {photo_id}")))
    }

    /// One photo by its stable uuid — the chairphoto:// deep-link target.
    /// Served by `idx_photos_uuid`.
    pub fn get_photo_by_uuid(&self, uuid: &str) -> Result<Photo> {
        let Some(identity) = photo_identity_for(uuid) else {
            return Err(CatalogError::NotFound(format!("photo uuid {uuid:?}")));
        };
        self.conn
            .query_row(
                &format!(
                    "SELECT {cols} FROM photos {face} WHERE photos.uuid = ?1",
                    cols = query::photo_columns("photos"),
                    face = query::face_join("photos")
                ),
                params![identity],
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
        let uuids: Vec<String> = uuids.iter().filter_map(|u| photo_identity_for(u)).collect();
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
            // `value_norm` is retired (A7): nothing ever read it, and normalizing every one
            // of ~274 values per photo was pure cost on every scan. A catalog that still has
            // the column gets an empty string — the column is NOT NULL and only a compaction
            // can remove it — and the normalization is not computed either way.
            let mut stmt = if self.legacy_value_norm.get() {
                match self.conn.prepare(
                    "INSERT OR IGNORE INTO photo_metadata(photo_id, key, group_name, value, value_norm)
                     VALUES(?1, ?2, ?3, ?4, '')",
                ) {
                    Ok(stmt) => stmt,
                    Err(_)
                        if !has_column(&self.conn, "photo_metadata", "value_norm")? =>
                    {
                        // Compaction can run while a scan owns a secondary connection. Its
                        // cached legacy shape is then stale, so switch that connection to
                        // the new INSERT without making every metadata write query the schema.
                        self.legacy_value_norm.set(false);
                        self.conn.prepare(
                            "INSERT OR IGNORE INTO photo_metadata(photo_id, key, group_name, value)
                             VALUES(?1, ?2, ?3, ?4)",
                        )?
                    }
                    Err(error) => return Err(error.into()),
                }
            } else {
                self.conn.prepare(
                    "INSERT OR IGNORE INTO photo_metadata(photo_id, key, group_name, value)
                     VALUES(?1, ?2, ?3, ?4)",
                )?
            };
            for e in entries {
                stmt.execute(params![photo_id, e.key, e.group_name, e.value])?;
            }
        }
        self.conn.execute(
            "UPDATE photos SET width = ?1, height = ?2, capture_time = ?3, camera_make = ?4,
                camera_model = ?5, lens = ?6, focal_length = ?7, aperture = ?8, shutter_speed = ?9,
                iso = ?10, gps_latitude = ?11, gps_longitude = ?12, updated_at = ?13,
                exif_orientation = ?15
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
                photo_id,
                promoted.exif_orientation.map(i64::from),
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

    /// Turn a photo's orientation override by `delta` degrees clockwise (±90, 180) and
    /// return the new absolute rotation (0/90/180/270). The inspector's Orientation buttons.
    pub fn rotate_photo(&self, photo_id: i64, delta: i64) -> Result<i64> {
        let current = self.photo_rotation(photo_id)?;
        self.set_photo_rotation(photo_id, current + delta)
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
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {cols} FROM photos {face} WHERE photos.stack_parent_id = ?1 ORDER BY photos.path COLLATE NOCASE",
            cols = query::photo_columns("photos"),
            face = query::face_join("photos")
        ))?;
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

    /// Write a photo's authored IPTC fields to the catalog, and owe the photo's sidecar the
    /// fields this changed (#148, `iptc_owed`), in one transaction (a savepoint, so it nests
    /// in a caller's). Returns the sidecar write that pays everything the photo owes — this
    /// change and any earlier write that never landed. The caller runs it off the lock
    /// ([`IptcSidecarWrite::run`]) and records the outcome ([`Catalog::settle_iptc_write`]);
    /// a caller that does neither leaves the fields owed for the repair pass.
    pub fn set_iptc(&self, photo_id: i64, f: &IptcFields) -> Result<IptcSidecarWrite> {
        self.set_iptc_carried(photo_id, f, IptcMask::NONE)
    }

    /// [`Catalog::set_iptc`] for values that arrive beside a sidecar of their own (a bundle
    /// import): the fields in `carried` — the ones that sidecar already holds a value for —
    /// are stored but not owed, so the import does not overwrite what another tool wrote
    /// there (#144's rule; review of #148, M1). Everything else it changed is owed as usual.
    pub fn set_iptc_carried(&self, photo_id: i64, f: &IptcFields, carried: IptcMask) -> Result<IptcSidecarWrite> {
        self.conn.execute_batch("SAVEPOINT set_iptc")?;
        let out = self.set_iptc_owing(photo_id, f, carried);
        let end = if out.is_ok() { "RELEASE set_iptc" } else { "ROLLBACK TO set_iptc; RELEASE set_iptc" };
        self.conn.execute_batch(end)?;
        out
    }

    fn set_iptc_owing(&self, photo_id: i64, f: &IptcFields, carried: IptcMask) -> Result<IptcSidecarWrite> {
        let before = self.get_iptc(photo_id)?;
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
        self.owe_iptc(photo_id, IptcMask::changed(&before, f).without(carried))
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

    /// Distinct non-empty values of a photo column, for the camera/lens filter dropdowns.
    /// `column` is a fixed allowlist (camera_model | lens) — never user input.
    pub fn distinct_photo_values(&self, column: &str) -> Result<Vec<String>> {
        let col = match column {
            "camera" => "camera_model",
            "lens" => "lens",
            other => return Err(CatalogError::Tag(format!("unknown column: {other}"))),
        };
        let sql = format!(
            "SELECT DISTINCT {col} FROM photos_visible \
             WHERE {col} IS NOT NULL AND {col} <> '' \
             ORDER BY {col} COLLATE NOCASE"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
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
            "SELECT id, path, capture_time FROM photos_visible",
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
                    // A tag born under a padlocked parent is padlocked from birth: the
                    // recursive padlock (`set_tag_private`) is a one-time sweep, so
                    // inheritance here is what keeps a person added later out of cloud
                    // prompts.
                    let private = inherited_private(&self.conn, parent_id)?;
                    self.conn.execute(
                        "INSERT INTO tags(uuid, name, name_norm, parent_id, full_path,
                            full_path_norm, private, created_at, updated_at)
                         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
                        params![
                            uuid::Uuid::new_v4().to_string(),
                            component,
                            normalize_lookup(component),
                            parent_id,
                            full_path,
                            full_path_norm,
                            private as i64,
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
    /// already on this photo, and auto-tags (they cannot be assigned by hand). Returns
    /// empty if the photo has no capture time. `photo_count` carries the neighbour frequency.
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
             FROM photos_visible p
             JOIN photo_tags pt ON pt.photo_id = p.id
             JOIN tags t ON t.id = pt.tag_id
             WHERE p.capture_time BETWEEN ?1 AND ?2 AND p.id != ?3
               AND t.auto_rule IS NULL
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

    /// Ids of every tag inside a private subtree: its own flag set, or any ancestor's.
    /// This is the set a cloud prompt must withhold — privacy is a property of the
    /// subtree, so a padlocked "People" covers a name under it even when the name's own
    /// flag was never set (rows created before creation-time inheritance existed).
    pub fn private_subtree_tag_ids(&self) -> Result<std::collections::HashSet<i64>> {
        let mut stmt = self.conn.prepare(
            "WITH RECURSIVE priv(id) AS (
                 SELECT id FROM tags WHERE private = 1
                 UNION
                 SELECT tags.id FROM tags JOIN priv ON tags.parent_id = priv.id
             )
             SELECT id FROM priv",
        )?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<std::collections::HashSet<i64>>>()?)
    }

    /// All tags with a recursive photo count (counts include descendant tags).
    /// Every tag, in path order, with the number of *distinct* visible photos in its
    /// subtree — a photo tagged both `Birds` and `Birds/Owls` counts once for `Birds`.
    ///
    /// Two cheap queries and a walk, instead of one recursive join: the tag closure joined
    /// against every assignment cost ~225 ms on a 144k-photo / 1.6k-tag catalog, and this is
    /// the query the Library asks for on every boot and every cold return from Develop. The
    /// per-subtree distinct count is computed here — each visible assignment is credited to
    /// the tag and every ancestor, deduplicated per (tag, photo) — which is the same set
    /// `COUNT(DISTINCT p.id)` over the closure produced; `tags_with_counts_match_the_sql_closure`
    /// keeps the two in step.
    pub fn list_tags_with_counts(&self) -> Result<Vec<TagWithCount>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, full_path, parent_id, description, auto_rule, uuid, private
             FROM tags ORDER BY full_path_norm",
        )?;
        let tags = stmt
            .query_map([], row_to_tag)?
            .collect::<rusqlite::Result<Vec<Tag>>>()?;
        let parent_of: std::collections::HashMap<i64, Option<i64>> =
            tags.iter().map(|t| (t.id, t.parent_id)).collect();

        // Visible assignments only: the grid hides missing/trashed photos, so a tag's count
        // must match what selecting it shows.
        let mut stmt = self.conn.prepare(
            "SELECT pt.tag_id, pt.photo_id
             FROM photo_tags pt
             JOIN photos_visible p ON p.id = pt.photo_id",
        )?;
        let assignments = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<(i64, i64)>>>()?;

        let mut counts: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
        let mut credited: std::collections::HashSet<(i64, i64)> = std::collections::HashSet::new();
        let depth_limit = tags.len() + 1; // a corrupt parent cycle terminates rather than spins
        for (tag_id, photo_id) in assignments {
            let mut cur = Some(tag_id);
            let mut hops = 0;
            while let Some(id) = cur {
                if hops > depth_limit {
                    break;
                }
                hops += 1;
                if credited.insert((id, photo_id)) {
                    *counts.entry(id).or_insert(0) += 1;
                }
                cur = parent_of.get(&id).copied().flatten();
            }
        }

        Ok(tags
            .into_iter()
            .map(|tag| {
                let photo_count = counts.get(&tag.id).copied().unwrap_or(0);
                TagWithCount { tag, photo_count }
            })
            .collect())
    }

    /// The previous, single-query form of [`Self::list_tags_with_counts`]: the tag closure
    /// joined against every visible assignment with `COUNT(DISTINCT p.id)`. Kept as the
    /// oracle the fast path is tested against.
    #[cfg(test)]
    pub(crate) fn list_tags_with_counts_via_closure(&self) -> Result<Vec<TagWithCount>> {
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
             LEFT JOIN photos_visible p ON p.id = pt.photo_id
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

    /// Assign a tag by hand. Refuses an auto-tag ([`CatalogError::AutoTag`]): the engine
    /// rebuilds its membership from its rule, so a hand assignment would be dropped silently on
    /// the next pass (#181). A caller writing many tags uses [`Self::assign_tags`], which skips them.
    pub fn assign_tag(&self, photo_id: i64, tag_id: i64) -> Result<()> {
        self.refuse_auto_tag(tag_id)?;
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
    /// An auto-tag's row is never pruned: its membership is the rule's alone, and the next
    /// pass would only put it back (#181).
    pub fn prune_redundant_tags(&self, photo_id: i64) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM photo_tags
             WHERE photo_id = ?1 AND tag_id IN (
                 SELECT a.id FROM tags a
                 JOIN photo_tags pa ON pa.tag_id = a.id AND pa.photo_id = ?1
                 WHERE a.auto_rule IS NULL
                   AND EXISTS (
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
             JOIN photos_visible p ON p.id = pt.photo_id
             GROUP BY t.id HAVING c > 0 ORDER BY c DESC",
        )?;
        let nodes = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut stmt = self.conn.prepare(
            "SELECT a.tag_id, b.tag_id, COUNT(*) AS w
             FROM photo_tags a
             JOIN photo_tags b ON a.photo_id = b.photo_id AND a.tag_id < b.tag_id
             JOIN photos_visible p ON p.id = a.photo_id
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
             JOIN photos_visible p ON p.id = pt.photo_id
             GROUP BY t.id HAVING c > 0 ORDER BY c DESC",
        )?;
        let tags = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        // Camera nodes
        let mut stmt = self.conn.prepare(
            "SELECT camera_model, COUNT(*) FROM photos_visible WHERE camera_model IS NOT NULL AND camera_model != '' GROUP BY camera_model ORDER BY COUNT(*) DESC",
        )?;
        let cameras = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        // Tag co-occurrence edges
        let mut stmt = self.conn.prepare(
            "SELECT a.tag_id, b.tag_id, COUNT(*) AS w
             FROM photo_tags a
             JOIN photo_tags b ON a.photo_id = b.photo_id AND a.tag_id < b.tag_id
             JOIN photos_visible p ON p.id = a.photo_id
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
            "SELECT p.camera_model, pt.tag_id, COUNT(*) FROM photos_visible p
             JOIN photo_tags pt ON pt.photo_id = p.id
             WHERE p.camera_model IS NOT NULL AND p.camera_model != ''
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
             FROM photos_visible p JOIN photo_tags pt ON pt.photo_id = p.id
             GROUP BY p.id",
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
             JOIN photos_visible p ON p.id = pt.photo_id
             GROUP BY t.id HAVING c > 0",
        )?;
        let tags = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut stmt = self.conn.prepare(
            "SELECT pt.photo_id, pt.tag_id FROM photo_tags pt
             JOIN photos_visible p ON p.id = pt.photo_id",
        )?;
        let edges = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((photos, tags, edges))
    }

    /// Library-wide tidy: prune redundant ancestor tags from every photo at once. Returns
    /// the total number of assignments removed. Like [`Self::prune_redundant_tags`], it
    /// never removes an auto-tag's row.
    pub fn tidy_redundant_tags(&self) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM photo_tags
             WHERE (photo_id, tag_id) IN (
                 SELECT pa.photo_id, a.id FROM tags a
                 JOIN photo_tags pa ON pa.tag_id = a.id
                 WHERE a.auto_rule IS NULL
                   AND EXISTS (
                     SELECT 1 FROM tags d
                     JOIN photo_tags pd ON pd.tag_id = d.id AND pd.photo_id = pa.photo_id
                     WHERE d.id != a.id
                       AND substr(d.full_path, 1, length(a.full_path) + 1) = a.full_path || '/'
                 )
             )",
            [],
        )?)
    }

    /// Remove a tag by hand. Refuses an auto-tag, as [`Self::assign_tag`] does: the engine's
    /// next pass would put it back.
    pub fn remove_tag(&self, photo_id: i64, tag_id: i64) -> Result<()> {
        self.refuse_auto_tag(tag_id)?;
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
        version_count: r.get(22)?,
        cover_token: r.get(23)?,
        // `photo_columns`: NULL = automatic, 0 = the original, else the pinned version.
        cover_pin: match r.get::<_, Option<i64>>(24)? {
            None => CoverPin::Auto,
            Some(0) => CoverPin::Original,
            Some(v) => CoverPin::Version(v),
        },
    })
}

/// The `private` flag a newly created tag starts with: its parent's. Every tag-creation
/// site (`create_tag`, merge, tag maintenance) goes through this so no path can mint a
/// cloud-visible tag inside a padlocked subtree.
pub(crate) fn inherited_private(
    conn: &rusqlite::Connection,
    parent_id: Option<i64>,
) -> rusqlite::Result<bool> {
    match parent_id {
        Some(pid) => Ok(conn
            .query_row("SELECT private FROM tags WHERE id = ?1", params![pid], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?
            .unwrap_or(0)
            != 0),
        None => Ok(false),
    }
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

/// Whether `table` currently has `column`. Used for the one shape difference a catalog can
/// have that migration does not settle immediately — see `Catalog::legacy_value_norm`.
fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(columns.iter().any(|c| c == column))
}

/// Where an upsert's identity comes from.
enum IdentitySource<'a> {
    /// A value the caller trusts as an identity (a manifest id, a test's UUID): mapped
    /// through [`photo_identity_for`], and recorded as a legacy identifier if it is not a UUID.
    Trusted(Option<&'a str>),
    /// What a scanned file's sidecar holds, read through [`Catalog::scan_identity`].
    Sidecar(Option<&'a str>),
}

impl<'a> IdentitySource<'a> {
    /// The value to record as the upserted row's legacy identifier, when it is one (see
    /// [`Catalog::record_legacy_identifier`]).
    ///
    /// A scanned sidecar's foreign value counts too (#150, review F4 of #146): a file
    /// catalogued after #141 whose sidecar holds a DAM id gets a minted UUID, and that DAM id
    /// is then the only thing linking the file to its row until a person resolves the
    /// conflict. Recorded, it lets a later move re-home the row under the same guards as a
    /// v23 row ([`Catalog::scan_identity`]) instead of cataloguing the file a second time.
    /// It is recorded whichever way the row was matched, so a row catalogued before this
    /// change gains the record at its next rescan; a value another row already holds is not
    /// recorded again.
    ///
    /// A trusted value is not recorded on a row that `matched_by_path` (#150, a nit of the
    /// #146 re-review): the value names the bundle's photo, and the row at that path is the
    /// file already there — a name collision the importer skipped onto may be the
    /// user's own, different photo. A sidecar's value describes the file at that path, so
    /// it is recorded either way.
    fn legacy_value(&self, matched_by_path: bool) -> Option<&'a str> {
        match *self {
            IdentitySource::Trusted(_) if matched_by_path => None,
            IdentitySource::Trusted(value) | IdentitySource::Sidecar(value) => value,
        }
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #135 M1: a catalog's identity is minted once — on its first open — and is the same on
    /// every reopen and on a second connection; another catalog has its own.
    #[test]
    fn the_catalog_identity_is_minted_once_and_survives_a_reopen() {
        let dir = crate::test_support::TestTmpDir::new("catalog-uuid");
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let db = dir.join("a.chairphoto");
        let first = Catalog::open(&db, &root).unwrap();
        let id = first.get_setting(CATALOG_UUID_KEY).unwrap().expect("minted on open");
        assert!(uuid::Uuid::parse_str(&id).is_ok(), "{id}");
        assert_eq!(first.catalog_uuid().unwrap(), id);
        let secondary = Catalog::open_secondary(&db, &root).unwrap();
        assert_eq!(secondary.catalog_uuid().unwrap(), id);
        drop((first, secondary));
        assert_eq!(Catalog::open(&db, &root).unwrap().catalog_uuid().unwrap(), id, "reopened");
        let other = Catalog::open(&dir.join("b.chairphoto"), &root).unwrap();
        assert_ne!(other.catalog_uuid().unwrap(), id);
    }

    /// `migrate_sidecar_identity_fields` rebuilds `pending_sidecar_identity` via
    /// `DROP TABLE` + rename whenever it finds a v20-shaped table (no `field` column) —
    /// necessary because SQLite can't alter a PRIMARY KEY in place. `DROP TABLE` drops the
    /// table's indexes along with it, including `idx_pending_sidecar_identity_copy`, which
    /// `schema::SCHEMA_SQL` (run moments earlier in the same `migrate_locked` pass, against
    /// the still-v20 table) had just created. Without recreating it inside the rebuild, a
    /// v20-to-v21 upgrade would lose that index for its entire first session — the session
    /// most likely to carry a large pending-identity queue, since it self-heals (via
    /// `CREATE INDEX IF NOT EXISTS` in `SCHEMA_SQL`) on the NEXT open. Assert both indexes
    /// exist after the very FIRST open of a v20 catalog, not a later one, so a self-healing
    /// regression can't hide behind this test.
    #[test]
    fn v20_to_v21_migration_keeps_both_pending_sidecar_identity_indexes_on_first_open() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "chairphoto-v20-migration-test-{}-{suffix}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let catalog_path = dir.join("v20.chairphoto");
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();

        // Simulate a v20 catalog: only `pending_sidecar_identity` is pre-created, in its
        // pre-field shape (one UUID debt per copy, no `field` column, and no `idx_*`
        // indexes of its own yet — those come from `SCHEMA_SQL`/the migration, same as a
        // real upgrade). Every other table is created fresh by `SCHEMA_SQL` on the open
        // below, exactly as it would be for any other upgrade.
        {
            let raw = Connection::open(&catalog_path).unwrap();
            raw.execute_batch(
                "CREATE TABLE pending_sidecar_identity (
                    photo_id        INTEGER NOT NULL,
                    volume_id       INTEGER NOT NULL,
                    relative_path   TEXT NOT NULL,
                    attempts        INTEGER NOT NULL DEFAULT 1,
                    error           TEXT NOT NULL DEFAULT '',
                    queued_at       INTEGER NOT NULL,
                    last_attempt_at INTEGER NOT NULL,
                    PRIMARY KEY(photo_id, volume_id, relative_path)
                );",
            )
            .unwrap();
        }

        let catalog = Catalog::open(&catalog_path, &root).unwrap();
        let indexes: Vec<String> = {
            let mut stmt = catalog
                .conn
                .prepare(
                    "SELECT name FROM sqlite_master
                     WHERE type = 'index' AND tbl_name = 'pending_sidecar_identity'",
                )
                .unwrap();
            stmt.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert!(
            indexes.iter().any(|n| n == "idx_pending_sidecar_identity_photo"),
            "expected idx_pending_sidecar_identity_photo on the FIRST open of a v20 \
             catalog, got {indexes:?}"
        );
        assert!(
            indexes.iter().any(|n| n == "idx_pending_sidecar_identity_copy"),
            "expected idx_pending_sidecar_identity_copy on the FIRST open of a v20 catalog \
             — migrate_sidecar_identity_fields's DROP TABLE rebuild must recreate it, not \
             just the neighbouring index (D1), got {indexes:?}"
        );

        drop(catalog);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Schema v22 (#33) adds `pending_sidecar_identity.dismissed_at`, and it must be there
    /// after the FIRST open of an older catalog — every query in `catalog/identity.rs` now
    /// names that column, so a catalog missing it does not degrade, it fails outright.
    ///
    /// Both older shapes are exercised, because they take different routes to the column: a
    /// **v21** catalog already has the table (`SCHEMA_SQL`'s `CREATE TABLE IF NOT EXISTS` is
    /// a no-op) and gets the column from `ensure_column`; a **v20** catalog is additionally
    /// rebuilt by `migrate_sidecar_identity_fields`, whose `CREATE TABLE`/`DROP TABLE` does
    /// NOT include `dismissed_at` — so ordering `ensure_column` before that rebuild, rather
    /// than after it, would leave a v20 upgrade without the column while a v21 upgrade
    /// passed. Existing rows must default to 0 (not dismissed): the column records a human
    /// decision, and nobody has made one for a row that predates the feature.
    ///
    /// The v21 fixture carries a pre-existing queue row and asserts it comes back not
    /// dismissed. The v20 one deliberately does not: `migrate_sidecar_identity_fields`
    /// copies rows into a table that declares `REFERENCES photos(id)`, so a synthetic row
    /// pointing at no photo would abort the whole open on a foreign-key violation — which
    /// says nothing about this column.
    #[test]
    fn older_catalogs_gain_the_dismissed_at_column_on_first_open() {
        for (label, create_sql, seeded_row) in [
            (
                "v21",
                "CREATE TABLE pending_sidecar_identity (
                    photo_id        INTEGER NOT NULL,
                    field           TEXT NOT NULL DEFAULT 'identifier',
                    volume_id       INTEGER NOT NULL,
                    relative_path   TEXT NOT NULL,
                    attempts        INTEGER NOT NULL DEFAULT 1,
                    error           TEXT NOT NULL DEFAULT '',
                    queued_at       INTEGER NOT NULL,
                    last_attempt_at INTEGER NOT NULL,
                    PRIMARY KEY(photo_id, field, volume_id, relative_path)
                 );
                 INSERT INTO pending_sidecar_identity
                    (photo_id, field, volume_id, relative_path, queued_at, last_attempt_at)
                 VALUES(1, 'identifier', 1, 'old.arw', 10, 10);",
                true,
            ),
            (
                "v20",
                "CREATE TABLE pending_sidecar_identity (
                    photo_id        INTEGER NOT NULL,
                    volume_id       INTEGER NOT NULL,
                    relative_path   TEXT NOT NULL,
                    attempts        INTEGER NOT NULL DEFAULT 1,
                    error           TEXT NOT NULL DEFAULT '',
                    queued_at       INTEGER NOT NULL,
                    last_attempt_at INTEGER NOT NULL,
                    PRIMARY KEY(photo_id, volume_id, relative_path)
                 );",
                false,
            ),
        ] {
            let suffix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "chairphoto-v22-migration-{label}-{}-{suffix}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            let root = dir.join("photos");
            std::fs::create_dir_all(&root).unwrap();
            let catalog_path = dir.join("old.chairphoto");
            {
                let raw = Connection::open(&catalog_path).unwrap();
                raw.execute_batch(create_sql).unwrap();
            }

            let catalog = Catalog::open(&catalog_path, &root).unwrap();
            let columns: Vec<String> = {
                let mut stmt = catalog
                    .conn
                    .prepare("PRAGMA table_info(pending_sidecar_identity)")
                    .unwrap();
                stmt.query_map([], |r| r.get::<_, String>(1))
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap()
            };
            assert!(
                columns.iter().any(|c| c == "dismissed_at"),
                "{label}: expected dismissed_at on the FIRST open, got {columns:?}"
            );
            if seeded_row {
                let dismissed: i64 = catalog
                    .conn
                    .query_row(
                        "SELECT dismissed_at FROM pending_sidecar_identity
                         WHERE relative_path = 'old.arw'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(dismissed, 0, "{label}: a pre-existing row is not dismissed");
            }
            // The queue is usable end to end, not merely column-complete.
            catalog.summarize_pending_identity().unwrap();

            drop(catalog);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// Timing on a real catalog: `CHAIRPHOTO_TAG_BENCH_DB=/path/to/x.chairphoto cargo test
    /// --release tag_count_bench -- --ignored --nocapture`. Opens a *copy*, never the live file.
    #[test]
    #[ignore = "tag-count bench on a real catalog; needs CHAIRPHOTO_TAG_BENCH_DB"]
    fn tag_count_bench() {
        let Ok(src) = std::env::var("CHAIRPHOTO_TAG_BENCH_DB") else {
            println!("SKIPPED: tag_count_bench — set CHAIRPHOTO_TAG_BENCH_DB");
            return;
        };
        let dir = crate::test_support::TestTmpDir::new("tag-bench");
        let db = dir.join("copy.chairphoto");
        std::fs::copy(&src, &db).unwrap();
        if let Ok(wal) = std::fs::read(format!("{src}-wal")) {
            std::fs::write(dir.join("copy.chairphoto-wal"), wal).unwrap();
        }
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let cat = Catalog::open(&db, &root).unwrap();
        for (label, f) in [
            ("closure (old)", Box::new(|| cat.list_tags_with_counts_via_closure().unwrap()) as Box<dyn Fn() -> Vec<TagWithCount>>),
            ("fast (new)", Box::new(|| cat.list_tags_with_counts().unwrap())),
        ] {
            let mut ms = Vec::new();
            let mut n = 0;
            for _ in 0..5 {
                let t = std::time::Instant::now();
                n = f().len();
                ms.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
            println!("{label}: {n} tags, median {:.1} ms, min {:.1} ms", ms[2], ms[0]);
        }
        let a = cat.list_tags_with_counts_via_closure().unwrap();
        let b = cat.list_tags_with_counts().unwrap();
        let pairs = |v: &[TagWithCount]| v.iter().map(|t| (t.tag.id, t.photo_count)).collect::<Vec<_>>();
        assert_eq!(pairs(&a), pairs(&b), "fast and closure disagree on the real catalog");
        println!("counts identical on the real catalog");
    }

    /// The fast tag-count path (two queries + a walk) must agree with the recursive
    /// closure query it replaced, on the shapes that make them differ if either is wrong:
    /// a photo tagged with both a tag and its ancestor (counted once), a three-level chain,
    /// a photo under two sibling subtrees, and missing / trashed photos (excluded).
    #[test]
    fn tags_with_counts_match_the_sql_closure() {
        let dir = crate::test_support::TestTmpDir::new("tag-counts");
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let cat = Catalog::open(&dir.join("test.chairphoto"), &root).unwrap();
        let c = cat.conn();
        for id in 1..=6 {
            c.execute(
                "INSERT INTO photos(id, uuid, path, mtime_ns, size, extension, created_at, updated_at)
                 VALUES(?1, ?2, ?3, 0, 0, 'jpg', 0, 0)",
                params![id, format!("u{id}"), format!("p{id}.jpg")],
            )
            .unwrap();
        }
        let animals = cat.create_tag("Animals").unwrap();
        let birds = cat.create_tag("Animals/Birds").unwrap();
        let owls = cat.create_tag("Animals/Birds/Owls").unwrap();
        let dogs = cat.create_tag("Animals/Dogs").unwrap();
        let places = cat.create_tag("Places").unwrap();
        // 1: owl only → Owls, Birds, Animals each get it once.
        cat.assign_tag(1, owls).unwrap();
        // 2: owl AND its ancestor Birds → Birds must not count it twice.
        cat.assign_tag(2, owls).unwrap();
        cat.assign_tag(2, birds).unwrap();
        // 3: under two sibling subtrees → Animals counts it once, Birds and Dogs once each.
        cat.assign_tag(3, birds).unwrap();
        cat.assign_tag(3, dogs).unwrap();
        // 4: missing, 5: trashed → excluded everywhere.
        cat.assign_tag(4, owls).unwrap();
        c.execute("UPDATE photos SET missing = 1 WHERE id = 4", []).unwrap();
        cat.assign_tag(5, dogs).unwrap();
        cat.trash_photos(&[5]).unwrap();
        // 6: untagged; Places has no photos at all.
        let _ = places;

        let fast = cat.list_tags_with_counts().unwrap();
        let oracle = cat.list_tags_with_counts_via_closure().unwrap();
        let pairs = |v: &[TagWithCount]| v.iter().map(|t| (t.tag.full_path.clone(), t.photo_count)).collect::<Vec<_>>();
        assert_eq!(pairs(&fast), pairs(&oracle));
        let by_path: std::collections::HashMap<_, _> = pairs(&fast).into_iter().collect();
        assert_eq!(by_path["Animals"], 3, "photos 1, 2, 3 — each once");
        assert_eq!(by_path["Animals/Birds"], 3, "1 via Owls, 2 once despite both tags, 3 direct");
        assert_eq!(by_path["Animals/Birds/Owls"], 2, "1 and 2; 4 is missing");
        assert_eq!(by_path["Animals/Dogs"], 1, "3; 5 is trashed");
        assert_eq!(by_path["Places"], 0);
        assert_eq!(animals, fast[0].tag.id, "path order: Animals first");
    }

    /// The Orientation buttons turn relative to the stored override and wrap at 360°.
    #[test]
    fn rotate_photo_turns_relative_to_the_override_and_wraps() {
        let dir = crate::test_support::TestTmpDir::new("rotate-photo");
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let c = Catalog::open(&dir.join("test.chairphoto"), &root).unwrap();
        let id = c.upsert_photo(&root.join("a.ARW"), None, 0, 1).unwrap().id;
        assert_eq!(c.rotate_photo(id, -90).unwrap(), 270);
        assert_eq!(c.rotate_photo(id, 180).unwrap(), 90);
        assert_eq!(c.rotate_photo(id, 90).unwrap(), 180);
        assert_eq!(c.photo_rotation(id).unwrap(), 180);
    }

    fn exif_orientation_of(c: &Catalog, id: i64) -> Option<i64> {
        c.conn
            .query_row("SELECT exif_orientation FROM photos WHERE id = ?1", [id], |r| r.get(0))
            .unwrap()
    }

    /// A scan records the EXIF Orientation it extracted; a later extraction without one
    /// clears it rather than keeping a stale code.
    #[test]
    fn set_photo_metadata_records_the_exif_orientation() {
        let dir = crate::test_support::TestTmpDir::new("exif-orientation-set");
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let c = Catalog::open(&dir.join("test.chairphoto"), &root).unwrap();
        let id = c.upsert_photo(&root.join("a.ARW"), None, 0, 1).unwrap().id;
        assert_eq!(exif_orientation_of(&c, id), None, "unknown until extracted");
        let rotated = PromotedMetadata { exif_orientation: Some(6), ..Default::default() };
        c.set_photo_metadata(id, &rotated, &[]).unwrap();
        assert_eq!(exif_orientation_of(&c, id), Some(6));
        c.set_photo_metadata(id, &PromotedMetadata::default(), &[]).unwrap();
        assert_eq!(exif_orientation_of(&c, id), None);
    }

    /// #136: opening a catalog from before `exif_orientation` fills the column from what its
    /// scans stored in `photo_metadata` — exiftool's `EXIF:Orientation` phrase, else the AF
    /// pass's numeric code — and leaves a photo with neither unknown. The migration is
    /// gated on [`EXIF_ORIENTATION_SINCE`], so the test follows a renumbering.
    #[test]
    fn opening_an_older_catalog_backfills_the_exif_orientation() {
        let dir = crate::test_support::TestTmpDir::new("exif-orientation-backfill");
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let db = dir.join("test.chairphoto");
        let c = Catalog::open(&db, &root).unwrap();
        let photo = |name: &str| c.upsert_photo(&root.join(name), None, 0, 1).unwrap().id;
        let (phrase, af_only, both, none, kept) =
            (photo("a.ARW"), photo("b.ARW"), photo("c.ARW"), photo("d.ARW"), photo("e.ARW"));
        let entry = |key: &str, group: &str, value: &str| MetadataEntry {
            key: key.into(),
            group_name: group.into(),
            value: value.into(),
        };
        let af = |code: &str| {
            entry(crate::metadata::AF_ORIENTATION_KEY, crate::metadata::AF_GROUP, code)
        };
        let exif = |phrase: &str| entry("Orientation", "EXIF", phrase);
        let unknown = PromotedMetadata::default();
        c.set_photo_metadata(phrase, &unknown, &[exif("Rotate 270 CW")]).unwrap();
        c.set_photo_metadata(af_only, &unknown, &[af("6")]).unwrap();
        c.set_photo_metadata(both, &unknown, &[af("1"), exif("Mirror horizontal")]).unwrap();
        c.set_photo_metadata(none, &unknown, &[entry("Orientation", "XMP", "Rotate 90 CW")])
            .unwrap();
        c.set_photo_metadata(kept, &unknown, &[exif("Rotate 180")]).unwrap();
        c.conn.execute("UPDATE photos SET exif_orientation = 5 WHERE id = ?1", [kept]).unwrap();
        c.set_setting("schema_version", &(EXIF_ORIENTATION_SINCE - 1).to_string()).unwrap();
        // The column as an older catalog has it for every row: never written.
        c.conn
            .execute("UPDATE photos SET exif_orientation = NULL WHERE id <> ?1", [kept])
            .unwrap();
        drop(c);

        let c = Catalog::open(&db, &root).unwrap();
        assert_eq!(exif_orientation_of(&c, phrase), Some(8));
        assert_eq!(exif_orientation_of(&c, af_only), Some(6));
        assert_eq!(exif_orientation_of(&c, both), Some(2), "EXIF:Orientation outranks the AF code");
        assert_eq!(exif_orientation_of(&c, none), None, "no EXIF orientation: unknown");
        assert_eq!(exif_orientation_of(&c, kept), Some(5), "a value already recorded is not replaced");
        assert_eq!(c.get_setting("schema_version").unwrap().as_deref(),
            Some(schema::SCHEMA_VERSION.to_string().as_str()));
    }
}
