//! Storage-lifecycle commands: the multi-tier location model (local cache + NAS +
//! backup), the backup/offload/restore operations, volume management, and the
//! pending-operation queue with its reconcile pass.
//!
//! "Nothing ever leaves home" is binding here — see `docs/storage-and-import.md`.

use super::*;
use std::collections::HashMap;
use tauri::State;

/// The reconcile queue — storage ops deferred until the NAS is reachable.
#[tauri::command]
pub fn list_pending_operations(
    state: State<'_, AppState>,
) -> Result<Vec<crate::catalog::PendingOperation>, String> {
    with_catalog(&state, |c| c.list_pending_operations())
}

/// Queue a deferred storage op (kind: "backup" | "offload" | "restore").
#[tauri::command]
pub fn enqueue_operation(
    state: State<'_, AppState>,
    kind: String,
    photo_id: i64,
) -> Result<i64, String> {
    with_catalog(&state, |c| c.enqueue_operation(&kind, photo_id))
}

// --- shared async op runners (used by the per-photo commands AND the drain) ---
// Each runs the E3 plan→IO→record split so the (possibly network) copy never holds
// the catalog lock or blocks the UI thread.

async fn do_backup(state: &State<'_, AppState>, photo_id: i64, backup_id: i64) -> Result<(), String> {
    // Idempotent: if a verified backup file already exists, do NOT re-copy it. Re-copying
    // a good backup is pointless and — over a flaky mount — risks destroying it. A missing
    // backup returns false here, so it's still (re-)created below.
    if with_catalog(state, |c| c.has_verified_backup(photo_id))? {
        return Ok(());
    }
    let (source, dest, rel, volume_id) = with_catalog(state, |c| {
        let p = c.plan_backup(photo_id, backup_id)?;
        Ok((p.source, p.dest, p.rel, p.volume_id))
    })?;
    let hash = tauri::async_runtime::spawn_blocking(move || {
        crate::catalog::copy_and_verify(&source, &dest, None)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    with_catalog(state, |c| c.record_backup(photo_id, volume_id, &rel, &hash))
}

async fn do_offload(state: &State<'_, AppState>, photo_id: i64) -> Result<(), String> {
    let plan = with_catalog(state, |c| c.plan_offload(photo_id))?;
    let volume_ids = plan.local_volume_ids.clone();
    // Persist an id-keyed thumbnail from a local copy BEFORE it's deleted, so the photo
    // stays visible in the grid once only the (possibly offline) NAS copy remains.
    if let Some(local) = plan.local_files.first().cloned() {
        let _ = tauri::async_runtime::spawn_blocking(move || {
            crate::thumbnails::ensure_persistent_thumb(photo_id, &local)
        })
        .await;
    }
    tauri::async_runtime::spawn_blocking(move || crate::catalog::verify_and_delete_locals(&plan))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    with_catalog(state, |c| c.commit_offload(photo_id, &volume_ids))
}

/// Setting key for the age-based offload policy ("keep last N days on local disk").
/// Empty / "0" = disabled (no automatic offload).
const OFFLOAD_AGE_SETTING: &str = "offload_age_days";

/// Apply the "keep last N days local" policy: offload every photo older than the
/// configured age that has a verified NAS backup, freeing local space while keeping it
/// visible (via the persistent thumbnail). No-op when the policy is unset or the NAS is
/// unreachable. Returns how many photos were offloaded.
#[tauri::command]
pub async fn apply_offload_policy(state: State<'_, AppState>) -> Result<usize, String> {
    let age: i64 = with_catalog(&state, |c| c.get_setting(OFFLOAD_AGE_SETTING))?
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    if age <= 0 {
        return Ok(0); // policy disabled
    }
    let candidates = with_catalog(&state, |c| c.photos_eligible_for_offload(age))?;
    let mut offloaded = 0usize;
    for id in candidates {
        if do_offload(&state, id).await.is_ok() {
            offloaded += 1;
        }
    }
    Ok(offloaded)
}

async fn do_restore(state: &State<'_, AppState>, photo_id: i64, local_id: i64) -> Result<(), String> {
    let (source, dest, rel, volume_id, expected_hash) = with_catalog(state, |c| {
        let p = c.plan_restore(photo_id, local_id)?;
        Ok((p.source, p.dest, p.rel, p.volume_id, p.expected_hash))
    })?;
    let hash = tauri::async_runtime::spawn_blocking(move || {
        crate::catalog::copy_and_verify(&source, &dest, expected_hash.as_deref())
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    with_catalog(state, |c| c.record_restore(photo_id, volume_id, &rel, &hash))
}

/// Back up a photo to the single backup volume. If the NAS is offline this errors;
/// the UI queues a backup op instead (drained on reconcile).
#[tauri::command]
pub async fn backup_photo(state: State<'_, AppState>, photo_id: i64) -> Result<(), String> {
    let backup_id =
        with_catalog(&state, |c| single_volume_of_kind(c, crate::catalog::VolumeKind::Backup, "backup"))?;
    do_backup(&state, photo_id, backup_id).await
}

/// Free a photo's local copies (only after re-verifying its backup). Off the UI thread.
#[tauri::command]
pub async fn offload_photo(state: State<'_, AppState>, photo_id: i64) -> Result<(), String> {
    do_offload(&state, photo_id).await
}

/// Forget a photo whose original is gone: delete its catalog row (and all dependent
/// records). Never deletes files on disk or on the NAS — only the catalog entry. For the
/// "Remove from catalog" option on a missing/black placeholder tile.
#[tauri::command]
pub fn remove_photo_from_catalog(state: State<'_, AppState>, photo_id: i64) -> Result<(), String> {
    with_catalog(&state, |c| c.remove_photo(photo_id))
}

/// Re-point a photo at a file the user moved to a new location (under the library root):
/// update its path/stats/primary-location, clear `missing`, and bind the file's sidecar
/// to the photo's UUID (merge-safe). For the "Relocate…" option. The new file must be
/// under the library root, else this errors with a clear message.
///
/// The relocation and the identity binding are one operation: if the sidecar can't be
/// bound the row still moves (the user asked for that, and the file is where they said),
/// but the debt is queued in `pending_sidecar_identity` for `repair_pending_identity`
/// instead of being logged and forgotten. Sidecar IO runs off the catalog lock — a
/// hung mount must not stall every other catalog user.
#[tauri::command]
pub async fn relocate_photo(
    state: State<'_, AppState>,
    photo_id: i64,
    new_path: String,
) -> Result<(), String> {
    let path = expand_home(&new_path);
    relocate_photo_in_state(&state, photo_id, path).await
}

async fn relocate_photo_in_state(
    state: &AppState,
    photo_id: i64,
    path: PathBuf,
) -> Result<(), String> {
    let target = path.clone();
    let bind_path = path.clone();
    let (uuid, db_path, root) = with_catalog_blocking(state, move |c| {
        let uuid = c.relocate_photo(photo_id, &target)?;
        Ok((uuid, c.db_path().to_path_buf(), c.root().to_path_buf()))
    })
    .await?;
    // The file usually already carries the UUID (its sidecar moved with it); a sidecar
    // holding somebody else's identity is left alone and recorded as a conflict.
    let outcome = tauri::async_runtime::spawn_blocking(move || {
        let found = crate::xmp::read_identifier(&bind_path);
        crate::catalog::bind_sidecar_identity(&bind_path, &uuid, found.as_deref())
    })
    .await
    .map_err(|e| e.to_string())?;
    record_identity_on_catalog(db_path, root, photo_id, path, outcome).await
}

/// Sidecar identity fields that are in the catalog but not (yet) in XMP: photo UUID
/// (`xmp:Identifier`) and import batch UUID (`chairphoto:ImportBatch`).
///
/// One row per COPY, not per (copy, field) — a copy owing both fields is one entry here
/// with both folded into `fields` — so `limit`/`offset` and this list's length are always
/// in the same unit `summarize_pending_identity`'s `total` counts. Bounded: the queue
/// reached 74,488 rows on the 100k harness shape in #20 (tens of MB of JSON if pulled
/// whole), so this always pages via `LIMIT`/`OFFSET` — the debt panel fetches one page at
/// a time and shows "showing N of M". There is no unbounded frontend-facing variant.
/// `Catalog::list_pending_identity()` returns the whole (flat, field-grain) queue instead,
/// off the IPC boundary — but it has no production caller today: the repair pass
/// (`Catalog::repair_pending_identity`, below) plans its own field-grained query
/// (`Catalog::plan_identity_repairs`), since each repair needs the target value bound
/// per field. `list_pending_identity()` is exercised only by this crate's tests.
#[tauri::command]
pub async fn list_pending_identity(
    state: State<'_, AppState>,
    limit: i64,
    offset: i64,
) -> Result<Vec<crate::catalog::PendingIdentity>, String> {
    with_catalog_blocking(&state, move |c| c.list_pending_identity_page(limit, offset)).await
}

/// Cheap counts (total debt + conflicts) over the pending-identity queue, for a summary
/// badge/header that shouldn't have to pull every row — the queue reached 74,488 rows on
/// the 100k harness shape in #20. Prefer this over `list_pending_identity().len()`.
#[tauri::command]
pub async fn summarize_pending_identity(
    state: State<'_, AppState>,
) -> Result<crate::catalog::PendingIdentitySummary, String> {
    with_catalog_blocking(&state, |c| c.summarize_pending_identity()).await
}

/// Retry every queued sidecar identity repair, clearing the copies that now succeed.
/// Runs the work on a secondary connection so the sidecar parsing/writing (possibly
/// over a slow mount) never holds the app's catalog lock. Copies whose file is
/// unreachable stay queued; so do sidecars that still can't be written or that carry a
/// conflicting photo identity — those need a human, and the queue remembers them.
#[tauri::command]
pub async fn repair_pending_identity(
    state: State<'_, AppState>,
) -> Result<crate::catalog::IdentityRepairSummary, String> {
    let (db_path, root) =
        with_catalog(&state, |c| Ok((c.db_path().to_path_buf(), c.root().to_path_buf())))?;
    tauri::async_runtime::spawn_blocking(move || {
        let catalog = Catalog::open_secondary(&db_path, &root).map_err(|e| e.to_string())?;
        catalog.repair_pending_identity().map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

async fn record_identity_on_catalog(
    db_path: PathBuf,
    root: PathBuf,
    photo_id: i64,
    target_path: PathBuf,
    outcome: crate::catalog::SidecarIdentity,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let catalog = Catalog::open_secondary(&db_path, &root).map_err(|e| e.to_string())?;
        catalog
            .record_sidecar_identity(photo_id, &target_path, &outcome)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
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
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(relocate_photo_in_state(&state, up.id, moved.clone()))
            .unwrap();

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

/// A catalog photo whose original is gone (for the cleanup preview/report).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnavailablePhoto {
    pub id: i64,
    pub path: String,
}

/// Preview: photos with no reachable, existing copy (and none possibly on an offline
/// volume). Drives the confirm dialog before removal. Reads files but no DB writes.
#[tauri::command]
pub async fn find_unavailable_photos(
    state: State<'_, AppState>,
) -> Result<Vec<UnavailablePhoto>, String> {
    let gone = with_catalog_blocking(&state, |c| c.find_unavailable_photos()).await?;
    Ok(gone
        .into_iter()
        .map(|(id, path)| UnavailablePhoto { id, path })
        .collect())
}

/// Remove every photo whose original is gone from both the library and the (reachable)
/// backup. Deletes only catalog rows — never any file. Returns what was removed.
#[tauri::command]
pub async fn purge_unavailable_photos(
    state: State<'_, AppState>,
) -> Result<Vec<UnavailablePhoto>, String> {
    let gone = with_catalog_blocking(&state, |c| c.purge_unavailable_photos()).await?;
    Ok(gone
        .into_iter()
        .map(|(id, path)| UnavailablePhoto { id, path })
        .collect())
}

/// Preview: photos whose only existing copy is a 0-byte (empty/corrupt) file. Reads files,
/// no DB writes.
#[tauri::command]
pub async fn find_empty_photos(
    state: State<'_, AppState>,
) -> Result<Vec<UnavailablePhoto>, String> {
    let gone = with_catalog_blocking(&state, |c| c.find_empty_photos()).await?;
    Ok(gone
        .into_iter()
        .map(|(id, path)| UnavailablePhoto { id, path })
        .collect())
}

/// Remove every photo whose image data is gone (all its copies are 0-byte files). Deletes
/// only catalog rows — never the empty files themselves. Returns what was removed.
#[tauri::command]
pub async fn purge_empty_photos(
    state: State<'_, AppState>,
) -> Result<Vec<UnavailablePhoto>, String> {
    let gone = with_catalog_blocking(&state, |c| c.purge_empty_photos()).await?;
    Ok(gone
        .into_iter()
        .map(|(id, path)| UnavailablePhoto { id, path })
        .collect())
}

/// Restore a photo's backup copy to the single local volume, hash-verified.
#[tauri::command]
pub async fn restore_photo(state: State<'_, AppState>, photo_id: i64) -> Result<(), String> {
    let local_id =
        with_catalog(&state, |c| single_volume_of_kind(c, crate::catalog::VolumeKind::Local, "local"))?;
    do_restore(&state, photo_id, local_id).await
}

/// Drain the reconcile queue (E4): run each pending op via the E3 lifecycle when a
/// backup volume is reachable, off the UI thread. Clears each op on success, marks it
/// failed (kept) otherwise. With no reachable backup it does nothing (skippedOffline).
#[tauri::command]
pub async fn reconcile_now(
    state: State<'_, AppState>,
) -> Result<crate::catalog::DrainSummary, String> {
    use crate::catalog::{DrainSummary, VolumeKind};
    // Pure SQL under the lock; the reachability stat happens off-lock below so a hung
    // NAS mount can't stall every other catalog user for its duration.
    let (vols, pending) = with_catalog(&state, |c| Ok((c.volume_rows()?, c.list_pending_operations()?)))?;
    // Reconcile decides whether to run at all based on the backup volume being reachable —
    // it wants live truth, so drop any cached state and stat fresh (on a blocking worker).
    state.volume_health.invalidate();
    let pairs: Vec<(i64, String)> = vols.iter().map(|v| (v.id, v.base_path.clone())).collect();
    let health = state.volume_health.clone();
    let reachable = tauri::async_runtime::spawn_blocking(move || health.refresh(&pairs))
        .await
        .map_err(|e| e.to_string())?;
    let backup_id = vols
        .iter()
        .find(|v| v.kind == VolumeKind::Backup && reachable.get(&v.id).copied().unwrap_or(false))
        .map(|v| v.id);
    let local_id = vols.iter().find(|v| v.kind == VolumeKind::Local).map(|v| v.id);

    let mut summary = DrainSummary::default();
    let backup_id = match backup_id {
        Some(id) => id,
        None => {
            summary.skipped_offline = true;
            return Ok(summary);
        }
    };
    for op in pending {
        let result = match op.kind.as_str() {
            "backup" => do_backup(&state, op.photo_id, backup_id).await,
            "offload" => do_offload(&state, op.photo_id).await,
            "restore" => match local_id {
                Some(l) => do_restore(&state, op.photo_id, l).await,
                None => Err("no local volume".into()),
            },
            other => Err(format!("unknown operation: {other}")),
        };
        match result {
            Ok(()) => {
                with_catalog(&state, |c| c.remove_operation(op.id))?;
                summary.ran += 1;
            }
            Err(e) => {
                with_catalog(&state, |c| c.set_operation_failed(op.id, &e))?;
                summary.failed += 1;
            }
        }
    }
    // Reconciliation moves files between volumes (backup/offload/restore), so the cached
    // reachability could be stale — drop it.
    state.volume_health.invalidate();
    Ok(summary)
}

/// The id of the single volume of a kind, or an error if there are zero or many.
fn single_volume_of_kind(
    c: &Catalog,
    kind: crate::catalog::VolumeKind,
    label: &str,
) -> crate::catalog::Result<i64> {
    let ids: Vec<i64> = c
        .list_volumes()?
        .into_iter()
        .filter(|v| v.kind == kind)
        .map(|v| v.id)
        .collect();
    match ids.as_slice() {
        [one] => Ok(*one),
        [] => Err(crate::catalog::CatalogError::Validation(format!(
            "no {label} volume configured"
        ))),
        _ => Err(crate::catalog::CatalogError::Validation(format!(
            "multiple {label} volumes — choosing one isn't supported yet"
        ))),
    }
}

/// List storage volumes, each flagged with whether it's currently reachable.
#[tauri::command]
pub fn list_volumes(state: State<'_, AppState>) -> Result<Vec<Volume>, String> {
    with_catalog(&state, |c| c.list_volumes())
}

/// Register a named storage volume (e.g. a NAS mount). `basePath` is this machine's
/// mount point; a leading "~" is expanded to $HOME. `kind` is "local" or "backup".
#[tauri::command]
pub fn add_volume(
    state: State<'_, AppState>,
    name: String,
    base_path: String,
    kind: crate::catalog::VolumeKind,
) -> Result<i64, String> {
    let expanded = expand_home(&base_path);
    let id = with_catalog(&state, |c| c.add_volume(&name, &expanded, kind))?;
    // A new volume changes the set to stat — drop cached reachability.
    state.volume_health.invalidate();
    Ok(id)
}

/// Storage status (local-only / backed-up / archived / offline / missing) for many
/// photos at once — for grid indicators. Returns `[photoId, status]` pairs.
#[tauri::command]
pub async fn photo_statuses(
    state: State<'_, AppState>,
    photo_ids: Vec<i64>,
) -> Result<Vec<(i64, crate::catalog::StorageStatus)>, String> {
    // Hand-rolled (rather than `with_catalog_blocking`) because it needs to lock →
    // release → stat → lock again: the volume stats must happen OFF the catalog lock so
    // a slow/offline NAS can't serialize the whole app.
    let catalog = state.catalog.clone();
    let health = state.volume_health.clone();
    tauri::async_runtime::spawn_blocking(move || {
        // 1. Under the lock: pull the (id, base_path) pairs (pure SQL, no stats).
        let pairs: Vec<(i64, String)> = {
            let guard = catalog.lock().map_err(|e| e.to_string())?;
            let c = guard.as_ref().ok_or("No catalog is open")?;
            grid_status_volume_pairs(c).map_err(|e| e.to_string())?
        };
        // 2. Off the lock, on this worker: stat (or reuse cached) reachability.
        let reachable = health.refresh(&pairs);
        // 3. Back under the lock: derive statuses using the off-lock reachability.
        let guard = catalog.lock().map_err(|e| e.to_string())?;
        let c = guard.as_ref().ok_or("No catalog is open")?;
        grid_photo_statuses_with_reachability(c, &photo_ids, &reachable).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

pub(crate) fn grid_status_volume_pairs(c: &Catalog) -> crate::catalog::Result<Vec<(i64, String)>> {
    Ok(c.volume_rows()?
        .into_iter()
        .map(|v| (v.id, v.base_path))
        .collect())
}

pub(crate) fn grid_photo_statuses_with_reachability(
    c: &Catalog,
    photo_ids: &[i64],
    reachable: &HashMap<i64, bool>,
) -> crate::catalog::Result<Vec<(i64, crate::catalog::StorageStatus)>> {
    c.photo_storage_statuses(photo_ids, reachable)
}

/// Remove a storage volume registration (not the default catalog-root volume). This
/// forgets the catalog's location pointers on that volume; it never deletes files.
#[tauri::command]
pub fn remove_volume(state: State<'_, AppState>, volume_id: i64) -> Result<(), String> {
    with_catalog(&state, |c| c.remove_volume(volume_id))?;
    // A removed volume no longer belongs in the reachability cache.
    state.volume_health.invalidate();
    Ok(())
}

/// The physical locations recorded for a photo (where its bytes live).
#[tauri::command]
pub fn get_photo_locations(
    state: State<'_, AppState>,
    photo_id: i64,
) -> Result<Vec<PhotoLocation>, String> {
    with_catalog(&state, |c| c.photo_locations(photo_id))
}
