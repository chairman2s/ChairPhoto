//! The storage lifecycle's jobs — the bodies the GPUI app runs for the back-up
//! queue, reconcile-on-focus and the Trash dialog.
//!
//! Every function here **blocks** (file copies over a possibly slow mount, hashing, volume
//! stats): run it on a worker, never a UI thread. Each operation is plan-under-lock → file IO
//! off the lock → record-under-lock, so a NAS copy never holds the catalog lock.
//!
//! "Nothing ever leaves home" is binding here — see `docs/storage-and-import.md`.

use super::{now_secs, with_catalog, AppState};
use crate::catalog::{
    BackupReport, Catalog, DrainSummary, LocationRole, OffloadReport, PhotoBackup, PhotoRestore, RestoreReport,
    SkippedPhoto, VolumeKind,
};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Where a lifecycle step plans and records: the shared handle (`AppState` — whichever
/// catalog is open at each step) or one bound connection (a drain, which must never apply
/// one catalog's photo and operation ids to another's rows).
trait CatalogAccess {
    fn with<T>(&self, f: impl FnOnce(&Catalog) -> crate::catalog::Result<T>) -> Result<T, String>;
}

impl CatalogAccess for AppState {
    fn with<T>(&self, f: impl FnOnce(&Catalog) -> crate::catalog::Result<T>) -> Result<T, String> {
        with_catalog(self, f)
    }
}

impl CatalogAccess for Catalog {
    fn with<T>(&self, f: impl FnOnce(&Catalog) -> crate::catalog::Result<T>) -> Result<T, String> {
        f(self).map_err(|e| e.to_string())
    }
}

// ── Backup, offload, restore ─────────────────────────────────────────────────

/// Why a claimed stack step left a member: its drain or sweep is no longer the owner (a
/// catalog switch or a newer drain tripped the reconcile generation). The member is reported
/// skipped with this reason, so a drain requeues it rather than replaying the members that
/// finished.
const SUPERSEDED: &str = "storage operation superseded or catalog switched";

/// Whether the claim this step runs under (if any) has been taken over. Unclaimed steps — a
/// verb the user pressed, bound to its catalog by [`bound`] — run to the end.
fn superseded(abort: Option<&AtomicBool>) -> bool {
    abort.is_some_and(|flag| flag.load(Ordering::Relaxed))
}

/// Back a photo up to volume `backup_id` — **and the frames stacked under it** (#82): a
/// stack is how a burst is stored, so a tile is a moment rather than a file. The plan
/// carries the whole stack, so every caller (the inspector, the reconcile drain) inherits
/// the cascade rather than each deciding for itself.
///
/// The named photo's failure is the call's failure; a frame that fails is reported and the
/// rest continue, because the master is already at home by then.
pub fn backup_to(state: &AppState, photo_id: i64, backup_id: i64) -> Result<BackupReport, String> {
    backup_in(state, photo_id, backup_id, None)
}

/// `abort`: the reconcile claim a drain runs this under. The named photo — the op in flight,
/// started while the drain still owned it — runs to the end; once the claim is tripped no
/// further frame is started, and each is reported skipped with [`SUPERSEDED`].
fn backup_in(
    cat: &impl CatalogAccess,
    photo_id: i64,
    backup_id: i64,
    abort: Option<&AtomicBool>,
) -> Result<BackupReport, String> {
    let plan = cat.with(|c| c.plan_backup(photo_id, backup_id))?;
    let mut report = BackupReport { skipped: plan.skipped, total: plan.total, ..Default::default() };
    backup_one(cat, plan.named)?;
    report.backed_up.push(photo_id);
    for frame in plan.frames {
        let frame_id = frame.photo_id;
        let result = if superseded(abort) { Err(SUPERSEDED.to_string()) } else { backup_one(cat, frame) };
        match result {
            Ok(()) => report.backed_up.push(frame_id),
            Err(e) => report.skipped.push(SkippedPhoto { photo_id: frame_id, reason: e }),
        }
    }
    Ok(report)
}

/// Back up one photo — the named photo or one frame of its stack — idempotently. Split out
/// so the stack cascade applies exactly the same rules to a frame as to the master.
///
/// If a verified backup already exists the image is not re-copied (over a flaky mount that
/// risks a good backup), but its companions are reconciled: every backup made before
/// companions existed has a verified image and no carried sidecars (#80).
fn backup_one(cat: &impl CatalogAccess, plan: PhotoBackup) -> Result<(), String> {
    let PhotoBackup { photo_id, source, dest, rel, volume_id } = plan;
    if cat.with(|c| c.has_verified_backup(photo_id))? {
        let carried = crate::catalog::carry_companions(&source, &dest).map_err(|e| e.to_string())?;
        return cat.with(|c| {
            c.record_companions_at(photo_id, volume_id, LocationRole::Backup, &carried.carried)
        });
    }
    // A copy is the image plus its declared companions; `copy_with_companions` is the one
    // place that knows the set.
    let outcome = crate::catalog::copy_with_companions(&source, &dest, None).map_err(|e| e.to_string())?;
    cat.with(|c| c.record_copy(photo_id, volume_id, &rel, LocationRole::Backup, &outcome))
}

/// A connection of its own to the catalog `expected` names, if that is the open one —
/// otherwise `CATALOG_CHANGED`. For a one-photo lifecycle step a front end started on ids it
/// read from that catalog: its plan, file IO and record then all run against that catalog
/// even if a switch lands mid-copy (as a drain's do, [`ReconcileClaim`]), instead of
/// recording into whichever catalog is open by then.
fn bound(state: &AppState, expected: super::CatalogIdentity) -> Result<Catalog, String> {
    let (db, root) = super::with_catalog_as(state, expected, |c| Ok((c.db_path().to_path_buf(), c.root().to_path_buf())))?;
    Catalog::open_secondary(&db, &root).map_err(|e| e.to_string())
}

/// [`backup_photo`] of a photo read from the catalog `expected` names (see [`bound`]).
pub fn backup_photo_as(
    state: &AppState,
    expected: super::CatalogIdentity,
    photo_id: i64,
) -> Result<BackupReport, String> {
    let cat = bound(state, expected)?;
    let backup_id = cat.with(|c| single_volume_of_kind(c, VolumeKind::Backup, "backup"))?;
    backup_in(&cat, photo_id, backup_id, None)
}

/// Queue a backup of a photo read from the catalog `expected` names; `CATALOG_CHANGED` once
/// another is open.
pub fn enqueue_backup_as(state: &AppState, expected: super::CatalogIdentity, photo_id: i64) -> Result<(), String> {
    super::with_catalog_as(state, expected, |c| c.enqueue_operation("backup", photo_id)).map(drop)
}

/// [`offload_photo`] of a photo read from the catalog `expected` names (see [`bound`]).
pub fn offload_photo_as(
    state: &AppState,
    expected: super::CatalogIdentity,
    photo_id: i64,
) -> Result<OffloadReport, String> {
    offload_in(&bound(state, expected)?, photo_id, None)
}

/// [`restore_photo`] of a photo read from the catalog `expected` names (see [`bound`]).
pub fn restore_photo_as(
    state: &AppState,
    expected: super::CatalogIdentity,
    photo_id: i64,
) -> Result<RestoreReport, String> {
    let cat = bound(state, expected)?;
    let local_id = cat.with(|c| single_volume_of_kind(c, VolumeKind::Local, "local"))?;
    restore_in(&cat, photo_id, local_id, None)
}

/// Free a photo's local copies **and its stack frames'** (#82), each only after re-verifying
/// its own backup, and report what it did: which photos were freed, which frames were left
/// local and why, and how many sidecar backups it deliberately left on disk. Persists an
/// id-keyed thumbnail from a local copy of every member first, so each stays visible once
/// only the NAS copy remains (frames are what the inspector's Stack section shows).
pub fn offload_photo(state: &AppState, photo_id: i64) -> Result<OffloadReport, String> {
    offload_in(state, photo_id, None)
}

/// `abort`: as [`backup_in`]'s, but stricter, because this is the verb that deletes: the
/// delete re-checks it before **every** member (`verify_and_delete_locals_abortable`), the
/// named photo's included — a tripped claim before the named photo frees nothing and fails,
/// keeping the queue row; a frame it reaches tripped is reported skipped. Whatever was
/// already freed is recorded either way: a deleted local file must never keep its row.
fn offload_in(cat: &impl CatalogAccess, photo_id: i64, abort: Option<&AtomicBool>) -> Result<OffloadReport, String> {
    let plan = cat.with(|c| c.plan_offload(photo_id))?;
    for member in std::iter::once(&plan.named).chain(plan.frames.iter()) {
        if let Some(local) = member.local_files.first() {
            let _ = crate::thumbnails::ensure_persistent_thumb(member.photo_id, local);
        }
    }
    let carry = match abort {
        Some(abort) => crate::catalog::verify_and_delete_locals_abortable(&plan, abort),
        None => crate::catalog::verify_and_delete_locals(&plan),
    }
    .map_err(|e| e.to_string())?;
    // Companions are recorded before the local rows go away, per freed photo — see
    // `commit_offload_carry`.
    cat.with(|c| c.commit_offload_carry(carry))
}

/// Pull a photo's backup copy back to local volume `local_id`, hash-verified, companions
/// included (a restored photo arrives with the edit state an offload moved home) — **and the
/// frames stacked under it that are away** (#82): offload frees the moment, so restore brings
/// it back. A frame already local is left alone rather than overwritten.
pub fn restore_to(state: &AppState, photo_id: i64, local_id: i64) -> Result<RestoreReport, String> {
    restore_in(state, photo_id, local_id, None)
}

/// `abort`: as [`backup_in`]'s.
fn restore_in(
    cat: &impl CatalogAccess,
    photo_id: i64,
    local_id: i64,
    abort: Option<&AtomicBool>,
) -> Result<RestoreReport, String> {
    let plan = cat.with(|c| c.plan_restore(photo_id, local_id))?;
    let mut report = RestoreReport { skipped: plan.skipped, total: plan.total, ..Default::default() };
    restore_one(cat, plan.named)?;
    report.restored.push(photo_id);
    for frame in plan.frames {
        let frame_id = frame.photo_id;
        let result = if superseded(abort) { Err(SUPERSEDED.to_string()) } else { restore_one(cat, frame) };
        match result {
            Ok(()) => report.restored.push(frame_id),
            Err(e) => report.skipped.push(SkippedPhoto { photo_id: frame_id, reason: e }),
        }
    }
    Ok(report)
}

/// Bring one photo home — the named photo or one frame of its stack.
fn restore_one(cat: &impl CatalogAccess, plan: PhotoRestore) -> Result<(), String> {
    let PhotoRestore { photo_id, source, dest, rel, volume_id, expected_hash } = plan;
    let outcome = crate::catalog::copy_with_companions(&source, &dest, expected_hash.as_deref())
        .map_err(|e| e.to_string())?;
    cat.with(|c| c.record_copy(photo_id, volume_id, &rel, LocationRole::LocalCache, &outcome))
}

/// Back up to the single backup volume. Errors when the NAS is offline; the UI queues a
/// backup op instead (drained on reconcile).
pub fn backup_photo(state: &AppState, photo_id: i64) -> Result<BackupReport, String> {
    let backup_id = with_catalog(state, |c| single_volume_of_kind(c, VolumeKind::Backup, "backup"))?;
    backup_to(state, photo_id, backup_id)
}

/// Restore to the single local volume.
pub fn restore_photo(state: &AppState, photo_id: i64) -> Result<RestoreReport, String> {
    let local_id = with_catalog(state, |c| single_volume_of_kind(c, VolumeKind::Local, "local"))?;
    restore_to(state, photo_id, local_id)
}

// --- the storage verbs take the moment (#82) ---------------------------------------------
// The catalog-level cascade is pinned in `tests/catalog_integration.rs`; these pin that the
// service bodies the GPUI app runs carry it too, since they plan and record step by step
// rather than through the catalog's sync wrappers.
#[cfg(test)]
mod stack_tests {
    use super::*;
    use crate::catalog::StorageStatus;

    /// A catalog with a reachable NAS and a two-photo stack (a RAW master and its JPEG
    /// frame), both local. Returns the state, the master and frame ids, and their files.
    fn stacked(tag: &str) -> (crate::test_support::TestTmpDir, AppState, i64, i64, PathBuf, PathBuf) {
        let dir = crate::test_support::TestTmpDir::new(&format!("storage-stack-{tag}"));
        let root = dir.join("photos");
        let nas = dir.join("nas");
        std::fs::create_dir_all(root.join("2026/08")).unwrap();
        std::fs::create_dir_all(&nas).unwrap();
        let c = Catalog::open(&dir.join("c.chairphoto"), &root).unwrap();
        c.add_volume("NAS", &nas, VolumeKind::Backup).unwrap();
        let raw = root.join("2026/08/DSC1.ARW");
        let jpg = root.join("2026/08/DSC1.JPG");
        std::fs::write(&raw, b"raw-bytes").unwrap();
        std::fs::write(&jpg, b"jpeg-bytes").unwrap();
        let master = c.upsert_photo(&raw, None, 1, 9).unwrap().id;
        let frame = c.upsert_photo(&jpg, None, 1, 10).unwrap().id;
        c.set_stack_parent(frame, master).unwrap();
        let state = AppState::default();
        *state.catalog.lock().unwrap() = Some(c);
        (dir, state, master, frame, raw, jpg)
    }

    #[test]
    fn the_service_verbs_take_the_whole_stack_and_say_so() {
        let (_dir, state, master, frame, raw, jpg) = stacked("verbs");

        let backed = backup_photo(&state, master).unwrap();
        assert_eq!(backed.backed_up, vec![master, frame], "backup took the frame too");
        assert!(backed.skipped.is_empty());

        let freed = offload_photo(&state, master).unwrap();
        assert_eq!(freed.freed, vec![master, frame]);
        assert!(!raw.exists() && !jpg.exists(), "the whole moment was freed");
        assert_eq!(with_catalog(&state, |c| c.photo_storage_status(frame)).unwrap(), StorageStatus::Archived);

        let restored = restore_photo(&state, master).unwrap();
        assert_eq!(restored.restored, vec![master, frame], "and the whole moment came back");
        assert_eq!(std::fs::read(&jpg).unwrap(), b"jpeg-bytes");
    }

    /// The age sweep: the frame is a candidate in its own right, and the master's offload
    /// has already freed it. Counted once, not twice.
    #[test]
    fn the_offload_policy_counts_a_stack_frame_once() {
        let (_dir, state, master, frame, raw, jpg) = stacked("policy");
        backup_photo(&state, master).unwrap();
        with_catalog(&state, |c| {
            c.conn().execute("UPDATE photos SET created_at = created_at - 10 * 86400", [])?;
            c.set_setting(OFFLOAD_AGE_SETTING, "1")
        })
        .unwrap();
        let eligible = with_catalog(&state, |c| c.photos_eligible_for_offload(1)).unwrap();
        assert_eq!(eligible, vec![master, frame], "both are candidates, master first");

        assert_eq!(apply_offload_policy(&state).unwrap(), 2, "two photos freed, each counted once");
        assert!(!raw.exists() && !jpg.exists());
    }

    /// A drain that finishes only part of a stack keeps what it did and queues what it
    /// left: the master's offload row is replaced by a failed row for the frame, with the
    /// reason, so a retry does not replay the master (port of origin/main 227c87e).
    #[test]
    fn a_drain_requeues_each_skipped_frame_with_its_reason() {
        let (_dir, state, master, frame, raw, jpg) = stacked("drain-partial");
        // The master is backed up while the frame is out of the stack, so the frame has no
        // backup of its own when it rejoins.
        with_catalog(&state, |c| c.unstack(frame)).unwrap();
        let nas = with_catalog(&state, |c| single_volume_of_kind(c, VolumeKind::Backup, "backup")).unwrap();
        with_catalog(&state, |c| c.backup_photo(master, nas).map(drop)).unwrap();
        with_catalog(&state, |c| {
            c.set_stack_parent(frame, master)?;
            c.enqueue_operation("offload", master).map(drop)
        })
        .unwrap();

        let summary = reconcile_now(&state).unwrap();

        assert_eq!((summary.ran, summary.failed, summary.partial), (0, 0, 1), "{summary:?}");
        let pending = with_catalog(&state, |c| c.list_pending_operations()).unwrap();
        assert_eq!(pending.len(), 1, "{pending:?}");
        assert_eq!((pending[0].kind.as_str(), pending[0].photo_id), ("offload", frame));
        assert_eq!(pending[0].status, "failed");
        assert!(pending[0].error.contains("no verified backup"), "{}", pending[0].error);
        assert!(!raw.exists(), "the master's offload is kept");
        assert!(jpg.exists(), "the frame without a backup stays local");
    }

    /// A claimed offload whose claim was tripped deletes nothing and fails, so its queue
    /// row stays for the next drain.
    #[test]
    fn a_tripped_claim_frees_no_member() {
        let (_dir, state, master, _frame, raw, jpg) = stacked("tripped");
        backup_photo(&state, master).unwrap();
        let tripped = AtomicBool::new(true);
        let err = offload_in(&state, master, Some(&tripped)).unwrap_err();
        assert!(err.contains("catalog switched"), "{err}");
        assert!(raw.exists() && jpg.exists(), "nothing was freed");
    }

    /// A claimed backup tripped mid-stack: the named photo — the op in flight — finishes,
    /// and no further frame is started; the frame is reported with why, which is what the
    /// drain requeues.
    #[test]
    fn a_tripped_claim_starts_no_further_frame() {
        let (_dir, state, master, frame, _raw, _jpg) = stacked("tripped-backup");
        let nas = with_catalog(&state, |c| single_volume_of_kind(c, VolumeKind::Backup, "backup")).unwrap();
        let tripped = AtomicBool::new(true);
        let report = backup_in(&state, master, nas, Some(&tripped)).unwrap();
        assert_eq!(report.backed_up, vec![master]);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!((report.skipped[0].photo_id, report.skipped[0].reason.as_str()), (frame, SUPERSEDED));
        assert_eq!(report.total, 2);
        assert!(!with_catalog(&state, |c| c.has_verified_backup(frame)).unwrap(), "the frame was not copied");
    }
}

/// Re-point a photo at a file the user moved (under the library root), then bind that
/// file's sidecar to the photo's UUID — the body the GPUI app's Relocate… runs.
///
/// The relocation and the identity binding are one operation: when the sidecar cannot be
/// bound the row still moves (the user asked for that, and the file is where they said), and
/// the debt is queued in `pending_sidecar_identity` for the repair pass. The sidecar IO runs
/// off the catalog lock (a hung mount must not stall every other catalog user), and the
/// outcome is recorded on a connection of its own to the catalog that was relocated in.
///
/// `expected`: the catalog the id was read from. When another catalog is open the relocation
/// fails closed with `CATALOG_CHANGED`, touching nothing. **Blocks** (sidecar IO): run it on
/// a worker.
pub fn relocate_photo(
    state: &AppState,
    expected: Option<super::CatalogIdentity>,
    photo_id: i64,
    path: &std::path::Path,
) -> Result<(), String> {
    let relocate = |c: &Catalog| {
        let uuid = c.relocate_photo(photo_id, path)?;
        Ok((uuid, c.db_path().to_path_buf(), c.root().to_path_buf()))
    };
    let (uuid, db_path, root) = match expected {
        Some(expected) => super::with_catalog_as(state, expected, relocate)?,
        None => with_catalog(state, relocate)?,
    };
    // The file usually already carries the UUID (its sidecar moved with it); a sidecar
    // holding somebody else's identity is left alone and recorded as a conflict.
    let found = crate::xmp::read_identifier(path);
    let outcome = crate::catalog::bind_sidecar_identity(path, &uuid, found.as_deref());
    let catalog = Catalog::open_secondary(&db_path, &root).map_err(|e| e.to_string())?;
    catalog.record_sidecar_identity(photo_id, path, &outcome).map_err(|e| e.to_string())
}

// --- relocate_photo: the moved copy's identity debt ---------------------------------------
// Moved from the Tauri shell's `commands/storage.rs` when it was removed (#165), where it ran
// this body through the `relocate_photo` command (unbound, `expected = None`).
#[cfg(test)]
mod relocate_tests {
    use super::*;
    use crate::catalog::{Catalog, LocationRole, VolumeKind};

    fn temp_catalog(tag: &str) -> (Catalog, crate::test_support::TestSubPath) {
        let dir = crate::test_support::TestTmpDir::new(&format!("storage-command-{tag}"));
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let catalog = Catalog::open(&dir.join("test.chairphoto"), &root).unwrap();
        (catalog, dir.into_subpath("photos"))
    }

    fn state_with(catalog: Catalog) -> AppState {
        let state = AppState::default();
        *state.catalog.lock().unwrap() = Some(catalog);
        state
    }

    #[test]
    fn relocate_records_identity_debt_for_the_moved_copy() {
        let (catalog, root) = temp_catalog("relocate-identity-target");
        let old = root.join("old/DSC0007.ARW");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, b"old-raw").unwrap();
        let up = catalog.upsert_photo(&old, None, 1, 7).unwrap();

        let backup_dir = root.parent().unwrap().join("backup-relocate");
        std::fs::create_dir_all(&backup_dir).unwrap();
        let backup = backup_dir.join("DSC0007.ARW");
        std::fs::write(&backup, b"backup-raw").unwrap();
        let backup_volume = catalog
            .add_volume("Backup", &backup_dir, VolumeKind::Backup)
            .unwrap();
        catalog
            .add_location(up.id, backup_volume, "DSC0007.ARW", LocationRole::Backup)
            .unwrap();

        let moved = root.join("new/DSC0007.ARW");
        std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
        std::fs::write(&moved, b"moved-raw").unwrap();
        std::fs::write(
            crate::xmp::sidecar_path(&moved),
            b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF",
        )
        .unwrap();

        let state = state_with(catalog);
        relocate_photo(&state, None, up.id, &moved).unwrap();

        let guard = state.catalog.lock().unwrap();
        let catalog = guard.as_ref().unwrap();
        let pending = catalog.list_pending_identity().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].photo_id, up.id);
        assert_eq!(pending[0].field, "identifier");
        assert_eq!(PathBuf::from(&pending[0].target_path), moved);
        assert!(
            pending[0].error.contains("sidecar write failed"),
            "the corrupt moved sidecar should be recorded, got {:?}",
            pending[0].error
        );

        std::fs::remove_file(&moved).unwrap();
        let summary = catalog.repair_pending_identity().unwrap();
        assert_eq!(
            (summary.bound, summary.failed, summary.unreachable),
            (0, 0, 1),
            "repair must stay scoped to the moved copy, even while another copy is reachable"
        );
        assert!(
            crate::xmp::read_identifier(&backup).is_none(),
            "repairing the moved copy's debt must not bind the backup copy"
        );
        assert_eq!(catalog.count_pending_identity().unwrap(), 1);
    }
}

/// The id of the single volume of a kind, or an error if there are zero or many.
pub fn single_volume_of_kind(c: &Catalog, kind: VolumeKind, label: &str) -> crate::catalog::Result<i64> {
    let ids: Vec<i64> = c.list_volumes()?.into_iter().filter(|v| v.kind == kind).map(|v| v.id).collect();
    match ids.as_slice() {
        [one] => Ok(*one),
        [] => Err(crate::catalog::CatalogError::Validation(format!("no {label} volume configured"))),
        _ => Err(crate::catalog::CatalogError::Validation(format!(
            "multiple {label} volumes — choosing one isn't supported yet"
        ))),
    }
}

/// Setting key for the age-based offload policy ("keep last N days on local disk").
/// Empty / "0" = disabled (no automatic offload).
pub const OFFLOAD_AGE_SETTING: &str = "offload_age_days";

/// Apply the "keep last N days local" policy: offload every photo older than the configured
/// age that has a verified NAS backup. No-op when the policy is unset or the NAS is
/// unreachable. Returns how many photos were offloaded.
///
/// Claims the reconcile generation and works on that catalog only — see [`ReconcileClaim`].
pub fn apply_offload_policy(state: &AppState) -> Result<usize, String> {
    claim_reconcile(state)?.apply_offload_policy()
}

/// [`apply_offload_policy`] claimed only while the open catalog is `expected` (the one the
/// user asked in); otherwise `CATALOG_CHANGED`, with no generation installed.
pub fn apply_offload_policy_as(state: &AppState, expected: super::CatalogIdentity) -> Result<usize, String> {
    claim(state, Some(expected))?.apply_offload_policy()
}

// ── Reconcile ────────────────────────────────────────────────────────────────

/// A back-up drain's (or the offload policy's) ownership: the reconcile generation it
/// claimed, and the catalog it claimed it against.
///
/// Every step runs on **its own connection to that catalog**, never through the shared
/// handle. A drain plans an op from one catalog's queue, copies for minutes over a NAS, then
/// records it; through the shared handle a catalog switch in between would plan or record
/// the old catalog's photo and op ids in the new catalog's rows — backing up or offloading
/// unrelated photos and dropping unrelated queued ops. Bound, the op in flight finishes
/// against the catalog it came from (its plan, its file IO and its record agree), and the
/// drain then stops before the next op, because the switch tripped its generation. A newer
/// drain trips an older one too, so two never work one queue.
pub struct ReconcileClaim {
    db_path: PathBuf,
    root: PathBuf,
    abort: Arc<AtomicBool>,
}

/// Claim the reconcile generation against the open catalog. Takes the catalog lock and then
/// the reconcile abort (the `jobs` lock order), so a switch either completes first — and this
/// claims the new catalog — or runs after and trips this claim: a drain never holds an
/// un-tripped generation for a catalog that has been replaced. Blocking (the catalog lock):
/// run it on a worker.
pub fn claim_reconcile(state: &AppState) -> Result<ReconcileClaim, String> {
    claim(state, None)
}

fn claim(state: &AppState, expected: Option<super::CatalogIdentity>) -> Result<ReconcileClaim, String> {
    let guard = state.catalog.lock().map_err(|e| e.to_string())?;
    let c = guard.as_ref().ok_or("No catalog is open")?;
    if expected.is_some_and(|e| !e.is(c)) {
        return Err(super::CATALOG_CHANGED.into());
    }
    let (db_path, root) = (c.db_path().to_path_buf(), c.root().to_path_buf());
    let abort = state.jobs.reconcile.install_fresh()?;
    Ok(ReconcileClaim { db_path, root, abort })
}

impl ReconcileClaim {
    /// Whether a catalog switch or a newer drain has taken this claim over.
    pub fn aborted(&self) -> bool {
        self.abort.load(Ordering::Relaxed)
    }

    fn open(&self) -> Result<Catalog, String> {
        Catalog::open_secondary(&self.db_path, &self.root).map_err(|e| e.to_string())
    }

    /// Drain the reconcile queue (E4): run each pending op when a backup volume is reachable.
    /// Clears each op on success, marks it failed (kept) otherwise. With no reachable backup
    /// it does nothing (`skipped_offline`). Once the claim is tripped it stops before the next
    /// op (`aborted`); the ops it did not reach stay queued for the next drain of that catalog.
    pub fn drain(&self, state: &AppState) -> Result<DrainSummary, String> {
        self.drain_with(state, &mut |_| {})
    }

    /// [`Self::drain`], calling `before_op(i)` after op `i`'s ownership check and before it
    /// runs — where a test puts a catalog switch.
    fn drain_with(&self, state: &AppState, before_op: &mut dyn FnMut(usize)) -> Result<DrainSummary, String> {
        let mut summary = DrainSummary::default();
        if self.aborted() {
            summary.aborted = true;
            return Ok(summary);
        }
        let cat = self.open()?;
        let (vols, pending) = cat.with(|c| Ok((c.volume_rows()?, c.list_pending_operations()?)))?;
        // Reconcile decides whether to run at all on the backup volume being reachable — it
        // wants live truth, so drop any cached state and stat fresh.
        state.volume_health.invalidate();
        let pairs: Vec<(i64, String)> = vols.iter().map(|v| (v.id, v.base_path.clone())).collect();
        let reachable = state.volume_health.refresh(&pairs);
        let backup_id = vols
            .iter()
            .find(|v| v.kind == VolumeKind::Backup && reachable.get(&v.id).copied().unwrap_or(false))
            .map(|v| v.id);
        let local_id = vols.iter().find(|v| v.kind == VolumeKind::Local).map(|v| v.id);

        let Some(backup_id) = backup_id else {
            summary.skipped_offline = true;
            return Ok(summary);
        };
        for (i, op) in pending.into_iter().enumerate() {
            if self.aborted() {
                summary.aborted = true;
                break;
            }
            before_op(i);
            // The drain needs only what each op left undone: a stack op that completed for
            // some members is replaced by one failed row per skipped frame, so a retry
            // does not replay the members that already finished.
            let abort = Some(self.abort.as_ref());
            let result = match op.kind.as_str() {
                "backup" => backup_in(&cat, op.photo_id, backup_id, abort).map(|r| r.skipped),
                "offload" => offload_in(&cat, op.photo_id, abort).map(|r| r.skipped),
                "restore" => match local_id {
                    Some(l) => restore_in(&cat, op.photo_id, l, abort).map(|r| r.skipped),
                    None => Err("no local volume".into()),
                },
                other => Err(format!("unknown operation: {other}")),
            };
            match result {
                Ok(skipped) if skipped.is_empty() => {
                    cat.with(|c| c.remove_operation(op.id))?;
                    summary.ran += 1;
                }
                Ok(skipped) => {
                    cat.with(|c| c.replace_operation_with_skipped(op.id, &op.kind, &skipped))?;
                    summary.partial += 1;
                }
                Err(e) => {
                    cat.with(|c| c.set_operation_failed(op.id, &e))?;
                    summary.failed += 1;
                }
            }
        }
        // Reconciliation moves files between volumes, so cached reachability could be stale.
        state.volume_health.invalidate();
        Ok(summary)
    }

    /// The offload policy ([`apply_offload_policy`]) under this claim: candidates read from,
    /// and offloads recorded in, the claimed catalog; stops before the next photo once the
    /// claim is tripped.
    pub fn apply_offload_policy(&self) -> Result<usize, String> {
        if self.aborted() {
            return Ok(0);
        }
        let cat = self.open()?;
        let age: i64 = cat
            .with(|c| c.get_setting(OFFLOAD_AGE_SETTING))?
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        if age <= 0 {
            return Ok(0);
        }
        let candidates = cat.with(|c| c.photos_eligible_for_offload(age))?;
        let mut offloaded = 0;
        // A stack's frames are eligible in their own right, and the master's offload already
        // freed them (#82). Without this the sweep would run a second offload per frame and
        // count each one twice.
        let mut already_freed: HashSet<i64> = HashSet::new();
        for id in candidates {
            if self.aborted() {
                break;
            }
            if already_freed.contains(&id) {
                continue;
            }
            if let Ok(report) = offload_in(&cat, id, Some(self.abort.as_ref())) {
                offloaded += report.freed.len();
                already_freed.extend(report.freed);
            }
        }
        Ok(offloaded)
    }
}

/// Drain the reconcile queue under a fresh claim — see [`ReconcileClaim::drain`]. The
/// reachability stat runs off the catalog lock.
pub fn reconcile_now(state: &AppState) -> Result<DrainSummary, String> {
    claim_reconcile(state)?.drain(state)
}

/// Whether the reconcile-on-launch/focus check should drain now: ops are pending and a
/// backup volume is reachable (App.tsx `checkReconcile`). Returns the pending count with it.
/// Stats volumes off the catalog lock (through the short-TTL volume-health cache).
pub fn reconcile_due(state: &AppState) -> Result<(usize, bool), String> {
    let (vols, pending) = with_catalog(state, |c| {
        let pending = c.list_pending_operations()?.iter().filter(|o| o.status == "pending").count();
        Ok((c.volume_rows()?, pending))
    })?;
    if pending == 0 {
        return Ok((0, false));
    }
    let pairs: Vec<(i64, String)> = vols.iter().map(|v| (v.id, v.base_path.clone())).collect();
    let reachable = state.volume_health.refresh(&pairs);
    let backup_reachable =
        vols.iter().any(|v| v.kind == VolumeKind::Backup && reachable.get(&v.id).copied().unwrap_or(false));
    Ok((pending, backup_reachable))
}

// ── Trash ────────────────────────────────────────────────────────────────────

/// Bring photos back, along with whatever was trashed in the same act.
///
/// Trips the trash job generation first: an `empty_trash` worker can be part-way through the
/// filesystem with a plan minutes old, and restoring is the user saying "keep this" — the
/// delete must stand down. Tripping before the restore means it cannot slip one in between.
pub fn restore_trashed(state: &AppState, photo_ids: &[i64]) -> Result<usize, String> {
    state.jobs.trash.trip()?;
    with_catalog(state, |c| c.restore_photos(photo_ids))
}

/// [`restore_trashed`] for ids read from the catalog `expected` names: fails closed with
/// `CATALOG_CHANGED` (restoring nothing) once another catalog is open, because the same ids
/// there are other photos.
pub fn restore_trashed_as(
    state: &AppState,
    expected: super::CatalogIdentity,
    photo_ids: &[i64],
) -> Result<usize, String> {
    // Checked before the trip, under the same catalog lock hold (catalog → trash abort): a
    // stale restore must not stop the open catalog's delete.
    let guard = state.catalog.lock().map_err(|e| e.to_string())?;
    let c = guard.as_ref().ok_or("No catalog is open")?;
    if !expected.is(c) {
        return Err(super::CATALOG_CHANGED.into());
    }
    state.jobs.trash.trip()?;
    c.restore_photos(photo_ids).map_err(|e| e.to_string())
}

/// What emptying the trash did — and, as importantly, what it did not.
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmptyTrashReport {
    /// Photos destroyed: every expected file confirmed gone, then the catalog row.
    pub deleted: usize,
    /// Files removed — images and their declared companions.
    pub files_deleted: usize,
    /// Photos left alone because a volume holding a copy could not be reached. Deleting
    /// them would have destroyed the copies we *can* see while leaving an unreferenced
    /// survivor on a disconnected disk.
    pub skipped_unreachable: Vec<i64>,
    /// Photos whose deletion failed part-way, with the reason. Their catalog rows are
    /// **kept**: a row pointing at a file we could not remove is recoverable, whereas a
    /// file with no row is an orphan nothing in the app can ever find again.
    pub failed: Vec<(i64, String)>,
    /// Photos restored while this was running. Restore wins that race by design.
    pub restored_meanwhile: Vec<i64>,
    /// The run stopped early because it stopped being the owner — a catalog switch, or a
    /// restore. Whatever is reported here happened; the rest did not.
    pub aborted: bool,
}

/// What became of one photo's copies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// Every expected file is confirmed absent. Safe to forget the row.
    Destroyed,
    /// A volume holding a copy could not be reached; nothing was touched.
    Unreachable,
    /// Something survived. The message names the first path still present.
    Failed(String),
}

/// Destroy trashed photos: the only path in the app that deletes an original. Blocking
/// (file IO, a reachability stat per volume): run it on a worker.
///
/// Two gates, both deliberate:
///
/// - **`confirm` must be true.** The backend fails closed rather than trusting that a
///   caller meant it; this is the one verb with no undo.
/// - **Every known copy must be reachable.** Not just home — deleting the copies we can
///   see while a disconnected disk still holds one would leave an unreferenced survivor
///   and a deleted catalog row, which is worse than refusing. This is what replaces
///   "only on the master": a device that cannot reach a copy cannot destroy it, which is
///   a stronger guarantee than a role flag and needs nothing to be true about identity.
///
/// Companions go with the image (cluster B, D2). Leaving them behind would strand sidecars
/// at home — the mirror of #80, on the one path where nothing can be recovered afterwards.
pub fn empty_trash(
    state: &AppState,
    photo_ids: Option<Vec<i64>>,
    older_than_days: Option<i64>,
    confirm: bool,
) -> Result<EmptyTrashReport, String> {
    empty_trash_as(state, None, photo_ids, older_than_days, confirm)
}

/// [`empty_trash`] bound to the catalog `expected` names — what a front end that listed the
/// trash passes along with the ids it read. The plan and the record phases each check it
/// under the catalog lock: if another catalog is open by then, the plan fails closed with
/// `CATALOG_CHANGED` before any file is touched, and the record phase stops (`aborted`).
/// This holds even when the delete's worker starts after a switch that the front end has
/// not seen yet, where a fresh trash generation alone would not stop it.
pub fn empty_trash_as(
    state: &AppState,
    expected: Option<super::CatalogIdentity>,
    photo_ids: Option<Vec<i64>>,
    older_than_days: Option<i64>,
    confirm: bool,
) -> Result<EmptyTrashReport, String> {
    if !confirm {
        return Err("emptying the trash needs an explicit confirmation".into());
    }
    // Own this run through the job protocol before touching anything. A catalog switch
    // trips every family, so an in-flight delete stops being an owner the moment the
    // catalog under it is replaced — which is what stops one catalog's photo ids being
    // applied to another's rows. `restore_photos` trips it too, so a user pulling a photo
    // back out of the trash wins that race.
    //
    // The identity is checked before the generation is installed, under one catalog lock
    // hold (catalog → trash abort): a delete bound to a catalog that is no longer open must
    // fail closed without tripping the open catalog's delete.
    let abort = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let c = guard.as_ref().ok_or("No catalog is open")?;
        if expected.is_some_and(|id| !id.is(c)) {
            return Err(super::CATALOG_CHANGED.to_string());
        }
        state.jobs.trash.install_fresh()?
    };
    let catalog = state.catalog.clone();
    let health = state.volume_health.clone();
    (move || {
        // 1. Under the lock: which photos, and where every copy of each one lives.
        let (candidates, plans, pairs) = {
            let guard = catalog.lock().map_err(|e| e.to_string())?;
            let c = guard.as_ref().ok_or("No catalog is open")?;
            if expected.is_some_and(|id| !id.is(c)) {
                return Err(super::CATALOG_CHANGED.to_string());
            }
            let candidates: Vec<i64> = match (photo_ids, older_than_days) {
                (Some(ids), _) => ids,
                (None, Some(days)) => {
                    let cutoff = now_secs() - days.max(0) * 86_400;
                    c.trashed_before(cutoff).map_err(|e| e.to_string())?
                }
                (None, None) => c.trashed_before(i64::MAX).map_err(|e| e.to_string())?,
            };
            let mut plans = Vec::new();
            for &id in &candidates {
                // Only ever destroys something already in the trash: emptying the trash
                // must not be a way to delete a photo that was never put there.
                if !c.is_trashed(id).map_err(|e| e.to_string())? {
                    continue;
                }
                plans.push((id, c.photo_path_candidates(id).map_err(|e| e.to_string())?));
            }
            let pairs = c.volume_base_paths().map_err(|e| e.to_string())?;
            (candidates.len(), plans, pairs)
        };
        let _ = candidates;

        // 2. Off the lock: reachability, then the deletes themselves. Both can block on a
        //    slow mount and neither may hold the catalog.
        let reachable = health.refresh(&pairs);
        let (mut report, destroyed) = destroy_planned_photos(
            &plans,
            &reachable,
            &abort,
            &mut |id| {
                let guard = catalog.lock().map_err(|e| e.to_string())?;
                let c = guard.as_ref().ok_or("No catalog is open")?;
                if expected.is_some_and(|e| !e.is(c)) {
                    return Err(super::CATALOG_CHANGED.to_string());
                }
                c.is_trashed(id).map_err(|e| e.to_string())
            },
        )?;

        // 3. Back under the lock: forget only the rows whose files are confirmed gone. A
        //    photo that failed keeps its row, so the trash can still find it and the user
        //    can retry — the alternative is a file on disk that nothing points at.
        let guard = catalog.lock().map_err(|e| e.to_string())?;
        let c = guard.as_ref().ok_or("No catalog is open")?;
        for id in destroyed {
            // Ownership is re-checked here too, not just before the IO: the rows belong to
            // whichever catalog is installed *now*, and applying an old catalog's ids to a
            // new one is the failure this whole protocol exists to prevent. The files are
            // already gone; a rescan reconciles the rows, which is recoverable.
            if abort.load(Ordering::Relaxed) || expected.is_some_and(|e| !e.is(c)) {
                report.aborted = true;
                break;
            }
            c.remove_photo(id).map_err(|e| e.to_string())?;
            report.deleted += 1;
        }
        Ok(report)
    })()
}

/// Delete every copy of each photo, or none of them.
///
/// Split out of the command handler because this is where the destructive decision is made,
/// and a decision reachable only through a front end's own handler type is a decision
/// nobody can test. Pure file
/// IO — no catalog lock — so it runs on the blocking worker like the rest of the lifecycle.
///
/// Returns the report and the ids whose files are now gone, for the caller to forget.
/// Walk the planned photos, destroying each one's copies — the part of emptying the trash
/// where ownership actually matters.
///
/// Split out because the two interleavings that make this dangerous are otherwise
/// reachable only through a front end's own handler type, and a race nobody can test is a
/// race nobody has checked. `still_trashed` is a callback so a test can make a photo come
/// back mid-run the way Restore does.
///
/// Stops at the first sign it is no longer the owner. `abort` is tripped by a catalog
/// switch (so an old worker cannot apply one catalog's ids to another's rows) and by
/// Restore (so pulling a photo out of the trash beats a delete already in flight).
pub fn destroy_planned_photos(
    plans: &[(i64, Vec<crate::catalog::PathCandidate>)],
    reachable: &std::collections::HashMap<i64, bool>,
    abort: &std::sync::atomic::AtomicBool,
    still_trashed: &mut dyn FnMut(i64) -> Result<bool, String>,
) -> Result<(EmptyTrashReport, Vec<i64>), String> {
    let mut report = EmptyTrashReport::default();
    let mut destroyed: Vec<i64> = Vec::new();
    for (id, locations) in plans {
        if abort.load(Ordering::Relaxed) {
            report.aborted = true;
            break;
        }
        // Re-read trash membership immediately before deleting *this* photo, not once for
        // the batch at plan time: the plan can be minutes old over a slow mount, and
        // Restore clears `trashed_at` underneath it.
        if !still_trashed(*id)? {
            report.restored_meanwhile.push(*id);
            continue;
        }
        let (outcome, files) = delete_one_photos_copies(locations, reachable);
        report.files_deleted += files;
        match outcome {
            DeleteOutcome::Destroyed => destroyed.push(*id),
            DeleteOutcome::Unreachable => report.skipped_unreachable.push(*id),
            DeleteOutcome::Failed(why) => report.failed.push((*id, why)),
        }
    }
    Ok((report, destroyed))
}

pub fn delete_one_photos_copies(
    locations: &[crate::catalog::PathCandidate],
    reachable: &std::collections::HashMap<i64, bool>,
) -> (DeleteOutcome, usize) {
    // Every known copy, not just the one at home: deleting what we can see while a
    // disconnected disk still holds one would leave an unreferenced survivor.
    let all_reachable = locations.iter().all(|cand| {
        cand.volume_id
            .map(|v| reachable.get(&v).copied().unwrap_or(false))
            .unwrap_or(true)
    });
    if !all_reachable {
        return (DeleteOutcome::Unreachable, 0);
    }

    let mut files_deleted = 0usize;
    // Collect what we are responsible for *before* deleting, so the survivor check below
    // is against the full expected set rather than against whatever we happened to reach.
    let mut expected: Vec<std::path::PathBuf> = Vec::new();
    for cand in locations {
        for found in crate::companions::carried_beside(&cand.path) {
            expected.push(found.path.clone());
        }
        if cand.path.exists() {
            expected.push(cand.path.clone());
        }
    }

    for cand in locations {
        // Companions first: if a delete fails part-way, the image is still there to say
        // what the leftovers belonged to.
        for found in crate::companions::carried_beside(&cand.path) {
            if std::fs::remove_file(&found.path).is_ok() {
                files_deleted += 1;
            }
        }
        if cand.path.exists() && std::fs::remove_file(&cand.path).is_ok() {
            files_deleted += 1;
        }
    }

    // Success is *confirmed absence*, not "no error was returned". A permission error, a
    // read-only mount, or a volume that dropped after the reachability probe all leave the
    // file there — and reporting that photo deleted is how a catalog row disappears while
    // its original survives with nothing pointing at it.
    if let Some(survivor) = expected.iter().find(|p| p.exists()) {
        return (
            DeleteOutcome::Failed(format!("{} could not be removed", survivor.display())),
            files_deleted,
        );
    }
    (DeleteOutcome::Destroyed, files_deleted)
}

// --- emptying the trash: destroy_planned_photos / delete_one_photos_copies ------------------
// Moved from the Tauri shell's `commands/storage.rs` when it was removed (#165); they always
// exercised these core functions directly.
#[cfg(test)]
mod trash_delete_tests {
    use super::*;
    use crate::catalog::{LocationRole, PathCandidate};
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    fn candidate(path: PathBuf, volume_id: i64) -> PathCandidate {
        PathCandidate { path, role: LocationRole::Primary, volume_id: Some(volume_id) }
    }

    /// Make a directory refuse deletions, so a real IO failure can be forced rather than
    /// simulated. Unix-only; the assertion it supports is skipped elsewhere.
    #[cfg(unix)]
    fn set_readonly(dir: &Path, readonly: bool) {
        use std::os::unix::fs::PermissionsExt;
        let mode = if readonly { 0o555 } else { 0o755 };
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    use std::sync::atomic::AtomicBool;

    /// One trashed photo with a real file, so a delete has something to destroy.
    fn planned(dir: &Path, id: i64) -> (i64, Vec<PathCandidate>) {
        let p = dir.join(format!("DSC{id}.ARW"));
        std::fs::write(&p, b"bytes").unwrap();
        (id, vec![candidate(p, 1)])
    }

    /// Restore must beat a delete already walking the filesystem. The plan is made once and
    /// can be minutes old over a slow mount; a photo the user pulled back out of the trash
    /// in the meantime must survive, files and row alike.
    #[test]
    fn a_photo_restored_mid_run_is_not_destroyed() {
        let dir = crate::test_support::TestTmpDir::new("empty-trash-restored");
        std::fs::create_dir_all(&*dir).unwrap();
        let plans = vec![planned(&dir, 1), planned(&dir, 2), planned(&dir, 3)];
        let reachable = HashMap::from([(1, true)]);
        let abort = AtomicBool::new(false);

        // Photo 2 comes back out of the trash while the run is in progress.
        let mut still_trashed = |id: i64| Ok(id != 2);

        let (report, destroyed) =
            destroy_planned_photos(&plans, &reachable, &abort, &mut still_trashed).unwrap();

        assert_eq!(destroyed, vec![1, 3]);
        assert_eq!(report.restored_meanwhile, vec![2], "and it is reported, not silent");
        assert!(dir.join("DSC2.ARW").exists(), "the restored photo keeps its file");
        assert!(!dir.join("DSC1.ARW").exists());
    }

    /// A catalog switch trips every job family, including this one. The worker must stop
    /// touching files the moment it stops being the owner — continuing would delete the old
    /// catalog's files and then apply its numeric ids to the new catalog's rows.
    #[test]
    fn a_worker_that_loses_ownership_stops_deleting() {
        let dir = crate::test_support::TestTmpDir::new("empty-trash-switch");
        std::fs::create_dir_all(&*dir).unwrap();
        let plans = vec![planned(&dir, 1), planned(&dir, 2), planned(&dir, 3)];
        let reachable = HashMap::from([(1, true)]);
        let abort = AtomicBool::new(false);

        // Ownership is lost after the first photo — as a catalog switch would do.
        let mut seen = 0;
        let mut still_trashed = |_id: i64| {
            seen += 1;
            if seen == 1 {
                abort.store(true, Ordering::Relaxed);
            }
            Ok(true)
        };

        let (report, destroyed) =
            destroy_planned_photos(&plans, &reachable, &abort, &mut still_trashed).unwrap();

        assert!(report.aborted, "the run says it stopped early");
        assert_eq!(destroyed, vec![1], "only the photo already in flight");
        assert!(dir.join("DSC2.ARW").exists(), "nothing after the switch was touched");
        assert!(dir.join("DSC3.ARW").exists());
    }

    /// Ownership lost before the first photo means nothing is destroyed at all.
    #[test]
    fn a_run_that_never_owned_anything_destroys_nothing() {
        let dir = crate::test_support::TestTmpDir::new("empty-trash-preempted");
        std::fs::create_dir_all(&*dir).unwrap();
        let plans = vec![planned(&dir, 1)];
        let abort = AtomicBool::new(true);

        let (report, destroyed) = destroy_planned_photos(
            &plans,
            &HashMap::from([(1, true)]),
            &abort,
            &mut |_| Ok(true),
        )
        .unwrap();

        assert!(report.aborted);
        assert!(destroyed.is_empty());
        assert_eq!(report.files_deleted, 0);
        assert!(dir.join("DSC1.ARW").exists());
    }

    /// Every copy goes, and its declared companions with it — otherwise emptying the trash
    /// strands sidecars on the one path where nothing can be recovered afterwards.
    #[test]
    fn deleting_a_photo_takes_every_copy_and_its_companions() {
        let dir = crate::test_support::TestTmpDir::new("empty-trash");
        let local = dir.join("local");
        let nas = dir.join("nas");
        std::fs::create_dir_all(&local).unwrap();
        std::fs::create_dir_all(&nas).unwrap();
        for base in [&local, &nas] {
            std::fs::write(base.join("DSC1.ARW"), b"bytes").unwrap();
            std::fs::write(base.join("DSC1.ARW.xmp"), b"history").unwrap();
        }
        std::fs::write(local.join("DSC1.ARW.rrdata"), b"masks").unwrap();
        // Not a declared companion — must survive, because backup never claimed it either.
        std::fs::write(local.join("DSC1.ARW.txt"), b"notes").unwrap();

        let locations = vec![
            candidate(local.join("DSC1.ARW"), 1),
            candidate(nas.join("DSC1.ARW"), 2),
        ];
        let reachable = HashMap::from([(1, true), (2, true)]);

        let (outcome, files) = delete_one_photos_copies(&locations, &reachable);

        assert_eq!(outcome, DeleteOutcome::Destroyed);
        assert_eq!(files, 5, "2 images + 3 companions");
        assert!(!local.join("DSC1.ARW").exists());
        assert!(!nas.join("DSC1.ARW").exists());
        assert!(!local.join("DSC1.ARW.rrdata").exists());
        assert!(local.join("DSC1.ARW.txt").exists(), "an undeclared neighbour is not ours");
    }

    /// An unreachable copy means refuse, not "delete what we can". Deleting the reachable
    /// copies would leave an unreferenced survivor on the disconnected disk and a catalog
    /// row that no longer points at it.
    #[test]
    fn one_unreachable_copy_saves_every_copy() {
        let dir = crate::test_support::TestTmpDir::new("empty-trash-unreachable");
        let local = dir.join("local");
        std::fs::create_dir_all(&local).unwrap();
        std::fs::write(local.join("DSC1.ARW"), b"bytes").unwrap();

        let locations = vec![
            candidate(local.join("DSC1.ARW"), 1),
            candidate(dir.join("gone/DSC1.ARW"), 2),
        ];

        let (outcome, files) =
            delete_one_photos_copies(&locations, &HashMap::from([(1, true), (2, false)]));

        assert_eq!(outcome, DeleteOutcome::Unreachable);
        assert_eq!(files, 0);
        assert!(local.join("DSC1.ARW").exists(), "the reachable copy is untouched");
    }

    /// A volume the reachability map has never heard of is unreachable, not assumed fine.
    /// Failing open here would delete originals on the strength of a missing map entry.
    #[test]
    fn an_unknown_volume_counts_as_unreachable() {
        let dir = crate::test_support::TestTmpDir::new("empty-trash-unknown");
        std::fs::create_dir_all(dir.join("local")).unwrap();
        std::fs::write(dir.join("local/DSC1.ARW"), b"bytes").unwrap();

        let (outcome, _) = delete_one_photos_copies(
            &[candidate(dir.join("local/DSC1.ARW"), 1)],
            &HashMap::new(),
        );

        assert_eq!(outcome, DeleteOutcome::Unreachable);
        assert!(dir.join("local/DSC1.ARW").exists());
    }

    /// A copy already gone from disk is not an obstacle — the goal is "no copies left",
    /// and one that has already been removed satisfies it.
    #[test]
    fn a_copy_whose_file_is_already_gone_does_not_block_the_delete() {
        let dir = crate::test_support::TestTmpDir::new("empty-trash-absent");
        std::fs::create_dir_all(dir.join("local")).unwrap();

        let (outcome, files) = delete_one_photos_copies(
            &[candidate(dir.join("local/DSC1.ARW"), 1)],
            &HashMap::from([(1, true)]),
        );

        assert_eq!(outcome, DeleteOutcome::Destroyed);
        assert_eq!(files, 0);
    }

    /// The failure this contract exists for: `remove_file` returns an error, the file is
    /// still there, and reporting the photo deleted would let its catalog row disappear
    /// while the original survives with nothing pointing at it.
    #[cfg(unix)]
    #[test]
    fn an_image_that_cannot_be_removed_is_a_failure_not_a_deletion() {
        let dir = crate::test_support::TestTmpDir::new("empty-trash-readonly");
        let local = dir.join("local");
        std::fs::create_dir_all(&local).unwrap();
        std::fs::write(local.join("DSC1.ARW"), b"bytes").unwrap();
        set_readonly(&local, true);

        let (outcome, _) = delete_one_photos_copies(
            &[candidate(local.join("DSC1.ARW"), 1)],
            &HashMap::from([(1, true)]),
        );

        set_readonly(&local, false); // so the fixture can clean itself up
        assert!(
            matches!(outcome, DeleteOutcome::Failed(ref why) if why.contains("DSC1.ARW")),
            "expected a named failure, got {outcome:?}"
        );
        assert!(local.join("DSC1.ARW").exists(), "and the original is still there");
    }

    /// A companion that survives is just as much a failure as a surviving image: the point
    /// of carrying companions is that they are part of the copy.
    #[cfg(unix)]
    #[test]
    fn a_companion_that_cannot_be_removed_is_a_failure_too() {
        let dir = crate::test_support::TestTmpDir::new("empty-trash-companion-readonly");
        let local = dir.join("local");
        let locked = local.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("DSC1.ARW"), b"bytes").unwrap();
        std::fs::write(locked.join("DSC1.ARW.rrdata"), b"masks").unwrap();
        set_readonly(&locked, true);

        let (outcome, _) = delete_one_photos_copies(
            &[candidate(locked.join("DSC1.ARW"), 1)],
            &HashMap::from([(1, true)]),
        );

        set_readonly(&locked, false);
        assert!(matches!(outcome, DeleteOutcome::Failed(_)), "got {outcome:?}");
        assert!(locked.join("DSC1.ARW.rrdata").exists());
    }

    /// One copy of several fails. The photo must not be reported destroyed — some copies
    /// are gone and one is not, which is precisely the state a catalog row is needed for.
    #[cfg(unix)]
    #[test]
    fn a_partial_multi_copy_failure_is_not_a_deletion() {
        let dir = crate::test_support::TestTmpDir::new("empty-trash-partial");
        let ok = dir.join("ok");
        let locked = dir.join("locked");
        std::fs::create_dir_all(&ok).unwrap();
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(ok.join("DSC1.ARW"), b"bytes").unwrap();
        std::fs::write(locked.join("DSC1.ARW"), b"bytes").unwrap();
        set_readonly(&locked, true);

        let (outcome, files) = delete_one_photos_copies(
            &[candidate(ok.join("DSC1.ARW"), 1), candidate(locked.join("DSC1.ARW"), 2)],
            &HashMap::from([(1, true), (2, true)]),
        );

        set_readonly(&locked, false);
        assert!(matches!(outcome, DeleteOutcome::Failed(_)), "got {outcome:?}");
        assert_eq!(files, 1, "the reachable copy really was removed — and is reported");
        assert!(!ok.join("DSC1.ARW").exists());
        assert!(locked.join("DSC1.ARW").exists(), "the survivor keeps its catalog row");
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::catalogs::{detach_catalog_and_trip_jobs, publish_catalog_and_reset_jobs};
    use crate::catalog::StorageStatus;
    use std::path::Path;

    /// A catalog under `dir/<name>` with a reachable NAS and `n` photos whose originals
    /// exist, each with a queued backup. Returns the catalog and the photo ids.
    fn catalog_with_queued_backups(dir: &Path, name: &str, n: usize) -> (Catalog, Vec<i64>) {
        let root = dir.join(name).join("photos");
        let nas = dir.join(name).join("nas");
        std::fs::create_dir_all(root.join("2026")).unwrap();
        std::fs::create_dir_all(&nas).unwrap();
        let c = Catalog::open(&dir.join(name).join("c.chairphoto"), &root).unwrap();
        c.add_volume("NAS", &nas, VolumeKind::Backup).unwrap();
        let ids: Vec<i64> = (0..n)
            .map(|i| {
                let f = root.join(format!("2026/{name}{i}.jpg"));
                std::fs::write(&f, format!("{name} bytes {i}")).unwrap();
                let id = c.upsert_photo(&f, None, 10, 12).unwrap().id;
                c.enqueue_operation("backup", id).unwrap();
                id
            })
            .collect();
        (c, ids)
    }

    fn nas_files(dir: &Path, name: &str) -> usize {
        walkdir::WalkDir::new(dir.join(name).join("nas"))
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .count()
    }

    /// **Forced interleaving.** A catalog switch lands while a drain is part-way through the
    /// old catalog's queue (after op 0's ownership check, before op 0 runs). Op 0 finishes
    /// against the catalog it was read from; the drain then stops; and the new catalog —
    /// whose photo and op ids collide with the old one's — is untouched: its op stays queued,
    /// its photo is not backed up, nothing lands on its NAS.
    #[test]
    fn a_switch_mid_drain_never_reaches_the_new_catalog() {
        let dir = crate::test_support::TestTmpDir::new("reconcile-switch");
        let (a, a_ids) = catalog_with_queued_backups(&dir, "a", 2);
        let (b, b_ids) = catalog_with_queued_backups(&dir, "b", 1);
        assert_eq!(a_ids[0], b_ids[0], "the two catalogs' ids collide, as real ones do");
        let (b_path, b_root) = (b.db_path().to_path_buf(), b.root().to_path_buf());
        drop(b);
        let state = AppState::default();
        *state.catalog.lock().unwrap() = Some(a);

        let claim = claim_reconcile(&state).unwrap();
        let summary = claim
            .drain_with(&state, &mut |i| {
                if i == 0 {
                    detach_catalog_and_trip_jobs(&state).unwrap();
                    publish_catalog_and_reset_jobs(&state, Catalog::open(&b_path, &b_root).unwrap()).unwrap();
                }
            })
            .unwrap();
        assert_eq!((summary.ran, summary.failed, summary.aborted), (1, 0, true), "{summary:?}");

        // The new catalog: untouched.
        let pending_b = with_catalog(&state, |c| c.list_pending_operations()).unwrap();
        assert_eq!(pending_b.len(), 1, "the new catalog's queued op was not removed");
        assert_eq!(
            with_catalog(&state, |c| c.photo_storage_status(b_ids[0])).unwrap(),
            StorageStatus::LocalOnly,
            "the new catalog's photo was not backed up"
        );
        assert_eq!(nas_files(&dir, "b"), 0, "nothing was copied to the new catalog's NAS");

        // The old catalog: op 0 completed and was recorded there; op 1 still waits.
        let a_again = Catalog::open_secondary(&dir.join("a/c.chairphoto"), &dir.join("a/photos")).unwrap();
        assert_eq!(a_again.photo_storage_status(a_ids[0]).unwrap(), StorageStatus::BackedUp);
        let pending_a = a_again.list_pending_operations().unwrap();
        assert_eq!(pending_a.iter().map(|o| o.photo_id).collect::<Vec<_>>(), vec![a_ids[1]]);
        assert_eq!(nas_files(&dir, "a"), 1);
    }

    /// A newer drain trips an older one, and a claim a switch tripped does nothing.
    #[test]
    fn a_newer_claim_or_a_switch_stops_a_drain_before_it_starts() {
        let dir = crate::test_support::TestTmpDir::new("reconcile-supersede");
        let (a, _ids) = catalog_with_queued_backups(&dir, "a", 1);
        let state = AppState::default();
        *state.catalog.lock().unwrap() = Some(a);
        let old = claim_reconcile(&state).unwrap();
        let _new = claim_reconcile(&state).unwrap();
        let summary = old.drain(&state).unwrap();
        assert!(summary.aborted && summary.ran == 0, "{summary:?}");

        let tripped = claim_reconcile(&state).unwrap();
        detach_catalog_and_trip_jobs(&state).unwrap();
        assert!(tripped.aborted());
        assert_eq!(tripped.apply_offload_policy().unwrap(), 0);
    }

    /// Relocate… bound to the catalog its id was read from: with that catalog open it moves
    /// the row and binds the new file's sidecar; once a switch has opened another catalog
    /// whose photo has the same id, it fails closed and that photo keeps its path.
    #[test]
    fn a_relocate_bound_to_another_catalog_fails_closed() {
        let dir = crate::test_support::TestTmpDir::new("relocate-bound");
        let open = |name: &str| {
            let root = dir.join(name).join("photos");
            std::fs::create_dir_all(root.join("old")).unwrap();
            std::fs::create_dir_all(root.join("new")).unwrap();
            let c = Catalog::open(&dir.join(name).join("c.chairphoto"), &root).unwrap();
            let old = root.join("old/p.jpg");
            std::fs::write(&old, "bytes").unwrap();
            let id = c.upsert_photo(&old, None, 1, 5).unwrap().id;
            let moved = root.join("new/p.jpg");
            std::fs::write(&moved, "bytes").unwrap();
            (c, id, moved)
        };
        let (a, a_id, a_moved) = open("a");
        let (b, b_id, b_moved) = open("b");
        assert_eq!(a_id, b_id, "the ids collide");
        let state = AppState::default();
        *state.catalog.lock().unwrap() = Some(a);
        let from_a = crate::app::catalog_identity(&state).unwrap();

        relocate_photo(&state, Some(from_a), a_id, &a_moved).unwrap();
        let photo = with_catalog(&state, |c| c.get_photo(a_id)).unwrap();
        assert_eq!(photo.path, "new/p.jpg");
        assert_eq!(crate::xmp::read_identifier(&a_moved).as_deref(), Some(photo.uuid.as_str()), "the sidecar is bound");

        detach_catalog_and_trip_jobs(&state).unwrap();
        publish_catalog_and_reset_jobs(&state, b).unwrap();
        let err = relocate_photo(&state, Some(from_a), b_id, &b_moved).unwrap_err();
        assert_eq!(err, crate::app::CATALOG_CHANGED);
        assert_eq!(with_catalog(&state, |c| c.get_photo(b_id)).unwrap().path, "old/p.jpg", "B's photo kept its path");
        assert!(!crate::xmp::sidecar_path(&b_moved).exists(), "no sidecar was written for B's file");
    }
}
