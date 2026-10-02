//! Face-tagging commands (H13) — detection, recognition, clustering and the
//! confirm/reject surface, plus MWG-Regions sidecar sync.
//!
//! Gated on the `faces` Cargo feature; see `docs/face-tagging.md` and
//! `plugins/faces/`. All inference is local — no image ever leaves the machine.
//!
//! The settings, indexing and per-face bodies (#129), and the matching job and the People
//! view's summaries (#130), live in the core (`app::faces`), which the GPUI app runs too; the
//! commands here are their thin Tauri wrappers.

use super::*;
#[cfg(feature = "faces")]
use crate::app::faces as core_faces;
#[cfg(all(test, feature = "faces"))]
use crate::app::faces::begin_index_job;
use tauri::State;

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
/// Faces settings panel so GPU-vs-CPU is visible in the app (`app::faces::inference_info`).
#[cfg(feature = "faces")]
#[tauri::command(async)]
pub fn faces_inference_info(state: State<'_, AppState>) -> Result<core_faces::FacesInferenceInfo, String> {
    with_catalog(&state, core_faces::inference_info)
}

/// Set the global `indexing.speed` setting (`"background"` | `"full"`); it takes effect on
/// the next face-indexing run (`app::faces::set_indexing_speed`).
#[cfg(feature = "faces")]
#[tauri::command(async)]
pub fn faces_set_indexing_speed(state: State<'_, AppState>, speed: String) -> Result<(), String> {
    with_catalog(&state, |c| core_faces::set_indexing_speed(c, &speed))
}

// ── Face-indexing commands (H13b) ────────────────────────────────────────────

/// Begin (or resume) the background face-indexing job and return its id; progress arrives
/// as `faces:progress` and the end as `faces:index_done` (`app::faces::start_index`). Errors
/// when no catalog is open or the models are not downloaded yet.
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_index_photos(state: State<'_, AppState>) -> Result<u64, String> {
    let state = state.inner().clone();
    crate::app::spawn_blocking(move || core_faces::start_index(&state, None)).await.map_err(|e| e.to_string())?
}

/// Snapshot of the running face-indexing job, `None` when idle. Lets the panel re-attach
/// to a job that is still running after a remount (tab switch) instead of showing idle —
/// which both restores the progress display and prevents an accidental second start
/// (starting a new job aborts the running one).
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_index_status(
    state: State<'_, AppState>,
) -> Result<Option<FacesJobStatus>, String> {
    core_faces::index_status(&state)
}

/// Trip the abort flag of any running face-indexing job. The worker stops cleanly after
/// the current photo finishes; the queue retains unprocessed photos for the next run.
/// No-op if no job is running.
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_index_cancel(state: State<'_, AppState>) -> Result<(), String> {
    core_faces::cancel_index(&state)
}

// ── Seed / match / cluster engine commands (H13c) ────────────────────────────
//
// The recognition brain: auto-seed 1-face+1-person photos, per-person centroids,
// Hungarian-constrained matching for N-faces/M-tags photos, nearest-centroid open matching,
// incremental clustering for the rest. The job's body is the core's
// (`app::faces::matching`, #130), which the GPUI app runs too; these are its Tauri wrappers.

/// Run the full seed / match / cluster pipeline over all indexed faces as a background job
/// and return its id as soon as it has STARTED; progress arrives as `faces:match_progress`
/// and the counters as a terminal `faces:match_done` (`app::faces::start_match`). A start
/// supersedes a running match.
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_run_matching(state: State<'_, AppState>) -> Result<u64, String> {
    let state = state.inner().clone();
    crate::app::spawn_blocking(move || core_faces::start_match(&state, None)).await.map_err(|e| e.to_string())?
}

/// Snapshot of the running face-matching job, `None` when idle. Lets the panel re-attach to
/// a run that is still going after a remount instead of showing idle.
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_match_status(
    state: State<'_, AppState>,
) -> Result<Option<FacesMatchJobStatus>, String> {
    core_faces::match_status(&state)
}

/// Cancel the running match. The pipeline stops at its next item and still emits
/// `faces:match_done`, with `aborted: true`.
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_match_cancel(state: State<'_, AppState>) -> Result<(), String> {
    core_faces::cancel_match(&state)
}

/// Confirm a suggested/seeded face: mark it `confirmed` AND assign the person tag to the
/// photo through the catalog's normal `assign_tag` path (so keyword XMP export + merge apply).
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_accept(state: State<'_, AppState>, face_id: i64) -> Result<(), String> {
    with_catalog_blocking(&state, move |c| core_faces::accept(c, face_id)).await
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
    with_catalog_blocking(&state, move |c| core_faces::accept_person(c, &photo_ids, tag_id)).await
}

/// Reject the face's currently-suggested person: remember the (face, person) pair so it is
/// never re-proposed, and return the face to `unassigned`.
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_reject(state: State<'_, AppState>, face_id: i64) -> Result<(), String> {
    with_catalog_blocking(&state, move |c| core_faces::reject(c, face_id)).await
}

/// Mark a face `ignored` (photobomber / background crowd): excluded from centroids and
/// suggestions but kept so re-indexing doesn't resurrect it.
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_ignore(state: State<'_, AppState>, face_id: i64) -> Result<(), String> {
    with_catalog_blocking(&state, move |c| core_faces::ignore(c, face_id)).await
}

/// Manually assign a face to a specific person tag and confirm it, assigning the tag to the
/// photo through the catalog. Clears any prior rejection of that exact pair.
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_assign(state: State<'_, AppState>, face_id: i64, tag_id: i64) -> Result<(), String> {
    with_catalog_blocking(&state, move |c| core_faces::assign(c, face_id, tag_id)).await
}

/// Name an unnamed cluster: create/bind the person tag at `tag_path`, confirm every member
/// face still pending against it, and assign the tag to each member's photo
/// (`app::faces::name_clusters`). Refused when the cluster has no pending face left (a
/// matching run regrouped the faces since the list was read). The UI re-queries.
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_name_cluster(
    state: State<'_, AppState>,
    cluster: i64,
    tag_path: String,
) -> Result<(), String> {
    with_catalog_blocking(&state, move |c| core_faces::name_clusters(c, &[cluster], &tag_path).map(|_| ())).await
}

/// Insert a manually drawn face box for a face the detector missed. `x/y/w/h` are
/// normalized 0–1 in oriented-image space (the same space as detector bboxes). The row
/// starts `unassigned` with `source='drawn'`, no landmarks and no embedding — every
/// matching/centroid query requires an embedding, so a drawn box only ever becomes a
/// person through explicit assignment (`faces_assign`). Returns the new face id.
#[cfg(feature = "faces")]
#[tauri::command(async)]
pub fn faces_add_manual(
    state: State<'_, AppState>,
    photo_id: i64,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) -> Result<i64, String> {
    with_catalog(&state, |c| core_faces::add_manual(c, photo_id, x, y, w, h))
}

/// Delete a drawn, still-unassigned face box (a mis-draw). Guarded to `source='drawn'`:
/// detected faces must be rejected/ignored instead so re-indexing doesn't resurrect
/// them — a drawn box is never re-created by the indexer, so deleting it is safe.
/// (Once a drawn box is assigned, `faces_assign` flips its source to 'manual' and it is
/// treated like any other confirmed face.) It writes the photo's face regions to the sidecar
/// before the delete (#135), so it runs on a blocking worker like the other face writes.
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_delete_drawn(state: State<'_, AppState>, face_id: i64) -> Result<(), String> {
    with_catalog_blocking(&state, move |c| core_faces::delete_drawn(c, face_id)).await
}

/// Return all face rows for a single photo, joined to the tags table for the person name.
/// Used by the loupe overlay and the inspector panel.
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_for_photo(
    state: State<'_, AppState>,
    photo_id: i64,
) -> Result<Vec<crate::plugins::faces::store::FaceForPhoto>, String> {
    with_catalog_blocking(&state, move |c| core_faces::faces_for_photo(c, photo_id)).await
}

// ── People-view summary queries (H13e) ─────────────────────────────────────────
//
// The bodies are the core's (`app::faces::people`, #130).

/// Every named person (confirmed faces, ≥1 face each) with an avatar face — the People
/// view's wall.
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_people_summary(
    state: State<'_, AppState>,
) -> Result<Vec<core_faces::PersonSummary>, String> {
    with_catalog_blocking(&state, core_faces::people_summary).await
}

/// Every unnamed cluster with its member count and an avatar face — the People view's
/// "Unnamed clusters".
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_cluster_summary(
    state: State<'_, AppState>,
) -> Result<Vec<core_faces::ClusterSummary>, String> {
    with_catalog_blocking(&state, core_faces::cluster_summary).await
}

/// Every suggested face, most confident first — the People view's "Review suggestions".
#[cfg(feature = "faces")]
#[tauri::command]
pub async fn faces_suggestion_list(
    state: State<'_, AppState>,
) -> Result<Vec<core_faces::SuggestionEntry>, String> {
    with_catalog_blocking(&state, core_faces::suggestion_list).await
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
    use std::sync::Arc;
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

        let claim = core_faces::begin_match_job(&state, None).unwrap();
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

        let err = core_faces::begin_match_job(&state, None).unwrap_err();
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

        let first = core_faces::begin_match_job(&state, None).unwrap();
        let second = core_faces::begin_match_job(&state, None).unwrap();

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
    /// `begin_index_job` (the same transition `faces_index_photos` runs), the switch is
    /// driven synchronously, and the slot is read straight afterwards. No worker runs at all,
    /// so there is no window for one to clean up on our behalf and make this pass for the
    /// wrong reason — which is exactly what made the original defect invisible.
    #[test]
    fn a_catalog_switch_trips_and_unpublishes_a_running_index() {
        let (cat_a, db_a) = temp_catalog("index-switch-a");
        let (cat_b, _db_b) = temp_catalog("index-switch-b");
        let state = state_with(cat_a);

        let claim = begin_index_job(&state, None).unwrap();
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

        let index = begin_index_job(&state, None).unwrap();
        let matching = core_faces::begin_match_job(&state, None).unwrap();
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

        let claim = begin_index_job(&state, None).unwrap();
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
        let claim = begin_index_job(&state, None).unwrap();

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
