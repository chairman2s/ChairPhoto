//! The sidecar-identity repair pass (#34), conflict resolution (#33) and the bulk resolution of
//! non-UUID conflicts (#150, [`claim_resolve_foreign_conflicts`], its own job family) — the bodies of the
//! Tauri `repair_pending_identity`, `identity_repair_cancel`, `identity_repair_status` and
//! `resolve_identity_conflict` commands, and of the GPUI identity-debt panel.
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
