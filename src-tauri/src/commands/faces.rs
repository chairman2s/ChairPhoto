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
        .get_setting(crate::plugins::faces::indexer::INDEXING_SPEED_SETTING)
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

/// Set the global `indexing.speed` setting (`"background"` | `"full"`). Takes effect on
/// the NEXT app start: the inference session pool is sized once at first build and cached
/// for the process lifetime (see `engine::configure`).
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
        .set_setting(crate::plugins::faces::indexer::INDEXING_SPEED_SETTING, &v)
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

    let job_slot = state.faces_job.clone();

    // Snapshot the catalog, allocate the job id, trip the previous job, install this job's
    // abort flag and claim the status slot as ONE transition, holding all three locks.
    //
    // Two starts can run concurrently, since Tauri dispatches commands onto its runtime.
    // If the abort lock were released before the slot write they could interleave: A
    // installs its flag, B trips A and claims the slot, then A — already aborted —
    // overwrites the slot with itself and the panel tracks a dead job. Job ids cannot
    // arbitrate that, because they are allocated after the flag is installed.
    //
    // The catalog lock is held for the same reason, against switch_catalog rather than
    // against another start. The switch now holds that same lock while it trips the
    // current flag and drops the handle, and again while it publishes the new catalog and
    // fresh flags, so only three interleavings exist: the switch completes first and this
    // job snapshots the new catalog; it runs entirely after and trips the generation
    // installed here; or it is mid-switch, in which case the catalog reads as None above
    // and this job returns having touched nothing. Snapshotting outside this block would
    // allow a fourth — install an un-tripped generation after the switch's only abort
    // signal, then index the catalog it is about to close.
    //
    // Every *nested* acquisition in the backend is catalog → abort → slot: this block, the
    // same block in `begin_smarttags_job`, and both switch_catalog phases. The scan,
    // sharpness and pHash starts install their abort generation and release that lock
    // before reading the catalog, so they never hold two at once and cannot invert against
    // this order; switch_catalog covers them instead by tripping whatever is installed
    // before it replaces anything. faces_index_cancel takes only the abort lock and workers
    // only the slot lock.
    //
    // Everything fallible either precedes this block or is read-only within it, so an
    // error cannot leave the previous job aborted with no successor.
    let (db_path, root, abort, job) = {
        let cat_guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let c = cat_guard.as_ref().ok_or("No catalog is open")?;
        let (db_path, root) = (c.db_path().to_path_buf(), c.root().to_path_buf());

        let mut abort_guard = state.faces_abort.lock().map_err(|e| e.to_string())?;
        let mut slot_guard = job_slot.lock().map_err(|e| e.to_string())?;
        let job = state.faces_job_seq.fetch_add(1, Ordering::Relaxed) + 1;
        abort_guard.store(true, Ordering::Relaxed);
        let fresh = Arc::new(AtomicBool::new(false));
        *abort_guard = fresh.clone();
        // Claim the slot (total is unknown until the queue is populated) so a status query
        // between "command returned" and "first progress event" already sees it running.
        *slot_guard = Some(FacesJobStatus { job, done: 0, total: 0 });
        (db_path, root, fresh, job)
    };

    tauri::async_runtime::spawn_blocking(move || {
        use crate::catalog::Catalog;
        use crate::plugins::faces::indexer;

        // Release the status slot — but only if a newer job hasn't already claimed it.
        let clear_job_slot = || {
            if let Ok(mut slot) = job_slot.lock() {
                if slot.map(|s| s.job) == Some(job) {
                    *slot = None;
                }
            }
        };

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
            // but its in-flight chunk may still emit a few stragglers).
            if let Ok(mut slot) = progress_slot.lock() {
                if slot.map(|s| s.job) == Some(job) {
                    *slot = Some(FacesJobStatus { job, done: p.done, total: p.total });
                }
            }
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
        // to it before the first inference builds the pool. `configure` is a no-op once a
        // pool exists (first run wins), so `background` stays the responsive default.
        let plan = indexer::load_indexing_plan(sec.conn());
        crate::plugins::faces::engine::configure(plan.parallelism, plan.intra_threads);
        // Honor the `faces.force_cpu` setting: in a `faces-cuda` build this keeps inference on
        // CPU even when a GPU is available (inert in a CPU-only build). Also a no-op once the
        // pool exists, so it must be set before the first inference — same lifecycle as above.
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
    Ok(*state.faces_job.lock().map_err(|e| e.to_string())?)
}

#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_index_cancel(state: State<'_, AppState>) -> Result<(), String> {
    let guard = state
        .faces_abort
        .lock()
        .map_err(|e| e.to_string())?;
    guard.store(true, Ordering::Relaxed);
    Ok(())
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

/// Run the full seed / match / cluster pipeline over all indexed faces. Idempotent and
/// re-runnable (confirmed/ignored/manual faces are never touched, rejected pairs never
/// re-proposed). Returns per-step counters for the UI. Runs on a blocking worker so the UI
/// thread is never stalled.
/// Progress payload (`faces:progress`) for a matching run: the pipeline step label plus
/// counts. Carries no `job` field — the settings panel treats job-less progress events as
/// matching progress (model download emits nothing).
#[cfg(feature = "faces")]
#[derive(Debug, Clone, serde::Serialize)]
pub struct FacesMatchProgressEvent {
    pub done: usize,
    pub total: usize,
    pub phase: &'static str,
}

#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_run_matching(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<crate::plugins::faces::MatchOutcome, String> {
    use crate::plugins::faces::matcher::{self, MatchPhase, MatchSettings};
    with_catalog_blocking(&state, move |c| {
        let conn = c.conn();
        let settings = MatchSettings::load(conn)?;
        let now = now_secs();
        // Forward pipeline progress to the UI, throttled: phase changes and phase ends
        // always fire; within a phase, every 25th item (steps iterate thousands of
        // faces and per-item events would flood the IPC bridge).
        let mut last: Option<(MatchPhase, usize)> = None;
        let mut on_progress = |phase: MatchPhase, done: usize, total: usize| {
            let fire = match last {
                Some((p, d)) => p != phase || done >= total || done >= d + 25,
                None => true,
            };
            if !fire {
                return;
            }
            last = Some((phase, done));
            let _ = app.emit(
                "faces:progress",
                FacesMatchProgressEvent { done, total, phase: phase.label() },
            );
        };
        Ok(matcher::run_matching_with_progress(
            conn,
            &settings,
            now,
            &mut on_progress,
        )?)
    })
    .await
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

