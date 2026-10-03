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

/// A sidecar another tool wrote — raw XML, not ChairPhoto's writer, no `chairphoto:LastWrite`,
/// another frame size: a named MWG region carrying a foreign child element (digiKam's face
/// engine), an unnamed region in the nested-`rdf:Description` form (read since #139), a
/// Microsoft Photo `MP:RegionInfo` and a digiKam tag list beside them. Every property is in
/// element form; attribute-form sidecars are covered by the `xmp` tests (#138).
fn seed_foreign_sidecar(photo_path: &std::path::Path) {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:digiKam="http://www.digikam.org/ns/1.0/"
    xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/"
    xmlns:stDim="http://ns.adobe.com/xap/1.0/sType/Dimensions#"
    xmlns:stArea="http://ns.adobe.com/xmp/sType/Area#"
    xmlns:MP="http://ns.microsoft.com/photo/1.2/"
    xmlns:MPRI="http://ns.microsoft.com/photo/1.2/t/RegionInfo#"
    xmlns:MPReg="http://ns.microsoft.com/photo/1.2/t/Region#">
   <digiKam:TagsList>
    <rdf:Seq>
     <rdf:li>People/Stranger</rdf:li>
    </rdf:Seq>
   </digiKam:TagsList>
   <mwg-rs:Regions rdf:parseType="Resource">
    <mwg-rs:AppliedToDimensions rdf:parseType="Resource">
     <stDim:w>6000</stDim:w>
     <stDim:h>4000</stDim:h>
     <stDim:unit>pixel</stDim:unit>
    </mwg-rs:AppliedToDimensions>
    <mwg-rs:RegionList>
     <rdf:Bag>
      <rdf:li rdf:parseType="Resource">
       <mwg-rs:Name>Stranger</mwg-rs:Name>
       <mwg-rs:Type>Face</mwg-rs:Type>
       <mwg-rs:Area rdf:parseType="Resource">
        <stArea:x>0.7</stArea:x>
        <stArea:y>0.7</stArea:y>
        <stArea:w>0.2</stArea:w>
        <stArea:h>0.2</stArea:h>
        <stArea:unit>normalized</stArea:unit>
       </mwg-rs:Area>
       <digiKam:FaceEngine>dnn-yunet</digiKam:FaceEngine>
      </rdf:li>
      <rdf:li>
       <rdf:Description>
        <mwg-rs:Type>Pet</mwg-rs:Type>
        <mwg-rs:Area rdf:parseType="Resource">
         <stArea:x>0.31</stArea:x>
         <stArea:y>0.8</stArea:y>
         <stArea:w>0.1</stArea:w>
         <stArea:h>0.1</stArea:h>
        </mwg-rs:Area>
       </rdf:Description>
      </rdf:li>
     </rdf:Bag>
    </mwg-rs:RegionList>
   </mwg-rs:Regions>
   <MP:RegionInfo rdf:parseType="Resource">
    <MPRI:Regions>
     <rdf:Bag>
      <rdf:li rdf:parseType="Resource">
       <MPReg:PersonDisplayName>Stranger</MPReg:PersonDisplayName>
       <MPReg:Rectangle>0.6, 0.6, 0.2, 0.2</MPReg:Rectangle>
      </rdf:li>
     </rdf:Bag>
    </MPRI:Regions>
   </MP:RegionInfo>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
"#;
    std::fs::write(crate::xmp::sidecar_path(photo_path), xml).unwrap();
}

/// The foreign tool's content survives a ChairPhoto region write: the named region with its
/// area and its foreign extra, the unnamed nested-Description one, the MP regions and the tag
/// list, each in its own namespace — next to ChairPhoto's own regions, `ours`.
fn assert_foreign_kept(photo_path: &std::path::Path, ours: &[&str]) {
    let regions = crate::xmp::read_face_regions(photo_path);
    let xml = std::fs::read_to_string(crate::xmp::sidecar_path(photo_path)).unwrap();
    let Some(stranger) = regions.iter().find(|r| r.name == "Stranger") else {
        panic!("the foreign named region is lost: {regions:?}\n{xml}")
    };
    let (x, y, w, h) = stranger.bbox;
    assert!((x - 0.6).abs() < 1e-4 && (y - 0.6).abs() < 1e-4 && (w - 0.2).abs() < 1e-4 && (h - 0.2).abs() < 1e-4, "{:?}", stranger.bbox);
    // The unnamed nested-Description region is the foreign tool's, not ours (#139 reads it).
    let Some(pet) = regions.iter().find(|r| r.name.is_empty()) else {
        panic!("the foreign unnamed region is lost: {regions:?}\n{xml}")
    };
    let (x, y, w, h) = pet.bbox;
    assert!((x - 0.26).abs() < 1e-4 && (y - 0.75).abs() < 1e-4 && (w - 0.1).abs() < 1e-4 && (h - 0.1).abs() < 1e-4, "{:?}", pet.bbox);
    let mut names: Vec<&str> =
        regions.iter().map(|r| r.name.as_str()).filter(|n| *n != "Stranger" && !n.is_empty()).collect();
    names.sort();
    assert_eq!(names, ours, "ChairPhoto's regions");

    // Namespace-aware: each kept node must still be in its tool's namespace.
    let doc = xmltree::Element::parse(xml.as_bytes()).unwrap();
    let mut found: Vec<(String, String, String)> = Vec::new(); // (namespace, name, text)
    fn walk(e: &xmltree::Element, out: &mut Vec<(String, String, String)>) {
        let text = e.get_text().map(|t| t.trim().to_string()).unwrap_or_default();
        out.push((e.namespace.clone().unwrap_or_default(), e.name.clone(), text));
        for c in e.children.iter().filter_map(|n| n.as_element()) {
            walk(c, out);
        }
    }
    walk(&doc, &mut found);
    let has = |ns: &str, name: &str, text: &str| found.iter().any(|(n, l, t)| n == ns && l == name && t == text);
    const DIGIKAM: &str = "http://www.digikam.org/ns/1.0/";
    const MWG: &str = "http://www.metadataworkinggroup.com/schemas/regions/";
    const STAREA: &str = "http://ns.adobe.com/xmp/sType/Area#";
    const MPREG: &str = "http://ns.microsoft.com/photo/1.2/t/Region#";
    for (ns, name, text) in [
        (DIGIKAM, "FaceEngine", "dnn-yunet"),
        (MWG, "Type", "Pet"),
        (STAREA, "x", "0.31"),
        (MPREG, "PersonDisplayName", "Stranger"),
        (MPREG, "Rectangle", "0.6, 0.6, 0.2, 0.2"),
        ("http://www.w3.org/1999/02/22-rdf-syntax-ns#", "li", "People/Stranger"),
    ] {
        assert!(has(ns, name, text), "{ns}{name} = {text:?} was lost:\n{xml}");
    }
    assert!(found.iter().any(|(n, l, _)| n == DIGIKAM && l == "TagsList"), "the digiKam tag list is kept:\n{xml}");
}

/// Confirming writes the person as an MWG region into the photo's sidecar **and keeps the
/// foreign regions** another tool wrote there (AGENTS.md "XMP safety": face regions are
/// replaced by Name + Area match only; foreign namespaces are preserved).
#[test]
fn accepting_a_face_writes_its_region_and_preserves_a_foreign_one() {
    let (c, root) = temp_catalog("regions");
    let p = add_photo(&c, &root, "p.NEF");
    let photo_path = root.join("p.NEF");
    seed_foreign_sidecar(&photo_path);
    let alice = c.create_tag("People/Alice").unwrap();
    let f = add_face(&c, p, "[0.1,0.1,0.2,0.2]");
    suggest(&c, f, alice);

    accept(&c, f).unwrap();

    assert_eq!(face_state(&c, f), "confirmed");
    assert!(has_tag(&c, p, alice));
    assert_foreign_kept(&photo_path, &["Alice"]);

    // Rejecting re-exports an empty confirmed set: Alice's region carries ChairPhoto's marker,
    // so it goes (#135); the foreign ones carry none and stay.
    reject(&c, f).unwrap();
    assert_foreign_kept(&photo_path, &[]);
    assert_eq!(face_state(&c, f), "unassigned");
}

/// #135 M1, probe Q5's shape through the verbs: a second catalog over the same folder — or this
/// one rebuilt — never removes or takes over a region the first catalog marked. Catalog A
/// confirmed a drawn Carol the detector misses and exported her; catalog B, which never had
/// Carol, accepts and then rejects Alice on the same photo. Carol keeps A's marker throughout.
#[test]
fn another_catalogs_marked_region_survives_this_catalogs_face_writes() {
    let (a, root) = temp_catalog("q5-a");
    let pa = add_photo(&a, &root, "p.NEF");
    let photo_path = root.join("p.NEF");
    let carol = a.create_tag("People/Carol").unwrap();
    let fc = add_manual(&a, pa, 0.7, 0.7, 0.1, 0.1).unwrap();
    assign(&a, fc, carol).unwrap();
    let a_marker = format!("{}/{fc}", a.catalog_uuid().unwrap());
    let carol_region = || {
        let xml = std::fs::read_to_string(crate::xmp::sidecar_path(&photo_path)).unwrap();
        let got = crate::xmp::region_fixtures::mwg(&xml);
        let carol: Vec<_> = got.regions.iter().filter(|r| r.name == "Carol").cloned().collect();
        (carol, xml)
    };
    let (before, _) = carol_region();
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].face_id.as_deref(), Some(a_marker.as_str()));

    let b_dir = TestTmpDir::new("app-faces-q5-b");
    let b = Catalog::open(&b_dir.join("catalog.chairphoto"), &root).unwrap();
    store::ensure_schema(b.conn()).unwrap();
    assert_ne!(b.catalog_uuid().unwrap(), a.catalog_uuid().unwrap());
    let pb = b.upsert_photo(&photo_path, None, 0, 1).unwrap().id;
    let alice = b.create_tag("People/Alice").unwrap();
    let fb = add_face(&b, pb, "[0.1,0.1,0.2,0.2]");
    suggest(&b, fb, alice);
    accept(&b, fb).unwrap();
    let (after, xml) = carol_region();
    assert_eq!(after, before, "B's first face write touched A's Carol:\n{xml}");
    reject(&b, fb).unwrap();
    let (after, xml) = carol_region();
    assert_eq!(after, before, "B's reject removed A's Carol:\n{xml}");
    assert!(crate::xmp::read_face_regions(&photo_path).iter().all(|r| r.name != "Alice"), "{xml}");
}

/// A photo the pre-marker writer exported Alice to, on a portrait shot (EXIF Orientation 6,
/// stored 6000x4000): its sidecar holds her old-shaped, unmarked region in the display frame
/// with the stored dimensions, and the catalog's record names her. Returns the photo's path.
fn pre_marker_photo(c: &Catalog, root: &std::path::Path, name: &str) -> (i64, std::path::PathBuf, i64) {
    let p = add_photo(c, root, name);
    c.conn()
        .execute("UPDATE photos SET width = 6000, height = 4000, exif_orientation = 6 WHERE id = ?1", [p])
        .unwrap();
    let alice = c.create_tag("People/Alice").unwrap();
    let f = add_face(c, p, "[0.1,0.1,0.2,0.2]");
    assign(c, f, alice).unwrap();
    let path = root.join(name);
    std::fs::write(
        crate::xmp::sidecar_path(&path),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
<rdf:Description rdf:about="" xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/"
  xmlns:stDim="http://ns.adobe.com/xap/1.0/sType/Dimensions#" xmlns:stArea="http://ns.adobe.com/xmp/sType/Area#">
<mwg-rs:Regions rdf:parseType="Resource"><mwg-rs:AppliedToDimensions rdf:parseType="Resource"><stDim:w>6000</stDim:w><stDim:h>4000</stDim:h><stDim:unit>pixel</stDim:unit></mwg-rs:AppliedToDimensions>
<mwg-rs:RegionList><rdf:Bag><rdf:li rdf:parseType="Resource"><mwg-rs:Name>Alice</mwg-rs:Name><mwg-rs:Type>Face</mwg-rs:Type><mwg-rs:Area rdf:parseType="Resource"><stArea:x>0.2</stArea:x><stArea:y>0.2</stArea:y><stArea:w>0.2</stArea:w><stArea:h>0.2</stArea:h><stArea:unit>normalized</stArea:unit></mwg-rs:Area></rdf:li></rdf:Bag></mwg-rs:RegionList></mwg-rs:Regions>
</rdf:Description></rdf:RDF></x:xmpmeta>"#,
    )
    .unwrap();
    c.conn()
        .execute(
            "INSERT INTO faces__legacy_regions (face_id, photo_id, name, bbox) VALUES (?1, ?2, 'Alice', '[0.1,0.1,0.2,0.2]')",
            rusqlite::params![f, p],
        )
        .unwrap();
    (p, path, f)
}

fn legacy_photos(c: &Catalog) -> Vec<i64> {
    let mut stmt = c.conn().prepare("SELECT DISTINCT photo_id FROM faces__legacy_regions ORDER BY photo_id").unwrap();
    stmt.query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap()
}

/// Review L1: the index job first converts every photo on the pre-marker record — its old
/// display-frame region moved into the stored frame and marked, the record spent — before
/// any region is read. An offline photo is skipped and keeps its record; a photo no longer
/// in the catalog drops off it. The faces already have rows, so the index itself has
/// nothing to detect and needs no model.
#[test]
fn the_index_job_converts_the_pre_marker_regions_first() {
    let (c, root) = temp_catalog("legacy-pass");
    let (p1, path1, f1) = pre_marker_photo(&c, &root, "p1.NEF");
    let (p2, path2, _) = pre_marker_photo(&c, &root, "p2.NEF");
    let marker = format!("{}/{f1}", c.catalog_uuid().unwrap());
    std::fs::remove_file(&path2).unwrap(); // offline
    c.conn()
        .execute("INSERT INTO faces__legacy_regions (face_id, photo_id, name, bbox) VALUES (999, 4242, 'Gone', '[0,0,0.1,0.1]')", [])
        .unwrap();
    assert_eq!(legacy_photos(&c), [p1, p2, 4242]);
    let state = state_with(c);
    let claim = begin_index_job(&state, None).unwrap();
    run_index_job(&crate::app::NoEvents, claim);

    let xml = std::fs::read_to_string(crate::xmp::sidecar_path(&path1)).unwrap();
    let got = crate::xmp::region_fixtures::mwg(&xml);
    assert_eq!(got.regions.len(), 1, "{xml}");
    let alice = &got.regions[0];
    assert_eq!(alice.face_id.as_deref(), Some(marker.as_str()), "not marked:\n{xml}");
    let (x, y, w, h) = alice.area;
    assert!((x - 0.2).abs() < 1e-5 && (y - 0.8).abs() < 1e-5 && (w - 0.2).abs() < 1e-5 && (h - 0.2).abs() < 1e-5,
        "not in the stored frame: {:?}\n{xml}", alice.area);
    let left = crate::app::with_catalog(&state, |c| Ok(legacy_photos(c))).unwrap();
    assert_eq!(left, [p2], "the written photo spends its record, the offline one keeps it");
}

/// Review N3: the conversion reports through the index job's own progress — the status slot
/// and `faces:progress` with the job's id — counting photos to convert, `0/n` to `n/n`, before
/// the index's own count, instead of a silent `0/0` for however long the pass takes.
#[test]
fn the_index_jobs_conversion_reports_progress() {
    use std::sync::Mutex;
    struct Recorder {
        state: AppState,
        seen: Mutex<Vec<(usize, usize, u64, Option<(usize, usize)>)>>,
    }
    impl EventSink for Recorder {
        fn send(&self, event: CoreEvent) {
            if let CoreEvent::FacesProgress(p) = event {
                let slot = self.state.jobs.faces.status().unwrap().map(|s| (s.done, s.total));
                self.seen.lock().unwrap().push((p.done, p.total, p.job, slot));
            }
        }
    }
    let (c, root) = temp_catalog("legacy-pass-progress");
    pre_marker_photo(&c, &root, "p1.NEF");
    pre_marker_photo(&c, &root, "p2.NEF");
    let state = state_with(c);
    let claim = begin_index_job(&state, None).unwrap();
    let job = claim.job;
    let recorder = Recorder { state: state.clone(), seen: Mutex::new(Vec::new()) };
    run_index_job(&recorder, claim);
    let seen = recorder.seen.lock().unwrap().clone();
    assert!(seen.len() >= 3, "{seen:?}");
    assert_eq!(
        seen[..3],
        [(0, 2, job, Some((0, 2))), (1, 2, job, Some((1, 2))), (2, 2, job, Some((2, 2)))],
        "{seen:?}"
    );
}

/// #156, forced interleaving: the index job's pre-marker conversion, on its own connection,
/// is about to write a photo's sidecar when a face verb on the main connection ignores
/// Alice there and writes the sidecar without her. The pass must not then put her back: it
/// reads the photo's set once it holds the sidecar's file lock, so it writes the set the
/// verb left. (Before, it read the set first and wrote that older set over the verb's.)
#[test]
fn a_face_verb_during_the_pre_marker_conversion_is_not_overwritten() {
    use crate::plugins::faces::regions::tests::BEFORE_REGION_WRITE;
    let (c, root) = temp_catalog("legacy-pass-race");
    let (p, path, alice) = pre_marker_photo(&c, &root, "p1.NEF");
    let resolved = c.resolve_photo_path(p).unwrap().unwrap();
    let state = state_with(c);
    let verb = {
        let state = state.clone();
        move || crate::app::with_catalog(&state, |c| ignore(c, alice)).unwrap()
    };
    *BEFORE_REGION_WRITE.lock().unwrap() = Some((resolved.clone(), Box::new(verb)));

    let claim = begin_index_job(&state, None).unwrap();
    run_index_job(&crate::app::NoEvents, claim);

    let hook = BEFORE_REGION_WRITE.lock().unwrap();
    assert!(hook.as_ref().is_none_or(|(p, _)| *p != resolved), "the pass never reached the photo");
    drop(hook);
    let state_of_alice = crate::app::with_catalog(&state, |c| Ok(face_state(c, alice))).unwrap();
    assert_eq!(state_of_alice, "ignored");
    let xml = std::fs::read_to_string(crate::xmp::sidecar_path(&path)).unwrap();
    assert!(crate::xmp::read_face_regions(&path).iter().all(|r| r.name != "Alice"),
        "the pass wrote back the region the verb removed:\n{xml}");
    let left = crate::app::with_catalog(&state, |c| Ok(legacy_photos(c))).unwrap();
    assert!(left.is_empty(), "the written photo spends its record: {left:?}");
}

/// The pass stops at the job's abort flag: nothing written, every record row kept.
#[test]
fn an_aborted_index_job_leaves_the_pre_marker_record() {
    let (c, root) = temp_catalog("legacy-pass-abort");
    let (p1, path1, _) = pre_marker_photo(&c, &root, "p1.NEF");
    let before = std::fs::read(crate::xmp::sidecar_path(&path1)).unwrap();
    let state = state_with(c);
    let claim = begin_index_job(&state, None).unwrap();
    claim.abort.store(true, Ordering::SeqCst);
    run_index_job(&crate::app::NoEvents, claim);
    assert_eq!(std::fs::read(crate::xmp::sidecar_path(&path1)).unwrap(), before);
    let left = crate::app::with_catalog(&state, |c| Ok(legacy_photos(c))).unwrap();
    assert_eq!(left, [p1]);
}

/// Review N1, probe R2's shape through the verbs: a copy of a catalog file (a sync between
/// two machines, a restored backup) shares the original's identity *and* its face-id
/// counter. After the copy, A draws Carol on photo P (face k, exported as `U/k`) and B,
/// which never had her, gets its own face k on another photo Q. B's face writes on P — an
/// accept, then a reject — know no face k on P, so A's Carol is left exactly as she is.
#[test]
fn a_copied_catalogs_face_writes_leave_the_other_copys_regions() {
    let (a, root) = temp_catalog("n1-a");
    let p = add_photo(&a, &root, "p.NEF");
    let q = add_photo(&a, &root, "q.NEF");
    let copy_dir = TestTmpDir::new("app-faces-n1-b");
    let copy = copy_dir.join("copy.chairphoto");
    a.conn().execute("VACUUM INTO ?1", [copy.to_str().unwrap()]).unwrap();
    let b = Catalog::open(&copy, &root).unwrap();
    assert_eq!(b.catalog_uuid().unwrap(), a.catalog_uuid().unwrap(), "a copy shares the identity");

    let carol = a.create_tag("People/Carol").unwrap();
    let fc = add_manual(&a, p, 0.7, 0.7, 0.1, 0.1).unwrap();
    assign(&a, fc, carol).unwrap();
    let path = root.join("p.NEF");
    let carol_region = || {
        let xml = std::fs::read_to_string(crate::xmp::sidecar_path(&path)).unwrap();
        let got = crate::xmp::region_fixtures::mwg(&xml);
        (got.regions.iter().filter(|r| r.name == "Carol").cloned().collect::<Vec<_>>(), xml)
    };
    let (before, _) = carol_region();
    assert_eq!(before.len(), 1);

    let dave = b.create_tag("People/Dave").unwrap();
    let fd = add_face(&b, q, "[0.4,0.1,0.1,0.1]");
    assert_eq!(fd, fc, "the copies' id counters collide");
    assign(&b, fd, dave).unwrap();
    let alice = b.create_tag("People/Alice").unwrap();
    let fa = add_face(&b, p, "[0.1,0.1,0.2,0.2]");
    suggest(&b, fa, alice);
    accept(&b, fa).unwrap();
    let (after, xml) = carol_region();
    assert_eq!(after, before, "B's accept touched A's Carol:\n{xml}");
    reject(&b, fa).unwrap();
    let (after, xml) = carol_region();
    assert_eq!(after, before, "B's reject removed A's Carol:\n{xml}");
}

/// Review N1: deleting a drawn box writes the photo's regions while the row still exists, so
/// a region of it is removed — afterwards its id would be unknown and the region kept for
/// ever. (A box still `drawn` is normally never exported; the region here stands in for one
/// left by a write that was skipped while the photo was offline.)
#[test]
fn deleting_a_drawn_face_removes_its_region() {
    let (c, root) = temp_catalog("n1-drawn");
    let p = add_photo(&c, &root, "p.NEF");
    let path = root.join("p.NEF");
    let carol = c.create_tag("People/Carol").unwrap();
    let f = add_manual(&c, p, 0.7, 0.7, 0.1, 0.1).unwrap();
    assign(&c, f, carol).unwrap();
    assert!(crate::xmp::read_face_regions(&path).iter().any(|r| r.name == "Carol"));
    // The box is unassigned again and still `drawn`, its region still in the sidecar.
    c.conn()
        .execute(
            "UPDATE faces__faces SET state = 'unassigned', person_tag_id = NULL, source = 'drawn' WHERE id = ?1",
            [f],
        )
        .unwrap();
    delete_drawn(&c, f).unwrap();
    assert!(crate::xmp::read_face_regions(&path).iter().all(|r| r.name != "Carol"), "the region stayed");
}

/// #135 through the verbs: ignoring a confirmed face takes its region out of the sidecar,
/// as rejecting does, and every foreign region and structure stays.
#[test]
fn ignoring_a_confirmed_face_removes_its_region() {
    let (c, root) = temp_catalog("ignore-region");
    let p = add_photo(&c, &root, "p.NEF");
    let photo_path = root.join("p.NEF");
    seed_foreign_sidecar(&photo_path);
    let alice = c.create_tag("People/Alice").unwrap();
    let bob = c.create_tag("People/Bob").unwrap();
    let fa = add_face(&c, p, "[0.1,0.1,0.2,0.2]");
    let fb = add_face(&c, p, "[0.4,0.1,0.2,0.2]");
    suggest(&c, fa, alice);
    suggest(&c, fb, bob);
    accept(&c, fa).unwrap();
    accept(&c, fb).unwrap();
    assert_foreign_kept(&photo_path, &["Alice", "Bob"]);

    ignore(&c, fa).unwrap();
    assert_foreign_kept(&photo_path, &["Bob"]);
    reject(&c, fb).unwrap();
    assert_foreign_kept(&photo_path, &[]);
}

/// #135's rule for a catalog from before the marker: a face its old writer exported is on the
/// record the first faces call after the upgrade takes, so rejecting it — the very first thing
/// done — still removes its unmarked region, and the foreign ones stay.
#[test]
fn rejecting_a_face_exported_before_the_marker_removes_its_region() {
    let (c, root) = temp_catalog("legacy-reject");
    let p = add_photo(&c, &root, "p.NEF");
    let photo_path = root.join("p.NEF");
    seed_foreign_sidecar(&photo_path);
    let alice = c.create_tag("People/Alice").unwrap();
    let f = add_face(&c, p, "[0.1,0.1,0.2,0.2]");
    suggest(&c, f, alice);
    accept(&c, f).unwrap();
    // As the pre-marker writer left it: Alice's region without the marker, and no record yet.
    let xml = std::fs::read_to_string(crate::xmp::sidecar_path(&photo_path)).unwrap();
    let marker = format!("<chairphoto:FaceId>{}/{f}</chairphoto:FaceId>", c.catalog_uuid().unwrap());
    assert!(xml.contains(&marker), "{xml}");
    std::fs::write(crate::xmp::sidecar_path(&photo_path), xml.replace(&marker, "")).unwrap();
    c.conn().execute_batch("DROP TABLE faces__legacy_regions; DROP TABLE faces__once").unwrap();

    reject(&c, f).unwrap();

    assert_foreign_kept(&photo_path, &[]);
}

/// #136: the importer reads a region in the frame the detections are in. Lightroom's region
/// for Bob on a portrait shot (EXIF Orientation 6) is in the stored frame; the detector found
/// Bob on the EXIF-oriented preview, where the same face sits turned 90° clockwise.
#[test]
fn importing_a_region_on_a_rotated_photo_matches_the_face_in_the_display_frame() {
    let (c, root) = temp_catalog("import-rotated");
    let p = add_photo(&c, &root, "portrait.ARW");
    let photo_path = root.join("portrait.ARW");
    std::fs::write(crate::xmp::sidecar_path(&photo_path), crate::xmp::region_fixtures::LIGHTROOM_ROTATED)
        .unwrap();
    c.conn()
        .execute("UPDATE photos SET width = 6000, height = 4000, exif_orientation = 6 WHERE id = ?1", [p])
        .unwrap();
    // Bob's stored-frame box (0.225, 0.2, 0.15, 0.1), turned into the display frame.
    let f = add_face(&c, p, "[0.7,0.225,0.1,0.15]");

    import_regions(&c, c.conn(), p, &photo_path, "People");

    assert_eq!(face_state(&c, f), "confirmed", "Bob's region did not match his face");
    let bob: i64 = c
        .conn()
        .query_row("SELECT id FROM tags WHERE full_path = 'People/Bob'", [], |r| r.get(0))
        .unwrap();
    assert!(has_tag(&c, p, bob));
}

/// #154: a HEIC's display frame is its container's turn. Lightroom's stored-frame region for
/// Bob, on a HEIC whose `irot` turns it as EXIF 6 would and whose EXIF says nothing, matches
/// the face found turned 90° clockwise. On a HEIC whose EXIF says 6 while its container turns
/// nothing, the turn is doubted and nothing is imported, though by EXIF alone it would match.
#[test]
fn importing_a_region_on_a_heic_follows_its_container() {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/heif");
    for (fixture, exif, imported) in [("rot90.heic", None, true), ("plain_e6.heic", Some(6), false)] {
        let (c, root) = temp_catalog("import-heic");
        let p = add_photo(&c, &root, "IMG_0001.HEIC");
        let photo_path = root.join("IMG_0001.HEIC");
        std::fs::copy(fixtures.join(fixture), &photo_path).unwrap();
        std::fs::write(crate::xmp::sidecar_path(&photo_path), crate::xmp::region_fixtures::LIGHTROOM_ROTATED)
            .unwrap();
        c.conn()
            .execute(
                "UPDATE photos SET width = 6000, height = 4000, exif_orientation = ?2 WHERE id = ?1",
                rusqlite::params![p, exif],
            )
            .unwrap();
        let f = add_face(&c, p, "[0.7,0.225,0.1,0.15]");

        import_regions(&c, c.conn(), p, &photo_path, "People");

        assert_eq!(face_state(&c, f) == "confirmed", imported, "{fixture}");
    }
}

/// Review #181 L3: the importer fails closed when the auto-tag check itself errors — the face
/// stays unconfirmed, as for a refusal, rather than confirmed without its tag. The error is
/// forced by shadowing `tags` with a TEMP table that `create_tag` can read (same ids) but
/// that lacks `auto_rule`; the face's foreign key still resolves against `main.tags`.
#[test]
fn importing_a_region_fails_closed_when_the_auto_tag_check_errors() {
    let (c, root) = temp_catalog("import-check-error");
    let p = add_photo(&c, &root, "portrait.ARW");
    let photo_path = root.join("portrait.ARW");
    std::fs::write(crate::xmp::sidecar_path(&photo_path), crate::xmp::region_fixtures::LIGHTROOM_ROTATED)
        .unwrap();
    c.conn()
        .execute("UPDATE photos SET width = 6000, height = 4000, exif_orientation = 6 WHERE id = ?1", [p])
        .unwrap();
    let f = add_face(&c, p, "[0.7,0.225,0.1,0.15]");
    let bob = c.create_tag("People/Bob").unwrap();
    c.conn()
        .execute_batch("CREATE TEMP TABLE tags AS SELECT id, full_path, full_path_norm FROM main.tags")
        .unwrap();
    assert!(c.auto_tag_refusal(bob).is_err(), "the check errors");

    import_regions(&c, c.conn(), p, &photo_path, "People");
    c.conn().execute_batch("DROP TABLE temp.tags").unwrap();

    assert_ne!(face_state(&c, f), "confirmed", "a failed check confirmed the face");
    assert!(!has_tag(&c, p, bob));
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

/// The picker's tags: the people root and its descendants. With no root set (or a blank one)
/// the root is the matcher's and the People view's default, `People` — so a person created
/// in the picker goes to `People/<name>` and the matcher counts them.
#[test]
fn people_tags_follow_the_people_root() {
    let (c, root) = temp_catalog("people");
    c.create_tag("People/Alice").unwrap();
    c.create_tag("People/Family/Bob").unwrap();
    c.create_tag("Peoples Republic").unwrap();
    c.create_tag("Places/Oslo").unwrap();
    c.create_tag("Family/Ann").unwrap();
    let paths = |p: &PeopleTags| {
        let mut v: Vec<String> = p.tags.iter().map(|t| t.full_path.clone()).collect();
        v.sort();
        v
    };
    let under_people = vec!["People", "People/Alice", "People/Family", "People/Family/Bob"];

    for unset in [None, Some("  ")] {
        if let Some(blank) = unset {
            c.set_setting(matcher::PEOPLE_ROOT_SETTING, blank).unwrap();
        }
        let picker = people_tags(&c).unwrap();
        assert_eq!(picker.root, matcher::PEOPLE_ROOT_DEFAULT, "{unset:?}: the matcher's default root");
        assert_eq!(picker.root, effective_people_root(&c).unwrap(), "{unset:?}: the People view's root");
        assert_eq!(paths(&picker), under_people, "{unset:?}");
    }

    // A person created in the picker with no root set lands under People, where the matcher
    // looks: it seeds from that photo-level tag.
    c.conn().execute("DELETE FROM settings WHERE key = ?1", [matcher::PEOPLE_ROOT_SETTING]).unwrap();
    let p = add_photo(&c, &root, "eve.NEF");
    let f = add_embedded_face(&c, p, 0);
    let picker = people_tags(&c).unwrap();
    let eve = assign_new_person(&c, f, &person_path(&picker.root, "Eve")).unwrap();
    assert_eq!(c.find_tag_id_by_path("People/Eve").unwrap(), Some(eve));
    let seen = matcher::run_matching(c.conn(), &matcher::MatchSettings::load(c.conn()).unwrap(), 0).unwrap();
    assert_eq!(seen.people, 1, "the matcher has a centroid for Eve: {seen:?}");

    c.set_setting(matcher::PEOPLE_ROOT_SETTING, " Family ").unwrap();
    let family = people_tags(&c).unwrap();
    assert_eq!(family.root, "Family");
    assert_eq!(matcher::MatchSettings::load(c.conn()).unwrap().people_root, "Family", "the matcher trims it too");
    assert_eq!(paths(&family), vec!["Family", "Family/Ann"]);
    assert_eq!(person_path(&family.root, "Eve"), "Family/Eve");
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

/// ✓ and ✕ as shown (#208): a face drawn as "Alice?" that is now suggested as Bob is stale
/// for both — Bob is neither confirmed nor rejected, and Alice is not remembered against a
/// face the user never saw rejected; a face drawn with no person that a run has suggested
/// since is stale for ✕; one still as drawn is rejected (and confirmed) as before.
#[test]
fn verdicts_as_shown_never_apply_to_another_person() {
    let (c, root) = temp_catalog("shown");
    let alice = c.create_tag("People/Alice").unwrap();
    let bob = c.create_tag("People/Bob").unwrap();
    let p = add_photo(&c, &root, "s.NEF");
    let rejections = |f: i64| -> i64 {
        c.conn().query_row("SELECT COUNT(*) FROM faces__rejections WHERE face_id = ?1", [f], |r| r.get(0)).unwrap()
    };
    let moved = add_face(&c, p, "[0.1,0.1,0.2,0.2]");
    suggest(&c, moved, bob); // drawn as Alice?, re-suggested as Bob since
    assert_eq!(accept_shown(&c, moved, alice).unwrap(), ShownVerdict::Stale);
    assert_eq!(reject_shown(&c, moved, Some(alice)).unwrap(), ShownVerdict::Stale);
    assert_eq!(face_state(&c, moved), "suggested");
    assert!(!has_tag(&c, p, alice) && !has_tag(&c, p, bob), "nobody confirmed");
    assert_eq!(rejections(moved), 0, "nobody rejected");

    let drawn_empty = add_face(&c, p, "[0.4,0.1,0.2,0.2]");
    suggest(&c, drawn_empty, bob); // drawn with no person, suggested as Bob since
    assert_eq!(reject_shown(&c, drawn_empty, None).unwrap(), ShownVerdict::Stale);
    assert_eq!(face_state(&c, drawn_empty), "suggested");
    assert_eq!(rejections(drawn_empty), 0);

    let confirmed = add_face(&c, p, "[0.7,0.1,0.2,0.2]");
    assign(&c, confirmed, alice).unwrap(); // decided elsewhere since it was drawn as Alice?
    assert_eq!(reject_shown(&c, confirmed, Some(alice)).unwrap(), ShownVerdict::Stale);
    assert_eq!(face_state(&c, confirmed), "confirmed");
    assert_eq!(rejections(confirmed), 0);

    let as_drawn = add_face(&c, p, "[0.1,0.5,0.2,0.2]");
    suggest(&c, as_drawn, bob);
    assert_eq!(reject_shown(&c, as_drawn, Some(bob)).unwrap(), ShownVerdict::Applied);
    assert_eq!((face_state(&c, as_drawn), rejections(as_drawn)), ("unassigned".to_string(), 1));
    let plain = add_face(&c, p, "[0.4,0.5,0.2,0.2]");
    assert_eq!(reject_shown(&c, plain, None).unwrap(), ShownVerdict::Applied, "an unassigned face as drawn");
    assert_eq!(rejections(plain), 0);
    let ok = add_face(&c, p, "[0.7,0.5,0.2,0.2]");
    suggest(&c, ok, bob);
    assert_eq!(accept_shown(&c, ok, bob).unwrap(), ShownVerdict::Applied);
    assert_eq!(face_state(&c, ok), "confirmed");
    assert!(has_tag(&c, p, bob), "a confirmation tags the photo");
    let regions: Vec<String> = crate::xmp::read_face_regions(&root.join("s.NEF")).into_iter().map(|r| r.name).collect();
    assert_eq!(regions, vec!["Alice".to_string(), "Bob".to_string()], "and exports its region");
}

/// The Tauri `faces_accept` on a face with no person says why, not "no assigned person".
#[test]
fn accepting_a_face_with_no_suggestion_says_it_changed() {
    let (c, root) = temp_catalog("accept-none");
    let p = add_photo(&c, &root, "n.NEF");
    let f = add_face(&c, p, "[0.1,0.1,0.2,0.2]");
    let err = accept(&c, f).unwrap_err().to_string();
    assert!(err.contains(matcher::NO_SUGGESTION_TO_ACCEPT), "{err}");
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
///
/// And the inspector's and overlay's ✓/✕ (#208), on two faces a previous run suggested as
/// Alice, made after this run has reset them: ✕ is remembered against the person shown, so
/// the run does not suggest Alice again; ✓ is reported stale and confirms nobody (the run
/// suggests Alice again, for the user to confirm).
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
    // Two faces the inspector shows as "Alice?", from a previous run (#208).
    let pr = add_photo(&c, &root, "pr.NEF");
    let fr = add_embedded_face(&c, pr, 0);
    suggest(&c, fr, alice);
    let px = add_photo(&c, &root, "px.NEF");
    let fx = add_embedded_face(&c, px, 0);
    suggest(&c, fx, alice);

    let state = state_with(c);
    let claim = begin_match_job(&state, None).unwrap();
    let sink = DecideMidRun {
        state: state.clone(),
        at_seed: std::sync::Mutex::new(Some(Box::new(move |c: &Catalog| ignore(c, fs)))),
        after_load: std::sync::Mutex::new(Some(Box::new(move |c: &Catalog| {
            assign(c, fa, bob)?;
            ignore(c, fi)?;
            name_faces(c, &[fc1], "People/Carol")?;
            // The run has reset both (they are unassigned now); the user clicks what they saw.
            assert_eq!(face_state(c, fr), "unassigned", "the interleaving: after the reset");
            assert_eq!(reject_shown(c, fr, Some(alice))?, ShownVerdict::Applied, "✕ on Alice? stands");
            assert_eq!(accept_shown(c, fx, alice)?, ShownVerdict::Stale, "✓ on a reset Alice? is reported");
            Ok(())
        }))),
        done: Default::default(),
    };
    run_match_job(&sink, claim);

    assert!(sink.at_seed.lock().unwrap().is_none() && sink.after_load.lock().unwrap().is_none(), "both hooks ran");
    let done = sink.done.lock().unwrap().take().expect("the run ended");
    assert!(done.ok, "{done:?}");
    let outcome = done.outcome.unwrap();
    assert_eq!((outcome.seeded, outcome.open, outcome.clustered), (0, 1, 2), "{outcome:?}");

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
    let (fr_state, fr_person, _, fr_cluster) = row(fr);
    assert_eq!((fr_state.as_str(), fr_person), ("unassigned", None), "rejected Alice is not suggested again");
    let fr_cluster = fr_cluster.expect("with Alice rejected, it is an unknown face");
    let remembered: i64 = c
        .conn()
        .query_row("SELECT COUNT(*) FROM faces__rejections WHERE face_id = ?1 AND person_tag_id = ?2", [fr, alice], |r| r.get(0))
        .unwrap();
    assert_eq!(remembered, 1, "the rejection names the person the user saw");
    let mut clusters = cluster_rows(c);
    clusters.sort();
    let mut want = vec![(fc2_cluster, 1), (fr_cluster, 1)];
    want.sort();
    assert_eq!(clusters, want, "the clusters hold only the faces still pending");
    assert_eq!(row(fx).0, "suggested", "a stale ✓ confirmed nobody; the run suggested again");
    assert_eq!(row(fx).1, Some(alice));
    assert!(!has_tag(c, px, alice), "nor tagged the photo");
    for (photo, name) in [
        (p0, "p0.NEF"),
        (ps, "ps.NEF"),
        (pa, "pa.NEF"),
        (pi, "pi.NEF"),
        (pc1, "pc1.NEF"),
        (pc2, "pc2.NEF"),
        (pr, "pr.NEF"),
        (px, "px.NEF"),
    ] {
        assert_consistent(c, &root, photo, name);
    }
}

// --- the matching job's seeds reach the sidecar (#210) --------------------------------------

/// A photo with one detected face and one person tag (Alice): the run seeds it.
fn seed_candidate(c: &Catalog, root: &std::path::Path, name: &str, alice: i64) -> i64 {
    let p = add_photo(c, root, name);
    add_face(c, p, "[0.1,0.1,0.2,0.2]");
    c.assign_tag(p, alice).unwrap();
    p
}

/// The seeded faces' regions are written at the end of the run, through the face verbs' own
/// write: into a foreign sidecar, kept and backed up first; into a new sidecar; and not into
/// one whose declared frame cannot hold the boxes (left byte for byte). A suggestion is not
/// exported. The write reports its own phase while the job owns its slot.
#[test]
fn the_match_job_writes_the_faces_it_seeded() {
    let (c, root) = temp_catalog("match-seed-regions");
    let alice = c.create_tag("People/Alice").unwrap();
    let foreign = seed_candidate(&c, &root, "foreign.NEF", alice);
    seed_foreign_sidecar(&root.join("foreign.NEF"));
    let fresh = seed_candidate(&c, &root, "fresh.NEF", alice);
    let square = seed_candidate(&c, &root, "square.NEF", alice);
    // Turned a quarter, stored 6000×4000, but the sidecar declares a square frame: refused.
    c.conn()
        .execute("UPDATE photos SET exif_orientation = 6, width = 6000, height = 4000 WHERE id = ?1", [square])
        .unwrap();
    let square_xml = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/" xmlns:stDim="http://ns.adobe.com/xap/1.0/sType/Dimensions#"><mwg-rs:Regions rdf:parseType="Resource"><mwg-rs:AppliedToDimensions stDim:w="5000" stDim:h="5000" stDim:unit="pixel"/><mwg-rs:RegionList><rdf:Bag/></mwg-rs:RegionList></mwg-rs:Regions></rdf:Description></rdf:RDF></x:xmpmeta>"#;
    std::fs::write(crate::xmp::sidecar_path(&root.join("square.NEF")), square_xml).unwrap();
    // Two faces: not a seed, only suggested (Alice's centroid needs an embedding: none here).
    let two = add_photo(&c, &root, "two.NEF");
    add_face(&c, two, "[0.1,0.1,0.2,0.2]");
    add_face(&c, two, "[0.5,0.1,0.2,0.2]");

    let state = state_with(c);
    let claim = begin_match_job(&state, None).unwrap();
    let recorder = MatchRecorder { state: state.clone(), seen: Default::default() };
    run_match_job(&recorder, claim);

    let seen = recorder.seen.lock().unwrap();
    let CoreEvent::FacesMatchDone(d) = &seen.last().unwrap().2 else { panic!("the end") };
    assert!(d.ok, "{d:?}");
    assert_eq!(d.outcome.as_ref().map(|o| o.seeded), Some(3));
    let writing: Vec<(usize, usize, bool)> = seen
        .iter()
        .filter_map(|(_, busy, e)| match e {
            CoreEvent::FacesMatchProgress(p) if p.phase == matcher::MatchPhase::Regions.label() => Some((p.done, p.total, *busy)),
            _ => None,
        })
        .collect();
    assert_eq!(writing.first(), Some(&(0, 3, true)), "{writing:?}");
    assert_eq!(writing.last(), Some(&(3, 3, true)), "{writing:?}");
    drop(seen);

    let guard = state.catalog.lock().unwrap();
    let c = guard.as_ref().unwrap();
    assert_foreign_kept(&root.join("foreign.NEF"), &["Alice"]);
    let backup = root.join("foreign.NEF.xmp.chairphoto-backup");
    assert!(backup.exists(), "the foreign sidecar is backed up before ChairPhoto's first write");
    seed_foreign_sidecar(&root.join("original.NEF"));
    assert_eq!(std::fs::read(&backup).unwrap(), std::fs::read(root.join("original.NEF.xmp")).unwrap(), "as it was");
    let names = |name: &str| -> Vec<String> {
        crate::xmp::read_face_regions(&root.join(name)).into_iter().map(|r| r.name).collect()
    };
    assert_eq!(names("fresh.NEF"), ["Alice"]);
    assert_eq!(
        std::fs::read_to_string(crate::xmp::sidecar_path(&root.join("square.NEF"))).unwrap(),
        square_xml,
        "a frame the boxes cannot be placed in: refused, untouched"
    );
    assert!(!crate::xmp::sidecar_path(&root.join("two.NEF")).exists(), "nothing confirmed there, nothing written");
    assert_consistent(c, &root, fresh, "fresh.NEF");
    assert_eq!(faces_for_photo(c, foreign).unwrap()[0].state, "confirmed");
}

/// A cancel or a catalog switch during the write stops it at the next photo: the photo in
/// flight is written, the rest are not, and the run still ends with its one terminal event.
#[test]
fn the_seeded_regions_write_stops_on_a_cancel_or_a_switch() {
    for switch in [false, true] {
        let (c, root) = temp_catalog(if switch { "match-seed-switch" } else { "match-seed-cancel" });
        let alice = c.create_tag("People/Alice").unwrap();
        let names = ["s1.NEF", "s2.NEF", "s3.NEF"];
        for name in names {
            seed_candidate(&c, &root, name, alice);
        }
        let state = state_with(c);
        let claim = begin_match_job(&state, None).unwrap();

        struct StopAtWrite {
            state: AppState,
            switch: bool,
            done: std::sync::Mutex<Option<FacesMatchDone>>,
        }
        impl EventSink for StopAtWrite {
            fn send(&self, event: CoreEvent) {
                match event {
                    CoreEvent::FacesMatchProgress(p) if p.phase == matcher::MatchPhase::Regions.label() && p.done == 0 => {
                        if self.switch {
                            detach_catalog_and_trip_jobs(&self.state).unwrap();
                        } else {
                            cancel_match(&self.state).unwrap();
                        }
                    }
                    CoreEvent::FacesMatchDone(d) => *self.done.lock().unwrap() = Some(d),
                    _ => {}
                }
            }
        }
        let sink = StopAtWrite { state: state.clone(), switch, done: Default::default() };
        run_match_job(&sink, claim);

        let done = sink.done.lock().unwrap().take().expect("the run ended");
        assert!(done.aborted && !done.ok, "switch={switch}: {done:?}");
        let written: Vec<&str> =
            names.into_iter().filter(|n| crate::xmp::sidecar_path(&root.join(n)).exists()).collect();
        assert_eq!(written.len(), 1, "switch={switch}: only the photo in flight: {written:?}");
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
    seed_foreign_sidecar(&root.join("a.NEF"));
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
    assert_foreign_kept(&root.join("a.NEF"), &["Jane"]);
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
    assert_eq!(out, ReviewOutcome { confirmed: 1, rejected: 1, stale: 1, auto_tag: 0 });
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

// ── Auto-tags as person tags (#181) ──────────────────────────────────────────────────────

/// An auto-tag can't be assigned by hand, so a face verb that would tag the photo with one
/// is refused whole: the face keeps its state, the photo gets no tag. A review skips such a
/// confirmation and counts it, and still applies the rest.
#[test]
fn face_verbs_refuse_an_auto_tag_and_leave_the_face_as_it_was() {
    let (c, root) = temp_catalog("autotag");
    let p = add_photo(&c, &root, "a.NEF");
    let auto = c.create_tag("Technique/Long Exposure").unwrap();
    c.conn().execute("UPDATE tags SET auto_rule = 'long-exposure' WHERE id = ?1", [auto]).unwrap();
    let alice = c.create_tag("People/Alice").unwrap();
    let f1 = add_face(&c, p, "[0.1,0.1,0.2,0.2]");
    let f2 = add_face(&c, p, "[0.4,0.1,0.2,0.2]");
    let f3 = add_face(&c, p, "[0.7,0.1,0.2,0.2]");

    suggest(&c, f1, auto);
    assert!(matches!(accept(&c, f1), Err(CatalogError::AutoTag(_))));
    assert_eq!(face_state(&c, f1), "suggested", "the confirmation rolled back");
    assert!(matches!(assign(&c, f2, auto), Err(CatalogError::AutoTag(_))));
    assert_eq!(face_state(&c, f2), "unassigned");
    assert!(!has_tag(&c, p, auto));

    suggest(&c, f3, alice);
    let out = review_suggestions(
        &c,
        &[
            Review { face_id: f1, tag_id: auto, verdict: Verdict::Confirm },
            Review { face_id: f3, tag_id: alice, verdict: Verdict::Confirm },
        ],
    )
    .unwrap();
    assert_eq!(out, ReviewOutcome { confirmed: 1, rejected: 0, stale: 0, auto_tag: 1 });
    assert_eq!((face_state(&c, f1), face_state(&c, f3)), ("suggested".into(), "confirmed".into()));
    assert!(has_tag(&c, p, alice) && !has_tag(&c, p, auto));

    // Review #181 nit: a verdict on an auto-tag suggestion that has since changed (f1 is now
    // suggested as Alice) is stale, not an auto-tag skip.
    suggest(&c, f1, alice);
    let out = review_suggestions(&c, &[Review { face_id: f1, tag_id: auto, verdict: Verdict::Confirm }]).unwrap();
    assert_eq!(out, ReviewOutcome { confirmed: 0, rejected: 0, stale: 1, auto_tag: 0 });
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
