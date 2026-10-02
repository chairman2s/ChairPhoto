//! MWG Regions sidecar write/read wiring (H13f) — the catalog side of face-region export/import.
//!
//! The merge-safe XMP codec itself lives in `crate::xmp` (`write_face_regions` /
//! `read_face_regions`); this module bridges it to the `faces__faces` table and the catalog:
//!
//! - **Write** ([`write_photo_regions`]): gather a photo's **confirmed** faces (with a person
//!   tag), resolve the photo path via the resolver (never `photos.path`), take the photo's
//!   EXIF Orientation and recorded pixel dimensions ([`region_frame`]: never turned by the
//!   user rotation), and write them as `mwg-rs:Regions`. Triggered from the
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

use crate::xmp::{FaceRegion, ReadRegion, RegionFrame};

/// Minimum IoU for an imported region to be considered the same face as a detection.
/// Matches the design doc ("IoU-match to detections", `>= 0.5`).
pub const IMPORT_IOU: f32 = 0.5;

/// The `source` written for a face confirmed from an imported MWG region.
pub const SOURCE_XMP: &str = "xmp";

// ── Region frame ────────────────────────────────────────────────────────────────

/// What the catalog knows of the photo's frames ([`RegionFrame`]): the EXIF Orientation the
/// scan recorded (`photos.exif_orientation`, #136) and the recorded pixel size
/// (`photos.width`/`height`, EXIF `ExifImageWidth`/`Height`: the stored frame's).
///
/// The writer turns the stored face boxes (EXIF-oriented, the frame the indexer detects on)
/// into the stored frame MWG 2.0 § 5.9 measures regions in, and writes `AppliedToDimensions`
/// in that frame; the importer turns them back. An orientation or size that is missing or
/// out of range is `None` — unknown, never guessed (no more `(1, 1)` stand-in size).
///
/// **The non-destructive `user_rotation` plays no part.** It lives only in the catalog: the
/// original is never rewritten and the sidecar carries no orientation of ours, so a tool
/// reading these regions (digiKam, Lightroom, a later ChairPhoto import) sees the file as its
/// own metadata orients it, never turned by the user's override. The GPUI overlay turns a box
/// drawn on the rotated loupe back before storing it, so the stored boxes are unturned too.
pub fn region_frame(conn: &Connection, photo_id: i64) -> rusqlite::Result<RegionFrame> {
    let row: Option<(Option<i64>, Option<i64>, Option<i64>)> = conn
        .query_row(
            "SELECT width, height, exif_orientation FROM photos WHERE id = ?1",
            [photo_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((w, h, orientation)) = row else {
        return Ok(RegionFrame::default());
    };
    let size = |v: Option<i64>| v.filter(|v| (1..=i64::from(u32::MAX)).contains(v)).map(|v| v as u32);
    Ok(RegionFrame {
        orientation: orientation.filter(|o| (1..=8).contains(o)).map(|o| o as u8),
        stored_size: size(w).zip(size(h)),
    })
}

// ── Write path ──────────────────────────────────────────────────────────────────

/// Collect the confirmed face regions for a photo: every `confirmed` face that carries a
/// person tag, paired with that tag's leaf **name** (`tags.name`) and its stored top-left
/// normalized bbox. Ignored/suggested/unassigned faces are excluded — only human-confirmed
/// (or seed/manual/xmp `confirmed`) faces are exported.
pub fn confirmed_regions(conn: &Connection, photo_id: i64) -> rusqlite::Result<Vec<FaceRegion>> {
    face_regions(
        conn,
        "SELECT f.id, t.name, f.bbox
           FROM faces__faces f
           JOIN tags t ON t.id = f.person_tag_id
          WHERE f.photo_id = ?1
            AND f.state = 'confirmed'
            AND f.person_tag_id IS NOT NULL
          ORDER BY f.id",
        photo_id,
    )
}

/// This catalog's faces on `photo_id` that are not in its exported set `regions` — rejected,
/// ignored, unassigned, suggested: the only marked regions a write may remove (review N1).
/// A marker whose face id is neither here nor in the set is not this catalog's to touch.
pub fn retired_faces(conn: &Connection, photo_id: i64, regions: &[FaceRegion]) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT id FROM faces__faces WHERE photo_id = ?1 ORDER BY id")?;
    let ids = stmt.query_map([photo_id], |r| r.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for id in ids {
        let id = id?;
        if !regions.iter().any(|r| r.face_id == id) {
            out.push(id);
        }
    }
    Ok(out)
}

/// The faces ChairPhoto exported into this photo's sidecar before regions carried its marker
/// (`faces__legacy_regions`, see [`store::ensure_schema`](super::store::ensure_schema)), with
/// the name and box they were exported with. The writer recognises a pre-marker region of
/// ours by them, adopting it while its face is confirmed and removing it once it is not (#135).
pub fn legacy_regions(conn: &Connection, photo_id: i64) -> rusqlite::Result<Vec<FaceRegion>> {
    face_regions(
        conn,
        "SELECT face_id, name, bbox FROM faces__legacy_regions WHERE photo_id = ?1 ORDER BY face_id",
        photo_id,
    )
}

/// What one [`convert_legacy_regions`] pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LegacyConversion {
    /// Photos whose sidecar was written: their pre-marker regions adopted (converted to the
    /// stored frame and marked) or removed, and their record spent.
    pub written: usize,
    /// Photos whose original was unreachable: skipped, record kept for a later pass.
    pub offline: usize,
    /// Photos whose write failed or was refused (logged): record kept for a later pass.
    pub failed: usize,
    /// The pass stopped at its abort flag; the photos it did not reach keep their record.
    pub aborted: bool,
}

/// The one-time conversion of the pre-marker regions (#135, review L1): write every photo on
/// the pre-marker record through [`write_photo_regions`], so its old-shaped regions
/// ([`crate::xmp`]'s pre-marker shape check) are converted into the stored frame and marked,
/// or removed if their face has gone, and the photo's record is spent. Without it a photo's
/// old regions stay in the display frame — misread by a later import on a turned photo —
/// until a face verb happens to touch it.
///
/// Resumable and abortable: the record is the queue. `abort` is checked before each photo,
/// each photo's write-then-spend is its own step, and a photo that is offline, fails or is
/// refused keeps its rows for the next pass. Rows of photos no longer in the catalog are
/// dropped first. Blocking (sidecar IO per photo): run it on a worker with its own catalog
/// connection — the faces index job runs it before indexing.
pub fn convert_legacy_regions<R>(
    conn: &Connection,
    mut resolve: R,
    abort: &std::sync::atomic::AtomicBool,
) -> rusqlite::Result<LegacyConversion>
where
    R: FnMut(i64) -> Result<Option<std::path::PathBuf>, String>,
{
    super::store::ensure_schema(conn)?;
    conn.execute("DELETE FROM faces__legacy_regions WHERE photo_id NOT IN (SELECT id FROM photos)", [])?;
    let photos: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT DISTINCT photo_id FROM faces__legacy_regions ORDER BY photo_id")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut out = LegacyConversion::default();
    for photo in photos {
        if abort.load(std::sync::atomic::Ordering::Relaxed) {
            out.aborted = true;
            break;
        }
        let path = match resolve(photo) {
            Ok(Some(path)) => path,
            Ok(None) => {
                out.offline += 1;
                continue;
            }
            Err(e) => {
                eprintln!("faces: pre-marker regions of photo {photo} not converted: {e}");
                out.failed += 1;
                continue;
            }
        };
        match write_photo_regions(conn, photo, |_| Ok(Some(path))) {
            Ok(()) => out.written += 1,
            Err(e) => {
                eprintln!("faces: pre-marker regions of photo {photo} not converted: {e}");
                out.failed += 1;
            }
        }
    }
    Ok(out)
}

fn face_regions(conn: &Connection, sql: &str, photo_id: i64) -> rusqlite::Result<Vec<FaceRegion>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([photo_id], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (face_id, name, bbox_s) = row?;
        if let Some(bbox) = parse_bbox(&bbox_s) {
            out.push(FaceRegion { face_id, name, bbox });
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
///
/// Rejected, ignored and unnamed faces are not in the set, so their regions — marked as
/// ChairPhoto's, or on the pre-marker record — leave the sidecar; foreign regions stay
/// (#135). Once a write has gone through, the photo's pre-marker record is spent: every region
/// it described has been adopted (and now carries the marker) or removed, and keeping it would
/// let a region another tool writes later at the same place, under the same name, be taken
/// for ours.
pub fn write_photo_regions<R>(
    conn: &Connection,
    photo_id: i64,
    resolve: R,
) -> Result<(), String>
where
    R: FnOnce(i64) -> Result<Option<std::path::PathBuf>, String>,
{
    super::store::ensure_schema(conn).map_err(|e| e.to_string())?;
    let catalog = crate::catalog::catalog_uuid(conn).map_err(|e| e.to_string())?;
    let regions = confirmed_regions(conn, photo_id).map_err(|e| e.to_string())?;
    let retired = retired_faces(conn, photo_id, &regions).map_err(|e| e.to_string())?;
    let legacy = legacy_regions(conn, photo_id).map_err(|e| e.to_string())?;
    let frame = region_frame(conn, photo_id).map_err(|e| e.to_string())?;

    let Some(path) = resolve(photo_id)? else {
        return Ok(()); // offline — skip, re-sync later.
    };
    crate::xmp::write_face_regions(&path, &catalog, &regions, &retired, &legacy, frame)?;
    conn.execute("DELETE FROM faces__legacy_regions WHERE photo_id = ?1", [photo_id])
        .map_err(|e| e.to_string())?;
    Ok(())
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
                                  width INTEGER, height INTEGER, user_rotation INTEGER NOT NULL DEFAULT 0,
                                  exif_orientation INTEGER);
             CREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT NOT NULL DEFAULT '',
                                full_path TEXT NOT NULL DEFAULT '');
             CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .unwrap();
        store::ensure_schema(&conn).unwrap();
        conn
    }

    // ── region_frame ───────────────────────────────────────────────────────────

    #[test]
    fn region_frame_is_the_recorded_orientation_and_size() {
        let conn = mem_conn();
        conn.execute(
            "INSERT INTO photos (id, width, height, user_rotation, exif_orientation)
             VALUES (1, 6000, 4000, 90, 6)",
            [],
        )
        .unwrap();
        assert_eq!(
            region_frame(&conn, 1).unwrap(),
            RegionFrame { orientation: Some(6), stored_size: Some((6000, 4000)) },
            "the user rotation plays no part"
        );
    }

    /// Nothing missing is made up: no `(1, 1)` size, no orientation 1.
    #[test]
    fn region_frame_unknowns_stay_unknown() {
        let conn = mem_conn();
        conn.execute("INSERT INTO photos (id) VALUES (1)", []).unwrap();
        conn.execute(
            "INSERT INTO photos (id, width, height, exif_orientation) VALUES (2, 6000, 0, 9)",
            [],
        )
        .unwrap();
        for id in [1, 2, 3] {
            assert_eq!(region_frame(&conn, id).unwrap(), RegionFrame::default(), "photo {id}");
        }
    }

    /// A user rotation is the catalog's alone (never in the original or the sidecar), so the
    /// exported regions are the same at 0/90/180/270: the stored boxes' frame, with the
    /// recorded dimensions — coordinates and AppliedToDimensions in one frame. (Before, a
    /// 90°/270° rotation swapped the dimensions and left the coordinates unturned.)
    #[test]
    fn exported_regions_ignore_the_user_rotation() {
        let dir = crate::test_support::TestTmpDir::new("faces-regions-rotation");
        let photo_path = dir.join("DSC31.ARW");
        std::fs::write(&photo_path, b"raw").unwrap();
        let mut seen = Vec::new();
        for rotation in [0i64, 90, 180, 270] {
            let conn = mem_conn();
            conn.execute(
                "INSERT INTO photos (id, width, height, user_rotation) VALUES (1, 6000, 4000, ?1)",
                [rotation],
            )
            .unwrap();
            conn.execute("INSERT INTO tags (id, name, full_path) VALUES (10, 'Alice', 'People/Alice')", [])
                .unwrap();
            let f = store::insert_face(&conn, 1, "[0.1,0.2,0.3,0.4]", "[]", 0.9, None, "drawn", 0).unwrap();
            conn.execute("UPDATE faces__faces SET person_tag_id = 10, state = 'confirmed' WHERE id = ?1", [f])
                .unwrap();
            let _ = std::fs::remove_file(crate::xmp::sidecar_path(&photo_path));
            write_photo_regions(&conn, 1, |_| Ok(Some(photo_path.clone()))).unwrap();
            let read = crate::xmp::read_face_regions(&photo_path);
            assert_eq!(read.len(), 1, "{rotation}°");
            let (x, y, w, h) = read[0].bbox;
            let near = |a: f32, b: f32| (a - b).abs() < 1e-4;
            assert!(near(x, 0.1) && near(y, 0.2) && near(w, 0.3) && near(h, 0.4), "{rotation}°: {:?}", read[0].bbox);
            let xml = std::fs::read_to_string(crate::xmp::sidecar_path(&photo_path)).unwrap();
            let dims = |tag: &str| {
                let open = format!("<stDim:{tag}>");
                let at = xml.find(&open).unwrap_or_else(|| panic!("{rotation}°: no stDim:{tag} in {xml}")) + open.len();
                xml[at..xml[at..].find('<').unwrap() + at].to_string()
            };
            assert_eq!((dims("w"), dims("h")), ("6000".to_string(), "4000".to_string()), "{rotation}°");
            seen.push(read[0].bbox);
        }
        assert!(seen.windows(2).all(|p| p[0] == p[1]), "{seen:?}");
    }

    /// #136 through the catalog: a photo whose scan recorded EXIF Orientation 6 exports its
    /// face in the stored frame (worked out by hand: the display box (0.1, 0.2, 0.3, 0.4)
    /// turned back 90°), and the importer, given the same photo's frame, reads the display box.
    #[test]
    fn exported_regions_are_in_the_stored_frame() {
        let dir = crate::test_support::TestTmpDir::new("faces-regions-stored-frame");
        let photo_path = dir.join("DSC32.ARW");
        std::fs::write(&photo_path, b"raw").unwrap();
        let conn = mem_conn();
        conn.execute(
            "INSERT INTO photos (id, width, height, exif_orientation) VALUES (1, 6000, 4000, 6)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO tags (id, name, full_path) VALUES (10, 'Alice', 'People/Alice')", [])
            .unwrap();
        let f = store::insert_face(&conn, 1, "[0.1,0.2,0.3,0.4]", "[]", 0.9, None, "drawn", 0).unwrap();
        conn.execute("UPDATE faces__faces SET person_tag_id = 10, state = 'confirmed' WHERE id = ?1", [f])
            .unwrap();
        write_photo_regions(&conn, 1, |_| Ok(Some(photo_path.clone()))).unwrap();

        let near = |a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)| {
            [(a.0, b.0), (a.1, b.1), (a.2, b.2), (a.3, b.3)].iter().all(|(x, y)| (x - y).abs() < 1e-4)
        };
        let stored = crate::xmp::read_face_regions(&photo_path);
        assert!(near(stored[0].bbox, (0.2, 0.6, 0.4, 0.3)), "{:?}", stored[0].bbox);
        let frame = region_frame(&conn, 1).unwrap();
        let display = crate::xmp::read_face_regions_in(&photo_path, frame);
        assert!(near(display[0].bbox, (0.1, 0.2, 0.3, 0.4)), "{:?}", display[0].bbox);
    }

    // ── the pre-marker record (#135) ───────────────────────────────────────────

    fn confirm(conn: &Connection, photo: i64, tag: i64, bbox: &str, source: &str, state: &str) -> i64 {
        let f = store::insert_face(conn, photo, bbox, "[]", 0.9, None, source, 0).unwrap();
        conn.execute(
            "UPDATE faces__faces SET person_tag_id = ?2, state = ?3 WHERE id = ?1",
            rusqlite::params![f, tag, state],
        )
        .unwrap();
        f
    }

    fn record(conn: &Connection) -> Vec<(i64, i64, String, String)> {
        let mut stmt = conn
            .prepare("SELECT face_id, photo_id, name, bbox FROM faces__legacy_regions ORDER BY face_id")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// The record is taken once, when the faces tables predate it, from the confirmed, named
    /// faces the pre-marker writer exported as far as the catalog can tell (review M2): one a
    /// verb confirmed, and an auto-seeded one only on a photo a verb touched (a verb-confirmed
    /// face, an ignored face or a remembered rejection there). Not a seed on an untouched
    /// photo (the matching pass exported nothing), not one confirmed from another tool's
    /// region (`source = 'xmp'`), not one never confirmed. A face confirmed afterwards is
    /// written with the marker and never joins it.
    #[test]
    fn the_pre_marker_record_is_the_faces_the_old_writer_exported() {
        let conn = mem_conn();
        conn.execute_batch(
            "DROP TABLE faces__legacy_regions;
             DROP TABLE faces__once;
             INSERT INTO photos (id) VALUES (1), (2), (3), (4);
             INSERT INTO tags (id, name, full_path) VALUES (10, 'Alice', 'People/Alice'),
                                                          (11, 'Bob', 'People/Bob');",
        )
        .unwrap();
        let alice = confirm(&conn, 1, 10, "[0.1,0.1,0.2,0.2]", "manual", "confirmed");
        let bob_seed = confirm(&conn, 1, 11, "[0.4,0.1,0.1,0.1]", "seed", "confirmed");
        confirm(&conn, 1, 11, "[0.5,0.5,0.1,0.1]", "xmp", "confirmed");
        confirm(&conn, 1, 11, "[0.7,0.7,0.1,0.1]", "match", "suggested");
        // Photo 2: a seed nothing ever exported.
        confirm(&conn, 2, 10, "[0.1,0.1,0.2,0.2]", "seed", "confirmed");
        // Photo 3: a seed beside an ignored face; photo 4: beside a rejected one.
        let seed3 = confirm(&conn, 3, 10, "[0.1,0.1,0.2,0.2]", "seed", "confirmed");
        confirm(&conn, 3, 11, "[0.6,0.6,0.1,0.1]", "detect", "ignored");
        let seed4 = confirm(&conn, 4, 10, "[0.1,0.1,0.2,0.2]", "seed", "confirmed");
        let rejected = confirm(&conn, 4, 11, "[0.6,0.6,0.1,0.1]", "detect", "unassigned");
        conn.execute(
            "INSERT INTO faces__rejections (face_id, person_tag_id, rejected_at) VALUES (?1, 11, 0)",
            [rejected],
        )
        .unwrap();
        store::ensure_schema(&conn).unwrap();
        let ids: Vec<i64> = record(&conn).into_iter().map(|r| r.0).collect();
        assert_eq!(ids, [alice, bob_seed, seed3, seed4]);
        let want = record(&conn);
        assert_eq!(want[0], (alice, 1, "Alice".to_string(), "[0.1,0.1,0.2,0.2]".to_string()));

        confirm(&conn, 1, 11, "[0.3,0.3,0.1,0.1]", "manual", "confirmed");
        store::ensure_schema(&conn).unwrap();
        assert_eq!(record(&conn), want, "taken once, not on every open");
    }

    /// A write that reaches the sidecar spends the photo's record — the region it described
    /// was adopted or removed — while an offline photo keeps it for the write that will.
    #[test]
    fn a_written_photo_spends_its_pre_marker_record() {
        let dir = crate::test_support::TestTmpDir::new("faces-regions-legacy-spent");
        let photo_path = dir.join("DSC33.ARW");
        std::fs::write(&photo_path, b"raw").unwrap();
        let conn = mem_conn();
        conn.execute_batch(
            "INSERT INTO photos (id, width, height, exif_orientation) VALUES (1, 6000, 4000, 1);
             INSERT INTO photos (id) VALUES (2);
             INSERT INTO faces__legacy_regions (face_id, photo_id, name, bbox)
                 VALUES (100, 1, 'Dora', '[0.5,0.5,0.1,0.1]'), (200, 2, 'Eve', '[0.1,0.1,0.1,0.1]');",
        )
        .unwrap();
        // The pre-marker writer's Dora, unmarked; Dora has since been rejected.
        let ours = crate::xmp::FaceRegion { face_id: 0, name: "Dora".into(), bbox: (0.5, 0.5, 0.1, 0.1) };
        let unmarked = crate::xmp::RegionFrame { orientation: None, stored_size: Some((6000, 4000)) };
        let catalog = crate::catalog::catalog_uuid(&conn).unwrap();
        crate::xmp::write_face_regions(&photo_path, &catalog, &[ours], &[], &[], unmarked).unwrap();
        let xml = std::fs::read_to_string(crate::xmp::sidecar_path(&photo_path)).unwrap();
        let unmarked_xml = xml.replace(&format!("<chairphoto:FaceId>{catalog}/0</chairphoto:FaceId>"), "");
        assert_ne!(xml, unmarked_xml, "the marker was not where the test expects it");
        std::fs::write(crate::xmp::sidecar_path(&photo_path), unmarked_xml).unwrap();

        write_photo_regions(&conn, 2, |_| Ok(None)).unwrap();
        write_photo_regions(&conn, 1, |_| Ok(Some(photo_path.clone()))).unwrap();

        assert!(crate::xmp::read_face_regions(&photo_path).is_empty(), "Dora's old region stayed");
        let left: Vec<i64> = record(&conn).into_iter().map(|r| r.0).collect();
        assert_eq!(left, [200], "the offline photo keeps its record, the written one spent it");
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
