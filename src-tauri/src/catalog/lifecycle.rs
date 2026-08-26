//! Storage lifecycle (E3): backup local→backup volume, offload (free local space),
//! and restore (pull a backup back to local) — all SHA-256 hash-verified, enforcing
//! the non-negotiable safety invariants in docs/storage-and-import.md:
//!
//!   1. Never delete the last copy. 2. Never offload without a verified backup.
//!   3. Hash-verify the backup before deleting anything local.
//!
//! Each op is split so the (possibly slow, network) file IO never holds the catalog
//! lock or blocks the UI thread: a `plan_*_candidates` gathers copy rows under the lock
//! (PURE SQL — even a `Path::exists` against an unmounted NAS can block for seconds,
//! #85), a `resolve_*_plan` stats those candidates into a plan OFF the lock, a pure free
//! function does the copy/verify/delete off-thread, and a `record_*`/`commit_*` writes
//! the result under the lock. It is the resolver's split (`photo_path_candidates` /
//! `volume_health::pick_existing`) applied to planning. Sync `plan_*` and `*_photo`
//! wrappers compose the pieces for tests and simple callers.

use super::{Catalog, CatalogError, LocationRole, Result, VolumeKind};
use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedPhoto {
    pub photo_id: i64,
    pub reason: String,
}

impl SkippedPhoto {
    fn new(photo_id: i64, reason: impl Into<String>) -> Self {
        Self { photo_id, reason: reason.into() }
    }
}

/// One photo's share of a backup: where its image is now and where the copy goes.
pub struct PhotoBackup {
    pub photo_id: i64,
    pub source: PathBuf,
    pub dest: PathBuf,
    pub rel: String,
    pub volume_id: i64,
}

/// What backing up one tile covers: the photo the caller named **and its stack frames**.
///
/// Since stacking became the normal way a burst is stored, a tile is a moment rather than
/// a file — which is why trash has taken the whole stack since cluster B. Backup and
/// offload took the master alone, so the same tile behaved two ways (#82). The cascade
/// lives in the plan so that no caller — the inspector, the reconcile drain, the age-based
/// sweep — can inherit half of it.
///
/// Each frame is gated on its **own** copies, never on the master's: a frame with nothing
/// local to copy is skipped and named, not dragged along or silently dropped.
pub struct BackupPlan {
    /// The photo the caller named. Its refusal is the call's refusal.
    pub named: PhotoBackup,
    /// The frames that can travel with it.
    pub frames: Vec<PhotoBackup>,
    /// Frames left behind, with why.
    pub skipped: Vec<SkippedPhoto>,
    pub total: usize,
}

/// The rows a backup plan draws on, gathered in PURE SQL so the catalog lock is never
/// held across a filesystem stat (#85). Which of these copies is actually on disk is
/// deliberately not known yet: [`resolve_backup_plan`] stats the candidates OFF the
/// lock — the resolver's split ([`Catalog::photo_path_candidates`] under the lock,
/// `volume_health::pick_existing` off it) applied to planning, so a slow or unmounted
/// NAS can stall one plan but never every catalog reader queued behind the mutex.
pub struct BackupCandidates {
    named: BackupMember,
    frames: Vec<BackupMember>,
    total: usize,
    /// The backup volume's base path. The destination side needs no stat at all.
    base: String,
    volume_id: i64,
}

/// One photo's copy rows for a backup, with no reference to the stack it may be part of.
struct BackupMember {
    photo_id: i64,
    /// Local copy rows; the first whose file exists becomes the source.
    locals: Vec<Copy>,
    /// `photos.path`. `None` when the row is gone — a vanished id also has no location
    /// rows, so it still refuses as "no local copy to back up", the unsplit planner's
    /// answer.
    rel: Option<String>,
}

/// What one backup call achieved. Reported rather than counted, so the UI can say "4 of 7"
/// and name the three it did not take.
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupReport {
    /// Photos now backed up: the named photo, then the frames that went with it.
    pub backed_up: Vec<i64>,
    /// Frames left local, with why.
    pub skipped: Vec<SkippedPhoto>,
    pub total: usize,
}

/// One photo's share of an offload: the verified backup that permits it and the local
/// files it frees.
pub struct PhotoOffload {
    pub photo_id: i64,
    pub backup_abs: PathBuf,
    /// The `photo_locations.id` of the backup copy, so companions carried during the
    /// offload attach to the right row.
    pub backup_location_id: i64,
    pub expected_hash: String,
    pub local_files: Vec<PathBuf>,
    pub local_volume_ids: Vec<i64>,
}

/// What offloading one tile covers — the stack half of [`BackupPlan`]'s reasoning.
///
/// Offloading a 7-frame burst expecting ~600 MB back and getting the keeper's 90 MB
/// defeats the point of the verb. The per-frame gate matters more here than for backup:
/// invariant 2 ("never offload without a verified backup") is decided **per frame**, so a
/// frame whose own backup is missing stays local instead of being freed on the strength of
/// the master's.
pub struct OffloadPlan {
    /// The photo the caller named. Its refusal is the call's refusal.
    pub named: PhotoOffload,
    /// The frames that can be freed with it.
    pub frames: Vec<PhotoOffload>,
    /// Frames left local, with why.
    pub skipped: Vec<SkippedPhoto>,
    pub total: usize,
}

/// The rows an offload plan draws on — [`BackupCandidates`]' split, for the verb where
/// it matters most: invariant 2 is decided per frame, and each frame's gate is a stat
/// against the (possibly slow, possibly unmounted) backup volume.
pub struct OffloadCandidates {
    named: OffloadMember,
    frames: Vec<OffloadMember>,
    total: usize,
}

/// One photo's copy rows for an offload.
struct OffloadMember {
    photo_id: i64,
    /// Backup rows with a recorded verified hash; the first whose file exists is the
    /// gate. The hash requirement is row data (SQL); presence is the stat half's call.
    verified_backups: Vec<Copy>,
    /// Local rows — freed wholesale, so nothing here depends on a stat.
    locals: Vec<Copy>,
}

/// One photo's local copies, freed and confirmed at home — the caller still has to record
/// the companions and drop the location rows.
pub struct FreedPhoto {
    pub photo_id: i64,
    pub backup_location_id: i64,
    pub local_volume_ids: Vec<i64>,
    pub carried: Vec<CarriedCompanion>,
}

/// What the file half of an offload achieved, for [`Catalog::commit_offload_carry`].
pub struct OffloadCarry {
    pub freed: Vec<FreedPhoto>,
    /// Frames left local — planned-out, or refused by their own IO.
    pub skipped: Vec<SkippedPhoto>,
    pub total: usize,
    /// `<sidecar>.chairphoto-backup` files left beside a freed image. See
    /// [`crate::companions::sidecar_backups_beside`] for why they are left.
    pub sidecar_backups_left: usize,
}

/// What one offload call achieved.
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OffloadReport {
    /// Photos whose local copies were freed: the named photo, then its frames.
    pub freed: Vec<i64>,
    /// Frames left local, with why (no verified backup yet, a diverged companion).
    pub skipped: Vec<SkippedPhoto>,
    pub total: usize,
    /// Sidecar backups deliberately left in place, so offload says what it left rather
    /// than leaving it silently (#82).
    pub sidecar_backups_left: usize,
}

/// One photo's share of a restore.
pub struct PhotoRestore {
    pub photo_id: i64,
    pub source: PathBuf,
    pub dest: PathBuf,
    pub rel: String,
    pub volume_id: i64,
    pub expected_hash: Option<String>,
}

/// What restoring one tile covers. The third verb of the same rule: offload frees the
/// moment, so restore has to bring the moment back — a stack that goes to the NAS as seven
/// frames and returns as one would be the asymmetry #82 is about, pointing the other way.
pub struct RestorePlan {
    /// The photo the caller named. Its refusal is the call's refusal.
    pub named: PhotoRestore,
    /// Frames that are not at home and can be brought back.
    pub frames: Vec<PhotoRestore>,
    /// Frames left on the backup volume, with why.
    pub skipped: Vec<SkippedPhoto>,
    pub total: usize,
}

/// The rows a restore plan draws on — see [`BackupCandidates`] for the split (#85).
pub struct RestoreCandidates {
    named: RestoreMember,
    frames: Vec<RestoreMember>,
    total: usize,
    /// The local volume's base path.
    base: String,
    volume_id: i64,
}

/// One photo's copy rows for a restore.
struct RestoreMember {
    photo_id: i64,
    /// Local rows — a frame with one whose file exists is already home and needs
    /// nothing. Only frames consult these; the named photo is always attempted.
    locals: Vec<Copy>,
    /// Backup rows; the first whose file exists becomes the source.
    backups: Vec<Copy>,
    /// `photos.path`, `None` when the row is gone — see [`BackupMember::rel`].
    rel: Option<String>,
}

/// What one restore call achieved.
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreReport {
    /// Photos now local again: the named photo, then the frames that came with it.
    pub restored: Vec<i64>,
    /// Frames left on the backup volume, with why.
    pub skipped: Vec<SkippedPhoto>,
    pub total: usize,
}

/// One age-eligible photo and the copy paths that decide whether the offload sweep may
/// take it — the rows half of [`Catalog::photos_eligible_for_offload`] (#85).
pub struct OffloadEligibility {
    photo_id: i64,
    /// Backup rows with a recorded verified hash — one must be on disk.
    verified_backups: Vec<PathBuf>,
    /// Local rows — one must be on disk, else there is nothing left to free.
    locals: Vec<PathBuf>,
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

    /// Gather every copy row a backup plan might need — for the named photo **and every
    /// frame stacked under it** (see [`BackupPlan`]) — and validate the target is a
    /// backup volume. PURE SQL: safe to call under the catalog lock; the filesystem's
    /// half is [`resolve_backup_plan`], off it (#85).
    pub fn plan_backup_candidates(
        &self,
        photo_id: i64,
        backup_volume_id: i64,
    ) -> Result<BackupCandidates> {
        let (base, kind) = self.volume_base_kind(backup_volume_id)?;
        if kind != VolumeKind::Backup {
            return Err(CatalogError::Validation("target is not a backup volume".into()));
        }
        let named = self.backup_member(photo_id)?;
        let frame_ids = self.stack_frame_ids(photo_id)?;
        let total = 1 + frame_ids.len();
        let frames = frame_ids
            .into_iter()
            .map(|id| self.backup_member(id))
            .collect::<Result<Vec<_>>>()?;
        Ok(BackupCandidates { named, frames, total, base, volume_id: backup_volume_id })
    }

    /// One photo's rows for [`BackupCandidates`]. A photo with no local rows still gets
    /// a member — [`resolve_backup_plan`] refuses it with the same words it uses for
    /// copies missing on disk, which keeps skip order and the vanished-id answer stable.
    fn backup_member(&self, photo_id: i64) -> Result<BackupMember> {
        let locals = self.copies_on_kind(photo_id, VolumeKind::Local)?;
        let rel = self
            .conn
            .query_row(
                "-- includes-hidden: a point lookup for a row the plan already names.
                 -- Moving bytes is maintenance, not browsing (see `stack_frame_ids`):
                 -- a hidden (trashed, missing) photo still holds bytes to move.
                 SELECT path FROM photos WHERE id = ?1",
                params![photo_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(BackupMember { photo_id, locals, rel })
    }

    /// Statting convenience composing [`Catalog::plan_backup_candidates`] with
    /// [`resolve_backup_plan`] — for the sync wrappers and tests. Command paths split
    /// the two halves around the catalog lock instead (#85).
    pub fn plan_backup(&self, photo_id: i64, backup_volume_id: i64) -> Result<BackupPlan> {
        resolve_backup_plan(self.plan_backup_candidates(photo_id, backup_volume_id)?)
    }

    /// Record a verified backup location (after the copy+verify IO succeeded).
    ///
    /// Private on purpose: it records an image and nothing else, which is only half a copy.
    /// [`Catalog::record_copy`] is the way in, so no path outside this module can record a
    /// location while forgetting the companions that belong to it.
    fn record_backup(&self, photo_id: i64, volume_id: i64, rel: &str, hash: &str) -> Result<()> {
        self.add_location(photo_id, volume_id, rel, LocationRole::Backup)?;
        self.set_location_verified_hash(photo_id, volume_id, LocationRole::Backup, hash)
    }

    /// Sync convenience: back up a photo — and its stack — to a backup volume.
    ///
    /// Carries each photo's declared companions too: a copy is the image *plus* what
    /// describes it (cluster B, D2). A companion that differs at the destination is left
    /// alone rather than overwritten; it shows up as divergence for the freshness pass,
    /// because two edits exist and backup is not the place to pick one.
    ///
    /// A frame that fails is reported, not fatal — the master is already at home by then,
    /// and turning that into an error would report a copy that happened as one that did
    /// not.
    pub fn backup_photo(&self, photo_id: i64, backup_volume_id: i64) -> Result<BackupReport> {
        let plan = self.plan_backup(photo_id, backup_volume_id)?;
        let mut report = BackupReport { skipped: plan.skipped, total: plan.total, ..Default::default() };
        let outcome = copy_with_companions(&plan.named.source, &plan.named.dest, None)?;
        self.record_copy(
            plan.named.photo_id,
            plan.named.volume_id,
            &plan.named.rel,
            LocationRole::Backup,
            &outcome,
        )?;
        report.backed_up.push(plan.named.photo_id);
        for frame in &plan.frames {
            match copy_with_companions(&frame.source, &frame.dest, None) {
                Ok(outcome) => {
                    self.record_copy(
                        frame.photo_id,
                        frame.volume_id,
                        &frame.rel,
                        LocationRole::Backup,
                        &outcome,
                    )?;
                    report.backed_up.push(frame.photo_id);
                }
                Err(e) => report.skipped.push(SkippedPhoto::new(frame.photo_id, e.to_string())),
            }
        }
        Ok(report)
    }

    // --- offload -----------------------------------------------------------

    /// Gather every copy row an offload plan draws on — for the named photo **and every
    /// frame stacked under it** (see [`OffloadPlan`]). PURE SQL: safe to call under the
    /// catalog lock; invariants 1 & 2 are enforced by [`resolve_offload_plan`], off it
    /// (#85).
    pub fn plan_offload_candidates(&self, photo_id: i64) -> Result<OffloadCandidates> {
        let named = self.offload_member(photo_id)?;
        let mut frames = Vec::new();
        let frame_ids = self.stack_frame_ids(photo_id)?;
        let total = 1 + frame_ids.len();
        for frame_id in frame_ids {
            // Nothing local to free is not a refusal: an already-archived frame is the
            // state offload wants, and listing it as skipped would invite the user to act
            // on it. Rows, not files — the rows are what `commit_offload` drops — which
            // is why this stays on the SQL side of the split.
            let member = self.offload_member(frame_id)?;
            if member.locals.is_empty() {
                continue;
            }
            frames.push(member);
        }
        Ok(OffloadCandidates { named, frames, total })
    }

    /// One photo's rows for [`OffloadCandidates`].
    fn offload_member(&self, photo_id: i64) -> Result<OffloadMember> {
        let verified_backups = self
            .copies_on_kind(photo_id, VolumeKind::Backup)?
            .into_iter()
            .filter(|c| c.verified_hash.is_some())
            .collect();
        Ok(OffloadMember {
            photo_id,
            verified_backups,
            locals: self.copies_on_kind(photo_id, VolumeKind::Local)?,
        })
    }

    /// Statting convenience composing [`Catalog::plan_offload_candidates`] with
    /// [`resolve_offload_plan`] — for the sync wrappers and tests; command paths split
    /// the two halves around the catalog lock (#85). Errors (refuses) if the named photo
    /// has no verified backup — invariants 1 & 2.
    pub fn plan_offload(&self, photo_id: i64) -> Result<OffloadPlan> {
        resolve_offload_plan(self.plan_offload_candidates(photo_id)?)
    }

    /// The frames stacked under `photo_id`, in a stable order.
    ///
    /// Empty for a frame: stacks are one level deep (`set_stack_parent` flattens), so a
    /// storage verb pressed on a frame acts on that frame alone — the same asymmetry
    /// `restore_photos` has, where restoring a child does not restore its master.
    fn stack_frame_ids(&self, photo_id: i64) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare(
            "-- includes-hidden: moving bytes is maintenance, not browsing. A frame the grid
             -- hides (trashed, missing) still occupies the disk it is being freed from, and
             -- still holds edit state that has to reach home before anything is deleted.
             SELECT id FROM photos WHERE stack_parent_id = ?1 ORDER BY path COLLATE NOCASE",
        )?;
        let rows = stmt.query_map(params![photo_id], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Drop the local location records after the files were deleted + backup verified.
    pub fn commit_offload(&self, photo_id: i64, local_volume_ids: &[i64]) -> Result<()> {
        for &vid in local_volume_ids {
            self.remove_locations_on_volume(photo_id, vid)?;
        }
        Ok(())
    }

    /// Record what reached home and drop the local location rows, for every photo an
    /// offload freed.
    ///
    /// Recording comes first per photo: `commit_offload` deletes the local location rows
    /// and their companion rows cascade with them.
    pub fn commit_offload_carry(&self, carry: OffloadCarry) -> Result<OffloadReport> {
        let mut report = OffloadReport {
            skipped: carry.skipped,
            total: carry.total,
            sidecar_backups_left: carry.sidecar_backups_left,
            ..Default::default()
        };
        for freed in &carry.freed {
            self.record_companions(freed.backup_location_id, &freed.carried)?;
            self.commit_offload(freed.photo_id, &freed.local_volume_ids)?;
            report.freed.push(freed.photo_id);
        }
        Ok(report)
    }

    /// Sync convenience: offload a photo's local copies — and its stack's — after
    /// re-verifying each backup.
    ///
    /// Recovery note: files are deleted (IO) before the location records are dropped
    /// (`commit_offload`). A crash in between leaves stale local records pointing at
    /// now-missing files — **never data loss** (the verified backup is intact and the
    /// resolver falls back to it); a rescan/reconcile clears the stale records.
    pub fn offload_photo(&self, photo_id: i64) -> Result<OffloadReport> {
        let plan = self.plan_offload(photo_id)?;
        let carry = verify_and_delete_locals(&plan)?;
        self.commit_offload_carry(carry)
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
        // The home locations this companion ought to reach. Resolved first so a companion
        // that was never carried can still be *recorded* against them — an UPDATE alone
        // could only refresh evidence that already existed, which left "exists locally,
        // never carried home" invisible and reporting as safe (cluster B, D5).
        let mut stmt = self.conn.prepare(
            "SELECT l.id FROM photo_locations l JOIN volumes v ON v.id = l.volume_id
             WHERE l.photo_id = ?1 AND v.kind = 'backup' AND l.role IN ('primary','backup')
               AND (?2 IS NULL OR l.volume_id <> ?2)",
        )?;
        let homes: Vec<i64> = stmt
            .query_map(params![photo_id, scanned_volume], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);

        let mut noted = 0usize;
        for found in crate::companions::carried_beside(image) {
            let Ok(mtime) = mtime_secs(&found.path) else { continue };
            for home in &homes {
                // Insert leaves `carried_mtime` NULL — this is evidence the companion
                // exists here, not a claim that anything carried it. A row that *was*
                // carried keeps its `carried_mtime` and only has its sighting refreshed.
                self.conn.execute(
                    "INSERT INTO photo_location_companions(location_id, name, source_mtime_seen)
                     VALUES(?1, ?2, ?3)
                     ON CONFLICT(location_id, name)
                     DO UPDATE SET source_mtime_seen = excluded.source_mtime_seen",
                    params![home, found.name(), mtime],
                )?;
                noted += 1;
            }
        }
        Ok(noted)
    }

    /// Record a completed lifecycle copy: the location, its verified hash, and the
    /// companions that came with it.
    ///
    /// The single entry point for recording a copy, and the reason the two halves cannot
    /// drift apart again. Before this existed, the shipping backup path carried companions
    /// and the shipping restore path did not — the same omission as #80, in the other
    /// direction, six weeks later.
    pub fn record_copy(
        &self,
        photo_id: i64,
        volume_id: i64,
        rel: &str,
        role: LocationRole,
        outcome: &CopyOutcome,
    ) -> Result<()> {
        match role {
            LocationRole::Backup => self.record_backup(photo_id, volume_id, rel, &outcome.hash)?,
            _ => self.record_restore(photo_id, volume_id, rel, &outcome.hash)?,
        }
        self.record_companions_at(photo_id, volume_id, role, &outcome.carried)
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

    /// Gather every copy row a restore plan might need — for the named photo **and the
    /// frames stacked under it** — and validate the target is a local volume. PURE SQL:
    /// safe to call under the catalog lock; which frames are already home and which
    /// backup is reachable are [`resolve_restore_plan`]'s questions, off it (#85).
    pub fn plan_restore_candidates(
        &self,
        photo_id: i64,
        local_volume_id: i64,
    ) -> Result<RestoreCandidates> {
        let (base, kind) = self.volume_base_kind(local_volume_id)?;
        if kind != VolumeKind::Local {
            return Err(CatalogError::Validation("target is not a local volume".into()));
        }
        let named = self.restore_member(photo_id)?;
        let frame_ids = self.stack_frame_ids(photo_id)?;
        let total = 1 + frame_ids.len();
        let frames = frame_ids
            .into_iter()
            .map(|id| self.restore_member(id))
            .collect::<Result<Vec<_>>>()?;
        Ok(RestoreCandidates { named, frames, total, base, volume_id: local_volume_id })
    }

    /// One photo's rows for [`RestoreCandidates`].
    fn restore_member(&self, photo_id: i64) -> Result<RestoreMember> {
        let rel = self
            .conn
            .query_row(
                "-- includes-hidden: a point lookup for a row the plan already names.
                 -- Moving bytes is maintenance, not browsing (see `stack_frame_ids`):
                 -- a hidden (trashed, missing) photo still holds bytes to move.
                 SELECT path FROM photos WHERE id = ?1",
                params![photo_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(RestoreMember {
            photo_id,
            locals: self.copies_on_kind(photo_id, VolumeKind::Local)?,
            backups: self.copies_on_kind(photo_id, VolumeKind::Backup)?,
            rel,
        })
    }

    /// Statting convenience composing [`Catalog::plan_restore_candidates`] with
    /// [`resolve_restore_plan`] — for the sync wrappers and tests; command paths split
    /// the two halves around the catalog lock (#85).
    pub fn plan_restore(&self, photo_id: i64, local_volume_id: i64) -> Result<RestorePlan> {
        resolve_restore_plan(self.plan_restore_candidates(photo_id, local_volume_id)?)
    }

    /// As [`Catalog::record_backup`], for a local cache copy. Private for the same reason.
    fn record_restore(&self, photo_id: i64, volume_id: i64, rel: &str, hash: &str) -> Result<()> {
        self.add_location(photo_id, volume_id, rel, LocationRole::LocalCache)?;
        self.set_location_verified_hash(photo_id, volume_id, LocationRole::LocalCache, hash)
    }

    /// Sync convenience: restore a photo's backup copy — and its stack's — to a local
    /// volume.
    ///
    /// Brings the companions back with it, so a restored photo arrives with the edit
    /// state an offload freed rather than as bare pixels.
    pub fn restore_photo(&self, photo_id: i64, local_volume_id: i64) -> Result<RestoreReport> {
        let plan = self.plan_restore(photo_id, local_volume_id)?;
        let mut report = RestoreReport { skipped: plan.skipped, total: plan.total, ..Default::default() };
        let outcome = copy_with_companions(
            &plan.named.source,
            &plan.named.dest,
            plan.named.expected_hash.as_deref(),
        )?;
        self.record_copy(
            plan.named.photo_id,
            plan.named.volume_id,
            &plan.named.rel,
            LocationRole::LocalCache,
            &outcome,
        )?;
        report.restored.push(plan.named.photo_id);
        for frame in &plan.frames {
            match copy_with_companions(&frame.source, &frame.dest, frame.expected_hash.as_deref()) {
                Ok(outcome) => {
                    self.record_copy(
                        frame.photo_id,
                        frame.volume_id,
                        &frame.rel,
                        LocationRole::LocalCache,
                        &outcome,
                    )?;
                    report.restored.push(frame.photo_id);
                }
                Err(e) => report.skipped.push(SkippedPhoto::new(frame.photo_id, e.to_string())),
            }
        }
        Ok(report)
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

    /// The backup copies with a recorded verified hash, as absolute paths — the rows
    /// half of [`Catalog::has_verified_backup`], for callers that stat off the catalog
    /// lock through [`any_backup_present`] (#85).
    pub fn verified_backup_candidates(&self, photo_id: i64) -> Result<Vec<PathBuf>> {
        Ok(self
            .copies_on_kind(photo_id, VolumeKind::Backup)?
            .into_iter()
            .filter(|c| c.verified_hash.is_some())
            .map(|c| c.abs)
            .collect())
    }

    /// Whether a photo already has a verified backup whose file is present. Used to make
    /// backup idempotent: an existing good backup must never be re-copied (re-copying over
    /// a flaky mount is what risks destroying it). A *missing* backup still returns false,
    /// so it gets re-created (safely, via [`copy_and_verify`]'s temp+rename).
    ///
    /// Statting convenience over [`Catalog::verified_backup_candidates`] +
    /// [`any_backup_present`]; command paths split the two around the catalog lock (#85).
    pub fn has_verified_backup(&self, photo_id: i64) -> Result<bool> {
        Ok(any_backup_present(&self.verified_backup_candidates(photo_id)?))
    }

    /// Pure-SQL half of [`Catalog::photos_eligible_for_offload`] (#85): photos eligible
    /// for age-based offload ("keep last N days local") — older than `age_days` (by
    /// capture time, falling back to import time) and holding local rows — plus the copy
    /// paths [`filter_offload_eligible`] stats to confirm each one. Split because the
    /// sweep stats once per candidate across the whole library, the worst possible pass
    /// to hold the catalog mutex across.
    pub fn offload_eligibility_candidates(
        &self,
        age_days: i64,
    ) -> Result<Vec<OffloadEligibility>> {
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
            rows.collect::<rusqlite::Result<Vec<i64>>>()?
        };
        candidates
            .into_iter()
            .map(|photo_id| {
                Ok(OffloadEligibility {
                    photo_id,
                    verified_backups: self.verified_backup_candidates(photo_id)?,
                    locals: self
                        .copies_on_kind(photo_id, VolumeKind::Local)?
                        .into_iter()
                        .map(|c| c.abs)
                        .collect(),
                })
            })
            .collect()
    }

    /// Statting convenience composing [`Catalog::offload_eligibility_candidates`] with
    /// [`filter_offload_eligible`] — for tests and simple callers; the policy sweep
    /// splits the two halves around the catalog lock (#85).
    pub fn photos_eligible_for_offload(&self, age_days: i64) -> Result<Vec<i64>> {
        Ok(filter_offload_eligible(self.offload_eligibility_candidates(age_days)?))
    }
}

// --- the stat half of planning (#85) ---------------------------------------

/// The first candidate copy whose file is on disk — the planners' one filesystem
/// question, funnelled through [`crate::volume_health::candidate_exists`] so the unit
/// tests can count planner stats the way they count resolver stats.
fn first_present(copies: Vec<Copy>) -> Option<Copy> {
    copies
        .into_iter()
        .find(|c| crate::volume_health::candidate_exists(&c.abs))
}

/// The "which of these exists?" half of [`Catalog::plan_backup_candidates`]. Stats the
/// filesystem — call it from a blocking context with NO catalog lock held; everything it
/// needs travels inside the candidates.
pub fn resolve_backup_plan(candidates: BackupCandidates) -> Result<BackupPlan> {
    let BackupCandidates { named, frames: members, total, base, volume_id } = candidates;
    let named = resolve_backup_member(named, &base, volume_id)?;
    let mut frames = Vec::new();
    let mut skipped = Vec::new();
    for member in members {
        let frame_id = member.photo_id;
        match resolve_backup_member(member, &base, volume_id) {
            Ok(plan) => frames.push(plan),
            // A frame's own missing copy is a skip, not the master's failure. Anything
            // else is still an error.
            Err(CatalogError::Validation(why)) => skipped.push(SkippedPhoto::new(frame_id, why)),
            Err(e) => return Err(e),
        }
    }
    Ok(BackupPlan { named, frames, skipped, total })
}

fn resolve_backup_member(member: BackupMember, base: &str, volume_id: i64) -> Result<PhotoBackup> {
    let source = first_present(member.locals)
        .ok_or_else(|| CatalogError::Validation("no local copy to back up".into()))?;
    // A copy row implies a photos row, so `rel` is present whenever a source was found.
    let rel = member
        .rel
        .ok_or_else(|| CatalogError::Validation("photo row vanished while planning".into()))?;
    Ok(PhotoBackup {
        photo_id: member.photo_id,
        source: source.abs,
        dest: Path::new(base).join(&rel),
        rel,
        volume_id,
    })
}

/// The "which of these exists?" half of [`Catalog::plan_offload_candidates`] —
/// invariant 2's gate, statted with NO catalog lock held.
///
/// A stale answer here is never destructive. This stat only *admits* a photo to the
/// plan; before anything is deleted, [`free_local_copies`] re-hashes the backup file
/// itself (invariant 3), so a backup that vanishes between this stat and the delete
/// fails that re-check and the photo stays local. Going stale off the lock can delay or
/// refuse an offload — never make one wrong.
pub fn resolve_offload_plan(candidates: OffloadCandidates) -> Result<OffloadPlan> {
    let OffloadCandidates { named, frames: members, total } = candidates;
    let named = resolve_offload_member(named)?;
    let mut frames = Vec::new();
    let mut skipped = Vec::new();
    for member in members {
        let frame_id = member.photo_id;
        match resolve_offload_member(member) {
            Ok(plan) => frames.push(plan),
            // Invariant 2 is decided per frame: a frame without its own verified
            // backup stays local and is named, rather than being freed on the strength
            // of the master's backup.
            Err(CatalogError::Validation(why)) => skipped.push(SkippedPhoto::new(frame_id, why)),
            Err(e) => return Err(e),
        }
    }
    Ok(OffloadPlan { named, frames, skipped, total })
}

fn resolve_offload_member(member: OffloadMember) -> Result<PhotoOffload> {
    let backup = first_present(member.verified_backups).ok_or_else(|| {
        CatalogError::Validation("no verified backup — refusing to offload".into())
    })?;
    let mut local_volume_ids: Vec<i64> = member.locals.iter().map(|c| c.volume_id).collect();
    local_volume_ids.sort_unstable();
    local_volume_ids.dedup();
    Ok(PhotoOffload {
        photo_id: member.photo_id,
        backup_location_id: backup.location_id,
        backup_abs: backup.abs,
        expected_hash: backup.verified_hash.unwrap_or_default(),
        local_files: member.locals.into_iter().map(|c| c.abs).collect(),
        local_volume_ids,
    })
}

/// The "which of these exists?" half of [`Catalog::plan_restore_candidates`], statted
/// with NO catalog lock held.
pub fn resolve_restore_plan(candidates: RestoreCandidates) -> Result<RestorePlan> {
    let RestoreCandidates { named, frames: members, total, base, volume_id } = candidates;
    let named = resolve_restore_member(named, &base, volume_id)?;
    let mut frames = Vec::new();
    let mut skipped = Vec::new();
    for member in members {
        // A frame already at home needs nothing — and copying the backup over it would
        // replace a local file the user may have edited since. Restore brings back what
        // is away; it does not overwrite what is here.
        if member
            .locals
            .iter()
            .any(|c| crate::volume_health::candidate_exists(&c.abs))
        {
            continue;
        }
        let frame_id = member.photo_id;
        match resolve_restore_member(member, &base, volume_id) {
            Ok(plan) => frames.push(plan),
            Err(CatalogError::Validation(why)) => skipped.push(SkippedPhoto::new(frame_id, why)),
            Err(e) => return Err(e),
        }
    }
    Ok(RestorePlan { named, frames, skipped, total })
}

fn resolve_restore_member(
    member: RestoreMember,
    base: &str,
    volume_id: i64,
) -> Result<PhotoRestore> {
    let backup = first_present(member.backups)
        .ok_or_else(|| CatalogError::Validation("no reachable backup to restore".into()))?;
    // A copy row implies a photos row, so `rel` is present whenever a source was found.
    let rel = member
        .rel
        .ok_or_else(|| CatalogError::Validation("photo row vanished while planning".into()))?;
    Ok(PhotoRestore {
        photo_id: member.photo_id,
        source: backup.abs,
        dest: Path::new(base).join(&rel),
        rel,
        volume_id,
        expected_hash: backup.verified_hash,
    })
}

/// The "is one actually there?" half of [`Catalog::has_verified_backup`]. Stats — call
/// it with no catalog lock held.
pub fn any_backup_present(candidates: &[PathBuf]) -> bool {
    candidates
        .iter()
        .any(|p| crate::volume_health::candidate_exists(p))
}

/// Keep the candidates whose verified backup AND local copy are both actually on disk —
/// the stat half of [`Catalog::photos_eligible_for_offload`]. The backup requirement
/// means a photo is never freed unless its NAS copy is confirmed there — and only when
/// the NAS is reachable (an unreachable one confirms nothing), so the sweep never runs
/// blind. Stats — call it with no catalog lock held.
pub fn filter_offload_eligible(candidates: Vec<OffloadEligibility>) -> Vec<i64> {
    candidates
        .into_iter()
        .filter(|c| {
            any_backup_present(&c.verified_backups)
                && c.locals
                    .iter()
                    .any(|p| crate::volume_health::candidate_exists(p))
        })
        .map(|c| c.photo_id)
        .collect()
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

/// The result of one lifecycle copy: the verified hash **and** the companions that
/// travelled with it.
///
/// One value rather than two returns, because recording half of a copy is exactly what
/// issue #80 was — the image reached home, the edit state did not, and the app said
/// "backed up". [`Catalog::record_copy`] takes this whole thing, so there is no shape in
/// which a caller records a location and forgets what came with it.
#[derive(Debug, Clone)]
pub struct CopyOutcome {
    /// SHA-256 of the image, verified after the copy.
    pub hash: String,
    /// Companions now confirmed present and identical at the destination.
    pub carried: Vec<CarriedCompanion>,
    /// Companions that exist on both sides with different contents, left untouched.
    pub diverged: Vec<PathBuf>,
}

/// Copy an image to `dest` and carry its declared companions with it, all verified.
///
/// The one IO half of every lifecycle copy — backup, restore, and the carry an offload does
/// before it frees anything. Pure file work: no catalog lock, so it runs on a blocking
/// worker like the rest of this module.
pub fn copy_with_companions(
    src: &Path,
    dest: &Path,
    expected: Option<&str>,
) -> Result<CopyOutcome> {
    let hash = copy_and_verify(src, dest, expected)?;
    let carry = carry_companions(src, dest)?;
    Ok(CopyOutcome { hash, carried: carry.carried, diverged: carry.diverged })
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

/// Free the local copies of everything the plan covers: the named photo, then each frame.
///
/// The named photo's refusal is the call's refusal — the user pressed the button on that
/// tile. A **frame** that refuses is recorded and left local instead: aborting would not
/// un-free what is already gone, and it would hide which frame objected.
pub fn verify_and_delete_locals(plan: &OffloadPlan) -> Result<OffloadCarry> {
    verify_and_delete_locals_inner(plan, None)
}

pub fn verify_and_delete_locals_abortable(plan: &OffloadPlan, abort: &AtomicBool) -> Result<OffloadCarry> {
    verify_and_delete_locals_inner(plan, Some(abort))
}

fn verify_and_delete_locals_inner(plan: &OffloadPlan, abort: Option<&AtomicBool>) -> Result<OffloadCarry> {
    let mut out = OffloadCarry {
        freed: Vec::new(),
        skipped: plan.skipped.clone(),
        total: plan.total,
        sidecar_backups_left: 0,
    };
    let (freed, left_behind) = free_local_copies(&plan.named, abort)?;
    out.freed.push(freed);
    out.sidecar_backups_left += left_behind;
    for frame in &plan.frames {
        match free_local_copies(frame, abort) {
            Ok((freed, left_behind)) => {
                out.freed.push(freed);
                out.sidecar_backups_left += left_behind;
            }
            Err(e) => out.skipped.push(SkippedPhoto::new(frame.photo_id, e.to_string())),
        }
    }
    Ok(out)
}

/// Re-verify one photo's backup hash, carry its local companions home, then delete its
/// local files. Invariant 3: never delete a local copy unless the backup is present and
/// still hashes to the recorded value.
///
/// Returns what is now confirmed at home — for the caller to record *before*
/// `commit_offload`, since that drops the local location rows and the companion rows
/// cascade with them — and how many sidecar backups were left beside the freed files.
fn free_local_copies(photo: &PhotoOffload, abort: Option<&AtomicBool>) -> Result<(FreedPhoto, usize)> {
    if abort.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        return Err(CatalogError::Validation("storage operation superseded or catalog switched".into()));
    }
    let current = sha256_file(&photo.backup_abs)?;
    if current != photo.expected_hash {
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
    for file in &photo.local_files {
        let carry = carry_companions(file, &photo.backup_abs)?;
        if let Some(first) = carry.diverged.first() {
            return Err(CatalogError::Validation(format!(
                "{} differs from the copy at home — refusing to offload",
                first.display()
            )));
        }
        carried.extend(carry.carried);
    }

    let mut sidecar_backups_left = 0usize;
    for file in &photo.local_files {
        if abort.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err(CatalogError::Validation("storage operation superseded or catalog switched".into()));
        }
        // Companions first: if this fails part-way, the image is still local, so the
        // photo is never left with its edit state gone and its bytes freed.
        for found in crate::companions::carried_beside(file) {
            std::fs::remove_file(&found.path).map_err(io)?;
        }
        // Counted, never carried and never deleted — see
        // `companions::sidecar_backups_beside`. Counting is what turns "the one file
        // offload left behind" from a bug report into something the verb says (#82).
        sidecar_backups_left += crate::companions::sidecar_backups_beside(file).len();
        // Best-effort: an already-absent local file is fine (goal is "not local").
        if file.exists() {
            std::fs::remove_file(file).map_err(io)?;
        }
    }
    Ok((
        FreedPhoto {
            photo_id: photo.photo_id,
            backup_location_id: photo.backup_location_id,
            local_volume_ids: photo.local_volume_ids.clone(),
            carried,
        },
        sidecar_backups_left,
    ))
}

fn io(e: std::io::Error) -> CatalogError {
    CatalogError::Io(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestTmpDir;
    use crate::volume_health::take_candidate_stats;

    /// A catalog rooted at `<tmp>/photos` with an attached "NAS" backup volume, plus one
    /// local photo already backed up to it. Returns the fixture dir (kept alive for
    /// cleanup), the local file, the photo id, and the NAS volume id.
    fn backed_up_photo(tag: &str) -> (Catalog, TestTmpDir, PathBuf, i64, i64) {
        let dir = TestTmpDir::new(&format!("lifecycle-{tag}"));
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let catalog = Catalog::open(&dir.join("t.chairphoto"), &root).unwrap();
        let nas_base = dir.join("nas");
        std::fs::create_dir_all(&nas_base).unwrap();
        let nas = catalog.add_volume("NAS", &nas_base, VolumeKind::Backup).unwrap();
        let raw = root.join("2026/08/DSC1.ARW");
        std::fs::create_dir_all(raw.parent().unwrap()).unwrap();
        std::fs::write(&raw, b"raw-bytes").unwrap();
        let id = catalog.upsert_photo(&raw, None, 1, 9).unwrap().id;
        catalog.backup_photo(id, nas).unwrap();
        (catalog, dir, raw, id, nas)
    }

    /// The split's contract (#85): every `*_candidates` half is PURE SQL — zero
    /// filesystem stats, so it is safe to run while the catalog mutex is held — and the
    /// `resolve_*`/`filter_*`/`any_*` halves are where every planner stat happens, off
    /// the lock. Counted rather than timed, like the resolver's own tests
    /// (`locations.rs`). The lock interleaving itself (another catalog reader proceeding
    /// while a plan stats a dead NAS) is not forced here; this pins the property that
    /// makes it safe.
    #[test]
    fn candidate_halves_are_pure_sql_and_the_resolve_halves_stat() {
        let (catalog, _dir, _raw, id, nas) = backed_up_photo("split-contract");
        // Age eligibility keys on capture time falling back to import time; backdate the
        // import so the age-0 sweep below sees the photo without waiting.
        catalog
            .conn
            .execute("UPDATE photos SET created_at = created_at - 86400 WHERE id = ?1", params![id])
            .unwrap();
        let local = catalog.ensure_default_volume().unwrap();

        let _ = take_candidate_stats();
        let backup = catalog.plan_backup_candidates(id, nas).unwrap();
        let offload = catalog.plan_offload_candidates(id).unwrap();
        let restore = catalog.plan_restore_candidates(id, local).unwrap();
        let gate = catalog.verified_backup_candidates(id).unwrap();
        let sweep = catalog.offload_eligibility_candidates(0).unwrap();
        assert_eq!(take_candidate_stats(), 0, "the rows halves must never touch the filesystem");

        resolve_backup_plan(backup).unwrap();
        assert!(take_candidate_stats() > 0, "backup decides its source by statting");
        resolve_offload_plan(offload).unwrap();
        assert!(take_candidate_stats() > 0, "offload's gate is a stat");
        resolve_restore_plan(restore).unwrap();
        assert!(take_candidate_stats() > 0, "restore decides its source by statting");
        assert!(any_backup_present(&gate));
        assert!(take_candidate_stats() > 0, "the idempotency gate is a stat");
        assert_eq!(filter_offload_eligible(sweep), vec![id]);
        assert!(take_candidate_stats() > 0, "the sweep confirms each candidate by statting");
    }

    /// Candidates are rows; decisions are files. A backup the catalog records but the
    /// disk no longer holds is still among the candidates — pure SQL cannot know — and
    /// the stat half is what refuses it. Refusal is also all a stale stat can do: before
    /// anything is deleted, [`free_local_copies`] re-hashes the backup itself (invariant
    /// 3), so an answer that goes stale the other way aborts there instead of freeing on
    /// old evidence.
    #[test]
    fn the_stat_half_refuses_a_backup_the_rows_still_promise() {
        let (catalog, dir, raw, id, _nas) = backed_up_photo("stale-backup");
        std::fs::remove_file(dir.join("nas/2026/08/DSC1.ARW")).unwrap();

        let candidates = catalog.plan_offload_candidates(id).unwrap();
        assert!(
            !candidates.named.verified_backups.is_empty(),
            "the rows half still lists the recorded backup"
        );
        let err = match resolve_offload_plan(candidates) {
            Ok(_) => panic!("a backup missing on disk must refuse the offload"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("no verified backup"), "refused by the stat half: {err}");
        assert!(raw.exists(), "a refusal never touches the local copy");
    }
}
