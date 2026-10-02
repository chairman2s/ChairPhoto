//! The face-**matching** job (`faces_run_matching`, `faces_match_status`,
//! `faces_match_cancel`), moved from the Tauri commands so the GPUI app runs the same body
//! (#130): seed / constrained match / open match / cluster over the indexed faces
//! (`plugins::faces::matcher`), as a background job on its own catalog connection.
//!
//! **Ownership** is the indexing job's ([`super::begin_index_job`]): one transition claims the
//! catalog snapshot, the job id, the abort flag and the status slot
//! ([`crate::app::jobs::JobFamily::begin_as`]); with an expected [`CatalogIdentity`] it fails
//! closed — touching nothing — once another catalog is open. Matching is its own family, so
//! cancelling a match never stops an index. Progress goes out as `faces:match_progress`
//! (cosmetic, throttled) and the counters as the terminal `faces:match_done`; the status slot
//! is cleared — only while this job still owns it — **before** that terminal event.

use super::super::{
    now_secs, spawn_blocking, AppState, CatalogIdentity, CoreEvent, EventSink, FacesMatchDone, FacesMatchJobStatus,
    FacesMatchProgressEvent, JobClaim,
};
use crate::catalog::Catalog;
use crate::plugins::faces::matcher::{self, MatchPhase, MatchSettings};
use std::sync::atomic::Ordering;

/// Within a phase, a progress event goes out every this many items (phase changes and ends
/// always do): per-item events over thousands of faces would flood the event bridge.
pub const MATCH_PROGRESS_EVERY: usize = 25;

/// Claim ownership of the face-matching job: snapshot the catalog, allocate the job id, trip
/// the previous match, install this job's abort flag and claim the status slot as ONE
/// transition, holding catalog → abort → slot throughout. With `expected`, only while that
/// catalog is open.
pub fn begin_match_job(
    state: &AppState,
    expected: Option<CatalogIdentity>,
) -> Result<JobClaim<FacesMatchJobStatus>, String> {
    state.jobs.faces_match.begin_as(&state.catalog, expected, |job| FacesMatchJobStatus {
        job,
        done: 0,
        total: 0,
        phase: MatchPhase::Seed.label(),
    })
}

/// Start the seed / match / cluster pass as a background job and return its id as soon as it
/// has started; the counters arrive on `faces:match_done`. A start supersedes a running match
/// (its flag is tripped). Idempotent and re-runnable: confirmed, ignored and manual faces are
/// never touched and rejected pairs are never re-proposed. Needs no models.
pub fn start_match(state: &AppState, expected: Option<CatalogIdentity>) -> Result<u64, String> {
    let claim = begin_match_job(state, expected)?;
    let job = claim.job;
    let sink = state.clone();
    spawn_blocking(move || run_match_job(&sink, claim));
    Ok(job)
}

/// The running match's status, `None` when idle (`faces_match_status`).
pub fn match_status(state: &AppState) -> Result<Option<FacesMatchJobStatus>, String> {
    state.jobs.faces_match.status()
}

/// Trip whatever match is running (`faces_match_cancel`). It stops at its next face and still
/// sends `faces:match_done`, with `aborted: true`.
pub fn cancel_match(state: &AppState) -> Result<(), String> {
    state.jobs.faces_match.cancel()
}

/// Cancel match `job` only — a no-op (`false`) once a newer job owns the family.
pub fn cancel_match_job(state: &AppState, job: u64) -> Result<bool, String> {
    state.jobs.faces_match.cancel_job(job)
}

/// The matching worker: everything after the claim. Blocking — runs on a worker thread.
pub fn run_match_job(sink: &(impl EventSink + ?Sized), claim: JobClaim<FacesMatchJobStatus>) {
    let JobClaim { db_path, root, abort, job, slot } = claim;
    let fail = |error: String| {
        sink.send(CoreEvent::FacesMatchDone(FacesMatchDone { ok: false, outcome: None, aborted: false, job, error: Some(error) }))
    };

    // Secondary connection — never contends with the primary's UI reads.
    let sec = match Catalog::open_secondary(&db_path, &root) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("faces_match: couldn't open secondary connection: {e}");
            slot.clear();
            fail(format!("couldn't open catalog connection: {e}"));
            return;
        }
    };
    let settings = match MatchSettings::load(sec.conn()) {
        Ok(s) => s,
        Err(e) => {
            slot.clear();
            fail(e.to_string());
            return;
        }
    };

    // The abort flag is read on EVERY call, not only the ones that emit: throttling progress
    // must not throttle cancellation.
    let mut last: Option<(MatchPhase, usize)> = None;
    let mut on_progress = |phase: MatchPhase, done: usize, total: usize| -> bool {
        if abort.load(Ordering::Relaxed) {
            return false;
        }
        let fire = match last {
            Some((p, d)) => p != phase || done >= total || done >= d + MATCH_PROGRESS_EVERY,
            None => true,
        };
        if fire {
            last = Some((phase, done));
            // Never overwrite a newer job's slot — `JobSlot::publish` is the shared guard.
            slot.publish(|job| FacesMatchJobStatus { job, done, total, phase: phase.label() });
            sink.send(CoreEvent::FacesMatchProgress(FacesMatchProgressEvent { done, total, phase: phase.label(), job }));
        }
        true
    };

    let result = matcher::run_matching_with_progress(sec.conn(), &settings, now_secs(), &mut on_progress);
    let aborted = abort.load(Ordering::Relaxed);

    // Clear the slot before the terminal event, and only if this job still owns it.
    slot.clear();
    sink.send(CoreEvent::FacesMatchDone(match result {
        Ok(outcome) => FacesMatchDone { ok: !aborted, outcome: Some(outcome), aborted, job, error: None },
        Err(e) => {
            eprintln!("faces_match: job failed: {e}");
            FacesMatchDone { ok: false, outcome: None, aborted, job, error: Some(e.to_string()) }
        }
    }));
}
