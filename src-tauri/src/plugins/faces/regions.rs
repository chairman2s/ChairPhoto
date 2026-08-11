//! MWG Regions sidecar write/read wiring (H13f) — the catalog side of face-region export/import.
//!
//! The merge-safe XMP codec itself lives in `crate::xmp` (`write_face_regions` /
//! `read_face_regions`); this module bridges it to the `faces__faces` table and the catalog:
//!
//! - **Write** ([`write_photo_regions`]): gather a photo's **confirmed** faces (with a person
//!   tag), resolve the photo path via the resolver (never `photos.path`), compute the photo's
//!   oriented pixel dimensions, and write them as `mwg-rs:Regions`. Triggered from the
//!   confirm/unconfirm hook points in `commands.rs` (`faces_accept` / `faces_assign` /
//!   `faces_reject` / `faces_ignore` / `faces_name_cluster`), the same place keyword XMP
//!   export happens. Writing the full current set each time keeps the sidecar in sync as
//!   confirmations are added/removed.
//!
//! - **Import** ([`import_photo_regions`]): parse any existing `mwg-rs:Regions` from the
//!   sidecar (written by digiKam/Lightroom/Picasa or a prior chairphoto run), IoU-match each
//!   named region to a detected face (`>= IMPORT_IOU`), and for a match with a name: find or
//!   create the person tag under the people-root branch, mark the face `confirmed`,
//!   `source='xmp'`, and assign the person tag to the photo. Run during indexing, after a
//!   photo's faces are detected.
//!
//! The DB mutations here are pure over SQLite; the XMP + tag-creation side effects go through
//! `crate::xmp` and the `Catalog`, which the caller owns.

use rusqlite::{Connection, OptionalExtension};

use crate::xmp::{FaceRegion, ReadRegion};

/// Minimum IoU for an imported region to be considered the same face as a detection.
/// Matches the design doc ("IoU-match to detections", `>= 0.5`).
pub const IMPORT_IOU: f32 = 0.5;

/// The `source` written for a face confirmed from an imported MWG region.
pub const SOURCE_XMP: &str = "xmp";

// ── Oriented dimensions ─────────────────────────────────────────────────────────

/// The photo's oriented pixel dimensions `(w, h)` for `mwg-rs:AppliedToDimensions` — the
/// reference frame the normalized region coordinates apply to.
///
/// Face bboxes are normalized to the **oriented** preview, so the AppliedToDimensions must
/// describe the oriented image. We take the stored EXIF `width`/`height` and apply the
/// non-destructive `user_rotation` (90/270 swap the axes). When dimensions are missing we
/// fall back to a square unit frame — the region coordinates stay normalized and correct
/// regardless; only the declared reference size is approximate.
pub fn oriented_dimensions(conn: &Connection, photo_id: i64) -> rusqlite::Result<(u32, u32)> {
    let row: Option<(Option<i64>, Option<i64>, i64)> = conn
        .query_row(
            "SELECT width, height, user_rotation FROM photos WHERE id = ?1",
            [photo_id],
            |r| {
                Ok((
                    r.get::<_, Option<i64>>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?;

    let (w, h, rot) = match row {
        Some((Some(w), Some(h), rot)) if w > 0 && h > 0 => (w as u32, h as u32, rot),
        _ => return Ok((1, 1)),
    };

    // 90°/270° user rotation swaps width and height.
    let swap = matches!(((rot % 360) + 360) % 360, 90 | 270);
    Ok(if swap { (h, w) } else { (w, h) })
}

// ── Write path ──────────────────────────────────────────────────────────────────

/// Collect the confirmed face regions for a photo: every `confirmed` face that carries a
/// person tag, paired with that tag's leaf **name** (`tags.name`) and its stored top-left
/// normalized bbox. Ignored/suggested/unassigned faces are excluded — only human-confirmed
/// (or seed/manual/xmp `confirmed`) faces are exported.
pub fn confirmed_regions(conn: &Connection, photo_id: i64) -> rusqlite::Result<Vec<FaceRegion>> {
    let mut stmt = conn.prepare(
        "SELECT t.name, f.bbox
           FROM faces__faces f
           JOIN tags t ON t.id = f.person_tag_id
          WHERE f.photo_id = ?1
            AND f.state = 'confirmed'
            AND f.person_tag_id IS NOT NULL
          ORDER BY f.id",
    )?;
    let rows = stmt.query_map([photo_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (name, bbox_s) = row?;
        if let Some(bbox) = parse_bbox(&bbox_s) {
            out.push(FaceRegion { name, bbox });
        }
    }
    Ok(out)
}

/// Write a photo's confirmed face regions into its XMP sidecar, merge-safely.
///
/// `resolve` resolves the photo id to a reachable path (never `photos.path`) — in production
/// this is `catalog.resolve_photo_path`. When the photo is offline (`Ok(None)`) the write is
/// skipped silently (the catalog remains authoritative; the sidecar re-syncs later). Sidecar
/// write failures are returned as `Err` for the caller to log; they are non-fatal to the
/// confirm operation.
pub fn write_photo_regions<R>(
    conn: &Connection,
    photo_id: i64,
    resolve: R,
) -> Result<(), String>
where
    R: FnOnce(i64) -> Result<Option<std::path::PathBuf>, String>,
{
    let regions = confirmed_regions(conn, photo_id).map_err(|e| e.to_string())?;
    let (w, h) = oriented_dimensions(conn, photo_id).map_err(|e| e.to_string())?;

    match resolve(photo_id)? {
        Some(path) => crate::xmp::write_face_regions(&path, &regions, w, h),
        None => Ok(()), // offline — skip, re-sync later.
    }
}

// ── Import path ─────────────────────────────────────────────────────────────────

/// One region matched to a detected face during import.
pub struct ImportMatch {
    /// The detected face row that the region matched.
    pub face_id: i64,
    /// The region's name (person leaf name), guaranteed non-empty for a returned match.
    pub name: String,
}

/// Match parsed MWG regions against a photo's detected, still-**unassigned** faces by IoU
/// (`>= IMPORT_IOU`), greedily best-first. Only named regions are matched (an unnamed region
/// carries no person to import). Each detected face and each region is used at most once.
/// Pure over the given rows — no DB writes, no tag creation — so it is unit-testable.
///
/// `detected` is `(face_id, top-left-normalized bbox)`; `regions` are as read from the sidecar.
pub fn match_regions_to_faces(
    detected: &[(i64, (f32, f32, f32, f32))],
    regions: &[ReadRegion],
) -> Vec<ImportMatch> {
    // Score every (region, face) pair above threshold, then greedily take the best,
    // consuming both sides so nothing is double-assigned.
    let mut pairs: Vec<(f32, usize, i64)> = Vec::new(); // (iou, region_idx, face_id)
    for (ri, region) in regions.iter().enumerate() {
        if region.name.trim().is_empty() {
            continue;
        }
        for (face_id, fbbox) in detected {
            let iou = crate::xmp::region_iou(region.bbox, *fbbox);
            if iou >= IMPORT_IOU {
                pairs.push((iou, ri, *face_id));
            }
        }
    }
    // Highest IoU first; deterministic tie-break by region then face id.
    pairs.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
            .then(a.2.cmp(&b.2))
    });

    let mut used_regions = std::collections::HashSet::new();
    let mut used_faces = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (_iou, ri, face_id) in pairs {
        if used_regions.contains(&ri) || used_faces.contains(&face_id) {
            continue;
        }
        used_regions.insert(ri);
        used_faces.insert(face_id);
        out.push(ImportMatch {
            face_id,
            name: regions[ri].name.clone(),
        });
    }
    out
}

/// Load a photo's detected faces that are still candidates for region import: `unassigned`
/// (not yet confirmed/suggested/ignored/manual), with their top-left normalized bbox.
pub fn unassigned_faces(
    conn: &Connection,
    photo_id: i64,
) -> rusqlite::Result<Vec<(i64, (f32, f32, f32, f32))>> {
    let mut stmt = conn.prepare(
        "SELECT id, bbox FROM faces__faces
          WHERE photo_id = ?1 AND state = 'unassigned'
          ORDER BY id",
    )?;
    let rows = stmt.query_map([photo_id], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, bbox_s) = row?;
        if let Some(bbox) = parse_bbox(&bbox_s) {
            out.push((id, bbox));
        }
    }
    Ok(out)
}

/// Confirm a face from an imported region: set the person tag, `state='confirmed'`,
/// `source='xmp'`. The caller (owning the `Catalog`) resolves/creates the person tag id and
/// assigns it to the photo; here we only bind the face row.
pub fn confirm_imported_face(
    conn: &Connection,
    face_id: i64,
    person_tag_id: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE faces__faces
            SET person_tag_id = ?2, state = 'confirmed', source = ?3,
                match_confidence = 1.0, cluster_id = NULL
          WHERE id = ?1",
        rusqlite::params![face_id, person_tag_id, SOURCE_XMP],
    )?;
    Ok(())
}

// ── Helpers ─────────────────────────────────────────────────────────────────────

/// Parse a `[x,y,w,h]` JSON array (the stored `faces__faces.bbox` form) into a tuple.
fn parse_bbox(s: &str) -> Option<(f32, f32, f32, f32)> {
    let arr: [f32; 4] = serde_json::from_str(s).ok()?;
    Some((arr[0], arr[1], arr[2], arr[3]))
}

// ── Tests ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::faces::store;

    fn mem_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE photos (id INTEGER PRIMARY KEY, uuid TEXT DEFAULT '', path TEXT DEFAULT '',
                                  width INTEGER, height INTEGER, user_rotation INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT NOT NULL DEFAULT '',
                                full_path TEXT NOT NULL DEFAULT '');",
        )
        .unwrap();
        store::ensure_schema(&conn).unwrap();
        conn
    }

    // ── oriented_dimensions ────────────────────────────────────────────────────

    #[test]
    fn oriented_dims_no_rotation() {
        let conn = mem_conn();
        conn.execute(
            "INSERT INTO photos (id, width, height, user_rotation) VALUES (1, 6000, 4000, 0)",
            [],
        )
        .unwrap();
        assert_eq!(oriented_dimensions(&conn, 1).unwrap(), (6000, 4000));
    }

    #[test]
    fn oriented_dims_90_swaps() {
        let conn = mem_conn();
        conn.execute(
            "INSERT INTO photos (id, width, height, user_rotation) VALUES (1, 6000, 4000, 90)",
            [],
        )
        .unwrap();
        assert_eq!(oriented_dimensions(&conn, 1).unwrap(), (4000, 6000));
    }

    #[test]
    fn oriented_dims_missing_falls_back() {
        let conn = mem_conn();
        conn.execute("INSERT INTO photos (id) VALUES (1)", []).unwrap();
        assert_eq!(oriented_dimensions(&conn, 1).unwrap(), (1, 1));
    }

    // ── confirmed_regions ──────────────────────────────────────────────────────

    #[test]
    fn confirmed_regions_only_confirmed_with_person() {
        let conn = mem_conn();
        conn.execute("INSERT INTO photos (id) VALUES (1)", []).unwrap();
        conn.execute(
            "INSERT INTO tags (id, name, full_path) VALUES (10, 'Alice', 'People/Alice')",
            [],
        )
        .unwrap();

        // Confirmed face for Alice.
        let f1 = store::insert_face(&conn, 1, "[0.1,0.2,0.3,0.4]", "[]", 0.9, None, "seed", 0).unwrap();
        conn.execute(
            "UPDATE faces__faces SET person_tag_id = 10, state = 'confirmed' WHERE id = ?1",
            [f1],
        )
        .unwrap();
        // A suggested face (must be excluded).
        let f2 = store::insert_face(&conn, 1, "[0.5,0.5,0.1,0.1]", "[]", 0.9, None, "match", 0).unwrap();
        conn.execute(
            "UPDATE faces__faces SET person_tag_id = 10, state = 'suggested' WHERE id = ?1",
            [f2],
        )
        .unwrap();

        let regions = confirmed_regions(&conn, 1).unwrap();
        assert_eq!(regions.len(), 1, "only the confirmed face is exported");
        assert_eq!(regions[0].name, "Alice");
        assert_eq!(regions[0].bbox, (0.1, 0.2, 0.3, 0.4));
    }

    // ── match_regions_to_faces (IoU import matching) ───────────────────────────

    #[test]
    fn import_matches_by_iou() {
        // Detected faces.
        let detected = vec![
            (1i64, (0.10, 0.10, 0.20, 0.20)),
            (2i64, (0.60, 0.60, 0.20, 0.20)),
        ];
        // Region overlapping face 1 strongly, plus an unnamed region (ignored).
        let regions = vec![
            ReadRegion { name: "Alice".into(), bbox: (0.11, 0.11, 0.20, 0.20) },
            ReadRegion { name: "".into(), bbox: (0.60, 0.60, 0.20, 0.20) },
        ];
        let matches = match_regions_to_faces(&detected, &regions);
        assert_eq!(matches.len(), 1, "only the named, overlapping region matches");
        assert_eq!(matches[0].face_id, 1);
        assert_eq!(matches[0].name, "Alice");
    }

    #[test]
    fn import_below_iou_does_not_match() {
        let detected = vec![(1i64, (0.10, 0.10, 0.20, 0.20))];
        // Region far away — IoU 0.
        let regions = vec![ReadRegion { name: "Alice".into(), bbox: (0.70, 0.70, 0.20, 0.20) }];
        assert!(match_regions_to_faces(&detected, &regions).is_empty());
    }

    #[test]
    fn import_greedy_best_first_no_double_assign() {
        // Two detections close together; two named regions. Best-IoU pairs win, one-to-one.
        let detected = vec![
            (1i64, (0.10, 0.10, 0.20, 0.20)),
            (2i64, (0.12, 0.12, 0.20, 0.20)),
        ];
        let regions = vec![
            ReadRegion { name: "Alice".into(), bbox: (0.10, 0.10, 0.20, 0.20) }, // matches face 1 exactly
            ReadRegion { name: "Bob".into(), bbox: (0.12, 0.12, 0.20, 0.20) },   // matches face 2 exactly
        ];
        let matches = match_regions_to_faces(&detected, &regions);
        assert_eq!(matches.len(), 2);
        // Each face used once, each name used once.
        let mut faces: Vec<i64> = matches.iter().map(|m| m.face_id).collect();
        faces.sort();
        assert_eq!(faces, vec![1, 2]);
        let alice = matches.iter().find(|m| m.name == "Alice").unwrap();
        assert_eq!(alice.face_id, 1, "Alice's exact match is face 1");
    }

    /// End-to-end import over a real sidecar fixture: a foreign (digiKam-style) named region is
    /// read, IoU-matched to a detected face, and confirmed as source='xmp'.
    #[test]
    fn import_from_fixture_sidecar_confirms_face() {
        let conn = mem_conn();
        conn.execute("INSERT INTO photos (id) VALUES (1)", []).unwrap();
        // Detected face overlapping the fixture region (fixture center 0.2,0.2 size 0.2 →
        // corner 0.1,0.1). Our detection is nearly the same box.
        let f = store::insert_face(&conn, 1, "[0.1,0.1,0.2,0.2]", "[]", 0.9, None, "detect", 0).unwrap();

        // Write a foreign sidecar fixture next to a temp "photo" file.
        let dir = crate::test_support::TestTmpDir::new("faces-import-fixture");
        let photo_path = dir.join("DSC30.ARW");
        std::fs::write(&photo_path, b"raw").unwrap();
        let sidecar = crate::xmp::sidecar_path(&photo_path);
        let fixture = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/"
    xmlns:stArea="http://ns.adobe.com/xmp/sType/Area#">
   <mwg-rs:Regions rdf:parseType="Resource">
    <mwg-rs:RegionList>
     <rdf:Bag>
      <rdf:li rdf:parseType="Resource">
       <mwg-rs:Name>Grandma</mwg-rs:Name>
       <mwg-rs:Type>Face</mwg-rs:Type>
       <mwg-rs:Area rdf:parseType="Resource">
        <stArea:x>0.2</stArea:x><stArea:y>0.2</stArea:y>
        <stArea:w>0.2</stArea:w><stArea:h>0.2</stArea:h>
        <stArea:unit>normalized</stArea:unit>
       </mwg-rs:Area>
      </rdf:li>
     </rdf:Bag>
    </mwg-rs:RegionList>
   </mwg-rs:Regions>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
        std::fs::write(&sidecar, fixture).unwrap();

        // Read → match → confirm, mirroring faces_import_regions.
        let read = crate::xmp::read_face_regions(&photo_path);
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].name, "Grandma");
        let detected = unassigned_faces(&conn, 1).unwrap();
        let matches = match_regions_to_faces(&detected, &read);
        assert_eq!(matches.len(), 1, "fixture region matches the detected face");
        assert_eq!(matches[0].face_id, f);

        // Simulate the tag as if created under People/Grandma.
        conn.execute(
            "INSERT INTO tags (id, name, full_path) VALUES (10, 'Grandma', 'People/Grandma')",
            [],
        )
        .unwrap();
        confirm_imported_face(&conn, matches[0].face_id, 10).unwrap();

        let (state, source): (String, String) = conn
            .query_row(
                "SELECT state, source FROM faces__faces WHERE id = ?1",
                [f],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "confirmed");
        assert_eq!(source, "xmp");
    }

    #[test]
    fn confirm_imported_face_sets_source_xmp() {
        let conn = mem_conn();
        conn.execute("INSERT INTO photos (id) VALUES (1)", []).unwrap();
        conn.execute(
            "INSERT INTO tags (id, name, full_path) VALUES (10, 'Alice', 'People/Alice')",
            [],
        )
        .unwrap();
        let f = store::insert_face(&conn, 1, "[0.1,0.1,0.2,0.2]", "[]", 0.9, None, "detect", 0).unwrap();
        confirm_imported_face(&conn, f, 10).unwrap();

        let (state, person, source): (String, Option<i64>, String) = conn
            .query_row(
                "SELECT state, person_tag_id, source FROM faces__faces WHERE id = ?1",
                [f],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, "confirmed");
        assert_eq!(person, Some(10));
        assert_eq!(source, "xmp");
    }
}
