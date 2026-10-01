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
use std::sync::atomic::Ordering;

// ── Backup, offload, restore ─────────────────────────────────────────────────

/// Back a photo up to volume `backup_id`, idempotently.
///
/// If a verified backup already exists the image is not re-copied (over a flaky mount that
/// risks a good backup), but its companions are reconciled: every backup made before
/// companions existed has a verified image and no carried sidecars (#80).
pub fn backup_to(state: &AppState, photo_id: i64, backup_id: i64) -> Result<(), String> {
    let plan = || {
        with_catalog(state, |c| {
            let p = c.plan_backup(photo_id, backup_id)?;
            Ok((p.source, p.dest, p.rel, p.volume_id))
        })
    };
    if with_catalog(state, |c| c.has_verified_backup(photo_id))? {
        let (source, dest, _rel, volume_id) = plan()?;
        let carried = crate::catalog::carry_companions(&source, &dest).map_err(|e| e.to_string())?;
        return with_catalog(state, |c| {
            c.record_companions_at(photo_id, volume_id, LocationRole::Backup, &carried.carried)
        });
    }
    let (source, dest, rel, volume_id) = plan()?;
    // A copy is the image plus its declared companions; `copy_with_companions` is the one
    // place that knows the set.
    let outcome = crate::catalog::copy_with_companions(&source, &dest, None).map_err(|e| e.to_string())?;
    with_catalog(state, |c| c.record_copy(photo_id, volume_id, &rel, LocationRole::Backup, &outcome))
}

/// Free a photo's local copies, only after re-verifying its backup. Persists an id-keyed
/// thumbnail from a local copy first, so the photo stays visible once only the NAS copy
/// remains.
pub fn offload_photo(state: &AppState, photo_id: i64) -> Result<(), String> {
    let plan = with_catalog(state, |c| c.plan_offload(photo_id))?;
    let volume_ids = plan.local_volume_ids.clone();
    if let Some(local) = plan.local_files.first() {
        let _ = crate::thumbnails::ensure_persistent_thumb(photo_id, local);
    }
    let backup_location_id = plan.backup_location_id;
    let carried = crate::catalog::verify_and_delete_locals(&plan).map_err(|e| e.to_string())?;
    with_catalog(state, |c| {
        // Before `commit_offload`: it drops the local location rows, and companion rows
        // cascade with them.
        c.record_companions(backup_location_id, &carried)?;
        c.commit_offload(photo_id, &volume_ids)
    })
}

/// Pull a photo's backup copy back to local volume `local_id`, hash-verified, companions
/// included (a restored photo arrives with the edit state an offload moved home).
pub fn restore_to(state: &AppState, photo_id: i64, local_id: i64) -> Result<(), String> {
    let (source, dest, rel, volume_id, expected_hash) = with_catalog(state, |c| {
        let p = c.plan_restore(photo_id, local_id)?;
        Ok((p.source, p.dest, p.rel, p.volume_id, p.expected_hash))
    })?;
    let outcome = crate::catalog::copy_with_companions(&source, &dest, expected_hash.as_deref())
        .map_err(|e| e.to_string())?;
    with_catalog(state, |c| c.record_copy(photo_id, volume_id, &rel, LocationRole::LocalCache, &outcome))
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
pub fn apply_offload_policy(state: &AppState) -> Result<usize, String> {
    let age: i64 = with_catalog(state, |c| c.get_setting(OFFLOAD_AGE_SETTING))?
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    if age <= 0 {
        return Ok(0);
    }
    let candidates = with_catalog(state, |c| c.photos_eligible_for_offload(age))?;
    Ok(candidates.into_iter().filter(|&id| offload_photo(state, id).is_ok()).count())
}

// ── Reconcile ────────────────────────────────────────────────────────────────

/// Drain the reconcile queue (E4): run each pending op when a backup volume is reachable.
/// Clears each op on success, marks it failed (kept) otherwise. With no reachable backup it
/// does nothing (`skipped_offline`). The reachability stat runs off the catalog lock.
pub fn reconcile_now(state: &AppState) -> Result<DrainSummary, String> {
    let (vols, pending) = with_catalog(state, |c| Ok((c.volume_rows()?, c.list_pending_operations()?)))?;
    // Reconcile decides whether to run at all on the backup volume being reachable — it wants
    // live truth, so drop any cached state and stat fresh.
    state.volume_health.invalidate();
    let pairs: Vec<(i64, String)> = vols.iter().map(|v| (v.id, v.base_path.clone())).collect();
    let reachable = state.volume_health.refresh(&pairs);
    let backup_id = vols
        .iter()
        .find(|v| v.kind == VolumeKind::Backup && reachable.get(&v.id).copied().unwrap_or(false))
        .map(|v| v.id);
    let local_id = vols.iter().find(|v| v.kind == VolumeKind::Local).map(|v| v.id);

    let mut summary = DrainSummary::default();
    let Some(backup_id) = backup_id else {
        summary.skipped_offline = true;
        return Ok(summary);
    };
    for op in pending {
        let result = match op.kind.as_str() {
            "backup" => backup_to(state, op.photo_id, backup_id),
            "offload" => offload_photo(state, op.photo_id),
            "restore" => match local_id {
                Some(l) => restore_to(state, op.photo_id, l),
                None => Err("no local volume".into()),
            },
            other => Err(format!("unknown operation: {other}")),
        };
        match result {
            Ok(()) => {
                with_catalog(state, |c| c.remove_operation(op.id))?;
                summary.ran += 1;
            }
            Err(e) => {
                with_catalog(state, |c| c.set_operation_failed(op.id, &e))?;
                summary.failed += 1;
            }
        }
    }
    // Reconciliation moves files between volumes, so cached reachability could be stale.
    state.volume_health.invalidate();
    Ok(summary)
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
    if !confirm {
        return Err("emptying the trash needs an explicit confirmation".into());
    }
    // Own this run through the job protocol before touching anything. A catalog switch
    // trips every family, so an in-flight delete stops being an owner the moment the
    // catalog under it is replaced — which is what stops one catalog's photo ids being
    // applied to another's rows. `restore_photos` trips it too, so a user pulling a photo
    // back out of the trash wins that race.
    let abort = state.jobs.trash.install_fresh()?;
    let catalog = state.catalog.clone();
    let health = state.volume_health.clone();
    (move || {
        // 1. Under the lock: which photos, and where every copy of each one lives.
        let (candidates, plans, pairs) = {
            let guard = catalog.lock().map_err(|e| e.to_string())?;
            let c = guard.as_ref().ok_or("No catalog is open")?;
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
            if abort.load(Ordering::Relaxed) {
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
