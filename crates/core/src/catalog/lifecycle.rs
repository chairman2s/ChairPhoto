//! Storage lifecycle (E3): backup local→backup volume, offload (free local space),
//! and restore (pull a backup back to local) — all SHA-256 hash-verified, enforcing
//! the non-negotiable safety invariants in docs/storage-and-import.md:
//!
//!   1. Never delete the last copy. 2. Never offload without a verified backup.
//!   3. Hash-verify the backup before deleting anything local, and the local copy against
//!      it (#255): a local file that moved on since its backup is never deleted.
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
use crate::scanner::same_photo::Placed;
use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedPhoto {
    pub photo_id: i64,
    /// What the user reads: why this member was left.
    pub reason: String,
    /// What the code acts on (#256): whether the member refused, or was interrupted or busy
    /// and is retried. Never derived from `reason`'s wording.
    pub kind: SkipKind,
}

/// Why a stack member was left, as the code needs to know it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SkipKind {
    /// It refused on its own account (no verified backup, a changed local copy, …): a
    /// queued op records it failed, with the reason.
    Refused,
    /// Its run stopped being the owner ([`SUPERSEDED_REASON`]).
    Superseded,
    /// Another storage operation held it ([`IN_PROGRESS_REASON`]).
    InProgress,
}

/// Why a member was left because its run stopped being the owner (a catalog switch or a
/// newer drain tripped its claim) — an interruption, not a failure of that member.
pub const SUPERSEDED_REASON: &str = "storage operation superseded or catalog switched";

/// Why a storage verb refused, or left a stack member, that another storage operation is
/// working on right now (#254: `app::storage::StorageClaims`). Like a superseded member, a
/// queued one is requeued as pending work: the photo did not refuse, it was busy.
pub const IN_PROGRESS_REASON: &str = "a storage operation on this photo is already in progress";

impl SkippedPhoto {
    /// A member that refused on its own account, for `reason`.
    pub fn refused(photo_id: i64, reason: impl Into<String>) -> Self {
        Self { photo_id, reason: reason.into(), kind: SkipKind::Refused }
    }

    /// A member left because its run was superseded.
    pub fn interrupted(photo_id: i64) -> Self {
        Self { photo_id, reason: SUPERSEDED_REASON.into(), kind: SkipKind::Superseded }
    }

    /// A member left because another storage operation held it.
    pub fn busy(photo_id: i64) -> Self {
        Self { photo_id, reason: IN_PROGRESS_REASON.into(), kind: SkipKind::InProgress }
    }

    /// Whether this member was left only because the run was superseded. Such a member is
    /// requeued as pending work, never recorded as failed: nothing about it failed, and a
    /// failed row is one the reconcile check (`reconcile_due`) and the queue chip do not
    /// count, so the work would never be retried by itself.
    pub fn superseded(&self) -> bool {
        self.kind == SkipKind::Superseded
    }

    /// Whether this member was left only because another storage operation held it.
    pub fn in_progress(&self) -> bool {
        self.kind == SkipKind::InProgress
    }

    /// Whether a queued op should retry this member as pending work rather than record it
    /// failed: it was interrupted ([`Self::superseded`]) or busy ([`Self::in_progress`]),
    /// not refused on its own account.
    pub fn retry_later(&self) -> bool {
        self.superseded() || self.in_progress()
    }
}

/// A catalog error as a storage verb's user reads it (#256): a refusal's own words, without
/// the "invalid input:" every validation error carries in its `Display`. Other errors keep
/// theirs ("io error: …", "database error: …"), which say what kind of failure it was.
pub fn user_reason(e: &CatalogError) -> String {
    match e {
        CatalogError::Validation(why) => why.clone(),
        other => other.to_string(),
    }
}

/// A path as a refusal names it: its file name. The user knows the photo; the folder is
/// noise in a status line, and an absolute path in a queued op's error says more about the
/// machine than about the problem.
fn name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
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
    /// The `photo_locations.id` of each local row the plan read — exactly the rows
    /// [`Catalog::commit_offload`] drops once the files are gone (#254).
    pub local_location_ids: Vec<i64>,
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
    pub local_location_ids: Vec<i64>,
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
                Err(e) => report.skipped.push(SkippedPhoto::refused(frame.photo_id, user_reason(&e))),
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
    pub fn stack_frame_ids(&self, photo_id: i64) -> Result<Vec<i64>> {
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
    ///
    /// Exactly the rows the offload planned from, by id — never "every row on the local
    /// volume": a row another writer added after the plan (a restore bringing the photo
    /// back) describes a file this offload never saw, and dropping it would leave that file
    /// with nothing pointing at it (#254). Storage ops on one photo are also serialised
    /// (`app::storage::StorageClaims`); this keeps the commit honest without relying on it.
    pub fn commit_offload(&self, photo_id: i64, local_location_ids: &[i64]) -> Result<()> {
        for &id in local_location_ids {
            self.conn.execute(
                "DELETE FROM photo_locations WHERE id = ?1 AND photo_id = ?2",
                params![id, photo_id],
            )?;
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
            self.commit_offload(freed.photo_id, &freed.local_location_ids)?;
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
                Err(e) => report.skipped.push(SkippedPhoto::refused(frame.photo_id, user_reason(&e))),
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

/// Recover what a crashed offload left beside the local copies a plan is about to look at
/// (`working_files::sweep_once`): a local file it had moved to a hidden name is put back
/// first, so the plan finds it where the catalog says it is. Once per folder per run.
fn sweep_local_folders<'a>(locals: impl Iterator<Item = &'a Vec<Copy>>) {
    super::working_files::sweep_beside(locals.flatten().map(|c| c.abs.as_path()));
}

/// The "which of these exists?" half of [`Catalog::plan_backup_candidates`]. Stats the
/// filesystem — call it from a blocking context with NO catalog lock held; everything it
/// needs travels inside the candidates.
pub fn resolve_backup_plan(candidates: BackupCandidates) -> Result<BackupPlan> {
    let BackupCandidates { named, frames: members, total, base, volume_id } = candidates;
    sweep_local_folders(std::iter::once(&named).chain(&members).map(|m| &m.locals));
    let named = resolve_backup_member(named, &base, volume_id)?;
    let mut frames = Vec::new();
    let mut skipped = Vec::new();
    for member in members {
        let frame_id = member.photo_id;
        match resolve_backup_member(member, &base, volume_id) {
            Ok(plan) => frames.push(plan),
            // A frame's own missing copy is a skip, not the master's failure. Anything
            // else is still an error.
            Err(CatalogError::Validation(why)) => skipped.push(SkippedPhoto::refused(frame_id, why)),
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
    sweep_local_folders(std::iter::once(&named).chain(&members).map(|m| &m.locals));
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
            Err(CatalogError::Validation(why)) => skipped.push(SkippedPhoto::refused(frame_id, why)),
            Err(e) => return Err(e),
        }
    }
    Ok(OffloadPlan { named, frames, skipped, total })
}

fn resolve_offload_member(member: OffloadMember) -> Result<PhotoOffload> {
    let backup = first_present(member.verified_backups).ok_or_else(|| {
        CatalogError::Validation("no verified backup — refusing to offload".into())
    })?;
    let local_location_ids = member.locals.iter().map(|c| c.location_id).collect();
    Ok(PhotoOffload {
        photo_id: member.photo_id,
        backup_location_id: backup.location_id,
        backup_abs: backup.abs,
        expected_hash: backup.verified_hash.unwrap_or_default(),
        local_files: member.locals.into_iter().map(|c| c.abs).collect(),
        local_location_ids,
    })
}

/// The "which of these exists?" half of [`Catalog::plan_restore_candidates`], statted
/// with NO catalog lock held.
pub fn resolve_restore_plan(candidates: RestoreCandidates) -> Result<RestorePlan> {
    let RestoreCandidates { named, frames: members, total, base, volume_id } = candidates;
    sweep_local_folders(std::iter::once(&named).chain(&members).map(|m| &m.locals));
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
            Err(CatalogError::Validation(why)) => skipped.push(SkippedPhoto::refused(frame_id, why)),
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
    /// The local file that was carried — what an offload deletes once it is confirmed home.
    pub source: PathBuf,
    /// SHA-256 of the bytes confirmed identical on both sides at carry time. An offload
    /// re-hashes the local file against it just before deleting (#255): a companion
    /// rewritten after it was carried is newer than what is at home and must not be freed.
    pub hash: String,
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
        let hash = if dest.is_file() {
            let source_hash = sha256_file(&found.path)?;
            if sha256_file(&dest)? != source_hash {
                out.diverged.push(found.path.clone());
                continue;
            }
            source_hash
        } else {
            copy_and_verify(&found.path, &dest, None)?
        };
        out.carried.push(CarriedCompanion {
            name: found.name(),
            source_mtime: mtime_secs(&found.path)?,
            source: found.path.clone(),
            hash,
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
///
/// **Concurrent-writer safe (#254):** each call writes its own temp file
/// (`.<name>.chairphoto-part-<pid>-<n>`, created exclusively), so two copies to one
/// destination never write into the same file; and the verified temp is given its name
/// **without replacing** whatever is there (`renameat2(RENAME_NOREPLACE)`, with the same
/// hard-link and exclusive-create fallbacks the import uses —
/// `scanner::same_photo::place_no_replace`). A destination that already exists is accepted
/// only if it hashes to the source's hash (another writer placed the same bytes, or they
/// were already there); otherwise the copy fails and that file is left untouched.
///
/// **On a filesystem with neither (exFAT, FAT)** the destination is claimed by an exclusive
/// create and the verified temp copied into it — a second copy, so it is hashed again
/// (#256); one that does not match is removed (this call created it) and the copy fails.
/// A crash during that second copy can leave a short file at `dst`, which a later copy then
/// refuses as "already exists with different contents" until it is removed by hand; on
/// every other filesystem a crash leaves at most the hidden temp file. A temp file whose
/// process is no longer running is removed by the next copy into its folder
/// (`working_files::sweep_once`).
pub fn copy_and_verify(src: &Path, dst: &Path, expected: Option<&str>) -> Result<String> {
    copy_and_verify_with(src, dst, expected, &mut || {})
}

/// [`copy_and_verify`], calling `opened` once this call's temp file exists and before any
/// byte is written to it — where a test runs a second writer to the same destination.
pub(crate) fn copy_and_verify_with(
    src: &Path,
    dst: &Path,
    expected: Option<&str>,
    opened: &mut dyn FnMut(),
) -> Result<String> {
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
        super::working_files::sweep_once(parent);
    }
    // Hidden (dot-prefixed, so scans skip it), unique to this call, and on the same
    // filesystem so the final step is one rename.
    let (part, file) = crate::scanner::same_photo::create_part(dst).map_err(io)?;
    opened();
    let copy_verify_place = |mut file: std::fs::File| -> Result<()> {
        {
            let mut input = std::fs::File::open(src).map_err(io)?;
            std::io::copy(&mut input, &mut file).map_err(io)?;
            file.set_permissions(input.metadata().map_err(io)?.permissions()).map_err(io)?;
            file.sync_all().map_err(io)?;
        }
        drop(file);
        if sha256_file(&part)? != src_hash {
            return Err(CatalogError::Validation(
                "copy verification failed (hash mismatch)".into(),
            ));
        }
        match crate::scanner::same_photo::place_no_replace_reporting(&part, dst) {
            Ok(Placed::InOneStep) => Ok(()),
            // A second copy, made without a no-replace rename or hard links: verify it
            // too. It is this call's own file (created exclusively), so a bad one goes.
            Ok(Placed::Copied) => match sha256_file(dst) {
                Ok(hash) if hash == src_hash => Ok(()),
                _ => {
                    std::fs::remove_file(dst).ok();
                    Err(CatalogError::Validation(
                        "copy verification failed at its destination (hash mismatch)".into(),
                    ))
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if sha256_file(dst)? == src_hash {
                    std::fs::remove_file(&part).ok();
                    Ok(())
                } else {
                    Err(CatalogError::Validation(format!(
                        "{} already exists with different contents — left untouched",
                        name(dst)
                    )))
                }
            }
            Err(e) => Err(io(e)),
        }
    };
    if let Err(e) = copy_verify_place(file) {
        std::fs::remove_file(&part).ok(); // clean up our temp; never touch dst
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
    verify_and_delete_locals_until(plan, &|| false)
}

/// [`verify_and_delete_locals`] under a claim: `abort` is re-checked before every member's
/// delete (and between its files). A tripped claim before the named photo frees nothing and
/// fails; a frame it reaches tripped is skipped with [`SUPERSEDED_REASON`].
pub fn verify_and_delete_locals_abortable(plan: &OffloadPlan, abort: &AtomicBool) -> Result<OffloadCarry> {
    verify_and_delete_locals_until(plan, &|| abort.load(Ordering::Relaxed))
}

/// The body of both, with the ownership check as a closure — so a test can trip it at an
/// exact point (after the named photo is freed, before a frame).
pub(crate) fn verify_and_delete_locals_until(plan: &OffloadPlan, stopped: &dyn Fn() -> bool) -> Result<OffloadCarry> {
    let mut out = OffloadCarry {
        freed: Vec::new(),
        skipped: plan.skipped.clone(),
        total: plan.total,
        sidecar_backups_left: 0,
    };
    let (freed, left_behind) = free_local_copies(&plan.named, stopped).map_err(Stop::into_error)?;
    out.freed.push(freed);
    out.sidecar_backups_left += left_behind;
    for frame in &plan.frames {
        match free_local_copies(frame, stopped) {
            Ok((freed, left_behind)) => {
                out.freed.push(freed);
                out.sidecar_backups_left += left_behind;
            }
            Err(Stop::Superseded) => out.skipped.push(SkippedPhoto::interrupted(frame.photo_id)),
            Err(Stop::Refused(e)) => out.skipped.push(SkippedPhoto::refused(frame.photo_id, user_reason(&e))),
        }
    }
    Ok(out)
}

/// Why one member's offload stopped: its run was superseded, or the member refused. Typed so
/// the caller tells them apart without reading the reason's words (#256).
enum Stop {
    Superseded,
    Refused(CatalogError),
}

impl From<CatalogError> for Stop {
    fn from(e: CatalogError) -> Self {
        Stop::Refused(e)
    }
}

impl Stop {
    /// As the named photo's error: the call's own failure.
    fn into_error(self) -> CatalogError {
        match self {
            Stop::Superseded => CatalogError::Validation(SUPERSEDED_REASON.into()),
            Stop::Refused(e) => e,
        }
    }
}

/// Re-verify one photo's backup hash, carry its local companions home, then free its local
/// files — each only once it is confirmed to hold what home holds. Invariant 3: never delete
/// a local copy unless the backup is present and still hashes to the recorded value.
///
/// Returns what is now confirmed at home — for the caller to record *before*
/// `commit_offload`, since that drops the local location rows and the companion rows
/// cascade with them — and how many sidecar backups were left beside the freed files.
fn free_local_copies(photo: &PhotoOffload, stopped: &dyn Fn() -> bool) -> std::result::Result<(FreedPhoto, usize), Stop> {
    if stopped() {
        return Err(Stop::Superseded);
    }
    let current = sha256_file(&photo.backup_abs)?;
    if current != photo.expected_hash {
        return Err(CatalogError::Validation(
            "backup hash changed — refusing to offload".into(),
        )
        .into());
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
                name(first)
            ))
            .into());
        }
        carried.extend(carry.carried);
    }
    // The carry can take a while over a NAS; a run taken over meanwhile stops before
    // anything is moved.
    if stopped() {
        return Err(Stop::Superseded);
    }
    let to_free = companions_to_free(photo, &carried)?;

    let mut aside = Aside::default();
    if let Err(e) = move_aside_confirmed(photo, &to_free, &carried, stopped, &mut aside) {
        return Err(aside.put_back(e.into()));
    }
    // Every file is now confirmed and under a hidden name; nothing has been deleted. Unlink
    // them. A failure here puts back whatever is left.
    if let Err(e) = aside.unlink() {
        return Err(aside.put_back(e.into()));
    }

    // Counted, never carried and never deleted — see `companions::sidecar_backups_beside`.
    // Counting is what turns "the one file offload left behind" from a bug report into
    // something the verb says (#82).
    let sidecar_backups_left = photo
        .local_files
        .iter()
        .map(|file| crate::companions::sidecar_backups_beside(file).len())
        .sum();
    Ok((
        FreedPhoto {
            photo_id: photo.photo_id,
            backup_location_id: photo.backup_location_id,
            local_location_ids: photo.local_location_ids.clone(),
            carried,
        },
        sidecar_backups_left,
    ))
}

/// The check that makes offload delete only what home holds byte for byte (#255), made on
/// each file **after** it has been moved to a hidden name (#256): companions first, then the
/// image, for every local copy. Each is moved aside, re-hashed — the image against the
/// verified backup's hash, a companion against the hash the carry confirmed at home — and
/// kept aside only if it matches. Then every name that was emptied is looked at again.
///
/// Re-hashing the backup (invariant 3) proves home is intact, not that home holds what is
/// here: a JPEG or DNG rewritten in place by another tool after its backup, or a sidecar
/// edited after the carry, would otherwise be deleted while home kept the older bytes.
///
/// Checking the moved file rather than the named one is what closes the window between the
/// check and the delete. A write through the photo's name either landed before the move —
/// the file moved aside holds it, and its hash says so — or comes after it, and then makes a
/// new file at that name, which the final look finds and keeps. What is deleted afterwards is
/// the hidden file, which no other writer knows by name. (A writer that already had the file
/// open can still write into it after the move; ChairPhoto's own sidecar writes never do —
/// they replace the file by a rename — and a photo's IPTC write holds the photo's storage
/// claim, `app::storage::StorageClaims`.)
///
/// The cost is one sequential read of each local copy — a local disk, usually far faster
/// than the NAS read of the backup that offload already makes — plus companions of a few KB.
/// Size or mtime would be cheaper and are not content checks: an in-place rewrite can keep
/// the size, and a tool may preserve the mtime (`exiftool -P`).
fn move_aside_confirmed(
    photo: &PhotoOffload,
    to_free: &[Vec<PathBuf>],
    carried: &[CarriedCompanion],
    stopped: &dyn Fn() -> bool,
    aside: &mut Aside,
) -> std::result::Result<(), Stop> {
    for (file, companions) in photo.local_files.iter().zip(to_free) {
        // Companions first: if this stops part-way, the image is still local, so the photo
        // is never left with its edit state gone and its bytes freed. Exactly the ones
        // listed before — never a fresh look, which could take one that appeared after the
        // carry and was never carried.
        for companion in companions {
            if stopped() {
                return Err(Stop::Superseded);
            }
            let hash = carried.iter().find(|c| &c.source == companion).map(|c| c.hash.as_str()).unwrap_or_default();
            aside.take(companion, hash, "changed since it was carried home — refusing to offload")?;
        }
        if stopped() {
            return Err(Stop::Superseded);
        }
        // An already-absent local file is fine: the goal is "not local".
        aside.take(file, &photo.expected_hash, LOCAL_CHANGED_REASON)?;
    }
    // A name emptied above that holds a file again was written after its file was moved: a
    // newer image, or a companion nobody carried. It is kept, and so is everything else.
    for file in &photo.local_files {
        let appeared = std::iter::once(file.clone())
            .filter(|f| std::fs::symlink_metadata(f).is_ok())
            .chain(crate::companions::carried_beside(file).into_iter().map(|c| c.path))
            .next();
        if let Some(path) = appeared {
            return Err(CatalogError::Validation(format!(
                "{} was written while the photo was being offloaded — refusing to offload",
                name(&path)
            ))
            .into());
        }
    }
    Ok(())
}

/// The local files an offload has moved to hidden names and confirmed
/// ([`move_aside_confirmed`]): each one's own name and its hidden one.
#[derive(Default)]
struct Aside {
    moved: Vec<(PathBuf, PathBuf)>,
}

impl Aside {
    /// Move `file` aside and confirm it hashes to `expected`; a file that is not there is
    /// nothing to free. On a mismatch the file is put back and the offload refused with
    /// `why` — or, if a new file took its name meanwhile, kept under its hidden name, which
    /// the refusal names: those bytes are not at home.
    fn take(&mut self, file: &Path, expected: &str, why: &str) -> Result<()> {
        #[cfg(test)]
        offload_hook::step(offload_hook::Step::BeforeMove, file);
        let Some(hidden) = super::working_files::move_aside(file).map_err(io)? else {
            return Ok(());
        };
        #[cfg(test)]
        offload_hook::step(offload_hook::Step::AfterMove, file);
        let matches = sha256_file(&hidden).map(|h| h == expected);
        if matches.as_ref().is_ok_and(|m| *m) {
            self.moved.push((file.to_path_buf(), hidden));
            return Ok(());
        }
        let refusal = match matches {
            Ok(_) => format!("{} {why}", name(file)),
            Err(e) => format!("{} could not be read back ({}) — refusing to offload", name(file), user_reason(&e)),
        };
        Err(CatalogError::Validation(match super::working_files::put_back(&hidden, file) {
            Ok(true) => refusal,
            _ => format!("{refusal}; its bytes were kept beside it as {} because its name was taken", name(&hidden)),
        }))
    }

    /// Delete every confirmed file. Stops at the first that cannot be deleted.
    fn unlink(&mut self) -> Result<()> {
        while let Some((file, hidden)) = self.moved.pop() {
            if let Err(e) = std::fs::remove_file(&hidden) {
                self.moved.push((file, hidden));
                return Err(io(e));
            }
        }
        Ok(())
    }

    /// Undo [`Self::take`] for everything still moved, newest first, and return `refusal`.
    /// A confirmed file whose name a new file took is at home byte for byte, so it is
    /// deleted rather than left hidden; one that cannot be put back for any other reason
    /// is named in the refusal.
    fn put_back(&mut self, refusal: Stop) -> Stop {
        let mut kept = Vec::new();
        while let Some((file, hidden)) = self.moved.pop() {
            match super::working_files::put_back(&hidden, &file) {
                Ok(true) => {}
                Ok(false) if std::fs::symlink_metadata(&file).is_ok() => {
                    let _ = std::fs::remove_file(&hidden);
                }
                _ => kept.push(name(&hidden)),
            }
        }
        if kept.is_empty() {
            return refusal;
        }
        let refusal = user_reason(&refusal.into_error());
        Stop::Refused(CatalogError::Validation(format!("{refusal}; could not put back {}", kept.join(", "))))
    }
}

/// Which companions beside each local file an offload frees with it: exactly the ones the
/// carry confirmed at home. One the carry did not take (it appeared after the carry) refuses
/// the photo. Content is checked later, on the moved file ([`move_aside_confirmed`]).
fn companions_to_free(photo: &PhotoOffload, carried: &[CarriedCompanion]) -> Result<Vec<Vec<PathBuf>>> {
    let mut to_free = Vec::with_capacity(photo.local_files.len());
    for file in &photo.local_files {
        let mut companions = Vec::new();
        for found in crate::companions::carried_beside(file) {
            if !carried.iter().any(|c| c.source == found.path) {
                return Err(CatalogError::Validation(format!(
                    "{} appeared after its companions were carried home — refusing to offload",
                    name(&found.path)
                )));
            }
            companions.push(found.path);
        }
        to_free.push(companions);
    }
    Ok(to_free)
}

/// Why offload left a photo whose local copy no longer matches its verified backup (#255).
/// The backup still holds the earlier bytes, and Back up does not replace a verified backup
/// that is present, so the reason says what is true rather than offering a retry.
pub const LOCAL_CHANGED_REASON: &str =
    "changed since its backup — refusing to offload; the copy at home is the earlier version";

fn io(e: std::io::Error) -> CatalogError {
    CatalogError::Io(e.to_string())
}

/// Where a test acts inside an offload's per-file check ([`Aside::take`]): just before a local
/// file is moved aside, and just after. Per thread, so parallel tests never see each other's.
#[cfg(test)]
pub(crate) mod offload_hook {
    use std::cell::RefCell;
    use std::path::Path;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Step {
        BeforeMove,
        AfterMove,
    }

    type Hook = Box<dyn FnMut(Step, &Path)>;
    thread_local! {
        static HOOK: RefCell<Option<Hook>> = RefCell::new(None);
    }

    /// Run `hook` at every step of offloads on this thread until the guard is dropped.
    pub(crate) fn set(hook: impl FnMut(Step, &Path) + 'static) -> Guard {
        HOOK.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
        Guard
    }

    pub(crate) struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            HOOK.with(|h| h.borrow_mut().take());
        }
    }

    pub(super) fn step(step: Step, file: &Path) {
        let hook = HOOK.with(|h| h.borrow_mut().take());
        if let Some(mut hook) = hook {
            hook(step, file);
            HOOK.with(|h| {
                let mut slot = h.borrow_mut();
                if slot.is_none() {
                    *slot = Some(hook);
                }
            });
        }
    }
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

    // --- typed skip reasons (#256) -----------------------------------------------------------

    /// What a queued op does with a member is decided by its kind, never by its words: a
    /// refusal whose text happens to end like an interruption is still a refusal.
    #[test]
    fn a_skipped_members_kind_not_its_words_decides_a_retry() {
        let lookalike = SkippedPhoto::refused(1, format!("a tool wrote: {SUPERSEDED_REASON}"));
        assert!(!lookalike.superseded() && !lookalike.retry_later());
        let lookalike = SkippedPhoto::refused(1, format!("a tool wrote: {IN_PROGRESS_REASON}"));
        assert!(!lookalike.in_progress() && !lookalike.retry_later());
        assert!(SkippedPhoto::interrupted(1).superseded() && SkippedPhoto::interrupted(1).retry_later());
        assert!(SkippedPhoto::busy(1).in_progress() && SkippedPhoto::busy(1).retry_later());
        assert_eq!(SkippedPhoto::interrupted(1).reason, SUPERSEDED_REASON);
        assert_eq!(SkippedPhoto::busy(1).reason, IN_PROGRESS_REASON);
    }

    /// A frame an offload reaches after its run was superseded is skipped as interrupted —
    /// typed through the offload, not recovered from the error text.
    #[test]
    fn an_offload_frame_reached_after_a_trip_is_typed_interrupted() {
        let (catalog, dir, raw, id, nas) = backed_up_photo("typed-interrupted");
        let jpg = raw.with_extension("JPG");
        std::fs::write(&jpg, b"jpeg").unwrap();
        let frame = catalog.upsert_photo(&jpg, None, 1, 10).unwrap().id;
        catalog.set_stack_parent(frame, id).unwrap();
        catalog.backup_photo(id, nas).unwrap();
        let plan = catalog.plan_offload(id).unwrap();

        let carry = verify_and_delete_locals_until(&plan, &|| !raw.exists()).unwrap();

        assert_eq!(carry.skipped.len(), 1);
        assert_eq!((carry.skipped[0].photo_id, carry.skipped[0].kind), (frame, SkipKind::Superseded));
        assert!(jpg.exists());
        let _ = dir;
    }

    // --- two writers, one destination (#254) ----------------------------------------------

    /// The temp files left in `dir`.
    fn parts_in(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.contains("chairphoto-part"))
            .collect()
    }

    /// **Forced interleaving.** A second copy to the same destination runs start to finish
    /// after the first has opened its temp file and before it has written a byte. With one
    /// shared temp name the first writer's bytes went into the file the second had already
    /// renamed onto the destination; here each has its own temp, the second's verified file
    /// is what the destination holds, and the first — whose bytes differ — refuses rather
    /// than replace it.
    #[test]
    fn a_second_copy_to_one_destination_never_shares_or_replaces_the_first() {
        let dir = TestTmpDir::new("lifecycle-two-writers");
        let (a, b, dst) = (dir.join("a.xmp"), dir.join("b.xmp"), dir.join("home/DSC1.ARW.xmp"));
        std::fs::write(&a, b"the first writer's longer history").unwrap();
        std::fs::write(&b, b"second").unwrap();
        let mut second = None;

        let first = copy_and_verify_with(&a, &dst, None, &mut || {
            second = Some(copy_and_verify(&b, &dst, None));
        });

        let second = second.expect("the second writer ran inside the first");
        assert_eq!(second.unwrap(), sha256_file(&b).unwrap(), "the second copy reported success");
        assert_eq!(std::fs::read(&dst).unwrap(), b"second", "and the destination holds exactly its bytes");
        let err = first.expect_err("the first must not replace a verified file it did not write").to_string();
        assert!(err.contains("already exists with different contents"), "{err}");
        assert!(parts_in(&dir.join("home")).is_empty(), "no temp left: {:?}", parts_in(&dir.join("home")));
    }

    /// The same race with identical bytes (two carries of one companion) is not a failure:
    /// the destination already holds what the first writer verified.
    #[test]
    fn a_second_copy_of_the_same_bytes_is_accepted() {
        let dir = TestTmpDir::new("lifecycle-two-writers-same");
        let (src, dst) = (dir.join("a.xmp"), dir.join("home/DSC1.ARW.xmp"));
        std::fs::write(&src, b"history").unwrap();
        let mut second = None;

        let first = copy_and_verify_with(&src, &dst, None, &mut || second = Some(copy_and_verify(&src, &dst, None)));

        assert!(second.unwrap().is_ok() && first.is_ok());
        assert_eq!(std::fs::read(&dst).unwrap(), b"history");
        assert!(parts_in(&dir.join("home")).is_empty());
    }

    // --- the copy fallback on a filesystem without no-replace rename or links (#256) --------

    /// exFAT/FAT: the verified temp is copied a second time into the destination. That copy
    /// is hashed too; one that does not match (damaged here by the test hook) is removed —
    /// it is this call's own file — and the copy fails, leaving nothing behind. An intact one
    /// is accepted, also leaving no temp file.
    #[test]
    fn a_copy_placed_by_the_copy_fallback_is_verified_at_its_destination() {
        let dir = TestTmpDir::new("lifecycle-copy-fallback");
        let (src, dst) = (dir.join("DSC1.ARW"), dir.join("home/DSC1.ARW"));
        std::fs::write(&src, b"raw-bytes").unwrap();
        let copies = std::rc::Rc::new(std::cell::Cell::new(0));
        let damaged = {
            let copies = copies.clone();
            crate::scanner::same_photo::copy_fallback::force(move |to| {
                copies.set(copies.get() + 1);
                std::fs::write(to, b"raw-").unwrap(); // a copy cut short
            })
        };

        let err = copy_and_verify(&src, &dst, None).expect_err("a short copy must not pass").to_string();

        drop(damaged);
        assert_eq!(copies.get(), 1, "the fallback was taken");
        assert!(err.contains("at its destination"), "{err}");
        assert!(!dst.exists(), "the bad copy, this call's own, is gone");
        assert!(parts_in(&dir.join("home")).is_empty());

        let _intact = crate::scanner::same_photo::copy_fallback::force(|_| {});
        assert_eq!(copy_and_verify(&src, &dst, None).unwrap(), sha256_file(&src).unwrap());
        assert_eq!(std::fs::read(&dst).unwrap(), b"raw-bytes");
        assert!(parts_in(&dir.join("home")).is_empty());
    }

    /// A temp file a crashed run left in a destination folder is removed by the next copy
    /// into that folder; the new copy's own temp never is.
    #[test]
    fn a_copy_sweeps_a_crashed_runs_temp_from_its_folder() {
        if !cfg!(target_os = "linux") {
            println!("SKIPPED: a_copy_sweeps_a_crashed_runs_temp_from_its_folder — needs /proc");
            return;
        }
        let dir = TestTmpDir::new("lifecycle-copy-sweep");
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let stale = home.join(format!(
            ".DSC0.ARW.chairphoto-part-{}-0",
            super::super::working_files::dead_pid()
        ));
        std::fs::write(&stale, b"half a copy").unwrap();
        let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 60 * 60);
        std::fs::File::options().write(true).open(&stale).unwrap().set_modified(hour_ago).unwrap();
        std::fs::write(dir.join("DSC1.ARW"), b"raw").unwrap();

        copy_and_verify(&dir.join("DSC1.ARW"), &home.join("DSC1.ARW"), None).unwrap();

        assert!(!stale.exists());
        assert!(parts_in(&home).is_empty());
    }

    /// Back up through the fallback with a copy that comes out wrong: nothing is recorded,
    /// so the photo is not taken as backed up (and offload stays refused).
    #[test]
    fn a_backup_whose_fallback_copy_is_wrong_is_not_recorded() {
        let dir = TestTmpDir::new("lifecycle-copy-fallback-backup");
        let root = dir.join("photos");
        std::fs::create_dir_all(root.join("2026/08")).unwrap();
        let catalog = Catalog::open(&dir.join("t.chairphoto"), &root).unwrap();
        std::fs::create_dir_all(dir.join("nas")).unwrap();
        let nas = catalog.add_volume("NAS", &dir.join("nas"), VolumeKind::Backup).unwrap();
        let raw = root.join("2026/08/DSC1.ARW");
        std::fs::write(&raw, b"raw-bytes").unwrap();
        let id = catalog.upsert_photo(&raw, None, 1, 9).unwrap().id;
        let _damaged = crate::scanner::same_photo::copy_fallback::force(|to| std::fs::write(to, b"x").unwrap());

        assert!(catalog.backup_photo(id, nas).is_err());

        assert!(!catalog.has_verified_backup(id).unwrap());
        assert!(!dir.join("nas/2026/08/DSC1.ARW").exists());
        assert!(catalog.offload_photo(id).is_err() && raw.exists());
    }

    /// A location row added after the offload planned (a restore bringing the photo back)
    /// describes a file the offload never saw: the commit drops the rows it planned from,
    /// by id, and leaves that one.
    #[test]
    fn the_offload_commit_drops_only_the_rows_it_planned() {
        let (catalog, _dir, raw, id, _nas) = backed_up_photo("commit-planned-rows");
        let local = catalog.ensure_default_volume().unwrap();
        let plan = catalog.plan_offload(id).unwrap();
        let carry = verify_and_delete_locals(&plan).unwrap();
        assert!(!raw.exists());
        // A restore lands between the delete and the commit.
        catalog.add_location(id, local, "2026/08/DSC1.ARW", LocationRole::LocalCache).unwrap();

        catalog.commit_offload_carry(carry).unwrap();

        let roles: Vec<_> = catalog.photo_locations(id).unwrap().into_iter().map(|l| l.role).collect();
        assert!(roles.contains(&LocationRole::LocalCache), "the restore's row survived: {roles:?}");
        assert!(!roles.contains(&LocationRole::Primary), "the planned local row went: {roles:?}");
    }

    // --- offload deletes only what home holds byte for byte (#255) --------------------------

    /// Review probe P5: a local original rewritten after its backup (an external tool
    /// writing into a JPEG or DNG in place). The backup re-hashes fine — it is the local copy
    /// that moved on — so offload must refuse, delete nothing, and keep the rows.
    #[test]
    fn offload_refuses_a_local_copy_rewritten_after_its_backup() {
        let (catalog, dir, raw, id, _nas) = backed_up_photo("local-changed");
        std::fs::write(&raw, b"raw-bytes edited elsewhere").unwrap();

        let err = catalog.offload_photo(id).err().expect("a changed local copy must refuse").to_string();

        assert!(err.contains(LOCAL_CHANGED_REASON), "{err}");
        assert_eq!(std::fs::read(&raw).unwrap(), b"raw-bytes edited elsewhere", "the newer bytes stay");
        assert_eq!(std::fs::read(dir.join("nas/2026/08/DSC1.ARW")).unwrap(), b"raw-bytes");
        assert_eq!(catalog.photo_storage_status(id).unwrap(), super::super::StorageStatus::BackedUp);
    }

    /// A companion carried at backup time and edited locally since: the carry finds the two
    /// sides differ and refuses, as before #255.
    #[test]
    fn offload_refuses_a_companion_edited_after_its_backup() {
        let (catalog, dir, raw, id, nas) = backed_up_photo("companion-changed");
        let xmp = crate::companions::appended_path(&raw, "xmp");
        std::fs::write(&xmp, b"history v1").unwrap();
        catalog.backup_photo(id, nas).unwrap(); // carries it home
        std::fs::write(&xmp, b"history v2").unwrap();

        let err = catalog.offload_photo(id).err().expect("an edited companion must refuse").to_string();

        assert!(err.contains("differs from the copy at home"), "{err}");
        assert!(raw.exists() && xmp.exists());
        assert_eq!(std::fs::read(&xmp).unwrap(), b"history v2");
        assert_eq!(std::fs::read(dir.join("nas/2026/08/DSC1.ARW.xmp")).unwrap(), b"history v1");
    }

    /// **Forced interleaving.** A companion that offload itself carries home, then edited
    /// (or a new one written) in the window between the carry and the delete. The check made
    /// just before deleting catches both; nothing local is deleted.
    #[test]
    fn offload_refuses_a_companion_written_after_its_carry() {
        for (tag, late) in [("edited", "rrdata"), ("new", "pp3")] {
            let (catalog, _dir, raw, id, _nas) = backed_up_photo(&format!("companion-after-carry-{tag}"));
            let rrdata = crate::companions::appended_path(&raw, "rrdata");
            std::fs::write(&rrdata, b"masks v1").unwrap(); // not at home yet: offload carries it
            let home = _dir.join("nas/2026/08/DSC1.ARW.rrdata");
            let plan = catalog.plan_offload(id).unwrap();
            let wrote = std::cell::Cell::new(false);
            // `stopped` is consulted after the carry; the first time the carried file is at
            // home, write the late change.
            let after_carry = || {
                if home.exists() && !wrote.replace(true) {
                    std::fs::write(crate::companions::appended_path(&raw, late), b"written late").unwrap();
                }
                false
            };

            let err = match verify_and_delete_locals_until(&plan, &after_carry) {
                Ok(_) => panic!("{tag}: a companion written after the carry must refuse"),
                Err(e) => e.to_string(),
            };

            assert!(wrote.get(), "{tag}: the write landed after the carry");
            assert!(err.contains("refusing to offload"), "{tag}: {err}");
            assert!(raw.exists() && rrdata.exists(), "{tag}: nothing local was deleted");
            assert!(crate::companions::appended_path(&raw, late).exists());
        }
    }
}
