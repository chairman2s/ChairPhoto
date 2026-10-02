//! The face verbs and the indexing job's ownership against **real catalogs**. No ONNX: the
//! verbs are model-free, and the job tests drive the claim and its guards directly (the
//! worker needs the models; `plugins::faces::indexer` tests its loop with a fake detector).

use super::*;
use crate::app::{detach_catalog_and_trip_jobs, publish_catalog_and_reset_jobs, with_catalog_as, CATALOG_CHANGED};
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
