//! The face verbs, the People view's verbs, and the indexing and matching jobs' ownership
//! against **real catalogs**. No ONNX: the verbs and the matcher are model-free; the index
//! tests drive the claim and its guards directly (its worker needs the models;
//! `plugins::faces::indexer` tests its loop with a fake detector), the match tests run the
//! real worker on synthetic embeddings.

use super::*;
use crate::app::{
    detach_catalog_and_trip_jobs, publish_catalog_and_reset_jobs, with_catalog_as, FacesMatchDone, CATALOG_CHANGED,
};
use crate::catalog::Catalog;
use crate::test_support::{TestSubPath, TestTmpDir};
use rusqlite::OptionalExtension;
use std::sync::atomic::Ordering;

fn temp_catalog(tag: &str) -> (Catalog, TestSubPath) {
    let dir = TestTmpDir::new(&format!("app-faces-{tag}"));
    let root = dir.join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let catalog = Catalog::open(&dir.join("catalog.chairphoto"), &root).unwrap();
    store::ensure_schema(catalog.conn()).unwrap();
    (catalog, dir.into_subpath("photos"))
}

fn state_with(catalog: Catalog) -> AppState {
    let state = AppState::default();
    *state.catalog.lock().unwrap() = Some(catalog);
    state
}

/// A photo with a real file under the root (so the sidecar path resolves).
fn add_photo(c: &Catalog, root: &std::path::Path, name: &str) -> i64 {
    let path = root.join(name);
    std::fs::write(&path, b"raw").unwrap();
    c.upsert_photo(&path, None, 0, 1).unwrap().id
}

fn add_face(c: &Catalog, photo_id: i64, bbox: &str) -> i64 {
    store::insert_face(c.conn(), photo_id, bbox, "[]", 0.99, None, "detect", 0).unwrap()
}

fn suggest(c: &Catalog, face_id: i64, tag_id: i64) {
    c.conn()
        .execute(
            "UPDATE faces__faces SET person_tag_id = ?2, state = 'suggested', match_confidence = 0.9 WHERE id = ?1",
            rusqlite::params![face_id, tag_id],
        )
        .unwrap();
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
    c.conn().query_row("SELECT state FROM faces__faces WHERE id = ?1", [face_id], |r| r.get(0)).unwrap()
}

// --- batch confirm (#68), moved with its body from the Tauri commands ----------------------

/// The person tag reaches every photo whose suggestion was confirmed — and only those.
#[test]
fn accept_person_tags_exactly_the_photos_it_confirmed() {
    let (c, root) = temp_catalog("tags");
    let alice = c.create_tag("People/Alice").unwrap();
    let bob = c.create_tag("People/Bob").unwrap();
    let ids: Vec<i64> = (1..=3).map(|i| add_photo(&c, &root, &format!("p{i}.NEF"))).collect();
    let f1 = add_face(&c, ids[0], "[0.1,0.1,0.2,0.2]");
    suggest(&c, f1, alice);
    let f2 = add_face(&c, ids[1], "[0.1,0.1,0.2,0.2]");
    suggest(&c, f2, alice);
    let f3 = add_face(&c, ids[2], "[0.1,0.1,0.2,0.2]");
    suggest(&c, f3, bob); // Alice was never suggested here

    let (outcome, changed) = accept_person_in_catalog(&c, &ids, alice).unwrap();

    assert_eq!((outcome.photos_confirmed, outcome.faces_confirmed, outcome.photos_without_suggestion), (2, 2, 1));
    assert_eq!(changed, vec![ids[0], ids[1]], "only these need a sidecar re-export");
    assert_eq!(face_state(&c, f1), "confirmed");
    assert_eq!(face_state(&c, f2), "confirmed");
    assert_eq!(face_state(&c, f3), "suggested", "Bob's suggestion is untouched");
    assert!(has_tag(&c, ids[0], alice) && has_tag(&c, ids[1], alice));
    assert!(!has_tag(&c, ids[2], alice), "a photo with no suggestion is not tagged");
    assert!(!has_tag(&c, ids[2], bob), "and confirming Alice does not confirm Bob");
}

/// Confirming a person twice reports them as already confirmed and tags the photo once.
#[test]
fn accept_person_is_idempotent() {
    let (c, root) = temp_catalog("idempotent");
    let alice = c.create_tag("People/Alice").unwrap();
    let p = add_photo(&c, &root, "p.NEF");
    let f = add_face(&c, p, "[0.1,0.1,0.2,0.2]");
    suggest(&c, f, alice);

    accept_person(&c, &[p], alice).unwrap();
    let (outcome, changed) = accept_person_in_catalog(&c, &[p], alice).unwrap();

    assert_eq!((outcome.photos_confirmed, outcome.photos_already_confirmed), (0, 1));
    assert!(changed.is_empty(), "nothing changed, so nothing to re-export");
    let tags: i64 = c
        .conn()
        .query_row("SELECT COUNT(*) FROM photo_tags WHERE photo_id = ?1 AND tag_id = ?2", [p, alice], |r| r.get(0))
        .unwrap();
    assert_eq!(tags, 1);
}

/// One transaction: a tagging failure on a later photo rolls back the earlier confirmation
/// (injected with a trigger — `assign_tag` failing after the face rows changed has no natural
/// trigger in a test).
#[test]
fn a_failed_assignment_rolls_back_the_whole_batch() {
    let (c, root) = temp_catalog("rollback");
    let alice = c.create_tag("People/Alice").unwrap();
    let p1 = add_photo(&c, &root, "p1.NEF");
    let p2 = add_photo(&c, &root, "p2.NEF");
    let f1 = add_face(&c, p1, "[0.1,0.1,0.2,0.2]");
    suggest(&c, f1, alice);
    let f2 = add_face(&c, p2, "[0.1,0.1,0.2,0.2]");
    suggest(&c, f2, alice);
    c.conn()
        .execute_batch(&format!(
            "CREATE TRIGGER refuse_tagging_p2 BEFORE INSERT ON photo_tags WHEN NEW.photo_id = {p2}
               BEGIN SELECT RAISE(ABORT, 'injected tagging failure'); END;"
        ))
        .unwrap();

    assert!(accept_person_in_catalog(&c, &[p1, p2], alice).is_err());
    assert_eq!(face_state(&c, f1), "suggested", "a later failure must roll back the earlier confirmation");
    assert_eq!(face_state(&c, f2), "suggested");
    assert!(!has_tag(&c, p1, alice), "…and its photo-level tag with it");
}

// --- the per-face verbs ---------------------------------------------------------------------

/// Confirming writes the person as an MWG region into the photo's sidecar **and keeps a
/// foreign region** another tool wrote there (AGENTS.md "XMP safety": face regions are
/// replaced by Name + Area match only).
#[test]
fn accepting_a_face_writes_its_region_and_preserves_a_foreign_one() {
    let (c, root) = temp_catalog("regions");
    let p = add_photo(&c, &root, "p.NEF");
    let photo_path = root.join("p.NEF");
    // A region a different tool wrote: another name, elsewhere in the frame.
    crate::xmp::write_face_regions(
        &photo_path,
        &[crate::xmp::FaceRegion { name: "Stranger".into(), bbox: (0.6, 0.6, 0.2, 0.2) }],
        100,
        100,
    )
    .unwrap();
    let alice = c.create_tag("People/Alice").unwrap();
    let f = add_face(&c, p, "[0.1,0.1,0.2,0.2]");
    suggest(&c, f, alice);

    accept(&c, f).unwrap();

    assert_eq!(face_state(&c, f), "confirmed");
    assert!(has_tag(&c, p, alice));
    let mut names: Vec<String> = crate::xmp::read_face_regions(&photo_path).into_iter().map(|r| r.name).collect();
    names.sort();
    assert_eq!(names, vec!["Alice".to_string(), "Stranger".to_string()], "ours added, the foreign region kept");

    // Rejecting re-exports an empty confirmed set. The writer then cannot tell its own old
    // region from a foreign one (none matches a name it is writing), so it preserves both —
    // "when in doubt, preserve" (docs/face-tagging.md) — and the foreign one survives again.
    reject(&c, f).unwrap();
    let names: Vec<String> = crate::xmp::read_face_regions(&photo_path).into_iter().map(|r| r.name).collect();
    assert!(names.contains(&"Stranger".to_string()), "{names:?}");
    assert_eq!(face_state(&c, f), "unassigned");
}

/// Assigning to a new person creates the tag under the people root and confirms the face on
/// it; ignore drops the person; a blank name is refused before anything is created.
#[test]
fn assign_new_person_creates_the_tag_and_confirms() {
    let (c, root) = temp_catalog("new-person");
    let p = add_photo(&c, &root, "p.NEF");
    let f = add_face(&c, p, "[0.1,0.1,0.2,0.2]");

    let tag = assign_new_person(&c, f, &person_path("People", " Dana ")).unwrap();
    let path: String = c.conn().query_row("SELECT full_path FROM tags WHERE id = ?1", [tag], |r| r.get(0)).unwrap();
    assert_eq!(path, "People/Dana");
    assert_eq!(face_state(&c, f), "confirmed");
    assert!(has_tag(&c, p, tag));
    let rows = faces_for_photo(&c, p).unwrap();
    assert_eq!(rows[0].person_name.as_deref(), Some("Dana"));
    assert_eq!(rows[0].source, "manual");

    ignore(&c, f).unwrap();
    let rows = faces_for_photo(&c, p).unwrap();
    assert_eq!((rows[0].state.as_str(), rows[0].person_tag_id), ("ignored", None));

    let before: i64 = c.conn().query_row("SELECT COUNT(*) FROM tags", [], |r| r.get(0)).unwrap();
    assert!(assign_new_person(&c, f, "  ").is_err());
    assert!(assign_new_person(&c, f, "/").is_err());
    let after: i64 = c.conn().query_row("SELECT COUNT(*) FROM tags", [], |r| r.get(0)).unwrap();
    assert_eq!(before, after, "a refused name creates no tag");
}

/// A drawn box is clamped to the image, refused when it collapses, and only drawn boxes can
/// be deleted (a detected face is rejected or ignored instead).
#[test]
fn drawn_boxes_are_clamped_and_only_they_can_be_deleted() {
    let (c, root) = temp_catalog("drawn");
    let p = add_photo(&c, &root, "p.NEF");
    let detected = add_face(&c, p, "[0.1,0.1,0.2,0.2]");

    let drawn = add_manual(&c, p, 0.9, -0.1, 0.3, 0.3).unwrap();
    let rows = faces_for_photo(&c, p).unwrap();
    let row = rows.iter().find(|r| r.id == drawn).unwrap();
    assert_eq!(row.source, "drawn");
    assert_eq!(row.state, "unassigned");
    let b = row.bbox;
    assert!((b.x - 0.9).abs() < 1e-6 && b.y == 0.0, "clamped origin: {b:?}");
    assert!((b.w - 0.1).abs() < 1e-6 && (b.h - 0.2).abs() < 1e-6, "clamped size: {b:?}");

    assert!(add_manual(&c, p, 0.5, 0.5, 0.001, 0.3).is_err(), "too thin");
    assert!(add_manual(&c, p, 1.2, 0.5, 0.3, 0.3).is_err(), "entirely outside the image");

    assert!(delete_drawn(&c, detected).is_err(), "a detected face is never deleted");
    assert_eq!(faces_for_photo(&c, p).unwrap().len(), 2);
    delete_drawn(&c, drawn).unwrap();
    assert_eq!(faces_for_photo(&c, p).unwrap().iter().map(|r| r.id).collect::<Vec<_>>(), vec![detected]);
}

/// The picker's tags: the people root and its descendants when one is set, else every tag
/// that is not an auto-tag.
#[test]
fn people_tags_follow_the_people_root() {
    let (c, _root) = temp_catalog("people");
    c.create_tag("People/Alice").unwrap();
    c.create_tag("People/Family/Bob").unwrap();
    c.create_tag("Peoples Republic").unwrap();
    c.create_tag("Places/Oslo").unwrap();

    let all = people_tags(&c).unwrap();
    assert_eq!(all.root, "");
    assert!(all.tags.iter().any(|t| t.full_path == "Places/Oslo"), "no root: every non-auto tag");

    c.set_setting(matcher::PEOPLE_ROOT_SETTING, " People ").unwrap();
    let people = people_tags(&c).unwrap();
    assert_eq!(people.root, "People");
    let mut paths: Vec<&str> = people.tags.iter().map(|t| t.full_path.as_str()).collect();
    paths.sort();
    assert_eq!(paths, vec!["People", "People/Alice", "People/Family", "People/Family/Bob"]);
    assert_eq!(person_path(&people.root, "Eve"), "People/Eve");
    assert_eq!(person_path("", "Eve"), "Eve");
}

/// `indexing.speed` accepts only its two values and reads back as the inference line's speed.
#[test]
fn indexing_speed_is_validated_and_read_back() {
    let (c, _root) = temp_catalog("speed");
    assert_eq!(inference_info(&c).unwrap().speed, "background", "the default");
    set_indexing_speed(&c, " FULL ").unwrap();
    assert_eq!(inference_info(&c).unwrap().speed, "full");
    assert!(set_indexing_speed(&c, "turbo").is_err());
    assert_eq!(c.get_setting(crate::plugins::indexing::INDEXING_SPEED_SETTING).unwrap().as_deref(), Some("full"));
    assert_eq!(inference_info(&c).unwrap().cuda_built, cfg!(feature = "faces-cuda"));
}

// --- the indexing job's ownership -----------------------------------------------------------

/// A start bound to a catalog that is no longer open fails closed and touches nothing: the
/// open catalog's job keeps its flag and slot and no job id is consumed.
#[test]
fn an_index_start_bound_to_another_catalog_touches_nothing() {
    let (a, _ra) = temp_catalog("bound-a");
    let state = state_with(a);
    let from_a = crate::app::catalog_identity(&state).unwrap();
    let running = begin_index_job(&state, Some(from_a)).unwrap();

    let (b, _rb) = temp_catalog("bound-b");
    detach_catalog_and_trip_jobs(&state).unwrap();
    publish_catalog_and_reset_jobs(&state, b).unwrap();
    let on_b = begin_index_job(&state, None).unwrap();
    let issued = state.jobs.faces.abort().job_ids_issued();

    let err = begin_index_job(&state, Some(from_a)).unwrap_err();
    assert_eq!(err, CATALOG_CHANGED);
    assert!(!on_b.abort.load(Ordering::Relaxed), "the open catalog's job must not be tripped");
    assert_eq!(index_status(&state).unwrap().map(|s| s.job), Some(on_b.job));
    assert_eq!(state.jobs.faces.abort().job_ids_issued(), issued, "no job id consumed");
    assert!(running.abort.load(Ordering::Relaxed), "the switch tripped the old catalog's job");
}

/// Cancel is scoped to its job: a Cancel meant for a superseded run stops nothing, the
/// owner's Cancel trips the owner, and a finished job (slot cleared) cancels nothing.
#[test]
fn cancel_job_stops_only_the_job_it_names() {
    let (c, _r) = temp_catalog("cancel");
    let state = state_with(c);
    let first = begin_index_job(&state, None).unwrap();
    let second = begin_index_job(&state, None).unwrap();
    assert!(first.abort.load(Ordering::Relaxed), "superseded");

    assert!(!cancel_index_job(&state, first.job).unwrap(), "the old job's Cancel is a no-op");
    assert!(!second.abort.load(Ordering::Relaxed), "…and must not stop the newer job");

    assert!(cancel_index_job(&state, second.job).unwrap());
    assert!(second.abort.load(Ordering::Relaxed));

    let third = begin_index_job(&state, None).unwrap();
    third.slot.clear(); // the worker finished: it released the slot before its terminal event
    assert!(!cancel_index_job(&state, third.job).unwrap(), "nothing to cancel once it ended");
    assert!(!third.abort.load(Ordering::Relaxed));
}

/// The worker's terminal path against a catalog it cannot open: the slot is released before
/// the one `faces:index_done`, which carries the job id and the error.
#[test]
fn a_worker_that_cannot_open_its_catalog_clears_the_slot_then_reports() {
    use std::sync::Mutex;
    struct Recorder {
        state: AppState,
        seen: Mutex<Vec<(String, bool, Option<u64>)>>,
    }
    impl EventSink for Recorder {
        fn send(&self, event: CoreEvent) {
            let slot_busy = self.state.jobs.faces.status().unwrap().is_some();
            let job = match &event {
                CoreEvent::FacesIndexDone(d) => {
                    assert!(!d.ok && d.error.is_some(), "{d:?}");
                    Some(d.job)
                }
                _ => None,
            };
            self.seen.lock().unwrap().push((event.name().to_string(), slot_busy, job));
        }
    }
    let (c, _r) = temp_catalog("worker-fail");
    let state = state_with(c);
    let mut claim = begin_index_job(&state, None).unwrap();
    let job = claim.job;
    claim.db_path = claim.db_path.with_file_name("missing-dir/none.chairphoto");
    let recorder = Recorder { state: state.clone(), seen: Mutex::new(Vec::new()) };
    run_index_job(&recorder, claim);
    assert_eq!(*recorder.seen.lock().unwrap(), vec![("faces:index_done".to_string(), false, Some(job))]);
}

/// Writes keyed by face ids, bound with `with_catalog_as`, refuse a catalog with colliding
/// ids once it replaced the one the ids were read from.
#[test]
fn a_face_write_bound_to_the_old_catalog_refuses_the_new_one() {
    let (a, ra) = temp_catalog("ident-a");
    let pa = add_photo(&a, &ra, "a.NEF");
    let fa = add_face(&a, pa, "[0.1,0.1,0.2,0.2]");
    let state = state_with(a);
    let (from, rows) = crate::app::with_catalog_identified(&state, |c| faces_for_photo(c, pa)).unwrap();
    assert_eq!(rows[0].id, fa);

    let (b, rb) = temp_catalog("ident-b");
    let pb = add_photo(&b, &rb, "b.NEF");
    let fb = add_face(&b, pb, "[0.1,0.1,0.2,0.2]");
    assert_eq!((pa, fa), (pb, fb), "the ids collide, as two real catalogs' do");
    detach_catalog_and_trip_jobs(&state).unwrap();
    publish_catalog_and_reset_jobs(&state, b).unwrap();

    assert_eq!(with_catalog_as(&state, from, |c| ignore(c, fa)).unwrap_err(), CATALOG_CHANGED);
    let state_b = crate::app::with_catalog(&state, |c| Ok(face_state(c, fb))).unwrap();
    assert_eq!(state_b, "unassigned", "the new catalog's face was not touched");
}

// --- the matching job (#130) ----------------------------------------------------------------

/// A face with an embedding pointing along `axis` (a unit vector), clustered or matched by
/// the real matcher.
fn add_embedded_face(c: &Catalog, photo_id: i64, axis: usize) -> i64 {
    let mut e = vec![0.0f32; 8];
    e[axis] = 1.0;
    let blob = store::embedding_to_blob(&e);
    store::insert_face(c.conn(), photo_id, "[0.1,0.1,0.2,0.2]", "[]", 0.99, Some(&blob), "detect", 0).unwrap()
}

/// What a worker sent, with whether the matching slot was still claimed at that moment.
struct MatchRecorder {
    state: AppState,
    seen: std::sync::Mutex<Vec<(String, bool, CoreEvent)>>,
}

impl EventSink for MatchRecorder {
    fn send(&self, event: CoreEvent) {
        let slot_busy = self.state.jobs.faces_match.status().unwrap().is_some();
        self.seen.lock().unwrap().push((event.name().to_string(), slot_busy, event));
    }
}

/// The real worker on a real catalog: progress carries the job id, the faces end up in one
/// cluster, and the slot is released **before** the one terminal `faces:match_done`, which
/// carries the counters.
#[test]
fn the_match_worker_clusters_then_clears_the_slot_before_its_end() {
    let (c, root) = temp_catalog("match-run");
    let p1 = add_photo(&c, &root, "m1.NEF");
    let p2 = add_photo(&c, &root, "m2.NEF");
    let f1 = add_embedded_face(&c, p1, 0);
    let f2 = add_embedded_face(&c, p2, 0);
    let state = state_with(c);
    let from = crate::app::catalog_identity(&state).unwrap();
    let claim = begin_match_job(&state, Some(from)).unwrap();
    let job = claim.job;
    assert_eq!(match_status(&state).unwrap().map(|s| s.job), Some(job), "running as soon as it is claimed");

    let recorder = MatchRecorder { state: state.clone(), seen: Default::default() };
    run_match_job(&recorder, claim);

    let seen = recorder.seen.lock().unwrap();
    let (last_name, last_busy, last) = seen.last().unwrap();
    assert_eq!((last_name.as_str(), *last_busy), ("faces:match_done", false), "the slot is clear before the end");
    let CoreEvent::FacesMatchDone(d) = last else { unreachable!() };
    assert!(d.ok && !d.aborted && d.job == job, "{d:?}");
    assert_eq!(d.outcome.as_ref().map(|o| o.clustered), Some(2));
    assert_eq!(seen.iter().filter(|(n, ..)| n == "faces:match_done").count(), 1, "one terminal event");
    assert!(seen.iter().any(|(n, ..)| n == "faces:match_progress"));
    for (name, busy, e) in seen.iter().filter(|(n, ..)| n == "faces:match_progress") {
        let CoreEvent::FacesMatchProgress(p) = e else { unreachable!() };
        assert!(*busy && p.job == job, "{name}: progress while owned, with the job id");
    }
    drop(seen);
    let clusters = crate::app::with_catalog(&state, cluster_summary).unwrap();
    assert_eq!(clusters.len(), 1);
    assert_eq!(clusters[0].member_count, 2);
    let members: Vec<i64> = crate::app::with_catalog(&state, |c| cluster_faces(c, clusters[0].cluster_id))
        .unwrap()
        .iter()
        .map(|f| f.face_id)
        .collect();
    assert_eq!(members, vec![f1, f2]);
}

/// A tripped run stops, still sends its one `faces:match_done` (`aborted`, not ok), and a
/// superseded run's end does not clear the newer job's slot.
#[test]
fn a_cancelled_match_still_ends_and_never_clears_a_newer_jobs_slot() {
    let (c, root) = temp_catalog("match-cancel");
    let p = add_photo(&c, &root, "m.NEF");
    add_embedded_face(&c, p, 1);
    let state = state_with(c);
    let first = begin_match_job(&state, None).unwrap();
    let second = begin_match_job(&state, None).unwrap();
    assert!(first.abort.load(Ordering::Relaxed), "the newer start tripped the first");
    assert!(!cancel_match_job(&state, first.job).unwrap(), "a superseded job's Cancel is a no-op");
    assert!(!second.abort.load(Ordering::Relaxed));

    let first_job = first.job;
    let recorder = MatchRecorder { state: state.clone(), seen: Default::default() };
    run_match_job(&recorder, first);
    let seen = recorder.seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "a tripped run stops before its first progress: {:?}", seen.iter().map(|s| &s.0).collect::<Vec<_>>());
    let CoreEvent::FacesMatchDone(d) = &seen[0].2 else { panic!("the end") };
    assert!(d.aborted && !d.ok && d.job == first_job);
    assert_eq!(match_status(&state).unwrap().map(|s| s.job), Some(second.job), "the newer job keeps its slot");
    drop(seen);

    assert!(cancel_match_job(&state, second.job).unwrap());
    assert!(second.abort.load(Ordering::Relaxed));
}

/// A start bound to a catalog that is no longer open fails closed: the open catalog's match
/// keeps its flag and slot, and no job id is consumed.
#[test]
fn a_match_start_bound_to_another_catalog_touches_nothing() {
    let (a, _ra) = temp_catalog("mbound-a");
    let state = state_with(a);
    let from_a = crate::app::catalog_identity(&state).unwrap();
    let (b, _rb) = temp_catalog("mbound-b");
    detach_catalog_and_trip_jobs(&state).unwrap();
    publish_catalog_and_reset_jobs(&state, b).unwrap();
    let on_b = begin_match_job(&state, None).unwrap();
    let issued = state.jobs.faces_match.abort().job_ids_issued();

    assert_eq!(start_match(&state, Some(from_a)).unwrap_err(), CATALOG_CHANGED);
    assert!(!on_b.abort.load(Ordering::Relaxed));
    assert_eq!(match_status(&state).unwrap().map(|s| s.job), Some(on_b.job));
    assert_eq!(state.jobs.faces_match.abort().job_ids_issued(), issued);
}

/// A user decision to make while the matching worker is running, on the **main** connection.
type Decision = Box<dyn FnOnce(&Catalog) -> CatalogResult<()> + Send>;

/// A sink that makes the user's decisions in the middle of a real run: `at_seed` when the
/// seed phase starts (its candidates already read), `after_load` at the first progress of a
/// later phase (the pending faces already read by `load_pending_faces`). The worker is on
/// its own connection, so this is the interleaving the inspector, overlay or Tauri UI can
/// produce at any time (#137).
struct DecideMidRun {
    state: AppState,
    at_seed: std::sync::Mutex<Option<Decision>>,
    after_load: std::sync::Mutex<Option<Decision>>,
    done: std::sync::Mutex<Option<FacesMatchDone>>,
}

impl EventSink for DecideMidRun {
    fn send(&self, event: CoreEvent) {
        match event {
            CoreEvent::FacesMatchProgress(p) => {
                let hook = if p.phase == matcher::MatchPhase::Seed.label() { &self.at_seed } else { &self.after_load };
                if let Some(decide) = hook.lock().unwrap().take() {
                    crate::app::with_catalog(&self.state, decide).unwrap();
                }
            }
            CoreEvent::FacesMatchDone(d) => *self.done.lock().unwrap() = Some(d),
            _ => {}
        }
    }
}

/// Catalog, tags and sidecar agree on a photo: every confirmed face's person is tagged on the
/// photo and exported as a region, and nothing else is exported.
fn assert_consistent(c: &Catalog, root: &std::path::Path, photo: i64, name: &str) {
    let mut confirmed: Vec<String> = Vec::new();
    for f in faces_for_photo(c, photo).unwrap().into_iter().filter(|f| f.state == "confirmed") {
        assert!(has_tag(c, photo, f.person_tag_id.unwrap()), "{name}: a confirmed face's person is tagged");
        confirmed.push(f.person_name.unwrap());
    }
    confirmed.sort();
    let mut exported: Vec<String> = crate::xmp::read_face_regions(&root.join(name)).into_iter().map(|r| r.name).collect();
    exported.sort();
    assert_eq!(confirmed, exported, "{name}: the sidecar's regions are the catalog's confirmed faces");
}

/// A decision made while matching runs stands (#137). The run reads its candidates, then the
/// user — on the main connection — ignores the face it was about to seed, assigns the face it
/// was about to suggest as Alice to Bob, ignores another, and names one it was about to
/// cluster. After the run each decision is intact and the catalog, photo tags and sidecars
/// agree.
#[test]
fn a_decision_made_during_a_match_run_is_never_overwritten() {
    let (c, root) = temp_catalog("match-race");
    let alice = c.create_tag("People/Alice").unwrap();
    let bob = c.create_tag("People/Bob").unwrap();
    // Alice's centroid: a face confirmed as her.
    let p0 = add_photo(&c, &root, "p0.NEF");
    let f0 = add_embedded_face(&c, p0, 0);
    assign(&c, f0, alice).unwrap();
    // A seed candidate: one face, one person tag on the photo.
    let ps = add_photo(&c, &root, "ps.NEF");
    let fs = add_embedded_face(&c, ps, 5);
    c.assign_tag(ps, alice).unwrap();
    // Two faces open matching would suggest as Alice.
    let pa = add_photo(&c, &root, "pa.NEF");
    let fa = add_embedded_face(&c, pa, 0);
    let pi = add_photo(&c, &root, "pi.NEF");
    let fi = add_embedded_face(&c, pi, 0);
    // Two unknown faces clustering would group.
    let pc1 = add_photo(&c, &root, "pc1.NEF");
    let fc1 = add_embedded_face(&c, pc1, 3);
    let pc2 = add_photo(&c, &root, "pc2.NEF");
    let fc2 = add_embedded_face(&c, pc2, 3);

    let state = state_with(c);
    let claim = begin_match_job(&state, None).unwrap();
    let sink = DecideMidRun {
        state: state.clone(),
        at_seed: std::sync::Mutex::new(Some(Box::new(move |c: &Catalog| ignore(c, fs)))),
        after_load: std::sync::Mutex::new(Some(Box::new(move |c: &Catalog| {
            assign(c, fa, bob)?;
            ignore(c, fi)?;
            name_faces(c, &[fc1], "People/Carol").map(|_| ())
        }))),
        done: Default::default(),
    };
    run_match_job(&sink, claim);

    assert!(sink.at_seed.lock().unwrap().is_none() && sink.after_load.lock().unwrap().is_none(), "both hooks ran");
    let done = sink.done.lock().unwrap().take().expect("the run ended");
    assert!(done.ok, "{done:?}");
    let outcome = done.outcome.unwrap();
    assert_eq!((outcome.seeded, outcome.open, outcome.clustered), (0, 0, 1), "{outcome:?}");

    let guard = state.catalog.lock().unwrap();
    let c = guard.as_ref().unwrap();
    let row = |f: i64| -> (String, Option<i64>, String, Option<i64>) {
        c.conn()
            .query_row(
                "SELECT state, person_tag_id, source, cluster_id FROM faces__faces WHERE id = ?1",
                [f],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap()
    };
    assert_eq!(row(f0), ("confirmed".into(), Some(alice), "manual".into(), None), "a prior decision is untouched");
    assert_eq!(row(fs).0, "ignored", "ignored after the seed candidates were read: not seeded");
    assert_eq!(row(fa), ("confirmed".into(), Some(bob), "manual".into(), None), "assigned to Bob: not re-suggested as Alice");
    assert_eq!(row(fi).0, "ignored", "ignored after the pending faces were read: not suggested");
    let carol = c.find_tag_id_by_path("People/Carol").unwrap().unwrap();
    assert_eq!(row(fc1), ("confirmed".into(), Some(carol), "manual".into(), None), "named: in no cluster");
    let fc2_cluster = row(fc2).3.expect("the still-unknown face is clustered");
    assert_eq!(cluster_rows(c), vec![(fc2_cluster, 1)], "the cluster holds only the face still pending");
    for (photo, name) in [(p0, "p0.NEF"), (ps, "ps.NEF"), (pa, "pa.NEF"), (pi, "pi.NEF"), (pc1, "pc1.NEF"), (pc2, "pc2.NEF")] {
        assert_consistent(c, &root, photo, name);
    }
}

// --- the People view's verbs (#130) ---------------------------------------------------------

fn put_in_cluster(c: &Catalog, face: i64, cluster: i64) {
    c.conn()
        .execute("INSERT OR IGNORE INTO faces__clusters (id, centroid, size, created_at) VALUES (?1, x'00', 0, 0)", [cluster])
        .unwrap();
    c.conn().execute("UPDATE faces__faces SET cluster_id = ?2 WHERE id = ?1", [face, cluster]).unwrap();
}

fn cluster_rows(c: &Catalog) -> Vec<(i64, i64)> {
    let mut stmt = c.conn().prepare("SELECT id, size FROM faces__clusters ORDER BY id").unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect()
}

/// Merge: two clusters named as one person confirm every member, tag every photo, drop both
/// clusters and export the regions (keeping a foreign one).
#[test]
fn naming_two_clusters_together_merges_them_into_one_person() {
    let (c, root) = temp_catalog("merge");
    let p1 = add_photo(&c, &root, "a.NEF");
    let p2 = add_photo(&c, &root, "b.NEF");
    crate::xmp::write_face_regions(
        &root.join("a.NEF"),
        &[crate::xmp::FaceRegion { name: "Stranger".into(), bbox: (0.6, 0.6, 0.2, 0.2) }],
        100,
        100,
    )
    .unwrap();
    let a1 = add_face(&c, p1, "[0.1,0.1,0.2,0.2]");
    let a2 = add_face(&c, p1, "[0.4,0.1,0.2,0.2]");
    let b1 = add_face(&c, p2, "[0.1,0.1,0.2,0.2]");
    put_in_cluster(&c, a1, 10);
    put_in_cluster(&c, b1, 11);
    put_in_cluster(&c, a2, 12); // another cluster, untouched

    let out = name_clusters(&c, &[10, 11], "People/Jane").unwrap();
    let jane = c.find_tag_id_by_path("People/Jane").unwrap().unwrap();
    assert_eq!(out, NameOutcome { tag_id: jane, faces: 2, photos: 2, skipped: 0 });
    assert_eq!(face_state(&c, a1), "confirmed");
    assert_eq!(face_state(&c, b1), "confirmed");
    assert_eq!(face_state(&c, a2), "unassigned");
    assert!(has_tag(&c, p1, jane) && has_tag(&c, p2, jane));
    assert_eq!(cluster_rows(&c).iter().map(|r| r.0).collect::<Vec<_>>(), vec![12], "both named clusters are gone");
    let mut names: Vec<String> = crate::xmp::read_face_regions(&root.join("a.NEF")).into_iter().map(|r| r.name).collect();
    names.sort();
    assert_eq!(names, vec!["Jane".to_string(), "Stranger".to_string()], "ours added, the foreign region kept");
    let people = people_summary(&c).unwrap();
    assert_eq!((people.len(), people[0].face_count, people[0].photo_count), (1, 2, 2));
}

/// Split: naming some of a cluster's faces confirms only those; the rest stay in the cluster
/// with its size brought up to date, and a face confirmed meanwhile is skipped, not renamed.
#[test]
fn naming_some_of_a_clusters_faces_splits_them_off() {
    let (c, root) = temp_catalog("split");
    let p = add_photo(&c, &root, "s.NEF");
    let faces: Vec<i64> = (0..4).map(|i| add_face(&c, p, &format!("[0.{i},0.1,0.1,0.1]"))).collect();
    for &f in &faces {
        put_in_cluster(&c, f, 20);
    }
    let bob = c.create_tag("People/Bob").unwrap();
    c.conn()
        .execute("UPDATE faces__faces SET state = 'confirmed', person_tag_id = ?2 WHERE id = ?1", [faces[1], bob])
        .unwrap();

    let out = name_faces(&c, &[faces[0], faces[1]], "People/Ann").unwrap();
    assert_eq!((out.faces, out.skipped), (1, 1));
    assert_eq!(face_state(&c, faces[0]), "confirmed");
    let bob_still: i64 =
        c.conn().query_row("SELECT person_tag_id FROM faces__faces WHERE id = ?1", [faces[1]], |r| r.get(0)).unwrap();
    assert_eq!(bob_still, bob, "a confirmed face is never renamed by a stale list");
    let left: Vec<i64> = cluster_faces(&c, 20).unwrap().iter().map(|f| f.face_id).collect();
    assert_eq!(left, vec![faces[2], faces[3]]);
    assert_eq!(cluster_rows(&c), vec![(20, 3)], "the size counts the faces still in it");

    // Ignoring the rest empties the cluster: its row goes; a confirmed face is not ignored.
    assert_eq!(ignore_faces(&c, &[faces[2], faces[3], faces[0]]).unwrap(), 2);
    assert_eq!(face_state(&c, faces[0]), "confirmed");
    c.conn().execute("UPDATE faces__faces SET cluster_id = NULL WHERE id = ?1", [faces[1]]).unwrap();
    matcher::tidy_clusters(c.conn(), &[20]).unwrap();
    assert!(cluster_rows(&c).is_empty());
    assert!(cluster_summary(&c).unwrap().is_empty());
}

/// A cluster a matching run has since regrouped (its id is never reused) names nothing: the
/// naming is refused and creates no tag.
#[test]
fn naming_a_stale_cluster_is_refused_and_creates_no_tag() {
    let (c, root) = temp_catalog("stale");
    let p = add_photo(&c, &root, "x.NEF");
    let f = add_face(&c, p, "[0.1,0.1,0.2,0.2]");
    put_in_cluster(&c, f, 30);
    c.conn().execute("UPDATE faces__faces SET state = 'ignored', cluster_id = NULL WHERE id = ?1", [f]).unwrap();
    assert!(name_clusters(&c, &[30], "People/Ghost").unwrap_err().to_string().contains(NOTHING_TO_NAME));
    assert!(name_faces(&c, &[f], "People/Ghost").unwrap_err().to_string().contains(NOTHING_TO_NAME));
    assert_eq!(c.find_tag_id_by_path("People/Ghost").unwrap(), None, "the transaction rolled the tag back");
    assert!(name_faces(&c, &[f], "  /  ").is_err(), "a blank name is refused");
}

/// The review queue confirms and rejects a suggestion only as it was shown: one re-matched
/// to someone else since is left alone (stale), a confirmation tags the photo, a rejection is
/// remembered.
#[test]
fn reviews_apply_only_to_the_suggestion_that_was_shown() {
    let (c, root) = temp_catalog("review");
    let p = add_photo(&c, &root, "r.NEF");
    let alice = c.create_tag("People/Alice").unwrap();
    let bob = c.create_tag("People/Bob").unwrap();
    let f1 = add_face(&c, p, "[0.1,0.1,0.2,0.2]");
    let f2 = add_face(&c, p, "[0.4,0.1,0.2,0.2]");
    let f3 = add_face(&c, p, "[0.7,0.1,0.2,0.2]");
    suggest(&c, f1, alice);
    suggest(&c, f2, alice);
    suggest(&c, f3, bob);
    assert_eq!(suggestion_list(&c).unwrap().len(), 3);
    suggest(&c, f2, bob); // a re-run now suggests Bob for f2

    let out = review_suggestions(
        &c,
        &[
            Review { face_id: f1, tag_id: alice, verdict: Verdict::Confirm },
            Review { face_id: f2, tag_id: alice, verdict: Verdict::Confirm },
            Review { face_id: f3, tag_id: bob, verdict: Verdict::Reject },
        ],
    )
    .unwrap();
    assert_eq!(out, ReviewOutcome { confirmed: 1, rejected: 1, stale: 1 });
    assert_eq!(face_state(&c, f1), "confirmed");
    assert_eq!(face_state(&c, f2), "suggested");
    assert_eq!(face_state(&c, f3), "unassigned");
    assert!(has_tag(&c, p, alice) && !has_tag(&c, p, bob));
    let remembered: i64 = c
        .conn()
        .query_row("SELECT COUNT(*) FROM faces__rejections WHERE face_id = ?1 AND person_tag_id = ?2", [f3, bob], |r| r.get(0))
        .unwrap();
    assert_eq!(remembered, 1);
    let regions: Vec<String> = crate::xmp::read_face_regions(&root.join("r.NEF")).into_iter().map(|r| r.name).collect();
    assert_eq!(regions, vec!["Alice".to_string()]);
}

/// The summaries carry each avatar photo's user rotation (the thumbnail is drawn turned), and
/// the people root defaults to the matcher's.
#[test]
fn summaries_carry_the_avatar_rotation() {
    let (c, root) = temp_catalog("rotation");
    let p = add_photo(&c, &root, "o.NEF");
    c.set_photo_rotation(p, 90).unwrap();
    let alice = c.create_tag("People/Alice").unwrap();
    let f = add_face(&c, p, "[0.1,0.2,0.3,0.4]");
    suggest(&c, f, alice);
    assert_eq!(suggestion_list(&c).unwrap()[0].rotation, 90);
    accept(&c, f).unwrap();
    let person = &people_summary(&c).unwrap()[0];
    assert_eq!((person.avatar_photo_id, person.avatar_rotation), (p, 90));
    assert_eq!(person.avatar_bbox, FaceBboxJson { x: 0.1, y: 0.2, w: 0.3, h: 0.4 });
    assert_eq!(effective_people_root(&c).unwrap(), "People");
    c.set_setting("faces.people_root", "Family").unwrap();
    assert_eq!(effective_people_root(&c).unwrap(), "Family");
}
