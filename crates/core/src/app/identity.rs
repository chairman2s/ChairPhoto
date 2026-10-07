//! The sidecar-identity repair pass (#34), conflict resolution (#33) and the bulk resolution of
//! non-UUID conflicts (#150, [`claim_resolve_foreign_conflicts`], its own job family) — the
//! bodies the GPUI identity-debt panel runs.
//!
//! The pass is a job (`JobRegistry::identity`): [`claim_identity_repair`] takes ownership
//! (catalog → abort → slot, one transition) and returns an [`IdentityRepairPass`] whose
//! [`run`](IdentityRepairPass::run) does the work on the caller's worker. The pass reports
//! through the state's event sink — `identity:repair_progress` (throttled, every event
//! carries the job id) and the required terminal `identity:repair_done` — and clears its
//! status slot before that terminal event, and only while it still owns it.

use super::jobs::{JobClaim, JobSlot};
use super::{
    AppState, CoreEvent, EventSink, IdentityRepairDone, IdentityRepairJobStatus, IdentityRepairProgress,
    IdentityResolveDone, IdentityResolveJobStatus, IdentityResolveProgress,
};
use crate::catalog::Catalog;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often the pass emits `identity:repair_progress`.
///
/// Time-based, not every-Nth-row: rows differ by four orders of magnitude in cost (a local
/// sidecar already carrying its identity versus a file on an unmounted NAS at its timeout),
/// so any row count is either tens of thousands of events on a fast local queue or a frozen
/// bar on a slow remote one. The final row always emits regardless.
pub const REPAIR_PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// Claim ownership of the identity repair pass: snapshot the catalog, allocate the job id,
/// trip the previous pass, install this one's abort flag and claim the status slot as ONE
/// transition, holding catalog → abort → slot throughout ([`super::jobs::JobFamily::begin`]).
/// `total` is not known until the worker has counted the queue; claiming with `0` is what
/// lets a status query between the claim and the first progress event see the pass running.
pub fn begin_identity_repair_job(state: &AppState) -> Result<JobClaim<IdentityRepairJobStatus>, String> {
    state.jobs.identity.begin(&state.catalog, |job| IdentityRepairJobStatus { job, done: 0, total: 0 })
}

/// A claimed repair pass, not yet running. Dropping it without [`run`](Self::run) would
/// leave the status slot claimed, so every caller runs it.
#[must_use = "a claimed pass holds the status slot until it runs"]
pub struct IdentityRepairPass {
    state: AppState,
    db_path: PathBuf,
    root: PathBuf,
    abort: Arc<AtomicBool>,
    slot: JobSlot<IdentityRepairJobStatus>,
    /// The pass's job id: every event it sends carries it.
    pub job: u64,
}

/// Start a pass over the queued sidecar identity repairs: claim it. Starting a second pass
/// supersedes the first rather than running both at the queue. Takes the catalog lock
/// briefly — not on a UI thread.
pub fn claim_identity_repair(state: &AppState) -> Result<IdentityRepairPass, String> {
    let JobClaim { db_path, root, abort, job, slot } = begin_identity_repair_job(state)?;
    Ok(IdentityRepairPass { state: state.clone(), db_path, root, abort, slot, job })
}

impl IdentityRepairPass {
    /// Run the pass to its end on a secondary connection, so sidecar IO (a network round
    /// trip per copy on a NAS) never holds the app's catalog lock. Blocking. Always ends with
    /// `identity:repair_done` for this job, after releasing the status slot.
    pub fn run(self) {
        let IdentityRepairPass { state, db_path, root, abort, slot, job } = self;
        // Release the slot — only if a newer pass hasn't claimed it — BEFORE the terminal
        // event: see `JobSlot::clear`.
        let finish = |summary: crate::catalog::IdentityRepairSummary, error: Option<String>| {
            slot.clear();
            state.send(CoreEvent::IdentityRepairDone(IdentityRepairDone { ok: error.is_none(), job, summary, error }));
        };
        let catalog = match Catalog::open_secondary(&db_path, &root) {
            Ok(c) => c,
            Err(e) => {
                finish(Default::default(), Some(format!("couldn't open catalog connection: {e}")));
                return;
            }
        };
        let progress_slot = slot.clone();
        let events = state.clone();
        let mut last_emit: Option<Instant> = None;
        let result = catalog.run_identity_repair(&abort, |s| {
            // Throttled, but never at the cost of the last update.
            let due = last_emit.is_none_or(|t| t.elapsed() >= REPAIR_PROGRESS_INTERVAL);
            if !due && s.done() < s.total {
                return;
            }
            last_emit = Some(Instant::now());
            let (done, total) = (s.done(), s.total);
            events.send(CoreEvent::IdentityRepairProgress(IdentityRepairProgress { done, total, job }));
            // A superseded pass reaches here routinely (the row it is on finishes first);
            // `JobSlot::publish` stops it overwriting the newer pass's slot.
            progress_slot.publish(|job| IdentityRepairJobStatus { job, done, total });
        });
        match result {
            Ok(summary) => finish(summary, None),
            Err(e) => finish(Default::default(), Some(e.to_string())),
        }
    }
}

/// Trip the running pass; it stops before its next copy. A no-op when nothing runs.
pub fn cancel_identity_repair(state: &AppState) -> Result<(), String> {
    state.jobs.identity.cancel()
}

/// The running pass's status, or `None` when idle — so a reopened panel re-attaches.
pub fn identity_repair_status(state: &AppState) -> Result<Option<IdentityRepairJobStatus>, String> {
    state.jobs.identity.status()
}

// ── Bulk resolution of non-UUID conflicts (#150) ──────────────────────────────────────

/// A claimed bulk Overwrite or Dismiss of the copies whose sidecar carries a non-UUID
/// identifier, not yet running. Like [`IdentityRepairPass`], dropping it without
/// [`run`](Self::run) would leave the status slot claimed, so every caller runs it.
#[must_use = "a claimed run holds the status slot until it runs"]
pub struct ForeignConflictRun {
    state: AppState,
    db_path: PathBuf,
    root: PathBuf,
    abort: Arc<AtomicBool>,
    slot: JobSlot<IdentityResolveJobStatus>,
    action: crate::catalog::ForeignConflictAction,
    /// The run's job id: every event it sends carries it.
    pub job: u64,
}

/// Claim a bulk resolution of the non-UUID identity conflicts (`JobRegistry::identity_resolve`):
/// snapshot the catalog, trip a previous run, install this one's abort flag and claim the
/// status slot as one transition. With `expected` (a front end's [`super::CatalogIdentity`],
/// captured when it read the debt it shows), a start that finds another catalog open fails
/// closed with [`super::CATALOG_CHANGED`] before its first mutation. Takes the catalog lock
/// briefly — not on a UI thread.
pub fn claim_resolve_foreign_conflicts(
    state: &AppState,
    expected: Option<super::CatalogIdentity>,
    action: crate::catalog::ForeignConflictAction,
) -> Result<ForeignConflictRun, String> {
    let JobClaim { db_path, root, abort, job, slot } = state
        .jobs
        .identity_resolve
        .begin_as(&state.catalog, expected, |job| IdentityResolveJobStatus { job, done: 0, total: 0 })?;
    Ok(ForeignConflictRun { state: state.clone(), db_path, root, abort, slot, action, job })
}

impl ForeignConflictRun {
    /// Run to the end on a secondary connection to the catalog the claim read, so a switch
    /// landing meanwhile cannot redirect it, and the sidecar IO never holds the app's catalog
    /// lock. Blocking. Always ends with `identity:resolve_done` for this job, after releasing
    /// the status slot (only while it still owns it).
    pub fn run(self) {
        let ForeignConflictRun { state, db_path, root, abort, slot, action, job } = self;
        let finish = |summary: crate::catalog::ForeignConflictSummary, error: Option<String>| {
            slot.clear();
            state.send(CoreEvent::IdentityResolveDone(IdentityResolveDone {
                ok: error.is_none(),
                job,
                action,
                summary,
                error,
            }));
        };
        let catalog = match Catalog::open_secondary(&db_path, &root) {
            Ok(c) => c,
            Err(e) => {
                finish(Default::default(), Some(format!("couldn't open catalog connection: {e}")));
                return;
            }
        };
        let progress_slot = slot.clone();
        let events = state.clone();
        let mut last_emit: Option<Instant> = None;
        let result = catalog.run_resolve_foreign_conflicts(action, &abort, |s| {
            let due = last_emit.is_none_or(|t| t.elapsed() >= REPAIR_PROGRESS_INTERVAL);
            if !due && s.done() < s.total {
                return;
            }
            last_emit = Some(Instant::now());
            let (done, total) = (s.done(), s.total);
            events.send(CoreEvent::IdentityResolveProgress(IdentityResolveProgress { done, total, job }));
            progress_slot.publish(|job| IdentityResolveJobStatus { job, done, total });
        });
        match result {
            Ok(summary) => finish(summary, None),
            Err(e) => finish(Default::default(), Some(e.to_string())),
        }
    }
}

/// Trip the running bulk resolution; it stops before its next copy. A no-op when idle.
pub fn cancel_resolve_foreign_conflicts(state: &AppState) -> Result<(), String> {
    state.jobs.identity_resolve.cancel()
}

/// Trip bulk resolution `job` only — never a newer run another view started. Returns whether
/// it was running.
pub fn cancel_resolve_foreign_conflicts_job(state: &AppState, job: u64) -> Result<bool, String> {
    state.jobs.identity_resolve.cancel_job(job)
}

/// The running bulk resolution's status, or `None` when idle — so a reopened panel re-attaches.
pub fn resolve_foreign_conflicts_status(state: &AppState) -> Result<Option<IdentityResolveJobStatus>, String> {
    state.jobs.identity_resolve.status()
}

/// Resolve one conflicted copy the way the user decided (#33): `adopt`, `overwrite`,
/// `dismiss` or `restore`. On a secondary connection, so the sidecar IO never holds the
/// app's catalog lock. Blocking. Refusals come back as plain messages naming what was
/// refused. Deliberately does not stop a running pass (each queue row has an owner).
pub fn resolve_identity_conflict(
    state: &AppState,
    photo_id: i64,
    volume_id: i64,
    relative_path: &str,
    action: crate::catalog::IdentityConflictAction,
) -> Result<crate::catalog::IdentityConflictOutcome, String> {
    let (db_path, root) = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let c = guard.as_ref().ok_or("No catalog is open")?;
        (c.db_path().to_path_buf(), c.root().to_path_buf())
    };
    resolve_in(&db_path, &root, photo_id, volume_id, relative_path, action)
}

/// [`resolve_identity_conflict`] of a copy read from the catalog `expected` names — what a
/// front end calls with the identity its debt rows were read with. Photo and volume ids and
/// relative paths are per catalog: once another catalog opening is open it fails closed with
/// [`CATALOG_CHANGED`](super::CATALOG_CHANGED) and touches nothing. The check and the capture
/// of the catalog's file run under one catalog lock, and the resolution then runs on a
/// secondary connection to *that* file, so the rows it changes and the sidecar it reads or
/// writes (resolved from that catalog's volumes) are the ones the user was shown, even if a
/// switch lands while it runs.
pub fn resolve_identity_conflict_as(
    state: &AppState,
    expected: super::CatalogIdentity,
    photo_id: i64,
    volume_id: i64,
    relative_path: &str,
    action: crate::catalog::IdentityConflictAction,
) -> Result<crate::catalog::IdentityConflictOutcome, String> {
    let (db_path, root) =
        super::with_catalog_as(state, expected, |c| Ok((c.db_path().to_path_buf(), c.root().to_path_buf())))?;
    resolve_in(&db_path, &root, photo_id, volume_id, relative_path, action)
}

fn resolve_in(
    db_path: &std::path::Path,
    root: &std::path::Path,
    photo_id: i64,
    volume_id: i64,
    relative_path: &str,
    action: crate::catalog::IdentityConflictAction,
) -> Result<crate::catalog::IdentityConflictOutcome, String> {
    let catalog = Catalog::open_secondary(db_path, root).map_err(|e| e.to_string())?;
    catalog
        .resolve_identity_conflict(photo_id, volume_id, relative_path, action)
        .map_err(|e| e.to_string())
}

// --- identity repair ownership (#34): switch, cancel, supersede, no catalog ---------------
// Moved from the Tauri shell's `commands/storage.rs` when it was removed (#165); they always
// drove this module's claim and the catalog's repair pass directly.
#[cfg(test)]
mod identity_repair_ownership_tests {
    use super::*;
    use crate::app::{detach_catalog_and_trip_jobs, publish_catalog_and_reset_jobs};
    use crate::catalog::SidecarIdentity;
    use std::path::Path;
    use std::sync::atomic::Ordering;
    use std::sync::Mutex;

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

        // Start the pass exactly the way `claim_identity_repair` does.
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
                    // Exactly what `cancel_identity_repair` does.
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

// --- identity debt bound to the catalog it was read from (#164) ---------------------------
// Moved from the Tauri shell's `commands/storage.rs` when it was removed (#165). There the
// shell's commands bound these same core calls to the identity the panel passed; here the
// test drives the core calls directly. The open catalog is swapped for a byte copy
// (`VACUUM INTO`: the same photo ids, volume ids, relative paths, UUIDs and owed
// generations), as a switch to a copied catalog publishes it before `catalog:switched`
// reaches the UI. Every read and action carrying the old identity fails closed and leaves
// the copy's debt as it was.
#[cfg(test)]
mod catalog_bound_debt_tests {
    use super::*;
    use crate::app::iptc_owed::{dismiss_owed_iptc_as, list_owed_iptc, retry_owed_iptc_as};
    use crate::app::{with_catalog_as, CatalogIdentity, CATALOG_CHANGED};
    use crate::catalog::{IdentityConflictAction, IptcFields, SidecarIdentity};

    struct Rig {
        state: AppState,
        /// The identity the panel captured, from catalog A.
        a: CatalogIdentity,
        photo_id: i64,
        uuid: String,
        generation: i64,
        volume_id: i64,
        relative_path: String,
        _dir: crate::test_support::TestTmpDir,
    }

    /// Catalog A with one photo whose copy is in conflict and whose IPTC is owed; then A is
    /// replaced, as the open catalog, by its byte copy B.
    fn swapped_for_a_copy(tag: &str) -> Rig {
        let dir = crate::test_support::TestTmpDir::new(&format!("bound-debt-{tag}"));
        let root = dir.join("photos");
        let file = root.join("2026/p0.ARW");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"raw").unwrap();
        let a = Catalog::open(&dir.join("a.chairphoto"), &root).unwrap();
        let photo_id = a.upsert_photo(&file, None, 0, 1).unwrap().id;
        a.record_sidecar_identity(photo_id, &file, &SidecarIdentity::Conflict("another photo's uuid".into()))
            .unwrap();
        a.set_iptc(photo_id, &IptcFields { title: "A's".into(), ..Default::default() }).unwrap();
        let owed = a.list_owed_iptc_page(10, 0).unwrap().remove(0);
        let copy = a.list_pending_identity_page(10, 0, false).unwrap().remove(0);
        let b_path = dir.join("b.chairphoto");
        a.conn().execute("VACUUM INTO ?1", [b_path.to_string_lossy()]).unwrap();
        let b = Catalog::open(&b_path, &root).unwrap();
        assert_eq!(b.list_owed_iptc_page(10, 0).unwrap(), vec![owed.clone()], "B is a copy of A");

        let state = AppState::default();
        *state.catalog.lock().unwrap() = Some(a);
        // What a front end captures when it opens, serialized and back: an identity is an
        // opaque string wherever it is stored or sent.
        let wire = serde_json::to_value(crate::app::catalog_identity(&state).unwrap()).unwrap();
        assert!(wire.is_string(), "an identity serializes as a string: {wire}");
        let a_identity: CatalogIdentity = serde_json::from_value(wire).unwrap();
        // The switch publishes B before `catalog:switched` reaches the UI.
        *state.catalog.lock().unwrap() = Some(b);
        Rig {
            state,
            a: a_identity,
            photo_id,
            uuid: owed.uuid,
            generation: owed.generation,
            volume_id: copy.volume_id,
            relative_path: copy.relative_path,
            _dir: dir,
        }
    }

    fn b_debt(rig: &Rig) -> (usize, i64) {
        let guard = rig.state.catalog.lock().unwrap();
        let c = guard.as_ref().unwrap();
        (c.list_owed_iptc_page(10, 0).unwrap().len(), c.summarize_pending_identity().unwrap().dismissed)
    }

    #[test]
    fn reads_bound_to_the_old_catalog_fail_closed() {
        let rig = swapped_for_a_copy("reads");
        let a = rig.a;
        let pending = with_catalog_as(&rig.state, a, |c| c.list_pending_identity_page(10, 0, false));
        assert_eq!(pending.unwrap_err(), CATALOG_CHANGED);
        assert_eq!(with_catalog_as(&rig.state, a, |c| c.summarize_pending_identity()).unwrap_err(), CATALOG_CHANGED);
        assert_eq!(with_catalog_as(&rig.state, a, |c| c.list_owed_iptc_page(10, 0)).unwrap_err(), CATALOG_CHANGED);
        // Unbound (the title bar's count) and bound to the open catalog, they read B.
        let (b, owed) = list_owed_iptc(&rig.state, 10, 0).unwrap();
        assert_ne!(b, a, "the copy is another catalog");
        assert_eq!(owed.len(), 1);
        assert_eq!(with_catalog_as(&rig.state, b, |c| c.summarize_pending_identity()).unwrap().iptc_owed, 1);
        assert_eq!(with_catalog_as(&rig.state, b, |c| c.list_pending_identity_page(10, 0, false)).unwrap().len(), 1);
    }

    #[test]
    fn actions_bound_to_the_old_catalog_never_touch_the_copy() {
        let rig = swapped_for_a_copy("actions");
        let a = rig.a;
        let dismiss = dismiss_owed_iptc_as(&rig.state, Some(a), rig.photo_id, &rig.uuid, rig.generation);
        assert_eq!(dismiss.unwrap_err(), CATALOG_CHANGED);
        let retry = retry_owed_iptc_as(&rig.state, Some(a), rig.photo_id, &rig.uuid);
        assert_eq!(retry.unwrap_err(), CATALOG_CHANGED);
        let resolve = resolve_identity_conflict_as(
            &rig.state,
            a,
            rig.photo_id,
            rig.volume_id,
            &rig.relative_path,
            IdentityConflictAction::Dismiss,
        );
        assert_eq!(resolve.unwrap_err(), CATALOG_CHANGED);
        assert_eq!(b_debt(&rig), (1, 0), "the copy's owed IPTC and conflict are as they were");

        // The control: bound to the open catalog, the same calls act.
        let b = crate::app::catalog_identity(&rig.state).unwrap();
        let resolve = resolve_identity_conflict_as(
            &rig.state,
            b,
            rig.photo_id,
            rig.volume_id,
            &rig.relative_path,
            IdentityConflictAction::Dismiss,
        );
        assert_eq!(resolve.unwrap().action, "dismiss");
        let dismiss = dismiss_owed_iptc_as(&rig.state, Some(b), rig.photo_id, &rig.uuid, rig.generation);
        assert_eq!(dismiss.unwrap(), crate::catalog::OwedDismissal::Dismissed);
        assert_eq!(b_debt(&rig), (0, 1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ForeignConflictAction, SidecarIdentity};
    use std::sync::atomic::Ordering;
    use std::sync::Mutex;

    /// Records each event's name, its job id, and whether the bulk-resolution status slot was
    /// already clear when it was sent.
    #[derive(Default)]
    struct Recorded {
        state: Mutex<Option<AppState>>,
        seen: Mutex<Vec<(String, u64, bool, Option<crate::catalog::ForeignConflictSummary>)>>,
    }
    impl EventSink for Recorded {
        fn send(&self, event: CoreEvent) {
            let slot_clear = self
                .state
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|s| s.jobs.identity_resolve.status().unwrap().is_none());
            let (job, summary) = match &event {
                CoreEvent::IdentityResolveProgress(p) => (p.job, None),
                CoreEvent::IdentityResolveDone(d) => (d.job, Some(d.summary)),
                _ => (0, None),
            };
            self.seen.lock().unwrap().push((event.name().to_string(), job, slot_clear, summary));
        }
    }

    /// A catalog at `dir` with `n` copies in conflict with a DAM id, open in a fresh state
    /// whose events `Recorded` keeps.
    fn state_with_conflicts(dir: &std::path::Path, n: usize) -> (AppState, Arc<Recorded>) {
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let catalog = Catalog::open(&dir.join("c.chairphoto"), &root).unwrap();
        for i in 0..n {
            let path = root.join(format!("{i}.jpg"));
            std::fs::write(&path, b"bytes").unwrap();
            crate::xmp::write_identifier(&path, &format!("dam:{i}")).unwrap();
            let up = catalog.upsert_photo(&path, None, 1, 5).unwrap();
            let found = crate::xmp::read_identifier(&path);
            let outcome = catalog.ensure_sidecar_identity(up.id, &path, &up.uuid, found.as_deref()).unwrap();
            assert!(matches!(outcome, SidecarIdentity::Conflict(_)));
        }
        let state = AppState::default();
        *state.catalog.lock().unwrap() = Some(catalog);
        let recorded = Arc::new(Recorded::default());
        *recorded.state.lock().unwrap() = Some(state.clone());
        state.set_events(recorded.clone());
        (state, recorded)
    }

    /// #150 (L6): a bulk resolution is an owned job. The claim publishes its status, the run
    /// does the work on a secondary connection, every event carries its job id, and the slot
    /// is clear before the terminal event, which carries the summary.
    #[test]
    fn a_bulk_resolution_runs_as_a_job_and_clears_its_slot_before_the_terminal_event() {
        let dir = crate::test_support::TestTmpDir::new("bulk-resolve-job");
        let (state, recorded) = state_with_conflicts(&dir, 3);
        let expected = super::super::catalog_identity(&state).unwrap();
        let run = claim_resolve_foreign_conflicts(&state, Some(expected), ForeignConflictAction::Dismiss).unwrap();
        let job = run.job;
        assert_eq!(resolve_foreign_conflicts_status(&state).unwrap().map(|s| s.job), Some(job));
        run.run();

        assert!(resolve_foreign_conflicts_status(&state).unwrap().is_none());
        let seen = recorded.seen.lock().unwrap();
        assert!(seen.iter().all(|(_, j, _, _)| *j == job), "{seen:?}");
        let (name, _, slot_clear, summary) = seen.last().unwrap();
        assert_eq!(name, "identity:resolve_done");
        assert!(slot_clear, "the slot is released before the terminal event");
        let summary = summary.unwrap();
        assert_eq!((summary.total, summary.dismissed, summary.aborted), (3, 3, false));
        let guard = state.catalog.lock().unwrap();
        assert_eq!(guard.as_ref().unwrap().summarize_pending_identity().unwrap().dismissed, 3);
    }

    /// A start bound to a catalog that is no longer open fails closed before its first
    /// mutation: no job id, nothing tripped, no slot.
    #[test]
    fn a_bulk_resolution_bound_to_another_catalog_fails_closed() {
        let dir = crate::test_support::TestTmpDir::new("bulk-resolve-stale");
        let (state, _) = state_with_conflicts(&dir, 1);
        let stale = super::super::catalog_identity(&state).unwrap();
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        *state.catalog.lock().unwrap() = Some(Catalog::open(&dir.join("o.chairphoto"), &other).unwrap());
        let installed = state.jobs.identity_resolve.installed().unwrap();

        let err = claim_resolve_foreign_conflicts(&state, Some(stale), ForeignConflictAction::Overwrite)
            .err()
            .unwrap();
        assert_eq!(err, super::super::CATALOG_CHANGED);
        assert_eq!(state.jobs.identity_resolve.abort().job_ids_issued(), 0);
        assert!(!installed.load(Ordering::Relaxed));
        assert!(Arc::ptr_eq(&installed, &state.jobs.identity_resolve.installed().unwrap()));
        assert!(resolve_foreign_conflicts_status(&state).unwrap().is_none());
    }

    /// A newer start supersedes a claimed run: the old one stops before its first copy,
    /// reports itself aborted under its own job id, and leaves the newer run's slot alone.
    #[test]
    fn a_newer_bulk_resolution_supersedes_the_older_one() {
        let dir = crate::test_support::TestTmpDir::new("bulk-resolve-supersede");
        let (state, recorded) = state_with_conflicts(&dir, 2);
        let first = claim_resolve_foreign_conflicts(&state, None, ForeignConflictAction::Dismiss).unwrap();
        let second = claim_resolve_foreign_conflicts(&state, None, ForeignConflictAction::Dismiss).unwrap();
        let (first_job, second_job) = (first.job, second.job);
        first.run();

        let done = recorded.seen.lock().unwrap().last().cloned().unwrap();
        assert_eq!((done.0.as_str(), done.1), ("identity:resolve_done", first_job));
        let summary = done.3.unwrap();
        assert!(summary.aborted && summary.dismissed == 0, "{summary:?}");
        assert_eq!(resolve_foreign_conflicts_status(&state).unwrap().map(|s| s.job), Some(second_job));
        assert!(cancel_resolve_foreign_conflicts_job(&state, first_job).is_ok_and(|ran| !ran));
        second.run();
        assert!(resolve_foreign_conflicts_status(&state).unwrap().is_none());
    }
}
