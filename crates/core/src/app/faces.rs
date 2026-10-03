//! Face tagging — the bodies of the Tauri `faces_*` commands the GPUI Faces module runs:
//! settings (inference line, `indexing.speed`), the indexing job, the per-photo face list and
//! the per-face review verbs, and the MWG-Regions sidecar wiring they share (#129); the
//! matching job ([`matching`]) and the People view's reads and verbs ([`people`]) (#130).
//!
//! **Blocking.** Everything here takes the catalog (or does disk/model work): call it on a
//! worker, never a UI thread. The per-face verbs take a `&Catalog`, so a caller picks the
//! lock: the Tauri commands run them under `with_catalog_blocking`, the GPUI app under
//! `with_catalog_as` with the [`super::CatalogIdentity`] the face rows were read under, so a
//! face id read from one catalog can never be written into the next (map #92, "Catalog
//! identity").
//!
//! **Sidecars.** Every verb that changes a photo's confirmed set re-exports that photo's
//! MWG regions ([`write_regions`]), merge-safe: only ChairPhoto's own regions are replaced,
//! foreign ones are preserved (`plugins::faces::regions`, AGENTS.md "XMP safety"). A sidecar
//! that cannot be written is logged, never fatal: the catalog is authoritative.
//!
//! See docs/face-tagging.md.

use super::{
    now_secs, spawn_blocking, AppState, CatalogIdentity, CoreEvent, EventSink, FacesIndexDone, FacesJobStatus,
    FacesProgressEvent, JobClaim,
};
use crate::catalog::{Catalog, CatalogError, Result as CatalogResult, Tag};
use crate::plugins::faces::{engine, indexer, matcher, models, regions, store};
use crate::plugins::indexing;
use rusqlite::OptionalExtension;

pub use matcher::AcceptPersonOutcome;
pub use store::{FaceBboxJson, FaceForPhoto};

pub mod matching;
pub mod people;
pub use matching::{begin_match_job, cancel_match, cancel_match_job, match_status, run_match_job, start_match};
pub use people::{
    cluster_faces, cluster_summary, effective_people_root, ignore_faces, name_clusters, name_faces, people_summary,
    review_suggestions, suggestion_list, ClusterFace, ClusterSummary, NameOutcome, PersonSummary, Review,
    ReviewOutcome, SuggestionEntry, Verdict, NOTHING_TO_NAME,
};

/// What starting an index answers while the models are missing.
pub const MODELS_MISSING: &str = "Face models are not downloaded. Use faces_download_models first.";

// ── Settings ─────────────────────────────────────────────────────────────────

/// Where face inference actually runs, plus the indexing-speed setting — the settings
/// panel's "Inference" and "Indexing speed" lines (`faces_inference_info`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FacesInferenceInfo {
    /// `"cuda"` | `"cpu"` | `"unbuilt"` (no inference has run yet this session).
    pub ep: String,
    /// Whether this binary was compiled with the `faces-cuda` feature at all.
    pub cuda_built: bool,
    /// Effective `indexing.speed` setting (`"background"` | `"full"`).
    pub speed: String,
}

/// The inference line and the effective `indexing.speed` (`background` unless set to `full`).
pub fn inference_info(c: &Catalog) -> CatalogResult<FacesInferenceInfo> {
    use engine::ActiveEp;
    let ep = match engine::active_ep() {
        ActiveEp::Cuda => "cuda",
        ActiveEp::Cpu => "cpu",
        ActiveEp::Unbuilt => "unbuilt",
    };
    let speed = match c.get_setting(indexing::INDEXING_SPEED_SETTING)? {
        Some(s) if s.trim().eq_ignore_ascii_case("full") => "full",
        _ => "background",
    };
    Ok(FacesInferenceInfo { ep: ep.into(), cuda_built: cfg!(feature = "faces-cuda"), speed: speed.into() })
}

/// Set the global `indexing.speed` (`"background"` | `"full"`). It takes effect on the next
/// indexing run: the run re-reads it and `engine::configure` rebuilds the session pool when
/// it changed (`engine::PoolKey`); a run in progress keeps the pool it started with.
pub fn set_indexing_speed(c: &Catalog, speed: &str) -> CatalogResult<()> {
    let v = speed.trim().to_ascii_lowercase();
    if v != "background" && v != "full" {
        return Err(CatalogError::Validation(format!("invalid indexing speed: {speed}")));
    }
    c.set_setting(indexing::INDEXING_SPEED_SETTING, &v)
}

/// The tags the person picker offers, and the people root new persons are created under: the
/// people root and its descendants. The root is the one the matcher and the People view use
/// ([`people::effective_people_root`]: `faces.people_root`, `People` when unset), so a person
/// created in the inspector or the loupe overlay is one the matcher counts. (React's
/// `loadPeopleTags` treated an unset root as "no root" and created top-level tags the matcher
/// never saw.)
#[derive(Debug, Clone, Default)]
pub struct PeopleTags {
    /// The effective people root (never empty for a catalog read).
    pub root: String,
    pub tags: Vec<Tag>,
}

pub fn people_tags(c: &Catalog) -> CatalogResult<PeopleTags> {
    let root = effective_people_root(c)?;
    let prefix = format!("{root}/");
    let tags = c
        .list_tags_with_counts()?
        .into_iter()
        .map(|t| t.tag)
        .filter(|t| t.full_path == root || t.full_path.starts_with(&prefix))
        .collect();
    Ok(PeopleTags { root, tags })
}

/// The tag path a new person named `name` gets: under the people root (`name` alone only for
/// an empty root, which [`people_tags`] never returns).
pub fn person_path(root: &str, name: &str) -> String {
    let (root, name) = (root.trim(), name.trim());
    if root.is_empty() {
        name.to_string()
    } else {
        format!("{root}/{name}")
    }
}

// ── The indexing job ─────────────────────────────────────────────────────────

/// Claim ownership of the face-**indexing** job: snapshot the catalog, allocate the job id,
/// trip the previous job, install this job's abort flag and claim the status slot as ONE
/// transition, holding catalog → abort → slot throughout
/// ([`super::jobs::JobFamily::begin_as`]). With `expected`, only while that catalog is open.
///
/// `total` is unknown until the queue is populated; claiming anyway is what lets a status
/// query between "start returned" and "first progress event" already see it running.
pub fn begin_index_job(
    state: &AppState,
    expected: Option<CatalogIdentity>,
) -> Result<JobClaim<FacesJobStatus>, String> {
    state.jobs.faces.begin_as(&state.catalog, expected, |job| FacesJobStatus { job, done: 0, total: 0 })
}

/// Begin (or resume) the background face-indexing job and return its id. The worker opens
/// its own secondary catalog connection; progress goes out as `faces:progress` and the end as
/// a terminal `faces:index_done`, both carrying the id, through `state`'s event sink.
///
/// A start supersedes a running index (its flag is tripped). Photos already indexed are
/// skipped; offline photos stay queued. Fails — touching nothing — when the models are
/// missing, no catalog is open, or (with `expected`) another catalog is open.
pub fn start_index(state: &AppState, expected: Option<CatalogIdentity>) -> Result<u64, String> {
    // The models are checked before any lock is taken: a start that fails here leaves the
    // running job and its status slot exactly as they were.
    if !models::status().ready {
        return Err(MODELS_MISSING.to_string());
    }
    let claim = begin_index_job(state, expected)?;
    let job = claim.job;
    let sink = state.clone();
    spawn_blocking(move || run_index_job(&sink, claim));
    Ok(job)
}

/// The running index's status, `None` when idle (`faces_index_status`): a panel re-attaches
/// to it instead of believing it is idle.
pub fn index_status(state: &AppState) -> Result<Option<FacesJobStatus>, String> {
    state.jobs.faces.status()
}

/// Trip whatever index is running (`faces_index_cancel`). It stops after the current photo;
/// the queue keeps the rest for the next run.
pub fn cancel_index(state: &AppState) -> Result<(), String> {
    state.jobs.faces.cancel()
}

/// Cancel index `job` only — a no-op (`false`) once a newer job owns the family.
pub fn cancel_index_job(state: &AppState, job: u64) -> Result<bool, String> {
    state.jobs.faces.cancel_job(job)
}

/// The indexing worker: everything after the claim. Blocking — runs on a worker thread.
/// The status slot is cleared (only while this job still owns it) **before** the terminal
/// event, so a re-attaching panel never adopts a job whose end it already missed.
pub fn run_index_job(sink: &(impl EventSink + ?Sized), claim: JobClaim<FacesJobStatus>) {
    let JobClaim { db_path, root, abort, job, slot } = claim;
    let send_done = |d: FacesIndexDone| sink.send(CoreEvent::FacesIndexDone(d));
    let failed = |error: String| FacesIndexDone {
        ok: false,
        done: 0,
        total: 0,
        offline: 0,
        failed: 0,
        aborted: false,
        job,
        error: Some(error),
    };

    // Secondary connection — never contends with the primary's UI reads.
    let sec = match Catalog::open_secondary(&db_path, &root) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("faces_index: couldn't open secondary connection: {e}");
            slot.clear();
            send_done(failed(format!("couldn't open catalog connection: {e}")));
            return;
        }
    };

    // Detection + embedding with the real engine.
    let detect_fn = |jpeg: &[u8]| -> Vec<indexer::IndexedFace> {
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
                let embedding = engine::embed_face(&img, &f.landmarks).ok().map(|e| e.to_vec());
                indexer::IndexedFace { bbox: f.bbox, landmarks: f.landmarks, confidence: f.confidence, embedding }
            })
            .collect()
    };

    let progress_slot = slot.clone();
    let emit_fn = |p: indexer::FacesProgress| {
        // Keep the slot current for a re-attaching panel; `publish` refuses once a newer job
        // owns it (a superseded run's in-flight chunk still emits a few stragglers).
        progress_slot.publish(|job| FacesJobStatus { job, done: p.done, total: p.total });
        sink.send(CoreEvent::FacesProgress(FacesProgressEvent { done: p.done, total: p.total, job }));
    };

    // The people root for the MWG-region import: an imported face's person tag is
    // created/found at "<people_root>/<region name>".
    let people_root = matcher::MatchSettings::load(sec.conn())
        .map(|s| s.people_root)
        .unwrap_or_else(|_| crate::plugins::faces::PEOPLE_ROOT_DEFAULT.to_string());

    // Size the ONNX session pool to `indexing.speed` before the first inference builds it
    // (the pool is keyed by this configuration, so a changed value rebuilds it), and honour
    // `faces.force_cpu` the same way.
    let plan = indexing::load_indexing_plan(sec.conn());
    engine::configure(plan.parallelism, plan.intra_threads);
    engine::configure_force_cpu(indexer::load_force_cpu(sec.conn()));

    // First, the one-time conversion of the pre-marker regions (#135): before indexing, so a
    // photo's old display-frame regions are converted and marked before anything reads them.
    // It shares this job's abort flag, ownership and connection; what it does not reach (an
    // abort, an offline photo, a failure) stays on the record for the next index run. Its
    // progress goes out through this job's own slot and `faces:progress` events — photos
    // converted of photos to convert — before the index's own count starts again from 0
    // (review N3; a phase label would mean a new field in both front ends). A failure to run
    // it is logged, never fatal to indexing.
    let resolve = |photo_id: i64| sec.resolve_photo_path(photo_id).map_err(|e| e.to_string());
    let converting = |done: usize, total: usize| emit_fn(indexer::FacesProgress { done, total });
    match regions::convert_legacy_regions(sec.conn(), resolve, &abort, converting) {
        Ok(c) if c == regions::LegacyConversion::default() => {}
        Ok(c) => eprintln!("faces_index: pre-marker regions: {c:?}"),
        Err(e) => eprintln!("faces_index: pre-marker region conversion failed: {e}"),
    }

    let result = {
        let resolve_fn = |photo_id: i64| sec.resolve_photo_path(photo_id).map_err(|e| e.to_string());
        let preview_fn = |path: &std::path::Path| crate::thumbnails::preview_bytes(path);
        // Import the photo's existing MWG face regions, IoU-matched to the fresh detections.
        let import_hook = |hook_conn: &rusqlite::Connection, photo_id: i64, path: &std::path::Path| {
            import_regions(&sec, hook_conn, photo_id, path, &people_root);
        };
        indexer::run_index_with_hook(
            sec.conn(),
            plan.parallelism,
            resolve_fn,
            preview_fn,
            detect_fn,
            &abort,
            emit_fn,
            import_hook,
        )
    };

    slot.clear();
    match result {
        Ok(o) => send_done(FacesIndexDone {
            ok: true,
            done: o.done,
            total: o.total,
            offline: o.offline,
            failed: o.failed,
            aborted: o.aborted,
            job,
            error: None,
        }),
        Err(e) => {
            eprintln!("faces_index: job failed: {e}");
            send_done(failed(e.to_string()));
        }
    }
}

/// Import existing MWG face regions from a photo's sidecar during indexing: parse
/// `mwg-rs:Regions`, IoU-match named regions to the photo's just-detected unassigned faces,
/// and for each match find/create the person tag under `<people_root>/<name>`, confirm the
/// face (`source='xmp'`) and assign the tag to the photo. All failures are logged and
/// swallowed — region import must never break indexing.
pub fn import_regions(
    catalog: &Catalog,
    conn: &rusqlite::Connection,
    photo_id: i64,
    path: &std::path::Path,
    people_root: &str,
) {
    // In the EXIF-oriented frame the detections are in (#136).
    let frame = match regions::region_frame(conn, photo_id) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("faces_import: read the region frame of photo {photo_id} failed: {e}");
            return;
        }
    };
    let read = crate::xmp::read_face_regions_in(path, frame);
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
        let tag_path = format!("{people_root}/{}", m.name);
        let tag_id = match catalog.create_tag(&tag_path) {
            Ok(id) => id,
            Err(e) => {
                eprintln!("faces_import: create_tag('{tag_path}') failed: {e}");
                continue;
            }
        };
        // Fail closed: a lookup that errors leaves the face unconfirmed, as a refusal does.
        match catalog.auto_tag_refusal(tag_id) {
            Ok(None) => {}
            Ok(Some(refusal)) => {
                eprintln!("faces_import: skipped face {}: {refusal}", m.face_id);
                continue;
            }
            Err(e) => {
                eprintln!("faces_import: auto-tag check for '{tag_path}' failed, face {} left unconfirmed: {e}", m.face_id);
                continue;
            }
        }
        if let Err(e) = regions::confirm_imported_face(conn, m.face_id, tag_id) {
            eprintln!("faces_import: confirm face {} failed: {e}", m.face_id);
            continue;
        }
        if let Err(e) = catalog.assign_tag(photo_id, tag_id) {
            eprintln!("faces_import: assign_tag failed for photo {photo_id}: {e}");
        }
    }
}

// ── Sidecar wiring ───────────────────────────────────────────────────────────

/// Write a photo's confirmed face regions into its XMP sidecar (`mwg-rs:Regions`),
/// merge-safe. Best-effort: a failure is logged, never fatal (the catalog is authoritative).
pub fn write_regions(c: &Catalog, photo_id: i64) {
    let resolve = |id: i64| c.resolve_photo_path(id).map_err(|e| e.to_string());
    if let Err(e) = regions::write_photo_regions(c.conn(), photo_id, resolve) {
        eprintln!("faces: MWG region sidecar write failed for photo {photo_id}: {e}");
    }
}

/// The photo a face belongs to, read before a mutation clears its association; `None` when
/// the face row is gone.
pub fn photo_of(conn: &rusqlite::Connection, face_id: i64) -> rusqlite::Result<Option<i64>> {
    conn.query_row("SELECT photo_id FROM faces__faces WHERE id = ?1", [face_id], |r| r.get::<_, i64>(0)).optional()
}

// ── The per-photo list and the per-face verbs ────────────────────────────────

/// Every face row of a photo, with the person's name (`faces_for_photo`): the inspector
/// panel's list and the loupe overlay's boxes.
pub fn faces_for_photo(c: &Catalog, photo_id: i64) -> CatalogResult<Vec<FaceForPhoto>> {
    store::ensure_schema(c.conn())?;
    Ok(store::faces_for_photo(c.conn(), photo_id)?)
}

/// Confirm a suggested face: mark it `confirmed` and assign the person tag to the photo
/// through `assign_tag` (keyword XMP export, merge by tag UUID), then re-export regions.
/// One transaction, so a refused tag (an auto-tag, #181) leaves the face unconfirmed.
pub fn accept(c: &Catalog, face_id: i64) -> CatalogResult<()> {
    let tx = c.conn().unchecked_transaction()?;
    let (photo_id, tag_id) = matcher::accept(&tx, face_id)?;
    // Same connection as `tx`, so this is part of the transaction.
    c.assign_tag(photo_id, tag_id)?;
    tx.commit()?;
    write_regions(c, photo_id);
    Ok(())
}

/// Confirm a person across a multi-selection (#68): the faces the matcher already suggested
/// as `tag_id`, never a photo where it did not — those are counted, not force-assigned. The
/// sidecars of the photos that changed are re-exported after the commit, so an offline NAS
/// cannot roll back a confirmation the catalog recorded.
pub fn accept_person(c: &Catalog, photo_ids: &[i64], tag_id: i64) -> CatalogResult<AcceptPersonOutcome> {
    let (outcome, changed) = accept_person_in_catalog(c, photo_ids, tag_id)?;
    for photo_id in changed {
        write_regions(c, photo_id);
    }
    Ok(outcome)
}

/// The catalog half of [`accept_person`]: confirm the suggested faces and assign the person
/// tag to the photos that changed, in one transaction — a failure part-way cannot leave
/// confirmed faces on photos that never got the tag. Returns the counters and the photos
/// that changed (the only ones needing a sidecar re-export).
pub fn accept_person_in_catalog(
    c: &Catalog,
    photo_ids: &[i64],
    tag_id: i64,
) -> CatalogResult<(AcceptPersonOutcome, Vec<i64>)> {
    let tx = c.conn().unchecked_transaction()?;
    let (outcome, changed) = matcher::accept_person_on_photos(&tx, photo_ids, tag_id)?;
    for &photo_id in &changed {
        // Same connection as `tx`, so these participate in the open transaction.
        c.assign_tag(photo_id, tag_id)?;
    }
    tx.commit()?;
    Ok((outcome, changed))
}

/// Reject the face's suggested person: remember the pair so it is never re-proposed, return
/// the face to `unassigned`, and re-export the photo's (possibly smaller) confirmed set.
pub fn reject(c: &Catalog, face_id: i64) -> CatalogResult<()> {
    let photo_id = photo_of(c.conn(), face_id)?;
    matcher::reject(c.conn(), face_id, now_secs())?;
    if let Some(pid) = photo_id {
        write_regions(c, pid);
    }
    Ok(())
}

/// Mark a face `ignored` (photobomber, background crowd): kept so re-indexing does not
/// resurrect it, excluded from centroids and suggestions, dropped from the exported regions.
pub fn ignore(c: &Catalog, face_id: i64) -> CatalogResult<()> {
    let photo_id = photo_of(c.conn(), face_id)?;
    matcher::ignore(c.conn(), face_id)?;
    if let Some(pid) = photo_id {
        write_regions(c, pid);
    }
    Ok(())
}

/// Assign a face to `tag_id` and confirm it, tagging the photo; clears a prior rejection of
/// that exact pair. An auto-tag is refused before the face changes (#181).
pub fn assign(c: &Catalog, face_id: i64, tag_id: i64) -> CatalogResult<()> {
    c.refuse_auto_tag(tag_id)?;
    let (photo_id, tag) = matcher::assign(c.conn(), face_id, tag_id)?;
    c.assign_tag(photo_id, tag)?;
    write_regions(c, photo_id);
    Ok(())
}

/// "＋ Create" in the person picker: find or create the person tag at `path`, then
/// [`assign`] the face to it — under one catalog lock, so the new tag and the assignment land
/// in the same catalog. Returns the tag id.
pub fn assign_new_person(c: &Catalog, face_id: i64, path: &str) -> CatalogResult<i64> {
    let path = path.trim();
    if path.is_empty() || path.split('/').all(|s| s.trim().is_empty()) {
        return Err(CatalogError::Validation("a person needs a name".into()));
    }
    let tag_id = c.create_tag(path)?;
    assign(c, face_id, tag_id)?;
    Ok(tag_id)
}

/// The smallest drawn box (normalized) a manual face may be, either side.
pub const MIN_DRAWN: f64 = 0.005;

/// Insert a manually drawn box for a face the detector missed (`faces_add_manual`). `x/y/w/h`
/// are normalized 0–1 against the oriented image (the detector's space); the box is clamped
/// to the image and refused when it collapses to nothing. It starts `unassigned`,
/// `source='drawn'`, with no embedding, so it only becomes a person by explicit assignment.
pub fn add_manual(c: &Catalog, photo_id: i64, x: f64, y: f64, w: f64, h: f64) -> CatalogResult<i64> {
    let (x0, y0) = (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0));
    let (x1, y1) = ((x + w).clamp(0.0, 1.0), (y + h).clamp(0.0, 1.0));
    let (bw, bh) = (x1 - x0, y1 - y0);
    if !(bw >= MIN_DRAWN && bh >= MIN_DRAWN) {
        return Err(CatalogError::Validation("face box is too small".into()));
    }
    store::ensure_schema(c.conn())?;
    Ok(store::insert_face(c.conn(), photo_id, &format!("[{x0},{y0},{bw},{bh}]"), "[]", 1.0, None, "drawn", now_secs())?)
}

/// Delete a drawn, still-unassigned box (a mis-draw). Only `source='drawn'`: a detected face
/// must be rejected or ignored instead, so re-indexing cannot resurrect it.
///
/// The photo's regions are written first, while the row still exists: a write removes a
/// region carrying this catalog's marker only for a face id it knows on the photo (review
/// N1), so a region of this face written now is removed, and after the delete it never could
/// be. (A face still `drawn` was never confirmed — confirming makes it `manual` or keeps the
/// matcher's `match` — so normally it has no region; this is the defensive half.)
pub fn delete_drawn(c: &Catalog, face_id: i64) -> CatalogResult<()> {
    let drawn: Option<i64> = c
        .conn()
        .query_row("SELECT photo_id FROM faces__faces WHERE id = ?1 AND source = 'drawn'", [face_id], |r| r.get(0))
        .optional()?;
    if let Some(photo_id) = drawn {
        write_regions(c, photo_id);
    }
    let n = c.conn().execute("DELETE FROM faces__faces WHERE id = ?1 AND source = 'drawn'", [face_id])?;
    if n == 0 {
        return Err(CatalogError::Validation("only unassigned drawn face boxes can be deleted".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
