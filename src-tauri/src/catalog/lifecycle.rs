//! Storage lifecycle (E3): backup local→backup volume, offload (free local space),
//! and restore (pull a backup back to local) — all SHA-256 hash-verified, enforcing
//! the non-negotiable safety invariants in docs/storage-and-import.md:
//!
//!   1. Never delete the last copy. 2. Never offload without a verified backup.
//!   3. Hash-verify the backup before deleting anything local.
//!
//! Each op is split so the (possibly slow, network) file IO never holds the catalog
//! lock or blocks the UI thread: a `plan_*` reads paths under the lock, a pure free
//! function does the copy/verify/delete off-thread, and a `record_*`/`commit_*` writes
//! the result under the lock. Sync `*_photo` wrappers compose the three for tests and
//! simple callers.

use super::{Catalog, CatalogError, LocationRole, Result, VolumeKind};
use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct BackupPlan {
    pub source: PathBuf,
    pub dest: PathBuf,
    pub rel: String,
    pub volume_id: i64,
}

pub struct OffloadPlan {
    pub backup_abs: PathBuf,
    /// The `photo_locations.id` of the backup copy, so companions carried during the
    /// offload attach to the right row.
    pub backup_location_id: i64,
    pub expected_hash: String,
    pub local_files: Vec<PathBuf>,
    pub local_volume_ids: Vec<i64>,
}

pub struct RestorePlan {
    pub source: PathBuf,
    pub dest: PathBuf,
    pub rel: String,
    pub volume_id: i64,
    pub expected_hash: Option<String>,
}

struct Copy {
    /// `photo_locations.id`. Carried so companions can be attached to the exact row: a
    /// volume can hold more than one role for the same photo (the owner's NAS has both
    /// `primary` and `backup` rows), so (photo, volume) alone does not identify a copy.
    location_id: i64,
    volume_id: i64,
    abs: PathBuf,
    verified_hash: Option<String>,
}

impl Catalog {
    // --- backup ------------------------------------------------------------

    /// Validate the target is a backup volume and a local source exists; compute paths.
    pub fn plan_backup(&self, photo_id: i64, backup_volume_id: i64) -> Result<BackupPlan> {
        let (base, kind) = self.volume_base_kind(backup_volume_id)?;
        if kind != VolumeKind::Backup {
            return Err(CatalogError::Validation("target is not a backup volume".into()));
        }
        let source = self
            .first_existing_copy(photo_id, VolumeKind::Local)?
            .ok_or_else(|| CatalogError::Validation("no local copy to back up".into()))?;
        let rel = self.get_photo(photo_id)?.path;
        Ok(BackupPlan {
            source: source.abs,
            dest: Path::new(&base).join(&rel),
            rel,
            volume_id: backup_volume_id,
        })
    }

    /// Record a verified backup location (after the copy+verify IO succeeded).
    pub fn record_backup(&self, photo_id: i64, volume_id: i64, rel: &str, hash: &str) -> Result<()> {
        self.add_location(photo_id, volume_id, rel, LocationRole::Backup)?;
        self.set_location_verified_hash(photo_id, volume_id, LocationRole::Backup, hash)
    }

    /// Sync convenience: back up a photo to a backup volume, returning the verified hash.
    ///
    /// Carries the photo's declared companions too — a copy is the image *plus* what
    /// describes it (cluster B, D2). A companion that differs at the destination is left
    /// alone rather than overwritten; it shows up as divergence for the freshness pass,
    /// because two edits exist and backup is not the place to pick one.
    pub fn backup_photo(&self, photo_id: i64, backup_volume_id: i64) -> Result<String> {
        let plan = self.plan_backup(photo_id, backup_volume_id)?;
        let hash = copy_and_verify(&plan.source, &plan.dest, None)?;
        self.record_backup(photo_id, plan.volume_id, &plan.rel, &hash)?;
        let carry = carry_companions(&plan.source, &plan.dest)?;
        self.record_companions(
            self.require_location_id(photo_id, plan.volume_id, LocationRole::Backup)?,
            &carry.carried,
        )?;
        Ok(hash)
    }

    // --- offload -----------------------------------------------------------

    /// Validate a verified backup exists and gather the local files to free. Errors
    /// (refuses) if there is no verified backup — invariants 1 & 2.
    pub fn plan_offload(&self, photo_id: i64) -> Result<OffloadPlan> {
        let backup = self
            .verified_backup(photo_id)?
            .ok_or_else(|| CatalogError::Validation("no verified backup — refusing to offload".into()))?;
        let locals = self.copies_on_kind(photo_id, VolumeKind::Local)?;
        let mut local_volume_ids: Vec<i64> = locals.iter().map(|c| c.volume_id).collect();
        local_volume_ids.sort_unstable();
        local_volume_ids.dedup();
        Ok(OffloadPlan {
            backup_location_id: backup.location_id,
            backup_abs: backup.abs,
            expected_hash: backup.verified_hash.unwrap_or_default(),
            local_files: locals.into_iter().map(|c| c.abs).collect(),
            local_volume_ids,
        })
    }

    /// Drop the local location records after the files were deleted + backup verified.
    pub fn commit_offload(&self, photo_id: i64, local_volume_ids: &[i64]) -> Result<()> {
        for &vid in local_volume_ids {
            self.remove_locations_on_volume(photo_id, vid)?;
        }
        Ok(())
    }

    /// Sync convenience: offload a photo's local copies (only after re-verifying backup).
    ///
    /// Recovery note: files are deleted (IO) before the location records are dropped
    /// (`commit_offload`). A crash in between leaves stale local records pointing at
    /// now-missing files — **never data loss** (the verified backup is intact and the
    /// resolver falls back to it); a rescan/reconcile clears the stale records.
    pub fn offload_photo(&self, photo_id: i64) -> Result<()> {
        let plan = self.plan_offload(photo_id)?;
        let carried = verify_and_delete_locals(&plan)?;
        // Record before the local rows go away: `commit_offload` deletes the local
        // location rows, and their companion rows cascade with them.
        self.record_companions(plan.backup_location_id, &carried)?;
        self.commit_offload(photo_id, &plan.local_volume_ids)
    }

    /// Record companions confirmed present at one location.
    ///
    /// Upsert rather than insert: carrying is idempotent, and some companions reached home
    /// by hand before this code existed — a second pass must refresh the reference point,
    /// not fail on a conflict.
    pub fn record_companions(&self, location_id: i64, carried: &[CarriedCompanion]) -> Result<()> {
        if carried.is_empty() {
            return Ok(());
        }
        let at = now();
        for c in carried {
            self.conn.execute(
                "INSERT INTO photo_location_companions(location_id, name, carried_mtime, carried_at)
                 VALUES(?1, ?2, ?3, ?4)
                 ON CONFLICT(location_id, name)
                 DO UPDATE SET carried_mtime = excluded.carried_mtime,
                               carried_at    = excluded.carried_at",
                params![location_id, c.name, c.source_mtime, at],
            )?;
        }
        Ok(())
    }

    /// Note how a photo's companions look on disk *right now*, for the freshness half of
    /// [`SafetyStatus`](crate::catalog::SafetyStatus).
    ///
    /// `image` is a copy of the photo the scanner just walked. Companion rows recorded at
    /// *other* locations get their `source_mtime_seen` set from what is beside this copy,
    /// so "the local file has moved on since we carried it home" becomes answerable in
    /// pure SQL later. Rows belonging to the scanned copy's own volume are skipped — a
    /// file cannot be evidence that it has diverged from itself, and setting them from
    /// the home copy would make every carried companion look permanently current.
    ///
    /// Cheap enough for the scanner's per-file loop: each update is two index probes
    /// (`idx_photo_locations_photo`, then the companion primary key), and photos with no
    /// carried companions match nothing.
    ///
    /// Returns how many companion rows were refreshed.
    pub fn note_companion_freshness(&self, photo_id: i64, image: &Path) -> Result<usize> {
        let scanned_volume = self.volume_for_path(image).map(|(id, _)| id).ok();
        let mut updated = 0usize;
        for found in crate::companions::carried_beside(image) {
            let Ok(mtime) = mtime_secs(&found.path) else { continue };
            updated += self.conn.execute(
                "UPDATE photo_location_companions SET source_mtime_seen = ?1
                 WHERE name = ?2 AND location_id IN (
                     SELECT id FROM photo_locations
                     WHERE photo_id = ?3 AND (?4 IS NULL OR volume_id <> ?4))",
                params![mtime, found.name(), photo_id, scanned_volume],
            )?;
        }
        Ok(updated)
    }

    /// Record companions at the location identified by (photo, volume, role) — the form
    /// the command layer has to hand, since it writes the location row and then needs to
    /// attach to it.
    pub fn record_companions_at(
        &self,
        photo_id: i64,
        volume_id: i64,
        role: LocationRole,
        carried: &[CarriedCompanion],
    ) -> Result<()> {
        if carried.is_empty() {
            return Ok(());
        }
        self.record_companions(self.require_location_id(photo_id, volume_id, role)?, carried)
    }

    /// As [`Catalog::location_id`], but an error when absent. Callers that have just
    /// written the location row use this: a missing row there is a programming error, not
    /// a state the user can reach.
    fn require_location_id(
        &self,
        photo_id: i64,
        volume_id: i64,
        role: LocationRole,
    ) -> Result<i64> {
        self.location_id(photo_id, volume_id, role)?.ok_or_else(|| {
            CatalogError::Validation("no location row to record companions against".into())
        })
    }

    /// The `photo_locations.id` for one (photo, volume, role), if it exists.
    fn location_id(&self, photo_id: i64, volume_id: i64, role: LocationRole) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM photo_locations
                 WHERE photo_id = ?1 AND volume_id = ?2 AND role = ?3",
                params![photo_id, volume_id, role.as_db_str()],
                |r| r.get::<_, i64>(0),
            )
            .optional()?)
    }

    /// Companion names recorded at one location, with the source mtime they were carried
    /// from. Ordered by name so callers and tests see a stable sequence.
    pub fn companions_at(
        &self,
        photo_id: i64,
        volume_id: i64,
        role: LocationRole,
    ) -> Result<Vec<(String, i64)>> {
        let Some(location_id) = self.location_id(photo_id, volume_id, role)? else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare(
            "SELECT name, carried_mtime FROM photo_location_companions
             WHERE location_id = ?1 ORDER BY name",
        )?;
        let rows = stmt.query_map(params![location_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // --- restore -----------------------------------------------------------

    /// Validate the target is a local volume and a backup copy exists; compute paths.
    pub fn plan_restore(&self, photo_id: i64, local_volume_id: i64) -> Result<RestorePlan> {
        let (base, kind) = self.volume_base_kind(local_volume_id)?;
        if kind != VolumeKind::Local {
            return Err(CatalogError::Validation("target is not a local volume".into()));
        }
        let backup = self
            .first_existing_copy(photo_id, VolumeKind::Backup)?
            .ok_or_else(|| CatalogError::Validation("no reachable backup to restore".into()))?;
        let rel = self.get_photo(photo_id)?.path;
        Ok(RestorePlan {
            source: backup.abs,
            dest: Path::new(&base).join(&rel),
            rel,
            volume_id: local_volume_id,
            expected_hash: backup.verified_hash,
        })
    }

    pub fn record_restore(&self, photo_id: i64, volume_id: i64, rel: &str, hash: &str) -> Result<()> {
        self.add_location(photo_id, volume_id, rel, LocationRole::LocalCache)?;
        self.set_location_verified_hash(photo_id, volume_id, LocationRole::LocalCache, hash)
    }

    /// Sync convenience: restore a photo's backup copy to a local volume.
    ///
    /// Brings the companions back with it, so a restored photo arrives with the edit
    /// state an offload freed rather than as bare pixels.
    pub fn restore_photo(&self, photo_id: i64, local_volume_id: i64) -> Result<String> {
        let plan = self.plan_restore(photo_id, local_volume_id)?;
        let hash = copy_and_verify(&plan.source, &plan.dest, plan.expected_hash.as_deref())?;
        self.record_restore(photo_id, plan.volume_id, &plan.rel, &hash)?;
        let carry = carry_companions(&plan.source, &plan.dest)?;
        self.record_companions(
            self.require_location_id(photo_id, plan.volume_id, LocationRole::LocalCache)?,
            &carry.carried,
        )?;
        Ok(hash)
    }

    // --- helpers -----------------------------------------------------------

    fn set_location_verified_hash(
        &self,
        photo_id: i64,
        volume_id: i64,
        role: LocationRole,
        hash: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE photo_locations SET verified_hash = ?1
             WHERE photo_id = ?2 AND volume_id = ?3 AND role = ?4",
            params![hash, photo_id, volume_id, role.as_db_str()],
        )?;
        Ok(())
    }

    fn volume_base_kind(&self, volume_id: i64) -> Result<(String, VolumeKind)> {
        self.conn
            .query_row(
                "SELECT base_path, kind FROM volumes WHERE id = ?1",
                params![volume_id],
                |r| Ok((r.get::<_, String>(0)?, VolumeKind::from_db_str(&r.get::<_, String>(1)?))),
            )
            .optional()?
            .ok_or_else(|| CatalogError::NotFound(format!("volume {volume_id}")))
    }

    /// A photo's copies on volumes of `kind`, as absolute paths + recorded hash.
    fn copies_on_kind(&self, photo_id: i64, kind: VolumeKind) -> Result<Vec<Copy>> {
        let mut stmt = self.conn.prepare(
            "SELECT v.id, v.base_path, l.relative_path, l.verified_hash, l.id
             FROM photo_locations l JOIN volumes v ON v.id = l.volume_id
             WHERE l.photo_id = ?1 AND v.kind = ?2",
        )?;
        let rows = stmt.query_map(params![photo_id, kind.as_db_str()], |r| {
            let base: String = r.get(1)?;
            let rel: String = r.get(2)?;
            Ok(Copy {
                location_id: r.get(4)?,
                volume_id: r.get(0)?,
                abs: Path::new(&base).join(&rel),
                verified_hash: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn first_existing_copy(&self, photo_id: i64, kind: VolumeKind) -> Result<Option<Copy>> {
        Ok(self
            .copies_on_kind(photo_id, kind)?
            .into_iter()
            .find(|c| c.abs.exists()))
    }

    /// A backup copy that has a recorded verified hash and whose file exists.
    fn verified_backup(&self, photo_id: i64) -> Result<Option<Copy>> {
        Ok(self
            .copies_on_kind(photo_id, VolumeKind::Backup)?
            .into_iter()
            .find(|c| c.verified_hash.is_some() && c.abs.exists()))
    }

    /// Whether a photo already has a verified backup whose file is present. Used to make
    /// backup idempotent: an existing good backup must never be re-copied (re-copying over
    /// a flaky mount is what risks destroying it). A *missing* backup still returns false,
    /// so it gets re-created (safely, via [`copy_and_verify`]'s temp+rename).
    pub fn has_verified_backup(&self, photo_id: i64) -> Result<bool> {
        Ok(self.verified_backup(photo_id)?.is_some())
    }

    /// Photos eligible for age-based offload ("keep last N days local"): older than
    /// `age_days` (by capture time, falling back to import time), that STILL have a local
    /// copy AND a present, verified backup. The verified-backup requirement means a photo
    /// is never freed unless its NAS copy is confirmed there — and only when the NAS is
    /// reachable (else `has_verified_backup` is false), so offload never runs blind.
    pub fn photos_eligible_for_offload(&self, age_days: i64) -> Result<Vec<i64>> {
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let cutoff_unix = now_unix - age_days.max(0) * 86_400;
        let cutoff_iso = chrono::DateTime::from_timestamp(cutoff_unix, 0)
            .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S").to_string())
            .unwrap_or_default();
        // Candidates: non-missing photos older than the cutoff that have a local location.
        //
        // Reads `photos`, not `photos_visible` (includes-hidden: freeing space is
        // maintenance, not browsing — a photo the user has hidden still occupies the disk,
        // and is if anything a better offload candidate than one they look at daily).
        let candidates: Vec<i64> = {
            let mut stmt = self.conn.prepare(
                "-- includes-hidden: freeing space is maintenance, not browsing. A photo
                 -- the user has hidden still occupies the disk, and is if anything a
                 -- better offload candidate than one they look at daily.
                 SELECT DISTINCT p.id FROM photos p
                 JOIN photo_locations l ON l.photo_id = p.id
                 JOIN volumes v ON v.id = l.volume_id AND v.kind = 'local'
                 WHERE p.missing = 0 AND (
                     (p.capture_time IS NOT NULL AND p.capture_time <> '' AND p.capture_time < ?1)
                     OR ((p.capture_time IS NULL OR p.capture_time = '') AND p.created_at < ?2)
                 )",
            )?;
            let rows = stmt.query_map(params![cutoff_iso, cutoff_unix], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        // Keep only those with a present verified backup AND a present local file.
        let mut out = Vec::new();
        for id in candidates {
            if self.has_verified_backup(id)?
                && self.first_existing_copy(id, VolumeKind::Local)?.is_some()
            {
                out.push(id);
            }
        }
        Ok(out)
    }
}

/// SHA-256 of a file as lowercase hex. Streams the file (constant memory).
/// A companion confirmed present and byte-identical at a destination.
#[derive(Debug, Clone)]
pub struct CarriedCompanion {
    /// File name at the destination (e.g. `DSC1.ARW.xmp`).
    pub name: String,
    /// The **source** file's mtime when it was carried — not the destination's. The
    /// question a later pass asks is "has the local file moved on since we copied it",
    /// so the local side is the reference point.
    pub source_mtime: i64,
}

/// What one carry pass achieved.
#[derive(Debug, Default, Clone)]
pub struct CompanionCarry {
    pub carried: Vec<CarriedCompanion>,
    /// Companions that exist on both sides with different contents. Left untouched: the
    /// two sides hold different edits and picking one would silently discard the other.
    pub diverged: Vec<PathBuf>,
}

/// Ensure every declared companion beside `src_image` is present beside `dest_image`.
///
/// A copy is the image *plus* its declared companions (cluster B, D2). Before this,
/// `backup_photo` copied one file, so darktable history and RapidRAW state never reached
/// home while the app reported the photo backed up (#80).
///
/// Idempotent by construction, which matters because some companions were carried by hand
/// before this code existed: a destination that already holds an identical file is recorded
/// as carried without being rewritten, and a destination that differs is reported rather
/// than overwritten. Copying is the same hash-verified atomic rename the image uses.
///
/// Pure file IO — no catalog lock, so it can run off-thread like the rest of this module.
pub fn carry_companions(src_image: &Path, dest_image: &Path) -> Result<CompanionCarry> {
    let mut out = CompanionCarry::default();
    for found in crate::companions::carried_beside(src_image) {
        let dest = found.destination(dest_image);
        if dest.is_file() {
            if sha256_file(&dest)? != sha256_file(&found.path)? {
                out.diverged.push(found.path.clone());
                continue;
            }
        } else {
            copy_and_verify(&found.path, &dest, None)?;
        }
        out.carried.push(CarriedCompanion {
            name: found.name(),
            source_mtime: mtime_secs(&found.path)?,
        });
    }
    Ok(out)
}

/// A file's mtime in whole seconds. Whole seconds because that is the resolution the
/// catalog stores and compares at; sub-second precision would make every comparison
/// filesystem-dependent.
fn mtime_secs(path: &Path) -> Result<i64> {
    let modified = std::fs::metadata(path).map_err(io)?.modified().map_err(io)?;
    Ok(modified
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0))
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path).map_err(io)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).map_err(io)?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// Copy `src` → `dst` (creating parents) and verify by hashing. Returns the verified
/// SHA-256.
///
/// The source is hashed **before** the copy, so the recorded hash can't reflect a
/// source modified mid-copy. When `expected` is given (restore: the backup's recorded
/// hash), the source must match it too — refusing to propagate a corrupted backup.
///
/// **Crash/flaky-mount safe:** the bytes go to a hidden temp sibling, which is verified
/// and only then atomically renamed onto `dst`. We NEVER write directly over `dst`.
/// Rationale: on a flaky mount (e.g. CIFS with `cache=strict`) the read-back verify can
/// spuriously fail; copying over `dst` and deleting it on failure would destroy an
/// existing good backup. With temp+rename, a failed copy/verify removes only the temp and
/// leaves any existing destination untouched. (This is the bug that silently deleted 33
/// verified NAS backups.)
pub fn copy_and_verify(src: &Path, dst: &Path, expected: Option<&str>) -> Result<String> {
    let src_hash = sha256_file(src)?;
    if let Some(exp) = expected {
        if exp != src_hash {
            return Err(CatalogError::Validation(
                "source does not match the expected hash (corrupted backup?)".into(),
            ));
        }
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(io)?;
    }
    // Hidden (dot-prefixed, so scans skip it) temp sibling on the same filesystem so the
    // final step is an atomic rename.
    let tmp = {
        let name = dst.file_name().and_then(|n| n.to_str()).unwrap_or("backup");
        dst.with_file_name(format!(".{name}.chairphoto-part"))
    };
    let copy_verify_rename = || -> Result<()> {
        std::fs::copy(src, &tmp).map_err(io)?;
        if sha256_file(&tmp)? != src_hash {
            return Err(CatalogError::Validation(
                "copy verification failed (hash mismatch)".into(),
            ));
        }
        std::fs::rename(&tmp, dst).map_err(io)?;
        Ok(())
    };
    if let Err(e) = copy_verify_rename() {
        std::fs::remove_file(&tmp).ok(); // clean up the temp; never touch dst
        return Err(e);
    }
    Ok(src_hash)
}

/// Re-verify the backup's hash, carry the local copy's companions home, then delete the
/// local files. Invariant 3: never delete a local copy unless the backup is present and
/// still hashes to the recorded value.
///
/// Returns the companions now confirmed at home, for the caller to record — which it must
/// do *before* `commit_offload`, since that drops the local location rows and the companion
/// rows cascade with them.
pub fn verify_and_delete_locals(plan: &OffloadPlan) -> Result<Vec<CarriedCompanion>> {
    let current = sha256_file(&plan.backup_abs)?;
    if current != plan.expected_hash {
        return Err(CatalogError::Validation(
            "backup hash changed — refusing to offload".into(),
        ));
    }

    // Invariant 1 ("never delete the last copy") applies to companions too: freeing the
    // local image must not strand the edit state sitting beside it (#80). Carry anything
    // missing to home *before* deleting, and refuse outright when a companion differs on
    // the two sides — that is two unreconciled edits, and offload is not the place to
    // choose between them.
    let mut carried = Vec::new();
    for file in &plan.local_files {
        let carry = carry_companions(file, &plan.backup_abs)?;
        if let Some(first) = carry.diverged.first() {
            return Err(CatalogError::Validation(format!(
                "{} differs from the copy at home — refusing to offload",
                first.display()
            )));
        }
        carried.extend(carry.carried);
    }

    for file in &plan.local_files {
        // Companions first: if this fails part-way, the image is still local, so the
        // photo is never left with its edit state gone and its bytes freed.
        for found in crate::companions::carried_beside(file) {
            std::fs::remove_file(&found.path).map_err(io)?;
        }
        // Best-effort: an already-absent local file is fine (goal is "not local").
        if file.exists() {
            std::fs::remove_file(file).map_err(io)?;
        }
    }
    Ok(carried)
}

fn io(e: std::io::Error) -> CatalogError {
    CatalogError::Io(e.to_string())
}
