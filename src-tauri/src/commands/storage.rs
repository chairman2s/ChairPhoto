//! Storage-lifecycle commands: the multi-tier location model (local cache + NAS +
//! backup), the backup/offload/restore operations, volume management, and the
//! pending-operation queue with its reconcile pass.
//!
//! "Nothing ever leaves home" is binding here — see `docs/storage-and-import.md`.

use super::*;
use crate::app::storage;
use crate::app::storage::EmptyTrashReport;
#[cfg(test)]
use crate::app::storage::{delete_one_photos_copies, destroy_planned_photos, DeleteOutcome};
use tauri::State;

/// The reconcile queue — storage ops deferred until the NAS is reachable.
#[tauri::command(async)]
pub fn list_pending_operations(
    state: State<'_, AppState>,
) -> Result<Vec<crate::catalog::PendingOperation>, String> {
    with_catalog(&state, |c| c.list_pending_operations())
}

/// Queue a deferred storage op (kind: "backup" | "offload" | "restore").
#[tauri::command(async)]
pub fn enqueue_operation(
    state: State<'_, AppState>,
    kind: String,
    photo_id: i64,
) -> Result<i64, String> {
    with_catalog(&state, |c| c.enqueue_operation(&kind, photo_id))
}

/// Queue an operation for many photos at once — the safety panel's batch action.
///
/// Returns how many were newly queued. Nothing is copied here: the existing reconcile
/// drain does the work when the NAS is reachable, and reports its own progress, so a
/// batch enqueue over an offline NAS is a promise kept later rather than an error now.
#[tauri::command]
pub async fn enqueue_operations(
    state: State<'_, AppState>,
    kind: String,
    photo_ids: Vec<i64>,
) -> Result<usize, String> {
    with_catalog_blocking(&state, move |c| c.enqueue_operations(&kind, &photo_ids)).await
}

// --- the storage jobs (`app::storage`, shared with the GPUI app) ---

/// Run one of `app::storage`'s blocking jobs on the blocking pool.
async fn run_storage<T: Send + 'static>(
    state: &State<'_, AppState>,
    job: impl FnOnce(&AppState) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let state = state.inner().clone();
    crate::app::spawn_blocking(move || job(&state)).await.map_err(|e| e.to_string())?
}
// ── Trash (cluster B, B2) ────────────────────────────────────────────────────

/// Move photos to the trash, taking each one's stack with it. Touches no bytes.
#[tauri::command]
pub async fn trash_photos(
    state: State<'_, AppState>,
    photo_ids: Vec<i64>,
) -> Result<crate::catalog::TrashSummary, String> {
    with_catalog_blocking(&state, move |c| c.trash_photos(&photo_ids)).await
}

/// Bring photos back, along with whatever was trashed in the same act. Trips the trash job
/// generation first, so an `empty_trash` already walking the filesystem stands down
/// (`app::storage::restore_trashed`).
#[tauri::command]
pub async fn restore_photos(
    state: State<'_, AppState>,
    photo_ids: Vec<i64>,
) -> Result<usize, String> {
    run_storage(&state, move |s| storage::restore_trashed(s, &photo_ids)).await
}

/// Everything in the trash, most recently trashed first.
#[tauri::command]
pub async fn list_trash(state: State<'_, AppState>) -> Result<Vec<Photo>, String> {
    with_catalog_blocking(&state, |c| c.list_trash()).await
}

/// Destroy trashed photos: the only path in the app that deletes an original. Gated twice —
/// `confirm` must be true, and every known copy must be reachable; the body and its
/// reasoning are `app::storage::empty_trash`.
#[tauri::command]
pub async fn empty_trash(
    state: State<'_, AppState>,
    photo_ids: Option<Vec<i64>>,
    older_than_days: Option<i64>,
    confirm: bool,
) -> Result<EmptyTrashReport, String> {
    run_storage(&state, move |s| storage::empty_trash(s, photo_ids, older_than_days, confirm)).await
}
/// Library-wide safety counts for the at-risk panel (cluster B, B1).
///
/// Pure SQL — deliberately never stats a volume, so an unmounted NAS cannot make this hang
/// or fail. That is also why freshness is recorded by the scanner rather than measured
/// here, and why `companionsUnchecked` matters: the `stale` count is a floor while it is
/// non-zero, and the panel has to say so rather than implying it is a total.
#[tauri::command]
pub async fn library_safety_summary(
    state: State<'_, AppState>,
) -> Result<crate::catalog::SafetySummary, String> {
    with_catalog_blocking(&state, |c| c.library_safety_summary()).await
}

/// One photo's safety bucket, for the inspector.
#[tauri::command]
pub async fn photo_safety_status(
    state: State<'_, AppState>,
    photo_id: i64,
) -> Result<crate::catalog::SafetyStatus, String> {
    with_catalog_blocking(&state, move |c| c.photo_safety_status(photo_id)).await
}

/// Apply the "keep last N days local" policy (`app::storage::apply_offload_policy`): offload
/// every photo older than the configured age that has a verified NAS backup. No-op when the
/// policy is unset or the NAS is unreachable. Returns how many photos were offloaded.
#[tauri::command]
pub async fn apply_offload_policy(state: State<'_, AppState>) -> Result<usize, String> {
    run_storage(&state, storage::apply_offload_policy).await
}

/// Back up a photo to the single backup volume. If the NAS is offline this errors;
/// the UI queues a backup op instead (drained on reconcile).
#[tauri::command]
pub async fn backup_photo(state: State<'_, AppState>, photo_id: i64) -> Result<(), String> {
    run_storage(&state, move |s| storage::backup_photo(s, photo_id)).await
}

/// Free a photo's local copies (only after re-verifying its backup). Off the UI thread.
#[tauri::command]
pub async fn offload_photo(state: State<'_, AppState>, photo_id: i64) -> Result<(), String> {
    run_storage(&state, move |s| storage::offload_photo(s, photo_id)).await
}

/// Forget a photo whose original is gone: delete its catalog row (and all dependent
/// records). Never deletes files on disk or on the NAS — only the catalog entry. For the
/// "Remove from catalog" option on a missing/black placeholder tile.
#[tauri::command(async)]
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
    let outcome = crate::app::spawn_blocking(move || {
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
///
/// `includeDismissed` switches the page from the active queue to every copy in it,
/// including the ones a human dismissed (#33) — the only way back to a dismissal, so it is
/// paired with Restore, not offered as a bare "show more".
#[tauri::command]
pub async fn list_pending_identity(
    state: State<'_, AppState>,
    limit: i64,
    offset: i64,
    include_dismissed: bool,
) -> Result<Vec<crate::catalog::PendingIdentity>, String> {
    with_catalog_blocking(&state, move |c| {
        c.list_pending_identity_page(limit, offset, include_dismissed)
    })
    .await
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

// ── The identity repair job (#34) ────────────────────────────────────────────
//
// The bodies are the core's `app::identity` (the GPUI identity-debt panel runs the same).

/// The claim, by its old path: the ownership tests below drive the start exactly as the
/// command does.
#[cfg(test)]
use crate::app::identity::begin_identity_repair_job;

/// Start a pass over the queued sidecar identity repairs, clearing the copies that now
/// succeed. Returns the new pass's **job id**; the result arrives as `identity:repair_done`.
///
/// Copies whose file is unreachable stay queued; so do sidecars that still can't be written
/// or that carry a conflicting photo identity — those need a human (#33). The claim
/// (`app::identity::claim_identity_repair`) snapshots the catalog, trips any previous pass
/// and claims the status slot as one transition; the pass itself runs on a blocking worker
/// on a secondary connection. A newer pass, `identity_repair_cancel` or a catalog switch
/// stops it; its events carry the job id.
#[tauri::command]
pub async fn repair_pending_identity(state: State<'_, AppState>) -> Result<u64, String> {
    let pass = crate::app::identity::claim_identity_repair(state.inner())?;
    let job = pass.job;
    crate::app::spawn_blocking(move || pass.run());
    Ok(job)
}

/// Trip the abort flag of any running identity repair pass. The pass stops before its next
/// copy, so a pass against an unmounted NAS costs one more file's timeout rather than the
/// rest of the queue's. No-op when nothing is running.
#[tauri::command]
pub async fn identity_repair_cancel(state: State<'_, AppState>) -> Result<(), String> {
    crate::app::identity::cancel_identity_repair(state.inner())
}

/// The live status of the running identity repair pass, or `None` when idle — so the debt
/// panel re-attaches to a pass in flight instead of reopening as if nothing were happening.
#[tauri::command]
pub async fn identity_repair_status(
    state: State<'_, AppState>,
) -> Result<Option<IdentityRepairJobStatus>, String> {
    crate::app::identity::identity_repair_status(state.inner())
}

/// Resolve one conflicted copy the way the user decided (#33): `adopt` the identifier the
/// file already carries, `overwrite` the file with the catalog's, `dismiss` the copy, or
/// `restore` a dismissed one. There is no default — the action is required, and an
/// unrecognised one fails to deserialize rather than falling back to the destructive path.
/// Runs on a blocking worker on a secondary connection (`app::identity`).
#[tauri::command]
pub async fn resolve_identity_conflict(
    state: State<'_, AppState>,
    photo_id: i64,
    volume_id: i64,
    relative_path: String,
    action: crate::catalog::IdentityConflictAction,
) -> Result<crate::catalog::IdentityConflictOutcome, String> {
    let state = state.inner().clone();
    crate::app::spawn_blocking(move || {
        crate::app::identity::resolve_identity_conflict(&state, photo_id, volume_id, &relative_path, action)
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
    crate::app::spawn_blocking(move || {
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

#[cfg(test)]
mod identity_repair_ownership_tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex;
    use crate::commands::catalog::{detach_catalog_and_trip_jobs, publish_catalog_and_reset_jobs};
    use crate::catalog::SidecarIdentity;
    use std::path::Path;

    /// A catalog with `photos` files, each already owing its sidecar identity — the queue a
    /// repair pass exists to work through. Every file is reachable and its sidecar carries no
    /// identifier, so a pass that reaches a row BINDS it, which is what makes "did the
    /// aborted worker keep going?" observable on disk.
    fn temp_catalog_with_debt(
        tag: &str,
        photos: usize,
    ) -> (Catalog, crate::test_support::TestSubPath, PathBuf) {
        let dir = crate::test_support::TestTmpDir::new(&format!("identity-own-{tag}"));
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let db = dir.join("catalog.chairphoto");
        let catalog = Catalog::open(&db, &root).unwrap();
        for i in 0..photos {
            let p = root.join(format!("p{i}.arw"));
            std::fs::write(&p, b"raw").unwrap();
            let up = catalog.upsert_photo(&p, None, 1, 3).unwrap();
            catalog
                .record_sidecar_identity(up.id, &p, &SidecarIdentity::Unreachable)
                .unwrap();
        }
        (catalog, dir.into_subpath("catalog.chairphoto"), root)
    }

    fn state_with(catalog: Catalog) -> AppState {
        let state = AppState::default();
        *state.catalog.lock().unwrap() = Some(catalog);
        state
    }

    /// Un-dismissed queue rows in the catalog at `db`, read on a connection of this test's
    /// own — so it reports what is durably in the file, not what some handle believes.
    fn queued_rows(db: &Path) -> i64 {
        let conn = rusqlite::Connection::open(db).unwrap();
        conn.query_row(
            "SELECT count(*) FROM pending_sidecar_identity WHERE dismissed_at = 0",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
    }

    /// **Forced race.** A catalog switch stops a running repair pass at its next row: no
    /// further sidecar written, no further queue row cleared in the catalog the user left,
    /// and nothing at all in the catalog they switched to.
    ///
    /// This is the AGENTS.md invariant the pass was the last command to violate — before
    /// #34 it held its own secondary connection to the old database and nothing could trip
    /// it, so a pass started against a 74k-row queue kept writing sidecars for a catalog the
    /// app had already closed.
    ///
    /// The interleaving is forced, not timed: the switch runs inside the pass's own progress
    /// callback, so it lands between copy 1 and copy 2 every time.
    #[test]
    fn a_catalog_switch_stops_a_running_repair_pass() {
        let (cat_a, db_a, root_a) = temp_catalog_with_debt("switch-a", 4);
        let (cat_b, db_b, _root_b) = temp_catalog_with_debt("switch-b", 0);
        let state = state_with(cat_a);

        // Start the pass exactly the way `repair_pending_identity` does.
        let JobClaim { db_path, root, abort, job, slot } =
            begin_identity_repair_job(&state).unwrap();
        assert_eq!(db_path, db_a.to_path_buf());
        assert_eq!(root, root_a);
        assert_eq!(
            state.jobs.identity.status().unwrap().map(|s| s.job),
            Some(job),
            "the start must claim the status slot before the command returns"
        );

        let sec = Catalog::open_secondary(&db_path, &root).unwrap();
        let switch_to = Mutex::new(Some(cat_b));
        let mut progress: Vec<usize> = Vec::new();
        let summary = sec
            .run_identity_repair(&abort, |s| {
                progress.push(s.done());
                slot.publish(|job| IdentityRepairJobStatus {
                    job,
                    done: s.done(),
                    total: s.total,
                });
                // The user switches catalogs while this worker is still running.
                if let Some(cat) = switch_to.lock().unwrap().take() {
                    detach_catalog_and_trip_jobs(&state).unwrap();
                    publish_catalog_and_reset_jobs(&state, cat).unwrap();
                }
            })
            .unwrap();

        assert!(summary.aborted, "the switch must abort the running pass");
        assert_eq!(summary.total, 4, "all four copies were queued");
        assert_eq!(
            (summary.bound, summary.done()),
            (1, 1),
            "only the copy already recorded when the switch happened: {summary:?}"
        );
        assert_eq!(progress, vec![1], "no progress after the switch");
        assert_eq!(
            queued_rows(&db_a),
            3,
            "the aborted worker must clear no further rows in the catalog it was repairing"
        );
        assert!(
            crate::xmp::read_identifier(&root_a.join("p0.arw")).is_some(),
            "sanity: the pass really did bind the copy it reached"
        );
        for i in 1..4 {
            assert!(
                crate::xmp::read_identifier(&root_a.join(format!("p{i}.arw"))).is_none(),
                "the aborted worker wrote a sidecar for copy {i} after the switch"
            );
        }
        assert_eq!(queued_rows(&db_b), 0, "nothing may land in the catalog switched to");

        // The switch made the pass unreachable as an OWNER, not just abortable: its slot was
        // cleared, and the straggler it published above could not put it back.
        assert!(
            state.jobs.identity.status().unwrap().is_none(),
            "phase one must clear the status slot, or the debt panel re-adopts a dead pass"
        );
        assert!(!slot.owns());

        // The old flag stays tripped and the new catalog's generation is a different,
        // un-tripped Arc, so no Cancel or later switch can revive the old worker.
        assert!(abort.load(Ordering::Relaxed), "the old flag must stay tripped");
        let installed = state.jobs.identity.installed().unwrap();
        assert!(
            !Arc::ptr_eq(&installed, &abort),
            "the switch must install a fresh generation, not reuse the aborted one"
        );
        assert!(!installed.load(Ordering::Relaxed));
    }

    /// **Forced race.** `identity_repair_cancel` stops the pass at its next row — the point
    /// of the whole exercise for a pass against an unmounted NAS, where every remaining row
    /// costs a mount timeout.
    ///
    /// Cancel runs inside the pass's own progress callback, so it lands between copy 1 and
    /// copy 2 every time rather than whenever a test thread happens to get scheduled.
    #[test]
    fn a_cancel_stops_the_running_repair_pass_at_its_next_copy() {
        let (cat, db, root) = temp_catalog_with_debt("cancel", 5);
        let state = state_with(cat);
        let JobClaim { db_path, root: claim_root, abort, job: _, slot: _ } =
            begin_identity_repair_job(&state).unwrap();

        let sec = Catalog::open_secondary(&db_path, &claim_root).unwrap();
        let cancelled = std::cell::Cell::new(false);
        let summary = sec
            .run_identity_repair(&abort, |_| {
                if !cancelled.get() {
                    cancelled.set(true);
                    // Exactly what the Cancel command does.
                    state.jobs.identity.cancel().unwrap();
                }
            })
            .unwrap();

        assert!(cancelled.get(), "the fixture never got to cancel");
        assert!(summary.aborted, "a cancelled pass must say it was cancelled: {summary:?}");
        assert_eq!(summary.done(), 1, "the pass ran on past the cancel: {summary:?}");
        assert_eq!(queued_rows(&db), 4, "four copies must be left for the next pass");
        assert!(
            crate::xmp::read_identifier(&root.join("p4.arw")).is_none(),
            "a cancelled pass must not keep writing sidecars"
        );
    }

    /// A second pass supersedes the first rather than running both at the queue: the first
    /// is tripped, loses the status slot, and its handle knows it.
    #[test]
    fn a_second_repair_pass_trips_the_first_and_takes_the_slot() {
        let (cat, _db, _root) = temp_catalog_with_debt("supersede", 2);
        let state = state_with(cat);

        let first = begin_identity_repair_job(&state).unwrap();
        let second = begin_identity_repair_job(&state).unwrap();

        assert!(first.abort.load(Ordering::Relaxed), "the superseded pass must be tripped");
        assert!(!second.abort.load(Ordering::Relaxed));
        assert_ne!(first.job, second.job, "the two passes must be told apart by id");
        assert!(!first.slot.owns());
        assert!(second.slot.owns());
        assert_eq!(state.jobs.identity.status().unwrap().map(|s| s.job), Some(second.job));

        // And the superseded pass's straggler cannot overwrite the running one's slot.
        first.slot.publish(|job| IdentityRepairJobStatus { job, done: 99, total: 99 });
        assert_eq!(
            state.jobs.identity.status().unwrap().map(|s| (s.job, s.done)),
            Some((second.job, 0)),
            "a superseded pass republished over the running one"
        );
    }

    /// A repair pass started with no catalog open must touch nothing — no generation
    /// installed, no job id consumed, no slot claimed. This is the state a start blocked
    /// between the two switch phases observes.
    #[test]
    fn a_repair_start_with_no_catalog_open_touches_nothing() {
        let state = AppState::default();
        let before = state.jobs.identity.installed().unwrap();

        let err = begin_identity_repair_job(&state).unwrap_err();

        assert_eq!(err, "No catalog is open");
        assert!(Arc::ptr_eq(&before, &state.jobs.identity.installed().unwrap()));
        assert_eq!(state.jobs.identity.abort().job_ids_issued(), 0);
        assert!(state.jobs.identity.status().unwrap().is_none());
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
    run_storage(&state, move |s| storage::restore_photo(s, photo_id)).await
}

/// Drain the reconcile queue (E4): run each pending op via the E3 lifecycle when a backup
/// volume is reachable, off the UI thread (`app::storage::reconcile_now`). Clears each op on
/// success, marks it failed (kept) otherwise. With no reachable backup it does nothing
/// (skippedOffline).
#[tauri::command]
pub async fn reconcile_now(state: State<'_, AppState>) -> Result<crate::catalog::DrainSummary, String> {
    run_storage(&state, storage::reconcile_now).await
}

/// List storage volumes, each flagged with whether it's currently reachable.
#[tauri::command(async)]
pub fn list_volumes(state: State<'_, AppState>) -> Result<Vec<Volume>, String> {
    with_catalog(&state, |c| c.list_volumes())
}

/// Register a named storage volume (e.g. a NAS mount). `basePath` is this machine's
/// mount point; a leading "~" is expanded to $HOME. `kind` is "local" or "backup".
#[tauri::command(async)]
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
    crate::app::spawn_blocking(move || {
        // 1. Under the lock: pull the (id, base_path) pairs (pure SQL, no stats).
        let pairs: Vec<(i64, String)> = {
            let guard = catalog.lock().map_err(|e| e.to_string())?;
            let c = guard.as_ref().ok_or("No catalog is open")?;
            c.volume_base_paths().map_err(|e| e.to_string())?
        };
        // 2. Off the lock, on this worker: stat (or reuse cached) reachability.
        let reachable = health.refresh(&pairs);
        // 3. Back under the lock: derive statuses using the off-lock reachability.
        let guard = catalog.lock().map_err(|e| e.to_string())?;
        let c = guard.as_ref().ok_or("No catalog is open")?;
        c.photo_storage_statuses(&photo_ids, &reachable).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Remove a storage volume registration (not the default catalog-root volume). This
/// forgets the catalog's location pointers on that volume; it never deletes files.
#[tauri::command(async)]
pub fn remove_volume(state: State<'_, AppState>, volume_id: i64) -> Result<(), String> {
    with_catalog(&state, |c| c.remove_volume(volume_id))?;
    // A removed volume no longer belongs in the reachability cache.
    state.volume_health.invalidate();
    Ok(())
}

/// The physical locations recorded for a photo (where its bytes live).
#[tauri::command(async)]
pub fn get_photo_locations(
    state: State<'_, AppState>,
    photo_id: i64,
) -> Result<Vec<PhotoLocation>, String> {
    with_catalog(&state, |c| c.photo_locations(photo_id))
}

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
