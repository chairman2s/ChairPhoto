//! The storage lifecycle's jobs — the bodies of the Tauri `reconcile_now`,
//! `apply_offload_policy`, `backup_photo`, `offload_photo`, `restore_photo`,
//! `restore_photos` and `empty_trash` commands, and what the GPUI app runs for the back-up
//! queue, reconcile-on-focus and the Trash dialog.
//!
//! Every function here **blocks** (file copies over a possibly slow mount, hashing, volume
//! stats): run it on a worker, never a UI thread. Each operation is plan-under-lock → file IO
//! off the lock → record-under-lock, so a NAS copy never holds the catalog lock.
//!
//! "Nothing ever leaves home" is binding here — see `docs/storage-and-import.md`.

use super::{now_secs, with_catalog, AppState};
use crate::catalog::{Catalog, DrainSummary, LocationRole, VolumeKind};
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

/// Back a photo up to volume `backup_id`, idempotently.
///
/// If a verified backup already exists the image is not re-copied (over a flaky mount that
/// risks a good backup), but its companions are reconciled: every backup made before
/// companions existed has a verified image and no carried sidecars (#80).
pub fn backup_to(state: &AppState, photo_id: i64, backup_id: i64) -> Result<(), String> {
    backup_in(state, photo_id, backup_id)
}

fn backup_in(cat: &impl CatalogAccess, photo_id: i64, backup_id: i64) -> Result<(), String> {
    let plan = || {
        cat.with(|c| {
            let p = c.plan_backup(photo_id, backup_id)?;
            Ok((p.source, p.dest, p.rel, p.volume_id))
        })
    };
    if cat.with(|c| c.has_verified_backup(photo_id))? {
        let (source, dest, _rel, volume_id) = plan()?;
        let carried = crate::catalog::carry_companions(&source, &dest).map_err(|e| e.to_string())?;
        return cat.with(|c| {
            c.record_companions_at(photo_id, volume_id, LocationRole::Backup, &carried.carried)
        });
    }
    let (source, dest, rel, volume_id) = plan()?;
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
pub fn backup_photo_as(state: &AppState, expected: super::CatalogIdentity, photo_id: i64) -> Result<(), String> {
    let cat = bound(state, expected)?;
    let backup_id = cat.with(|c| single_volume_of_kind(c, VolumeKind::Backup, "backup"))?;
    backup_in(&cat, photo_id, backup_id)
}

/// Queue a backup of a photo read from the catalog `expected` names; `CATALOG_CHANGED` once
/// another is open.
pub fn enqueue_backup_as(state: &AppState, expected: super::CatalogIdentity, photo_id: i64) -> Result<(), String> {
    super::with_catalog_as(state, expected, |c| c.enqueue_operation("backup", photo_id)).map(drop)
}

/// [`offload_photo`] of a photo read from the catalog `expected` names (see [`bound`]).
pub fn offload_photo_as(state: &AppState, expected: super::CatalogIdentity, photo_id: i64) -> Result<(), String> {
    offload_in(&bound(state, expected)?, photo_id)
}

/// [`restore_photo`] of a photo read from the catalog `expected` names (see [`bound`]).
pub fn restore_photo_as(state: &AppState, expected: super::CatalogIdentity, photo_id: i64) -> Result<(), String> {
    let cat = bound(state, expected)?;
    let local_id = cat.with(|c| single_volume_of_kind(c, VolumeKind::Local, "local"))?;
    restore_in(&cat, photo_id, local_id)
}

/// Free a photo's local copies, only after re-verifying its backup. Persists an id-keyed
/// thumbnail from a local copy first, so the photo stays visible once only the NAS copy
/// remains.
pub fn offload_photo(state: &AppState, photo_id: i64) -> Result<(), String> {
    offload_in(state, photo_id)
}

fn offload_in(cat: &impl CatalogAccess, photo_id: i64) -> Result<(), String> {
    let plan = cat.with(|c| c.plan_offload(photo_id))?;
    let volume_ids = plan.local_volume_ids.clone();
    if let Some(local) = plan.local_files.first() {
        let _ = crate::thumbnails::ensure_persistent_thumb(photo_id, local);
    }
    let backup_location_id = plan.backup_location_id;
    let carried = crate::catalog::verify_and_delete_locals(&plan).map_err(|e| e.to_string())?;
    cat.with(|c| {
        // Before `commit_offload`: it drops the local location rows, and companion rows
        // cascade with them.
        c.record_companions(backup_location_id, &carried)?;
        c.commit_offload(photo_id, &volume_ids)
    })
}

/// Pull a photo's backup copy back to local volume `local_id`, hash-verified, companions
/// included (a restored photo arrives with the edit state an offload moved home).
pub fn restore_to(state: &AppState, photo_id: i64, local_id: i64) -> Result<(), String> {
    restore_in(state, photo_id, local_id)
}

fn restore_in(cat: &impl CatalogAccess, photo_id: i64, local_id: i64) -> Result<(), String> {
    let (source, dest, rel, volume_id, expected_hash) = cat.with(|c| {
        let p = c.plan_restore(photo_id, local_id)?;
        Ok((p.source, p.dest, p.rel, p.volume_id, p.expected_hash))
    })?;
    let outcome = crate::catalog::copy_with_companions(&source, &dest, expected_hash.as_deref())
        .map_err(|e| e.to_string())?;
    cat.with(|c| c.record_copy(photo_id, volume_id, &rel, LocationRole::LocalCache, &outcome))
}

/// Back up to the single backup volume. Errors when the NAS is offline; the UI queues a
/// backup op instead (drained on reconcile).
pub fn backup_photo(state: &AppState, photo_id: i64) -> Result<(), String> {
    let backup_id = with_catalog(state, |c| single_volume_of_kind(c, VolumeKind::Backup, "backup"))?;
    backup_to(state, photo_id, backup_id)
}

/// Restore to the single local volume.
pub fn restore_photo(state: &AppState, photo_id: i64) -> Result<(), String> {
    let local_id = with_catalog(state, |c| single_volume_of_kind(c, VolumeKind::Local, "local"))?;
    restore_to(state, photo_id, local_id)
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
            let result = match op.kind.as_str() {
                "backup" => backup_in(&cat, op.photo_id, backup_id),
                "offload" => offload_in(&cat, op.photo_id),
                "restore" => match local_id {
                    Some(l) => restore_in(&cat, op.photo_id, l),
                    None => Err("no local volume".into()),
                },
                other => Err(format!("unknown operation: {other}")),
            };
            match result {
                Ok(()) => {
                    cat.with(|c| c.remove_operation(op.id))?;
                    summary.ran += 1;
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
        for id in candidates {
            if self.aborted() {
                break;
            }
            if offload_in(&cat, id).is_ok() {
                offloaded += 1;
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
/// Split out of the command because this is where the destructive decision is made, and a
/// decision reachable only through a Tauri `State` is a decision nobody can test. Pure file
/// IO — no catalog lock — so it runs on the blocking worker like the rest of the lifecycle.
///
/// Returns the report and the ids whose files are now gone, for the caller to forget.
/// Walk the planned photos, destroying each one's copies — the part of emptying the trash
/// where ownership actually matters.
///
/// Split out because the two interleavings that make this dangerous are otherwise
/// reachable only through a Tauri `State`, and a race nobody can test is a race nobody has
/// checked. `still_trashed` is a callback so a test can make a photo come back mid-run the
/// way Restore does.
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
}
