//! The sidecar-identity repair pass (#34) and conflict resolution (#33) — the bodies of the
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
use super::{AppState, CoreEvent, EventSink, IdentityRepairDone, IdentityRepairJobStatus, IdentityRepairProgress};
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
    let catalog = Catalog::open_secondary(&db_path, &root).map_err(|e| e.to_string())?;
    catalog
        .resolve_identity_conflict(photo_id, volume_id, relative_path, action)
        .map_err(|e| e.to_string())
}
