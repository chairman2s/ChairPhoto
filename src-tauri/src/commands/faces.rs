//! Face-tagging commands (H13) — detection, recognition, clustering and the
//! confirm/reject surface, plus MWG-Regions sidecar sync.
//!
//! Gated on the `faces` Cargo feature; see `docs/face-tagging.md` and
//! `plugins/faces/`. All inference is local — no image ever leaves the machine.

use super::*;
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

/// Report which face models are present, so the UI can offer a download and keep the module
/// inert until they are. Never fails — a missing model is a clean state. `async` so it runs
/// off the event-loop thread (per the AGENTS.md "no UI-thread disk work" invariant); the
/// underlying check is a cheap size-only stat, not a full re-hash.
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_models_status() -> crate::plugins::faces::ModelStatus {
    crate::plugins::faces::models::status()
}

/// Download any missing/corrupt face models (once) with checksum verification, returning the
/// post-download status. Runs off the UI thread; safe to re-invoke (already-present models
/// are skipped).
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_download_models() -> Result<crate::plugins::faces::ModelStatus, String> {
    Ok(crate::plugins::faces::models::ensure_all().await)
}

/// Where face inference actually runs plus the indexing-speed plan — surfaced in the
/// Faces settings panel so GPU-vs-CPU is visible in the app, not only on stderr
/// (I7d follow-up: `engine::active_ep()` existed but nothing consumed it).
#[cfg(feature = "faces")]
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FacesInferenceInfo {
    /// `"cuda"` | `"cpu"` | `"unbuilt"` (no inference has run yet this session).
    pub ep: String,
    /// Whether this binary was compiled with the `faces-cuda` feature at all.
    pub cuda_built: bool,
    /// Effective `indexing.speed` setting (`"background"` | `"full"`).
    pub speed: String,
}

#[cfg(feature = "faces")]
#[tauri::command]
pub fn faces_inference_info(state: State<'_, AppState>) -> Result<FacesInferenceInfo, String> {
    use crate::plugins::faces::engine::ActiveEp;
    let ep = match crate::plugins::faces::engine::active_ep() {
        ActiveEp::Cuda => "cuda",
        ActiveEp::Cpu => "cpu",
        ActiveEp::Unbuilt => "unbuilt",
    };
    let guard = state.catalog.lock().map_err(|e| e.to_string())?;
    let catalog = guard.as_ref().ok_or("No catalog is open")?;
    let speed = match catalog
        .get_setting(crate::plugins::indexing::INDEXING_SPEED_SETTING)
        .map_err(|e| e.to_string())?
    {
        Some(s) if s.trim().eq_ignore_ascii_case("full") => "full",
        _ => "background",
    };
    Ok(FacesInferenceInfo {
        ep: ep.into(),
        cuda_built: cfg!(feature = "faces-cuda"),
        speed: speed.into(),
    })
}

/// Set the global `indexing.speed` setting (`"background"` | `"full"`). Takes effect on the
/// NEXT face-indexing run: `faces_index_photos` re-reads this setting and calls
/// `engine::configure` before its parallel loop starts, and the session pool is now keyed by
/// that configuration (`engine::PoolKey`, issue #18), so a changed value rebuilds it rather
/// than being ignored until restart. A run already in progress keeps the pool it started
/// with — see `engine::configure`'s doc comment for the cross-job edge case.
#[cfg(feature = "faces")]
#[tauri::command]
pub fn faces_set_indexing_speed(state: State<'_, AppState>, speed: String) -> Result<(), String> {
    let v = speed.trim().to_ascii_lowercase();
    if v != "background" && v != "full" {
        return Err(format!("invalid indexing speed: {speed}"));
    }
    let guard = state.catalog.lock().map_err(|e| e.to_string())?;
    let catalog = guard.as_ref().ok_or("No catalog is open")?;
    catalog
        .set_setting(crate::plugins::indexing::INDEXING_SPEED_SETTING, &v)
        .map_err(|e| e.to_string())
}

// ── Face-indexing commands (H13b) ────────────────────────────────────────────

/// Terminal event payload for `faces_index_photos` (`faces:index_done`). Progress events
/// alone can't signal completion unambiguously (a run with nothing to index emits only
/// `{0, 0}`, which is indistinguishable from a failure), so completion gets its own event.
/// `job` identifies which run finished — starting a new run aborts the previous one, and
/// the UI must not mistake the superseded run's done-event for its own job completing.
/// `offline`/`failed`/`aborted` say *why* `done < total` instead of leaving the UI to guess.
#[cfg(feature = "faces")]
#[derive(Debug, Clone, serde::Serialize)]
pub struct FacesIndexDone {
    pub ok: bool,
    pub done: usize,
    pub total: usize,
    pub offline: usize,
    pub failed: usize,
    pub aborted: bool,
    pub job: u64,
    pub error: Option<String>,
}

/// Progress event payload (`faces:progress`) for the indexing job — the indexer's
/// `{done, total}` plus the job id, so a superseded job's stragglers can be ignored.
#[cfg(feature = "faces")]
#[derive(Debug, Clone, serde::Serialize)]
pub struct FacesProgressEvent {
    pub done: usize,
    pub total: usize,
    pub job: u64,
}

/// Claim ownership of the face-**indexing** job: snapshot the catalog, allocate the job id,
/// trip the previous job, install this job's abort flag and claim the status slot as ONE
/// transition, holding catalog -> abort -> slot throughout.
///
/// The transition itself lives in [`crate::commands::jobs::JobFamily::begin`] — one
/// implementation shared with face matching and Smart Tagging, fenced by the same locks as
/// the two `switch_catalog` phases. Read that function for why each lock is held across the
/// whole claim; this wrapper only supplies the initial status snapshot (`total` is unknown
/// until the queue is populated — claiming anyway is what lets a status query between
/// "command returned" and "first progress event" already see it running).
///
/// Kept as a named function rather than inlined so the interleaving tests can drive the start
/// exactly as `faces_index_photos` does; they have no Tauri `AppHandle`. Face indexing was
/// the one job family without this seam, and it was also the one whose status slot a catalog
/// switch forgot to clear (issue #51).
#[cfg(feature = "faces")]
fn begin_faces_index_job(state: &AppState) -> Result<JobClaim<FacesJobStatus>, String> {
    state
        .jobs
        .faces
        .begin(&state.catalog, |job| FacesJobStatus { job, done: 0, total: 0 })
}

/// Begin (or resume) the background face-indexing job. Opens its own secondary catalog
/// connection so the UI thread is never blocked. Detection + embedding run through the
/// engine (YuNet + AuraFace) — the command returns as soon as the job has STARTED; progress
/// is reported via `faces:progress {done, total}` events and completion via a terminal
/// `faces:index_done {ok, done, total, error}` event.
///
/// Re-invoking while a job is running trips the old job's abort flag first (same pattern as
/// `begin_scan_generation`), then starts a fresh run. Photos already indexed are skipped.
/// Photos that are offline (no reachable copy) are skipped for this run and stay in the queue.
///
/// Returns the new job's id (also carried by its progress/done events) so the caller can
/// tell this run's events apart from a superseded run's.
///
/// Returns an error if no catalog is open or the models are not yet downloaded.
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_index_photos(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<u64, String> {
    // Check that the models are present before spinning up a worker. This touches only the
    // filesystem, so it runs before any lock is taken: a start that fails here must leave
    // the running job and its status slot exactly as they were.
    if !crate::plugins::faces::models::status().ready {
        return Err(
            "Face models are not downloaded. Use faces_download_models first.".to_string(),
        );
    }

    let JobClaim { db_path, root, abort, job, slot: job_slot } =
        begin_faces_index_job(state.inner())?;

    tauri::async_runtime::spawn_blocking(move || {
        use crate::catalog::Catalog;
        use crate::plugins::faces::indexer;
        use crate::plugins::indexing as shared_indexing;

        // Release the status slot — but only if a newer job hasn't already claimed it.
        // `JobSlot::clear` is the shared guard; see its doc for why this always runs BEFORE
        // the terminal event is emitted, never after.
        let clear_job_slot = || job_slot.clear();

        // Secondary connection — never contends with the primary's UI reads.
        let sec = match Catalog::open_secondary(&db_path, &root) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("faces_index: couldn't open secondary connection: {e}");
                clear_job_slot();
                let _ = app.emit(
                    "faces:index_done",
                    FacesIndexDone {
                        ok: false,
                        done: 0,
                        total: 0,
                        offline: 0,
                        failed: 0,
                        aborted: false,
                        job,
                        error: Some(format!("couldn't open catalog connection: {e}")),
                    },
                );
                return;
            }
        };

        // Detection + embedding closure — runs the real engine.
        let detect_fn = |jpeg: &[u8]| -> Vec<indexer::IndexedFace> {
            use crate::plugins::faces::engine;
            let faces = match engine::detect_faces(jpeg, 0.6, 0.3) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("faces_index: detection failed: {e}");
                    return Vec::new();
                }
            };
            let img = match image::load_from_memory(jpeg) {
                Ok(i) => i.to_rgb8(),
                Err(e) => {
                    eprintln!("faces_index: image decode failed: {e}");
                    return Vec::new();
                }
            };
            faces
                .into_iter()
                .map(|f| {
                    let emb = engine::embed_face(&img, &f.landmarks)
                        .ok()
                        .map(|e| e.to_vec());
                    indexer::IndexedFace {
                        bbox: f.bbox,
                        landmarks: f.landmarks,
                        confidence: f.confidence,
                        embedding: emb,
                    }
                })
                .collect()
        };

        let emit_app = app.clone();
        let progress_slot = job_slot.clone();
        let emit_fn = move |p: indexer::FacesProgress| {
            // Keep the status slot current so a re-attaching UI gets live numbers —
            // never overwrite a newer job's slot (this job is then already aborted,
            // but its in-flight chunk may still emit a few stragglers). `JobSlot::publish`
            // is the shared guard that rejects those.
            progress_slot.publish(|job| FacesJobStatus { job, done: p.done, total: p.total });
            let _ = emit_app.emit(
                "faces:progress",
                FacesProgressEvent { done: p.done, total: p.total, job },
            );
        };

        // People-root branch (setting, with default) for the MWG-region import: an imported
        // face's person tag is created/found at "<people_root>/<region name>".
        let people_root = crate::plugins::faces::matcher::MatchSettings::load(sec.conn())
            .map(|s| s.people_root)
            .unwrap_or_else(|_| crate::plugins::faces::PEOPLE_ROOT_DEFAULT.to_string());

        // Resolve the indexing-speed plan (setting `indexing.speed`, default `background`)
        // against the host's core count, then size the ONNX session pool + intra-op threads
        // to it before the first inference builds the pool. The pool is keyed by this
        // configuration (`engine::PoolKey`), so a value that changed since the last run
        // rebuilds it here rather than reusing a stale pool.
        let plan = shared_indexing::load_indexing_plan(sec.conn());
        crate::plugins::faces::engine::configure(plan.parallelism, plan.intra_threads);
        // Honor the `faces.force_cpu` setting: in a `faces-cuda` build this keeps inference on
        // CPU even when a GPU is available (inert in a CPU-only build). Same lifecycle as
        // above — set before this run's first inference, which is what makes a toggle since
        // the last run rebuild the pool instead of being stuck on whatever it built with.
        crate::plugins::faces::engine::configure_force_cpu(indexer::load_force_cpu(sec.conn()));

        // Run the indexer. Uses c.conn() (pub(crate)) to access the raw rusqlite connection
        // and the Catalog's public resolver for path resolution.
        let result = {
            let conn = sec.conn();
            let resolve_fn = |photo_id: i64| {
                sec.resolve_photo_path(photo_id).map_err(|e| e.to_string())
            };
            let preview_fn =
                |path: &std::path::Path| crate::thumbnails::preview_bytes(path);
            // Post-index hook: import existing MWG face regions from the photo's sidecar,
            // IoU-matching them to the just-detected faces and confirming matched, named
            // regions (source='xmp'). Foreign labels become person tags under the people root.
            let import_hook = |hook_conn: &rusqlite::Connection,
                               photo_id: i64,
                               path: &std::path::Path| {
                faces_import_regions(&sec, hook_conn, photo_id, path, &people_root);
            };
            indexer::run_index_with_hook(
                conn,
                plan.parallelism,
                resolve_fn,
                preview_fn,
                detect_fn,
                &abort,
                emit_fn,
                import_hook,
            )
        };

        clear_job_slot();
        match result {
            Ok(o) => {
                let _ = app.emit(
                    "faces:index_done",
                    FacesIndexDone {
                        ok: true,
                        done: o.done,
                        total: o.total,
                        offline: o.offline,
                        failed: o.failed,
                        aborted: o.aborted,
                        job,
                        error: None,
                    },
                );
            }
            Err(e) => {
                eprintln!("faces_index: job failed: {e}");
                let _ = app.emit(
                    "faces:index_done",
                    FacesIndexDone {
                        ok: false,
                        done: 0,
                        total: 0,
                        offline: 0,
                        failed: 0,
                        aborted: false,
                        job,
                        error: Some(e.to_string()),
                    },
                );
            }
        }
    });

    Ok(job)
}

/// Trip the abort flag of any running face-indexing job. The worker stops cleanly after
/// the current photo finishes; the queue retains unprocessed photos for the next run.
/// No-op if no job is running.
/// Snapshot of the running face-indexing job, `None` when idle. Lets the panel re-attach
/// to a job that is still running after a remount (tab switch) instead of showing idle —
/// which both restores the progress display and prevents an accidental second start
/// (starting a new job aborts the running one).
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_index_status(
    state: State<'_, AppState>,
) -> Result<Option<FacesJobStatus>, String> {
    state.jobs.faces.status()
}

#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_index_cancel(state: State<'_, AppState>) -> Result<(), String> {
    state.jobs.faces.cancel()
}

/// Write a photo's confirmed face regions into its XMP sidecar (`mwg-rs:Regions`), merge-safe.
/// Called after any confirm/unconfirm state change so the sidecar reflects the current
/// confirmed set (H13f), mirroring how keyword export runs on tag assignment. Sidecar failures
/// are logged, never fatal to the confirm operation (the catalog is authoritative).
/// Import existing MWG face regions from a photo's sidecar during indexing (H13f read path):
/// parse `mwg-rs:Regions`, IoU-match named regions to the photo's just-detected unassigned
/// faces, and for each match find/create the person tag under `<people_root>/<name>`, confirm
/// the face (`source='xmp'`), and assign the tag to the photo. No-op when the sidecar has no
/// regions. All failures are logged and swallowed — region import must never break indexing.
#[cfg(feature = "faces")]
fn faces_import_regions(
    catalog: &crate::catalog::Catalog,
    conn: &rusqlite::Connection,
    photo_id: i64,
    path: &std::path::Path,
    people_root: &str,
) {
    use crate::plugins::faces::regions;

    let read = crate::xmp::read_face_regions(path);
    if read.is_empty() {
        return;
    }
    let detected = match regions::unassigned_faces(conn, photo_id) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("faces_import: load unassigned faces failed for {photo_id}: {e}");
            return;
        }
    };
    if detected.is_empty() {
        return;
    }
    for m in regions::match_regions_to_faces(&detected, &read) {
        // Find or create the person tag under the people root.
        let tag_path = format!("{people_root}/{}", m.name);
        let tag_id = match catalog.create_tag(&tag_path) {
            Ok(id) => id,
            Err(e) => {
                eprintln!("faces_import: create_tag('{tag_path}') failed: {e}");
                continue;
            }
        };
        if let Err(e) = regions::confirm_imported_face(conn, m.face_id, tag_id) {
            eprintln!("faces_import: confirm face {} failed: {e}", m.face_id);
            continue;
        }
        if let Err(e) = catalog.assign_tag(photo_id, tag_id) {
            eprintln!("faces_import: assign_tag failed for photo {photo_id}: {e}");
        }
    }
}

/// The photo id a face belongs to (before a mutation clears its association), or `None` if the
/// face row is gone. Used by reject/ignore so the affected photo's regions can be re-exported.
#[cfg(feature = "faces")]
fn faces_photo_of(conn: &rusqlite::Connection, face_id: i64) -> rusqlite::Result<Option<i64>> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT photo_id FROM faces__faces WHERE id = ?1",
        [face_id],
        |r| r.get::<_, i64>(0),
    )
    .optional()
}

#[cfg(feature = "faces")]
fn faces_write_regions(c: &crate::catalog::Catalog, photo_id: i64) {
    use crate::plugins::faces::regions;
    let resolve = |id: i64| c.resolve_photo_path(id).map_err(|e| e.to_string());
    if let Err(e) = regions::write_photo_regions(c.conn(), photo_id, resolve) {
        eprintln!("faces: MWG region sidecar write failed for photo {photo_id}: {e}");
    }
}

// ── Seed / match / cluster engine commands (H13c) ────────────────────────────
//
// The recognition brain: auto-seed 1-face+1-person photos, per-person centroids,
// Hungarian-constrained matching for N-faces/M-tags photos, nearest-centroid open matching,
// incremental clustering for the rest — plus the accept/reject/ignore/assign/name-cluster
// mutations. The engine itself lives in `plugins::faces::matcher` and is model-free; these
// commands only wire it to the open catalog. Confirming a face also assigns the person tag
// to the *photo* through the catalog's `assign_tag` (XMP export + cross-catalog merge).

/// Progress payload for `faces:match_progress`: the pipeline step label plus counts, and
/// the job id so the UI can drop stragglers from a superseded run.
///
/// Matching used to share `faces:progress` with indexing and be told apart by the *absence*
/// of a `job` field. Now that matching is a real job it has a job id of its own, so it also
/// has an event of its own — discriminating two job families by a missing field does not
/// survive both of them having one.
#[cfg(feature = "faces")]
#[derive(Debug, Clone, serde::Serialize)]
pub struct FacesMatchProgressEvent {
    pub done: usize,
    pub total: usize,
    pub phase: &'static str,
    pub job: u64,
}

/// Terminal event payload for `faces:match_done`.
///
/// The pipeline counters live here rather than in the command's return value: the command
/// returns as soon as the job has *started*, so this event is the run's actual result, not
/// a progress notification. `aborted` says why `outcome` may be short.
#[cfg(feature = "faces")]
#[derive(Debug, Clone, serde::Serialize)]
pub struct FacesMatchDone {
    pub ok: bool,
    pub outcome: Option<crate::plugins::faces::MatchOutcome>,
    pub aborted: bool,
    pub job: u64,
    pub error: Option<String>,
}

/// Claim ownership of the face-matching job: snapshot the catalog, allocate the job id,
/// trip the previous match, install this job's abort flag and claim the status slot as ONE
/// transition, holding catalog -> abort -> slot throughout.
///
/// The transition itself is [`crate::commands::jobs::JobFamily::begin`], shared with face
/// indexing and Smart Tagging; see it for why releasing any of the three locks early leaves
/// either a dead job owning the status slot or a worker no cancel can reach. Matching is its
/// own family, so cancelling a match does not stop an index and the two never collide on an
/// id.
///
/// Kept as a named function so the transition can be driven directly by tests, which have no
/// Tauri `AppHandle`.
#[cfg(feature = "faces")]
fn begin_faces_match_job(state: &AppState) -> Result<JobClaim<FacesMatchJobStatus>, String> {
    use crate::plugins::faces::matcher::MatchPhase;

    state.jobs.faces_match.begin(&state.catalog, |job| FacesMatchJobStatus {
        job,
        done: 0,
        total: 0,
        phase: MatchPhase::Seed.label(),
    })
}

/// Run the full seed / match / cluster pipeline over all indexed faces as a background job.
/// Idempotent and re-runnable (confirmed/ignored/manual faces are never touched, rejected
/// pairs never re-proposed).
///
/// The command returns as soon as the job has STARTED, yielding its job id; progress arrives
/// as `faces:match_progress` and the counters as a terminal `faces:match_done`. It runs on a
/// blocking worker against its own secondary connection, so a matching pass no longer holds
/// the primary catalog mutex for its whole run — which on a large library meant every grid
/// and inspector read queued behind clustering.
///
/// Re-invoking while a match is running trips the old job's abort flag first, then starts a
/// fresh run. The claim below is the same catalog -> abort -> slot transition as
/// `faces_index_photos`; see that command for why all three locks are held across it. The
/// matching job has its own flag and slot, so cancelling a match does not stop an index.
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_run_matching(app: AppHandle, state: State<'_, AppState>) -> Result<u64, String> {
    use crate::plugins::faces::matcher::{self, MatchPhase, MatchSettings};

    let JobClaim { db_path, root, abort, job, slot: job_slot } = begin_faces_match_job(&state)?;

    tauri::async_runtime::spawn_blocking(move || {
        use crate::catalog::Catalog;

        // Release the status slot — but only if a newer job hasn't already claimed it.
        // `JobSlot::clear` is the shared guard; it always runs BEFORE the terminal event.
        let clear_job_slot = || job_slot.clear();

        let fail = |app: &AppHandle, error: String| {
            let _ = app.emit(
                "faces:match_done",
                FacesMatchDone { ok: false, outcome: None, aborted: false, job, error: Some(error) },
            );
        };

        // Secondary connection — never contends with the primary's UI reads.
        let sec = match Catalog::open_secondary(&db_path, &root) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("faces_match: couldn't open secondary connection: {e}");
                clear_job_slot();
                fail(&app, format!("couldn't open catalog connection: {e}"));
                return;
            }
        };

        let settings = match MatchSettings::load(sec.conn()) {
            Ok(s) => s,
            Err(e) => {
                clear_job_slot();
                fail(&app, e.to_string());
                return;
            }
        };

        // Forward pipeline progress to the UI, throttled: phase changes and phase ends
        // always fire; within a phase, every 25th item (steps iterate thousands of
        // faces and per-item events would flood the IPC bridge). The abort flag is read
        // on EVERY call, not only the ones that emit — throttling progress must not
        // throttle cancellation.
        let mut last: Option<(MatchPhase, usize)> = None;
        let mut on_progress = |phase: MatchPhase, done: usize, total: usize| -> bool {
            if abort.load(Ordering::Relaxed) {
                return false;
            }
            let fire = match last {
                Some((p, d)) => p != phase || done >= total || done >= d + 25,
                None => true,
            };
            if fire {
                last = Some((phase, done));
                // Never overwrite a newer job's slot — `JobSlot::publish` is the shared guard.
                job_slot.publish(|job| FacesMatchJobStatus {
                    job,
                    done,
                    total,
                    phase: phase.label(),
                });
                let _ = app.emit(
                    "faces:match_progress",
                    FacesMatchProgressEvent { done, total, phase: phase.label(), job },
                );
            }
            true
        };

        let result =
            matcher::run_matching_with_progress(sec.conn(), &settings, now_secs(), &mut on_progress);
        let aborted = abort.load(Ordering::Relaxed);

        // Clear the slot before the terminal event, and only if this job still owns it.
        clear_job_slot();
        let _ = app.emit(
            "faces:match_done",
            match result {
                Ok(outcome) => {
                    FacesMatchDone { ok: !aborted, outcome: Some(outcome), aborted, job, error: None }
                }
                Err(e) => FacesMatchDone {
                    ok: false,
                    outcome: None,
                    aborted,
                    job,
                    error: Some(e.to_string()),
                },
            },
        );
    });

    Ok(job)
}

/// Snapshot of the running face-matching job, `None` when idle. Lets the panel re-attach to
/// a run that is still going after a remount instead of showing idle.
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_match_status(
    state: State<'_, AppState>,
) -> Result<Option<FacesMatchJobStatus>, String> {
    state.jobs.faces_match.status()
}

/// Cancel the running match. The pipeline stops at its next item and still emits
/// `faces:match_done`, with `aborted: true`.
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_match_cancel(state: State<'_, AppState>) -> Result<(), String> {
    state.jobs.faces_match.cancel()
}

/// Confirm a suggested/seeded face: mark it `confirmed` AND assign the person tag to the
/// photo through the catalog's normal `assign_tag` path (so keyword XMP export + merge apply).
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_accept(state: State<'_, AppState>, face_id: i64) -> Result<(), String> {
    use crate::plugins::faces::matcher;
    with_catalog_blocking(&state, move |c| {
        let (photo_id, tag_id) = matcher::accept(c.conn(), face_id)?;
        c.assign_tag(photo_id, tag_id)?;
        faces_write_regions(c, photo_id);
        Ok(())
    })
    .await
}

/// Confirm a person across a multi-selection (issue #68): for every photo in `photo_ids`,
/// accept the faces the matcher already suggested as `tag_id`, assign the person tag to
/// those photos through the catalog, and re-export their MWG regions.
///
/// This accepts suggestions; it does not create them. Photos where the person was never
/// suggested are counted and returned, not force-assigned — see
/// [`matcher::accept_person_on_photos`] for why. The returned counters are what the UI
/// reports, so "confirmed on 6 of 9" stays honest.
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_accept_person(
    state: State<'_, AppState>,
    photo_ids: Vec<i64>,
    tag_id: i64,
) -> Result<crate::plugins::faces::matcher::AcceptPersonOutcome, String> {
    with_catalog_blocking(&state, move |c| {
        let (outcome, changed) = accept_person_in_catalog(c, &photo_ids, tag_id)?;
        // Sidecar writes are file I/O and best-effort, so they run after the commit: a NAS
        // that went offline must not roll back a confirmation the catalog already recorded.
        for photo_id in changed {
            faces_write_regions(c, photo_id);
        }
        Ok(outcome)
    })
    .await
}

/// The catalog half of [`faces_accept_person`]: confirm the suggested faces and assign the
/// person tag to the photos that changed, in one transaction — the face rows and the
/// photo-level tags they imply land together, so a failure part-way cannot leave confirmed
/// faces on photos that never got the tag. Split out from the command so it can be tested
/// against a real catalog (the command itself needs a Tauri `State`).
///
/// Returns the counters and the photos that changed, which are the only ones needing a
/// sidecar re-export.
#[cfg(feature = "faces")]
fn accept_person_in_catalog(
    c: &crate::catalog::Catalog,
    photo_ids: &[i64],
    tag_id: i64,
) -> crate::catalog::Result<(crate::plugins::faces::matcher::AcceptPersonOutcome, Vec<i64>)> {
    use crate::plugins::faces::matcher;
    let tx = c.conn().unchecked_transaction()?;
    let (outcome, changed) = matcher::accept_person_on_photos(&tx, photo_ids, tag_id)?;
    for &photo_id in &changed {
        // Same connection as `tx`, so these participate in the open transaction.
        c.assign_tag(photo_id, tag_id)?;
    }
    tx.commit()?;
    Ok((outcome, changed))
}

/// Reject the face's currently-suggested person: remember the (face, person) pair so it is
/// never re-proposed, and return the face to `unassigned`.
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_reject(state: State<'_, AppState>, face_id: i64) -> Result<(), String> {
    use crate::plugins::faces::matcher;
    with_catalog_blocking(&state, move |c| {
        let photo_id = faces_photo_of(c.conn(), face_id)?;
        matcher::reject(c.conn(), face_id, now_secs())?;
        // Unconfirming a face may have removed a confirmed region — re-export the photo's
        // current confirmed set so the sidecar stays in sync.
        if let Some(pid) = photo_id {
            faces_write_regions(c, pid);
        }
        Ok(())
    })
    .await
}

/// Mark a face `ignored` (photobomber / background crowd): excluded from centroids and
/// suggestions but kept so re-indexing doesn't resurrect it.
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_ignore(state: State<'_, AppState>, face_id: i64) -> Result<(), String> {
    use crate::plugins::faces::matcher;
    with_catalog_blocking(&state, move |c| {
        let photo_id = faces_photo_of(c.conn(), face_id)?;
        matcher::ignore(c.conn(), face_id)?;
        // Ignoring a previously-confirmed face drops it from the exported region set.
        if let Some(pid) = photo_id {
            faces_write_regions(c, pid);
        }
        Ok(())
    })
    .await
}

/// Manually assign a face to a specific person tag and confirm it, assigning the tag to the
/// photo through the catalog. Clears any prior rejection of that exact pair.
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_assign(state: State<'_, AppState>, face_id: i64, tag_id: i64) -> Result<(), String> {
    use crate::plugins::faces::matcher;
    with_catalog_blocking(&state, move |c| {
        let (photo_id, tag) = matcher::assign(c.conn(), face_id, tag_id)?;
        c.assign_tag(photo_id, tag)?;
        faces_write_regions(c, photo_id);
        Ok(())
    })
    .await
}

/// Name an unnamed cluster: create/bind the person tag at `tag_path`, confirm every member
/// face against it, and assign the tag to each member's photo. Returns nothing; the UI
/// re-queries.
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_name_cluster(
    state: State<'_, AppState>,
    cluster: i64,
    tag_path: String,
) -> Result<(), String> {
    use crate::plugins::faces::matcher;
    with_catalog_blocking(&state, move |c| {
        // Create (or resolve) the person tag by path — the existing tag-creation path.
        let tag_id = c.create_tag(&tag_path)?;
        // Bind the cluster members to it and collect their photos.
        let photos = matcher::name_cluster(c.conn(), cluster, tag_id)?;
        for photo_id in photos {
            c.assign_tag(photo_id, tag_id)?;
            faces_write_regions(c, photo_id);
        }
        Ok(())
    })
    .await
}

/// Insert a manually drawn face box for a face the detector missed. `x/y/w/h` are
/// normalized 0–1 in oriented-image space (the same space as detector bboxes). The row
/// starts `unassigned` with `source='drawn'`, no landmarks and no embedding — every
/// matching/centroid query requires an embedding, so a drawn box only ever becomes a
/// person through explicit assignment (`faces_assign`). Returns the new face id.
#[cfg(feature = "faces")]
#[tauri::command]
pub fn faces_add_manual(
    state: State<'_, AppState>,
    photo_id: i64,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) -> Result<i64, String> {
    // Clamp to the image; reject boxes that collapse to (near) nothing.
    let x0 = x.clamp(0.0, 1.0);
    let y0 = y.clamp(0.0, 1.0);
    let x1 = (x + w).clamp(0.0, 1.0);
    let y1 = (y + h).clamp(0.0, 1.0);
    let (bw, bh) = (x1 - x0, y1 - y0);
    if bw < 0.005 || bh < 0.005 {
        return Err("face box is too small".into());
    }
    with_catalog(&state, |c| {
        crate::plugins::faces::store::insert_face(
            c.conn(),
            photo_id,
            &format!("[{x0},{y0},{bw},{bh}]"),
            "[]",
            1.0,
            None,
            "drawn",
            now_secs(),
        )
        .map_err(crate::catalog::CatalogError::Sqlite)
    })
}

/// Delete a drawn, still-unassigned face box (a mis-draw). Guarded to `source='drawn'`:
/// detected faces must be rejected/ignored instead so re-indexing doesn't resurrect
/// them — a drawn box is never re-created by the indexer, so deleting it is safe.
/// (Once a drawn box is assigned, `faces_assign` flips its source to 'manual' and it is
/// treated like any other confirmed face.)
#[cfg(feature = "faces")]
#[tauri::command]
pub fn faces_delete_drawn(state: State<'_, AppState>, face_id: i64) -> Result<(), String> {
    let n = with_catalog(&state, |c| {
        c.conn()
            .execute(
                "DELETE FROM faces__faces WHERE id = ?1 AND source = 'drawn'",
                [face_id],
            )
            .map_err(crate::catalog::CatalogError::Sqlite)
    })?;
    if n == 0 {
        return Err("only unassigned drawn face boxes can be deleted".into());
    }
    Ok(())
}

/// Return all face rows for a single photo, joined to the tags table for the person name.
/// Used by the loupe overlay and the inspector panel.
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_for_photo(
    state: State<'_, AppState>,
    photo_id: i64,
) -> Result<Vec<crate::plugins::faces::store::FaceForPhoto>, String> {
    with_catalog_blocking(&state, move |c| {
        crate::plugins::faces::store::faces_for_photo(c.conn(), photo_id)
            .map_err(crate::catalog::CatalogError::Sqlite)
    })
    .await
}

// ── People-view summary queries (H13e) ─────────────────────────────────────────

/// One row in the people summary — a named person with face/photo counts and a
/// representative face for the avatar (the first confirmed face for that person).
#[cfg(feature = "faces")]
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PersonSummary {
    /// Person tag id (FK → tags.id).
    pub tag_id: i64,
    /// Leaf name of the person tag.
    pub name: String,
    /// Full hierarchical path of the person tag (e.g. "People/Family/Alice").
    pub full_path: String,
    /// Total number of confirmed face rows for this person.
    pub face_count: i64,
    /// Number of distinct photos that have at least one confirmed face for this person.
    pub photo_count: i64,
    /// Photo id of the representative face (for the avatar crop).
    pub avatar_photo_id: i64,
    /// Bbox of the representative face (normalized 0–1), serialized as `{x,y,w,h}`.
    pub avatar_bbox: crate::plugins::faces::store::FaceBboxJson,
}

/// Return a summary of all named people (confirmed faces, ≥1 face each). Used by
/// the People main view to render the people wall.
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_people_summary(
    state: State<'_, AppState>,
) -> Result<Vec<PersonSummary>, String> {
    with_catalog_blocking(&state, move |c| {
        use crate::plugins::faces::store::{ensure_schema, FaceBboxJson};
        use rusqlite::OptionalExtension;

        ensure_schema(c.conn()).map_err(crate::catalog::CatalogError::Sqlite)?;

        // Aggregate confirmed faces grouped by person_tag_id.
        let mut stmt = c.conn().prepare(
            "SELECT f.person_tag_id,
                    t.name,
                    t.full_path,
                    COUNT(f.id)                                  AS face_count,
                    COUNT(DISTINCT f.photo_id)                   AS photo_count,
                    MIN(f.id)                                    AS rep_face_id
               FROM faces__faces f
               JOIN tags t ON t.id = f.person_tag_id
              WHERE f.state = 'confirmed' AND f.person_tag_id IS NOT NULL
              GROUP BY f.person_tag_id
              ORDER BY t.full_path",
        ).map_err(crate::catalog::CatalogError::Sqlite)?;

        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
            ))
        }).map_err(crate::catalog::CatalogError::Sqlite)?;

        let mut out = Vec::new();
        for row in rows {
            let (tag_id, name, full_path, face_count, photo_count, rep_face_id) =
                row.map_err(crate::catalog::CatalogError::Sqlite)?;

            // Fetch the representative face's photo_id and bbox.
            let rep: Option<(i64, String)> = c.conn().query_row(
                "SELECT photo_id, bbox FROM faces__faces WHERE id = ?1",
                [rep_face_id],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            ).optional().map_err(crate::catalog::CatalogError::Sqlite)?;

            if let Some((photo_id, bbox_s)) = rep {
                let bbox = FaceBboxJson::from_str(&bbox_s);
                out.push(PersonSummary {
                    tag_id,
                    name,
                    full_path,
                    face_count,
                    photo_count,
                    avatar_photo_id: photo_id,
                    avatar_bbox: bbox,
                });
            }
        }
        Ok(out)
    })
    .await
}

/// One row in the cluster summary — an unnamed cluster with member count and a
/// representative face for the avatar.
#[cfg(feature = "faces")]
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterSummary {
    /// Cluster id (FK → faces__clusters.id).
    pub cluster_id: i64,
    /// Number of faces in this cluster.
    pub member_count: i64,
    /// Photo id of the representative face.
    pub avatar_photo_id: i64,
    /// Bbox of the representative face.
    pub avatar_bbox: crate::plugins::faces::store::FaceBboxJson,
}

/// Return a summary of all unnamed clusters (faces with a cluster_id but no
/// confirmed person). Used by the People view's "Unnamed clusters" section.
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_cluster_summary(
    state: State<'_, AppState>,
) -> Result<Vec<ClusterSummary>, String> {
    with_catalog_blocking(&state, move |c| {
        use crate::plugins::faces::store::{ensure_schema, FaceBboxJson};
        use rusqlite::OptionalExtension;

        ensure_schema(c.conn()).map_err(crate::catalog::CatalogError::Sqlite)?;

        // One row per cluster_id: count of members + first face as representative.
        let mut stmt = c.conn().prepare(
            "SELECT cluster_id, COUNT(*) AS cnt, MIN(id) AS rep_face_id
               FROM faces__faces
              WHERE cluster_id IS NOT NULL
                AND (state = 'unassigned' OR state = 'suggested')
              GROUP BY cluster_id
              ORDER BY cnt DESC",
        ).map_err(crate::catalog::CatalogError::Sqlite)?;

        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        }).map_err(crate::catalog::CatalogError::Sqlite)?;

        let mut out = Vec::new();
        for row in rows {
            let (cluster_id, member_count, rep_face_id) =
                row.map_err(crate::catalog::CatalogError::Sqlite)?;

            let rep: Option<(i64, String)> = c.conn().query_row(
                "SELECT photo_id, bbox FROM faces__faces WHERE id = ?1",
                [rep_face_id],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            ).optional().map_err(crate::catalog::CatalogError::Sqlite)?;

            if let Some((photo_id, bbox_s)) = rep {
                let bbox = FaceBboxJson::from_str(&bbox_s);
                out.push(ClusterSummary {
                    cluster_id,
                    member_count,
                    avatar_photo_id: photo_id,
                    avatar_bbox: bbox,
                });
            }
        }
        Ok(out)
    })
    .await
}

/// One suggested face row for the review queue.
#[cfg(feature = "faces")]
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SuggestionEntry {
    pub face_id: i64,
    pub photo_id: i64,
    pub bbox: crate::plugins::faces::store::FaceBboxJson,
    pub person_tag_id: i64,
    pub person_name: String,
    pub person_full_path: String,
    pub confidence: f64,
}

/// Return the full list of suggested (unconfirmed) face assignments, ordered by
/// confidence descending. Used by the People view's "Review suggestions" queue.
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_suggestion_list(
    state: State<'_, AppState>,
) -> Result<Vec<SuggestionEntry>, String> {
    with_catalog_blocking(&state, move |c| {
        use crate::plugins::faces::store::{ensure_schema, FaceBboxJson};

        ensure_schema(c.conn()).map_err(crate::catalog::CatalogError::Sqlite)?;

        let mut stmt = c.conn().prepare(
            "SELECT f.id, f.photo_id, f.bbox,
                    f.person_tag_id, t.name, t.full_path,
                    COALESCE(f.match_confidence, 0.0)
               FROM faces__faces f
               JOIN tags t ON t.id = f.person_tag_id
              WHERE f.state = 'suggested' AND f.person_tag_id IS NOT NULL
              ORDER BY f.match_confidence DESC NULLS LAST",
        ).map_err(crate::catalog::CatalogError::Sqlite)?;

        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, f64>(6)?,
            ))
        }).map_err(crate::catalog::CatalogError::Sqlite)?;

        let mut out = Vec::new();
        for row in rows {
            let (face_id, photo_id, bbox_s, tag_id, name, full_path, conf) =
                row.map_err(crate::catalog::CatalogError::Sqlite)?;
            let bbox = FaceBboxJson::from_str(&bbox_s);
            out.push(SuggestionEntry {
                face_id,
                photo_id,
                bbox,
                person_tag_id: tag_id,
                person_name: name,
                person_full_path: full_path,
                confidence: conf,
            });
        }
        Ok(out)
    })
    .await
}

// ── H16b: Sharpness index job ────────────────────────────────────────────────

/// Catalog-switch ownership for both face job families, indexing and matching.
///
/// Each is a real background job, which means the catalog-switch protocol has to reach it: a
/// job left running against a catalog the user has left would keep writing into it, and would
/// keep owning a status slot the panel re-queries on mount.
///
/// These drive the two switch phases directly, as `commands::smarttags`' ownership tests do
/// — the commands need a Tauri `AppHandle`, which a unit test cannot build.
#[cfg(all(test, feature = "faces"))]
mod faces_job_ownership_tests {
    use super::*;
    use crate::commands::catalog::{detach_catalog_and_trip_jobs, publish_catalog_and_reset_jobs};

    fn temp_catalog(tag: &str) -> (Catalog, crate::test_support::TestSubPath) {
        let dir = crate::test_support::TestTmpDir::new(&format!("faces-match-own-{tag}"));
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let db = dir.join("catalog.chairphoto");
        let catalog = Catalog::open(&db, &root).unwrap();
        (catalog, dir.into_subpath("catalog.chairphoto"))
    }

    fn state_with(catalog: Catalog) -> AppState {
        let state = AppState::default();
        *state.catalog.lock().unwrap() = Some(catalog);
        state
    }

    /// Phase one trips the running match **and** clears its status slot. Tripping alone
    /// would leave the old job reachable as the slot's owner, so `faces_match_status`
    /// would keep reporting a run belonging to a catalog that is gone.
    #[test]
    fn a_catalog_switch_trips_and_unpublishes_a_running_match() {
        let (cat_a, db_a) = temp_catalog("switch-a");
        let (cat_b, _db_b) = temp_catalog("switch-b");
        let state = state_with(cat_a);

        let claim = begin_faces_match_job(&state).unwrap();
        assert_eq!(claim.db_path, db_a.to_path_buf());
        assert!(!claim.abort.load(Ordering::Relaxed));
        assert_eq!(
            state.jobs.faces_match.status().unwrap().map(|s| s.job),
            Some(claim.job)
        );
        let abort = claim.abort.clone();

        detach_catalog_and_trip_jobs(&state).unwrap();

        assert!(abort.load(Ordering::Relaxed), "phase one must trip the running match");
        assert!(
            state.jobs.faces_match.status().unwrap().is_none(),
            "phase one must clear the slot, or the panel re-adopts a dead job"
        );

        // Phase two installs a fresh, un-tripped generation without reviving the old one.
        publish_catalog_and_reset_jobs(&state, cat_b).unwrap();
        let current = state.jobs.faces_match.installed().unwrap();
        assert!(!current.load(Ordering::Relaxed), "the new generation must start clean");
        assert!(!Arc::ptr_eq(&current, &abort), "phase two must not reuse the tripped flag");
        assert!(abort.load(Ordering::Relaxed), "the old worker's flag must stay tripped");
    }

    /// A start that lands between the two switch phases finds no catalog and must touch
    /// nothing — no new generation, no slot, no consumed job id — so the switch's abort
    /// signal cannot be stranded behind a fresh, live flag.
    #[test]
    fn a_match_start_between_switch_phases_touches_nothing() {
        let (cat_a, _db_a) = temp_catalog("between-a");
        let state = state_with(cat_a);

        detach_catalog_and_trip_jobs(&state).unwrap();

        let before = state.jobs.faces_match.installed().unwrap();
        let seq_before = state.jobs.faces_match.abort().job_ids_issued();

        let err = begin_faces_match_job(&state).unwrap_err();
        assert_eq!(err, "No catalog is open");

        let after = state.jobs.faces_match.installed().unwrap();
        assert!(Arc::ptr_eq(&before, &after), "a start mid-switch must not install a generation");
        assert_eq!(
            state.jobs.faces_match.abort().job_ids_issued(),
            seq_before,
            "a rejected start must not consume a job id"
        );
        assert!(state.jobs.faces_match.status().unwrap().is_none());
    }

    /// Starting a second match trips the first: two matching runs on one catalog would
    /// race each other's suggestion writes.
    #[test]
    fn a_second_match_start_trips_the_first() {
        let (cat, _db) = temp_catalog("supersede");
        let state = state_with(cat);

        let first = begin_faces_match_job(&state).unwrap();
        let second = begin_faces_match_job(&state).unwrap();

        assert!(first.abort.load(Ordering::Relaxed), "the superseded run must be tripped");
        assert!(!second.abort.load(Ordering::Relaxed));
        assert_ne!(first.job, second.job);
        assert_eq!(
            state.jobs.faces_match.status().unwrap().map(|s| s.job),
            Some(second.job)
        );
        assert!(!first.slot.owns(), "the superseded run must lose the slot");
    }

    // --- issue #51: the face *indexing* slot ------------------------------------------

    /// The #51 regression. A switch tripped the indexing abort flag but never cleared the
    /// indexing status slot — it cleared Smart Tagging's and face matching's and omitted this
    /// one — so between the switch and the worker next noticing its flag,
    /// `faces_index_status` returned a `FacesJobStatus` describing the replaced catalog.
    ///
    /// The interleaving is forced, not timed: the job is claimed through the real
    /// `begin_faces_index_job` (the same transition `faces_index_photos` runs), the switch is
    /// driven synchronously, and the slot is read straight afterwards. No worker runs at all,
    /// so there is no window for one to clean up on our behalf and make this pass for the
    /// wrong reason — which is exactly what made the original defect invisible.
    #[test]
    fn a_catalog_switch_trips_and_unpublishes_a_running_index() {
        let (cat_a, db_a) = temp_catalog("index-switch-a");
        let (cat_b, _db_b) = temp_catalog("index-switch-b");
        let state = state_with(cat_a);

        let claim = begin_faces_index_job(&state).unwrap();
        assert_eq!(claim.db_path, db_a.to_path_buf());
        assert!(!claim.abort.load(Ordering::Relaxed));
        assert_eq!(
            state.jobs.faces.status().unwrap().map(|s| s.job),
            Some(claim.job),
            "the start must own the slot before the switch"
        );
        let abort = claim.abort.clone();

        detach_catalog_and_trip_jobs(&state).unwrap();

        assert!(abort.load(Ordering::Relaxed), "phase one must trip the running index");
        assert!(
            state.jobs.faces.status().unwrap().is_none(),
            "phase one must clear the face-indexing slot too — leaving it is #51, where \
             faces_index_status reports a job against the catalog the user has left"
        );

        // Phase two installs a fresh, un-tripped generation without reviving the old one.
        publish_catalog_and_reset_jobs(&state, cat_b).unwrap();
        let current = state.jobs.faces.installed().unwrap();
        assert!(!current.load(Ordering::Relaxed), "the new generation must start clean");
        assert!(!Arc::ptr_eq(&current, &abort), "phase two must not reuse the tripped flag");
        assert!(abort.load(Ordering::Relaxed), "the old worker's flag must stay tripped");
        assert!(
            state.jobs.faces.status().unwrap().is_none(),
            "phase two must not resurrect a slot phase one cleared"
        );
    }

    /// All three slots, one switch. The per-family tests above each prove their own slot is
    /// cleared; this one pins that a single switch clears *every* slot together, which is the
    /// property #51 is actually about — two of three were cleared, and nothing asserted the
    /// third alongside them.
    #[test]
    fn a_catalog_switch_clears_every_job_status_slot_at_once() {
        let (cat_a, _db_a) = temp_catalog("all-slots-a");
        let (cat_b, _db_b) = temp_catalog("all-slots-b");
        let state = state_with(cat_a);

        let index = begin_faces_index_job(&state).unwrap();
        let matching = begin_faces_match_job(&state).unwrap();
        assert_eq!(state.jobs.faces.status().unwrap().map(|s| s.job), Some(index.job));
        assert_eq!(
            state.jobs.faces_match.status().unwrap().map(|s| s.job),
            Some(matching.job)
        );

        detach_catalog_and_trip_jobs(&state).unwrap();

        assert!(state.jobs.faces.status().unwrap().is_none(), "face indexing slot");
        assert!(state.jobs.faces_match.status().unwrap().is_none(), "face matching slot");
        #[cfg(feature = "smarttags")]
        assert!(state.jobs.smarttags.status().unwrap().is_none(), "smart tagging slot");

        publish_catalog_and_reset_jobs(&state, cat_b).unwrap();
    }

    // --- issue #22: set_library_root runs the same transition -------------------------

    /// The #22 regression. `set_library_root` replaces the catalog handle exactly as a switch
    /// does, but used to hold the lock across persist -> reopen -> swap and trip nothing, so a
    /// running job kept indexing a catalog the app had replaced.
    ///
    /// This drives the transition the command now runs — the same seam the switch tests above
    /// use, since the command itself needs a Tauri `State` a unit test cannot build. The
    /// interleaving is forced: a real indexing job is claimed first, phase one runs
    /// synchronously, and the assertions read the flag and slot directly. No worker exists to
    /// tidy up and make this pass for the wrong reason.
    ///
    /// It also pins the ordering constraint peculiar to re-rooting: `catalog_root` is written
    /// through the *outgoing* handle inside phase one, because `Catalog::open` adopts the
    /// stored setting over its `root` argument. Persisting after the reopen — or skipping the
    /// write — would silently re-root back to the old path, which the final assertion catches.
    #[tokio::test]
    async fn a_root_change_trips_running_jobs_and_persists_the_new_root() {
        use crate::commands::catalog::reroot_library;

        let dir = crate::test_support::TestTmpDir::new("reroot-ownership");
        let old_root = dir.join("photos-old");
        let new_root = dir.join("photos-new");
        std::fs::create_dir_all(&old_root).unwrap();
        let db = dir.join("catalog.chairphoto");
        let state = state_with(Catalog::open(&db, &old_root).unwrap());

        let claim = begin_faces_index_job(&state).unwrap();
        assert_eq!(state.jobs.faces.status().unwrap().map(|s| s.job), Some(claim.job));
        let abort = claim.abort.clone();

        // The command's real body, not the transition it delegates to — see
        // `reroot_library`'s doc comment for why that distinction is the whole test.
        reroot_library(&state, new_root.clone(), db.clone()).await.unwrap();

        assert!(
            abort.load(Ordering::Relaxed),
            "a root change must trip a running index — leaving it live is #22, where the \
             worker keeps writing into a catalog the app has replaced"
        );
        assert!(
            state.jobs.faces.status().unwrap().is_none(),
            "a root change must clear the status slot, like a switch does"
        );

        let current = state.jobs.faces.installed().unwrap();
        assert!(!current.load(Ordering::Relaxed), "the new generation must start clean");
        assert!(!Arc::ptr_eq(&current, &abort), "phase two must not reuse the tripped flag");
        assert!(abort.load(Ordering::Relaxed), "the old worker's flag must stay tripped");

        // The re-root actually took, and took through the persisted setting: `Catalog::open`
        // adopts the stored `catalog_root` over its `root` argument, so had the write not
        // happened inside phase one this would still read the old root.
        let guard = state.catalog.lock().unwrap();
        assert_eq!(
            guard.as_ref().unwrap().root(),
            new_root.as_path(),
            "the published catalog must be rooted at the new path"
        );
    }

    /// A failing persist must abort the whole transition rather than leave a half-applied one:
    /// nothing tripped, slot intact, catalog still open. That is why `before_drop` runs after
    /// every guard is acquired but before the first mutation.
    #[test]
    fn a_failed_root_persist_leaves_the_transition_untouched() {
        use crate::commands::catalog::detach_catalog_and_trip_jobs_with;

        let (cat_a, _db_a) = temp_catalog("reroot-fail");
        let state = state_with(cat_a);
        let claim = begin_faces_index_job(&state).unwrap();

        let err = detach_catalog_and_trip_jobs_with(&state, |_| {
            Err("simulated persist failure".to_string())
        })
        .unwrap_err();
        assert_eq!(err, "simulated persist failure");

        assert!(
            !claim.abort.load(Ordering::Relaxed),
            "a failed persist must not trip the job"
        );
        assert_eq!(
            state.jobs.faces.status().unwrap().map(|s| s.job),
            Some(claim.job),
            "a failed persist must not clear the status slot"
        );
        assert!(
            state.catalog.lock().unwrap().is_some(),
            "a failed persist must leave the catalog open"
        );
    }
}

/// Batch confirm (issue #68) against a **real catalog**, which is where the bug actually
/// lived: the face rows were only half the job — the person tag has to reach the photo too,
/// through `assign_tag`, or the catalog and the face rows disagree about who is in the photo.
/// The matcher's own unit tests cover which faces are selected; these cover the catalog side
/// of the transaction.
#[cfg(all(test, feature = "faces"))]
mod accept_person_tests {
    use super::*;
    use crate::plugins::faces::store;
    use rusqlite::OptionalExtension;

    fn temp_catalog(tag: &str) -> (Catalog, crate::test_support::TestSubPath) {
        let dir = crate::test_support::TestTmpDir::new(&format!("faces-accept-person-{tag}"));
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let db = dir.join("catalog.chairphoto");
        let catalog = Catalog::open(&db, &root).unwrap();
        store::ensure_schema(catalog.conn()).unwrap();
        (catalog, dir.into_subpath("catalog.chairphoto"))
    }

    fn add_photo(c: &Catalog, id: i64) {
        c.conn()
            .execute(
                "INSERT INTO photos (id, uuid, path, mtime_ns, size, extension, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 0, 0, 'nef', 0, 0)",
                rusqlite::params![id, format!("uuid-{id}"), format!("{id}.NEF")],
            )
            .unwrap();
    }

    /// A face already suggested as `tag_id`, as the matcher would leave it.
    fn add_suggested_face(c: &Catalog, photo_id: i64, tag_id: i64) -> i64 {
        let face_id = store::insert_face(
            c.conn(),
            photo_id,
            "[0.1,0.1,0.2,0.2]",
            "[]",
            0.99,
            None,
            "detect",
            0,
        )
        .unwrap();
        c.conn()
            .execute(
                "UPDATE faces__faces
                    SET person_tag_id = ?2, state = 'suggested', match_confidence = 0.9
                  WHERE id = ?1",
                rusqlite::params![face_id, tag_id],
            )
            .unwrap();
        face_id
    }

    fn has_tag(c: &Catalog, photo_id: i64, tag_id: i64) -> bool {
        c.conn()
            .query_row(
                "SELECT 1 FROM photo_tags WHERE photo_id = ?1 AND tag_id = ?2",
                rusqlite::params![photo_id, tag_id],
                |_| Ok(()),
            )
            .optional()
            .unwrap()
            .is_some()
    }

    fn face_state(c: &Catalog, face_id: i64) -> String {
        c.conn()
            .query_row(
                "SELECT state FROM faces__faces WHERE id = ?1",
                [face_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// The person tag reaches every photo whose suggestion was confirmed — and only those.
    /// A photo where the person was never suggested keeps neither the tag nor a face row,
    /// which is the whole point of accepting suggestions rather than force-assigning.
    #[test]
    fn accept_person_tags_exactly_the_photos_it_confirmed() {
        let (c, _db) = temp_catalog("tags");
        let alice = c.create_tag("People/Alice").unwrap();
        let bob = c.create_tag("People/Bob").unwrap();
        for id in 1..=3 {
            add_photo(&c, id);
        }
        let f1 = add_suggested_face(&c, 1, alice);
        let f2 = add_suggested_face(&c, 2, alice);
        let f3 = add_suggested_face(&c, 3, bob); // Alice was never suggested here

        let (outcome, changed) = accept_person_in_catalog(&c, &[1, 2, 3], alice).unwrap();

        assert_eq!(outcome.photos_confirmed, 2);
        assert_eq!(outcome.faces_confirmed, 2);
        assert_eq!(outcome.photos_without_suggestion, 1);
        assert_eq!(changed, vec![1, 2], "only these need a sidecar re-export");

        assert_eq!(face_state(&c, f1), "confirmed");
        assert_eq!(face_state(&c, f2), "confirmed");
        assert_eq!(face_state(&c, f3), "suggested", "Bob's suggestion is untouched");

        assert!(has_tag(&c, 1, alice), "the confirmed photo must carry the person tag");
        assert!(has_tag(&c, 2, alice), "…on every confirmed photo, not just the last one");
        assert!(!has_tag(&c, 3, alice), "a photo with no suggestion is not tagged");
        assert!(!has_tag(&c, 3, bob), "and confirming Alice does not confirm Bob");
    }

    /// Confirming a person twice is harmless: the second call finds them already confirmed,
    /// reports it as such, and leaves the photo-level tag exactly once.
    #[test]
    fn accept_person_is_idempotent() {
        let (c, _db) = temp_catalog("idempotent");
        let alice = c.create_tag("People/Alice").unwrap();
        add_photo(&c, 1);
        add_suggested_face(&c, 1, alice);

        accept_person_in_catalog(&c, &[1], alice).unwrap();
        let (outcome, changed) = accept_person_in_catalog(&c, &[1], alice).unwrap();

        assert_eq!(outcome.photos_confirmed, 0);
        assert_eq!(outcome.photos_already_confirmed, 1);
        assert!(changed.is_empty(), "nothing changed, so nothing to re-export");
        let tags: i64 = c
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM photo_tags WHERE photo_id = 1 AND tag_id = ?1",
                [alice],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tags, 1);
    }

    /// The batch is one transaction. A photo whose tagging fails part-way through must take
    /// the whole call down with it: faces confirmed earlier in the same batch have to roll
    /// back too, or the catalog ends up with faces confirmed as a person the photo was never
    /// tagged with. The failure is injected with a trigger, because that seam — `assign_tag`
    /// failing after the face rows are already updated — has no natural trigger in a test.
    #[test]
    fn a_failed_assignment_rolls_back_the_whole_batch() {
        let (c, _db) = temp_catalog("rollback");
        let alice = c.create_tag("People/Alice").unwrap();
        add_photo(&c, 1);
        add_photo(&c, 2);
        let f1 = add_suggested_face(&c, 1, alice);
        let f2 = add_suggested_face(&c, 2, alice);
        // Photo 1 confirms and tags cleanly; photo 2's tag insert aborts.
        c.conn()
            .execute_batch(
                "CREATE TRIGGER refuse_tagging_photo_2 BEFORE INSERT ON photo_tags
                   WHEN NEW.photo_id = 2
                   BEGIN SELECT RAISE(ABORT, 'injected tagging failure'); END;",
            )
            .unwrap();

        assert!(accept_person_in_catalog(&c, &[1, 2], alice).is_err());

        assert_eq!(
            face_state(&c, f1),
            "suggested",
            "a failure on a later photo must roll back the earlier confirmation"
        );
        assert_eq!(face_state(&c, f2), "suggested");
        assert!(!has_tag(&c, 1, alice), "…and its photo-level tag with it");
    }
}
