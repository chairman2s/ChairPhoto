//! Volumes and physical photo locations — the storage layer described in
//! `docs/storage-and-import.md`.
//!
//! A photo's logical identity is its UUID; its *locations* are physical copies on
//! named *volumes*. The [`Catalog::resolve_photo_path`] resolver returns the best
//! currently-available copy, preferring a fast local cache over the primary over a
//! backup. All physical file access (thumbnails, previews, editor launch, export)
//! should go through the resolver rather than assuming a single path.
//!
//! For now everything lives under one default volume (the catalog root), so this
//! mirrors the previous single-path behaviour; multi-volume (NAS) support builds on
//! top of it without changing the read path.

use super::{
    sqlite_param_placeholders, Catalog, CatalogError, LocationRole, PhotoLocation, Result,
    StorageStatus, Volume, VolumeKind, SQLITE_PARAM_CHUNK,
};
use rusqlite::{params, OptionalExtension};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The volume that represents the catalog root. Created automatically.
const DEFAULT_VOLUME_NAME: &str = "catalog-root";

/// One place a photo's bytes might physically be, produced by
/// [`Catalog::photo_path_candidates`] in read-preference order. Pairing the absolute
/// `path` with its originating `volume_id` lets the off-lock stat pass consult the
/// volume-reachability cache to REORDER (never change) which copies it stats first.
#[derive(Debug, Clone)]
pub struct PathCandidate {
    /// The absolute path this copy would live at.
    pub path: PathBuf,
    /// The role this copy plays (local cache / primary / backup / export).
    pub role: LocationRole,
    /// The volume this copy is on, or `None` for the legacy catalog-root fallback
    /// (which is always tried, regardless of any volume's cached reachability).
    pub volume_id: Option<i64>,
}

impl Catalog {
    // --- volumes ------------------------------------------------------------

    /// Register a named volume of a given kind with a per-machine base path.
    pub fn add_volume(&self, name: &str, base_path: &Path, kind: VolumeKind) -> Result<i64> {
        let uuid = uuid::Uuid::new_v4().to_string();
        self.conn.execute(
            "INSERT INTO volumes(uuid, name, base_path, kind) VALUES(?1, ?2, ?3, ?4)",
            params![uuid, name, base_path.to_string_lossy(), kind.as_db_str()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// All volume rows, PURE SQL — never stats. `reachable` is a `false` placeholder;
    /// callers that need real reachability either stat themselves ([`list_volumes`]) or
    /// consult the off-lock reachability cache. Use this from under the catalog lock to
    /// avoid a slow NAS stat serializing the whole app.
    pub fn volume_rows(&self) -> Result<Vec<Volume>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, uuid, name, base_path, kind FROM volumes ORDER BY name")?;
        let rows = stmt.query_map([], |r| {
            Ok(Volume {
                id: r.get(0)?,
                uuid: r.get(1)?,
                name: r.get(2)?,
                base_path: r.get(3)?,
                kind: VolumeKind::from_db_str(&r.get::<_, String>(4)?),
                reachable: false,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// All volumes, each flagged with whether its base path currently exists. This
    /// STATS each volume, so it must not be called while holding a lock that a hung NAS
    /// stat could serialize; it's kept for the Volumes preferences UI and cold callers.
    pub fn list_volumes(&self) -> Result<Vec<Volume>> {
        Ok(self
            .volume_rows()?
            .into_iter()
            .map(|mut v| {
                v.reachable = Path::new(&v.base_path).is_dir();
                v
            })
            .collect())
    }

    /// Remove a volume registration. Refuses to remove the default (catalog-root)
    /// volume, which the resolver relies on. This forgets the catalog's *pointers* to
    /// copies on that volume (photo_locations cascade); it never deletes files on disk.
    pub fn remove_volume(&self, volume_id: i64) -> Result<()> {
        let name: Option<String> = self
            .conn
            .query_row("SELECT name FROM volumes WHERE id = ?1", params![volume_id], |r| {
                r.get(0)
            })
            .optional()?;
        match name {
            None => Err(CatalogError::NotFound(format!("volume {volume_id}"))),
            Some(n) if n == DEFAULT_VOLUME_NAME => Err(CatalogError::Validation(
                "the default catalog-root volume cannot be removed".into(),
            )),
            Some(_) => {
                self.conn
                    .execute("DELETE FROM volumes WHERE id = ?1", params![volume_id])?;
                Ok(())
            }
        }
    }

    fn volume_id_by_name(&self, name: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT id FROM volumes WHERE name = ?1", params![name], |r| {
                r.get(0)
            })
            .optional()?)
    }

    /// The default volume (catalog root), creating it if missing.
    pub fn ensure_default_volume(&self) -> Result<i64> {
        if let Some(id) = self.volume_id_by_name(DEFAULT_VOLUME_NAME)? {
            return Ok(id);
        }
        let root = self.root().to_path_buf();
        self.add_volume(DEFAULT_VOLUME_NAME, &root, VolumeKind::Local)
    }

    /// Keep the default (catalog-root) volume's base path in sync with the current
    /// catalog root — so re-rooting the catalog (set_library_root) moves the local
    /// volume too. Called on every open.
    pub(crate) fn sync_default_volume_root(&self) -> Result<()> {
        let id = self.ensure_default_volume()?;
        self.conn.execute(
            "UPDATE volumes SET base_path = ?1 WHERE id = ?2",
            params![self.root().to_string_lossy(), id],
        )?;
        Ok(())
    }

    /// Find the volume that contains `absolute` (longest matching base path) and the
    /// path relative to it. Falls back to the default volume (catalog root). PURE SQL:
    /// reachability checks are deliberately left to resolver callers that run off-lock.
    pub fn volume_for_path(&self, absolute: &Path) -> Result<(i64, String)> {
        let mut best: Option<(i64, usize, String)> = None;
        for volume in self.volume_rows()? {
            let base = PathBuf::from(&volume.base_path);
            if let Ok(rel) = absolute.strip_prefix(&base) {
                let len = volume.base_path.len();
                let rel = rel.to_string_lossy().replace('\\', "/");
                if best.as_ref().map_or(true, |(_, best_len, _)| len > *best_len) {
                    best = Some((volume.id, len, rel));
                }
            }
        }
        if let Some((id, _, rel)) = best {
            return Ok((id, rel));
        }
        // Nothing matched — fall back to the default volume.
        let id = self.ensure_default_volume()?;
        let rel = self.to_relative(absolute)?;
        Ok((id, rel))
    }

    // --- locations ----------------------------------------------------------

    /// Record (or update) a photo's primary location, derived from an absolute path.
    pub fn set_primary_location(&self, photo_id: i64, absolute: &Path) -> Result<()> {
        let (volume_id, relative) = self.volume_for_path(absolute)?;
        self.add_location(photo_id, volume_id, &relative, LocationRole::Primary)
    }

    /// Insert or update one location for a photo. Unique per (photo, volume, role).
    pub fn add_location(
        &self,
        photo_id: i64,
        volume_id: i64,
        relative_path: &str,
        role: LocationRole,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO photo_locations(photo_id, volume_id, relative_path, role, created_at)
             VALUES(?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(photo_id, volume_id, role)
             DO UPDATE SET relative_path = excluded.relative_path",
            params![photo_id, volume_id, relative_path, role.as_db_str(), now()],
        )?;
        Ok(())
    }

    /// Remove all of a photo's location records on a given volume (e.g. after an
    /// offload frees the local copy, or when a volume is no longer used). This forgets
    /// the catalog's pointers; it never deletes files on disk.
    pub fn remove_locations_on_volume(&self, photo_id: i64, volume_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM photo_locations WHERE photo_id = ?1 AND volume_id = ?2",
            params![photo_id, volume_id],
        )?;
        Ok(())
    }

    /// All physical locations recorded for a photo.
    pub fn photo_locations(&self, photo_id: i64) -> Result<Vec<PhotoLocation>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, photo_id, volume_id, relative_path, role
             FROM photo_locations WHERE photo_id = ?1",
        )?;
        let rows = stmt.query_map(params![photo_id], |r| {
            Ok(PhotoLocation {
                id: r.get(0)?,
                photo_id: r.get(1)?,
                volume_id: r.get(2)?,
                relative_path: r.get(3)?,
                role: LocationRole::from_db_str(&r.get::<_, String>(4)?),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Reconcile the `missing` flag across all photos after a scan: a photo is hidden
    /// (missing = 1) when its original can't be resolved AND it has no backup copy —
    /// i.e. genuinely gone (e.g. orphan rows left by a re-root). Photos that are merely
    /// offline (on an unmounted NAS) keep a backup location, so they're NOT hidden.
    /// Resolvable photos are un-hidden. Called at the end of a scan.
    pub fn reconcile_missing(&self) -> Result<()> {
        let ids: Vec<i64> = {
            let mut stmt = self.conn.prepare("SELECT id FROM photos")?;
            let ids = stmt
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            ids
        };
        for id in ids {
            let resolvable = self.resolve_photo_path(id)?.is_some();
            let has_backup: bool = self.conn.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM photo_locations l JOIN volumes v ON v.id = l.volume_id
                    WHERE l.photo_id = ?1 AND v.kind = 'backup')",
                params![id],
                |r| r.get(0),
            )?;
            let missing = !resolvable && !has_backup;
            self.conn.execute(
                "UPDATE photos SET missing = ?1 WHERE id = ?2",
                params![missing as i64, id],
            )?;
        }
        Ok(())
    }

    /// Photos that are provably gone: no copy exists on any *reachable* volume, and no
    /// copy sits on an *unreachable* volume that might still hold it. Returns `(id, path)`
    /// for each. Conservative by design — a photo with any location on an offline volume
    /// (e.g. an unmounted NAS) is NEVER returned, since the file may be there once mounted.
    /// This is the data behind "remove unavailable photos from the catalog". It stats
    /// files (via the resolver, which checks local copies before backups, so a present
    /// original short-circuits without touching the NAS).
    pub fn find_unavailable_photos(&self) -> Result<Vec<(i64, String)>> {
        let reachable = self.volume_reachability()?;
        // Photos that have at least one location on an unreachable volume — we can't prove
        // those are gone, so they're off-limits for removal.
        let mut uncertain: std::collections::HashSet<i64> = std::collections::HashSet::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT photo_id, volume_id FROM photo_locations")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
            for row in rows {
                let (pid, vid) = row?;
                if !reachable.get(&vid).copied().unwrap_or(false) {
                    uncertain.insert(pid);
                }
            }
        }
        let photos: Vec<(i64, String)> = {
            let mut stmt = self.conn.prepare("SELECT id, path FROM photos")?;
            let v = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            v
        };
        let mut out = Vec::new();
        for (id, path) in photos {
            if uncertain.contains(&id) {
                continue; // might still live on an offline volume — never remove
            }
            if self.resolve_photo_path(id)?.is_some() {
                continue; // a reachable copy actually exists
            }
            out.push((id, path));
        }
        Ok(out)
    }

    /// Remove every photo [`find_unavailable_photos`] reports — deletes only the catalog
    /// rows (cascading to tags, locations, etc.); never touches any file on disk or NAS.
    /// Returns the `(id, path)` list that was removed, for reporting. Wrapped in one
    /// transaction.
    pub fn purge_unavailable_photos(&self) -> Result<Vec<(i64, String)>> {
        let gone = self.find_unavailable_photos()?;
        let tx = self.begin()?;
        for (id, _) in &gone {
            self.conn
                .execute("DELETE FROM photos WHERE id = ?1", params![id])?;
        }
        tx.commit()?;
        Ok(gone)
    }

    /// Photos whose image data is gone: at least one of their copies exists on disk but
    /// every existing copy is 0 bytes (an empty/corrupt file, e.g. a long-ago bad sync).
    /// Such a photo can never be displayed. Returns `(id, path)`. A photo with any
    /// non-empty copy is excluded; one with no existing copy at all is "unavailable", not
    /// "empty" (see [`find_unavailable_photos`]).
    pub fn find_empty_photos(&self) -> Result<Vec<(i64, String)>> {
        // Gather every located absolute path per photo.
        let mut by_photo: std::collections::HashMap<i64, Vec<PathBuf>> =
            std::collections::HashMap::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT l.photo_id, v.base_path, l.relative_path
                 FROM photo_locations l JOIN volumes v ON v.id = l.volume_id",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
            })?;
            for row in rows {
                let (pid, base, rel) = row?;
                by_photo.entry(pid).or_default().push(Path::new(&base).join(&rel));
            }
        }
        let photos: Vec<(i64, String)> = {
            let mut stmt = self.conn.prepare("SELECT id, path FROM photos")?;
            let v = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            v
        };
        let mut out = Vec::new();
        for (id, path) in photos {
            let mut paths = by_photo.remove(&id).unwrap_or_default();
            paths.push(self.to_absolute(&path)); // also consider the catalog-root path
            let mut any_existing = false;
            let mut all_empty = true;
            for p in &paths {
                if let Ok(md) = std::fs::metadata(p) {
                    if md.is_file() {
                        any_existing = true;
                        if md.len() > 0 {
                            all_empty = false;
                            break; // a good copy exists → not empty
                        }
                    }
                }
            }
            if any_existing && all_empty {
                out.push((id, path));
            }
        }
        Ok(out)
    }

    /// Remove every photo [`find_empty_photos`] reports — deletes only the catalog rows
    /// (cascading); never deletes the (empty) file on disk or NAS. Returns the removed list.
    pub fn purge_empty_photos(&self) -> Result<Vec<(i64, String)>> {
        let gone = self.find_empty_photos()?;
        let tx = self.begin()?;
        for (id, _) in &gone {
            self.conn
                .execute("DELETE FROM photos WHERE id = ?1", params![id])?;
        }
        tx.commit()?;
        Ok(gone)
    }

    // --- per-photo storage status -------------------------------------------

    /// Storage status of one photo, derived from its locations' volume kinds and
    /// reachability (see `docs/storage-and-import.md`). A backup *record* counts as
    /// backed-up even if the NAS is currently unmounted (reachability only decides
    /// Archived vs Offline when there's no local copy).
    pub fn photo_storage_status(&self, photo_id: i64) -> Result<StorageStatus> {
        let reachable = self.volume_reachability()?;
        let mut stmt = self.conn.prepare(
            "SELECT v.kind, v.id FROM photo_locations l JOIN volumes v ON v.id = l.volume_id
             WHERE l.photo_id = ?1",
        )?;
        let rows: Vec<(String, i64)> = stmt
            .query_map(params![photo_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(status_from_locations(&rows, &reachable))
    }

    /// Batch storage status for many photos (one query) — for the grid. Returns
    /// `(photo_id, status)` only for the requested ids that have rows; callers treat
    /// a missing id as no-locations.
    ///
    /// `reachable` (`volume_id → reachable`) is supplied by the caller so the NAS stats
    /// can happen OFF the catalog lock (via the [`crate::volume_health`] cache); this
    /// method never stats. A volume absent from the map is treated as unreachable.
    pub fn photo_storage_statuses(
        &self,
        photo_ids: &[i64],
        reachable: &std::collections::HashMap<i64, bool>,
    ) -> Result<Vec<(i64, StorageStatus)>> {
        if photo_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut per_photo: std::collections::HashMap<i64, Vec<(String, i64)>> =
            std::collections::HashMap::new();
        for chunk in photo_ids.chunks(SQLITE_PARAM_CHUNK) {
            let placeholders = sqlite_param_placeholders(chunk.len());
            let mut stmt = self.conn.prepare(&format!(
                "SELECT l.photo_id, v.kind, v.id FROM photo_locations l
                 JOIN volumes v ON v.id = l.volume_id
                 WHERE l.photo_id IN ({placeholders})"
            ))?;
            let binds: Vec<&dyn rusqlite::ToSql> =
                chunk.iter().map(|i| i as &dyn rusqlite::ToSql).collect();
            let rows = stmt.query_map(binds.as_slice(), |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
            })?;
            for row in rows {
                let (pid, kind, vid) = row?;
                per_photo.entry(pid).or_default().push((kind, vid));
            }
        }
        Ok(photo_ids
            .iter()
            .map(|&pid| {
                let locs = per_photo.get(&pid).map(Vec::as_slice).unwrap_or(&[]);
                (pid, status_from_locations(locs, reachable))
            })
            .collect())
    }

    /// Map of volume id → reachable (computed once per status batch).
    fn volume_reachability(&self) -> Result<std::collections::HashMap<i64, bool>> {
        Ok(self
            .list_volumes()?
            .into_iter()
            .map(|v| (v.id, v.reachable))
            .collect())
    }

    // --- the resolver -------------------------------------------------------

    /// The ordered list of physical paths a photo *might* live at, best (most-preferred
    /// role) first, with the legacy catalog-root fallback appended LAST. PURE SQL — it
    /// never stats the filesystem, so it's safe to call while holding the catalog lock.
    ///
    /// This is the "which paths?" half of the resolver, split out so the "which of these
    /// exists?" half ([`crate::volume_health::pick_existing`]) can stat OFF the lock. A
    /// slow/offline NAS then can't serialize the whole app.
    pub fn photo_path_candidates(&self, photo_id: i64) -> Result<Vec<PathCandidate>> {
        let mut stmt = self.conn.prepare(
            "SELECT v.base_path, l.relative_path, l.role, v.id
             FROM photo_locations l JOIN volumes v ON v.id = l.volume_id
             WHERE l.photo_id = ?1",
        )?;
        let mut candidates: Vec<PathCandidate> = stmt
            .query_map(params![photo_id], |r| {
                let base: String = r.get(0)?;
                let relative: String = r.get(1)?;
                Ok(PathCandidate {
                    path: Path::new(&base).join(&relative),
                    role: LocationRole::from_db_str(&r.get::<_, String>(2)?),
                    volume_id: Some(r.get(3)?),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        candidates.sort_by_key(|c| c.role.priority());

        // Append the legacy catalog-root path LAST, for photos without locations (or as a
        // final fallback). volume_id = None marks it as the always-try root fallback.
        let photo = self.get_photo(photo_id)?;
        candidates.push(PathCandidate {
            path: self.to_absolute(&photo.path),
            role: LocationRole::Primary,
            volume_id: None,
        });

        Ok(candidates)
    }

    /// Return the best currently-available absolute path for a photo: among its
    /// locations on reachable volumes whose file actually exists, the one with the
    /// most-preferred role. Falls back to the catalog-root path. `None` means no
    /// copy is reachable right now (e.g. NAS unmounted).
    ///
    /// Single source of truth: builds the candidate list ([`photo_path_candidates`]) and
    /// returns the first path that exists. Statting callers on the hot path should split
    /// this themselves (candidates under the lock, `pick_existing` off it).
    pub fn resolve_photo_path(&self, photo_id: i64) -> Result<Option<PathBuf>> {
        Ok(self
            .photo_path_candidates(photo_id)?
            .into_iter()
            .find(|c| c.path.exists())
            .map(|c| c.path))
    }

    /// Resolve a photo's path or fail — convenience for callers that need a path.
    pub fn require_photo_path(&self, photo_id: i64) -> Result<PathBuf> {
        self.resolve_photo_path(photo_id)?
            .ok_or_else(|| CatalogError::NotFound(format!("no reachable copy of photo {photo_id}")))
    }

    // --- migration backfill -------------------------------------------------

    /// One-time backfill (schema < 2): create the default volume and give every
    /// photo that lacks a location a primary one derived from its catalog-root path.
    pub(crate) fn backfill_default_volume(&self) -> Result<()> {
        let volume_id = self.ensure_default_volume()?;
        let mut stmt = self.conn.prepare(
            "SELECT id, path FROM photos
             WHERE id NOT IN (SELECT photo_id FROM photo_locations)",
        )?;
        let rows: Vec<(i64, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (photo_id, path) in rows {
            self.add_location(photo_id, volume_id, &path, LocationRole::Primary)?;
        }
        Ok(())
    }
}

/// Pure status derivation from a photo's `(volume_kind, volume_id)` locations and a
/// map of volume reachability. Kept separate so it's trivially unit-testable.
fn status_from_locations(
    locations: &[(String, i64)],
    reachable: &std::collections::HashMap<i64, bool>,
) -> StorageStatus {
    if locations.is_empty() {
        return StorageStatus::Missing;
    }
    let has_local = locations.iter().any(|(kind, _)| kind == "local");
    let backups: Vec<i64> = locations
        .iter()
        .filter(|(kind, _)| kind == "backup")
        .map(|(_, vid)| *vid)
        .collect();
    let has_backup = !backups.is_empty();
    let backup_reachable = backups
        .iter()
        .any(|vid| reachable.get(vid).copied().unwrap_or(false));

    match (has_local, has_backup) {
        (true, true) => StorageStatus::BackedUp,
        (true, false) => StorageStatus::LocalOnly,
        (false, true) if backup_reachable => StorageStatus::Archived,
        (false, true) => StorageStatus::Offline,
        (false, false) => StorageStatus::Missing,
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
