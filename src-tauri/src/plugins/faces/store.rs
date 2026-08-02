//! `faces__faces` table + resumable indexing queue.
//!
//! Schema is created lazily on first use via [`ensure_schema`], following the same pattern as
//! `plugins/ai/mod.rs` and `plugins/map/mod.rs`. The connection passed in is the secondary
//! catalog connection opened by the indexer — the primary connection is never touched here.
//!
//! ## Table design
//!
//! `faces__faces`  — one row per detected face (photo_id FK → photos, normalized bbox +
//!                   landmarks, 512-d f32 embedding as BLOB, optional person_tag_id FK → tags,
//!                   state, match_confidence, source, created_at).
//! `faces__queue`  — one row per photo that needs (or still needs) face-indexing. The indexer
//!                   inserts a row for every eligible photo and deletes it when done. Survives a
//!                   crash/abort so the job can auto-resume — same pattern as `pending_enrichment`.
//! `faces__indexed` — sentinel table: one row per photo that has been run through the detector,
//!                   regardless of how many faces were found. Zero-face photos get a row here but
//!                   NOT in `faces__faces`. `populate_queue` excludes photos present in either
//!                   table so zero-face photos are never re-enqueued. Use `mark_indexed` after
//!                   detection completes (even when faces == 0), and `dequeue` afterwards.
//!
//! The embedding BLOB layout is: 512 × little-endian IEEE-754 f32 = 2048 bytes.

use rusqlite::{Connection, OptionalExtension};
use serde_json;

// ── Schema ────────────────────────────────────────────────────────────────────

/// Create the plugin-owned tables if they do not already exist. Idempotent — safe to call
/// on every connection open. Adding columns to a pre-existing table uses `ALTER TABLE … ADD
/// COLUMN` and ignores "duplicate column" errors, the same resilient migration pattern used
/// throughout the catalog.
pub fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS faces__faces (
            id                INTEGER PRIMARY KEY AUTOINCREMENT,
            photo_id          INTEGER NOT NULL,
            bbox              TEXT    NOT NULL,
            landmarks         TEXT    NOT NULL,
            detect_confidence REAL    NOT NULL,
            embedding         BLOB,
            person_tag_id     INTEGER REFERENCES tags(id) ON DELETE SET NULL,
            state             TEXT    NOT NULL DEFAULT 'unassigned',
            match_confidence  REAL,
            source            TEXT    NOT NULL DEFAULT 'match',
            created_at        INTEGER NOT NULL,
            FOREIGN KEY (photo_id) REFERENCES photos(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS faces__faces_photo
            ON faces__faces (photo_id);
        CREATE INDEX IF NOT EXISTS faces__faces_state
            ON faces__faces (state);

        CREATE TABLE IF NOT EXISTS faces__queue (
            photo_id   INTEGER PRIMARY KEY,
            queued_at  INTEGER NOT NULL
        );

        -- Rejection memory (H13c): a (face, person_tag) pair the user rejected is never
        -- re-proposed. One row per rejected pairing. `faces_reject(face_id)` records the
        -- pair currently suggested/assigned on that face; the matcher excludes it forever.
        CREATE TABLE IF NOT EXISTS faces__rejections (
            face_id       INTEGER NOT NULL,
            person_tag_id INTEGER NOT NULL,
            rejected_at   INTEGER NOT NULL,
            PRIMARY KEY (face_id, person_tag_id),
            FOREIGN KEY (face_id) REFERENCES faces__faces(id) ON DELETE CASCADE
        );

        -- Unnamed-cluster bookkeeping (H13c step 5): leftover faces that match no known
        -- person are grouped into incremental clusters. Naming a cluster binds a person tag
        -- and confirms its members. The centroid is the L2-normalized mean of member
        -- embeddings, cached as a BLOB so incremental joins don't re-scan every member.
        CREATE TABLE IF NOT EXISTS faces__clusters (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            centroid    BLOB    NOT NULL,
            size        INTEGER NOT NULL DEFAULT 0,
            created_at  INTEGER NOT NULL
        );

        -- Sentinel: one row per photo that has been run through the detector.
        -- Zero-face photos get a row here but NOT in faces__faces; this prevents
        -- populate_queue from re-enqueuing them on every subsequent call.
        CREATE TABLE IF NOT EXISTS faces__indexed (
            photo_id    INTEGER PRIMARY KEY,
            indexed_at  INTEGER NOT NULL,
            face_count  INTEGER NOT NULL DEFAULT 0
        );
        ",
    )?;
    // Future-proof: add new columns to pre-existing tables without breaking an existing DB.
    for sql in [
        "ALTER TABLE faces__faces ADD COLUMN cluster_id INTEGER",
    ] {
        let _ = conn.execute(sql, []);
    }
    Ok(())
}

// ── Queue helpers ─────────────────────────────────────────────────────────────

/// Insert a photo into the indexing queue. Idempotent (INSERT OR IGNORE on PK).
pub fn enqueue(conn: &Connection, photo_id: i64, now: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO faces__queue (photo_id, queued_at) VALUES (?1, ?2)",
        rusqlite::params![photo_id, now],
    )?;
    Ok(())
}

/// Remove a photo from the queue once its face rows have been written.
pub fn dequeue(conn: &Connection, photo_id: i64) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM faces__queue WHERE photo_id = ?1",
        [photo_id],
    )?;
    Ok(())
}

/// Count photos still waiting to be indexed.
pub fn queue_len(conn: &Connection) -> rusqlite::Result<usize> {
    conn.query_row("SELECT COUNT(*) FROM faces__queue", [], |r| {
        r.get::<_, i64>(0)
    })
    .map(|n| n as usize)
}

/// Load the full queue as a `Vec<photo_id>`, ordered by `queued_at` (oldest first).
pub fn load_queue(conn: &Connection) -> rusqlite::Result<Vec<i64>> {
    let mut stmt =
        conn.prepare("SELECT photo_id FROM faces__queue ORDER BY queued_at ASC")?;
    let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
    rows.collect()
}

// ── Faces table helpers ───────────────────────────────────────────────────────

/// True if the photo has already been run through the detector — either it has face rows
/// in `faces__faces`, OR it has a sentinel row in `faces__indexed` (zero-face result).
/// Either condition means the photo should be skipped on re-run.
pub fn photo_is_indexed(conn: &Connection, photo_id: i64) -> rusqlite::Result<bool> {
    // Check the sentinel table first (covers zero-face photos that have no faces__faces rows).
    let in_indexed = conn
        .query_row(
            "SELECT 1 FROM faces__indexed WHERE photo_id = ?1 LIMIT 1",
            [photo_id],
            |_| Ok(()),
        )
        .optional()
        .map(|r| r.is_some())?;
    if in_indexed {
        return Ok(true);
    }
    // Fallback: photos with face rows but no sentinel (e.g. rows written before this schema
    // version was installed) are also considered indexed.
    conn.query_row(
        "SELECT 1 FROM faces__faces WHERE photo_id = ?1 LIMIT 1",
        [photo_id],
        |_| Ok(()),
    )
    .optional()
    .map(|r| r.is_some())
}

/// Record that detection has been run for this photo (even when zero faces were found).
/// This sentinel prevents populate_queue from re-enqueuing the photo on future runs.
/// Must be called BEFORE `dequeue` so a crash between the two leaves the photo in the
/// queue with the sentinel — the next run will find it, see it's already indexed, and
/// dequeue it cleanly without re-running detection.
pub fn mark_indexed(
    conn: &Connection,
    photo_id: i64,
    face_count: usize,
    now: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO faces__indexed (photo_id, indexed_at, face_count) VALUES (?1, ?2, ?3)",
        rusqlite::params![photo_id, now, face_count as i64],
    )?;
    Ok(())
}

/// Insert one detected-face row.
/// `bbox` and `landmarks` are JSON strings (pre-serialized by the caller).
/// `embedding` is the raw 2048-byte f32-LE BLOB, `None` when no embedding was computed
/// (e.g. tiny face / error).
#[allow(clippy::too_many_arguments)]
pub fn insert_face(
    conn: &Connection,
    photo_id: i64,
    bbox: &str,
    landmarks: &str,
    detect_confidence: f32,
    embedding: Option<&[u8]>,
    source: &str,
    now: i64,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO faces__faces
            (photo_id, bbox, landmarks, detect_confidence, embedding, source, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            photo_id,
            bbox,
            landmarks,
            detect_confidence as f64,
            embedding,
            source,
            now
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

// ── Embedding codec (shared between store and tests) ─────────────────────────

/// Encode a 512-d f32 embedding as a 2048-byte little-endian BLOB.
pub fn embedding_to_blob(embedding: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(embedding.len() * 4);
    for &v in embedding {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Decode a little-endian BLOB back into a `Vec<f32>`.
/// Returns an error if the slice length is not a multiple of 4.
pub fn blob_to_embedding(blob: &[u8]) -> Result<Vec<f32>, String> {
    if blob.len() % 4 != 0 {
        return Err(format!(
            "embedding blob length {} is not a multiple of 4",
            blob.len()
        ));
    }
    let mut out = Vec::with_capacity(blob.len() / 4);
    for chunk in blob.chunks_exact(4) {
        out.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok(out)
}

/// One face row as returned to the frontend.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FaceForPhoto {
    pub id: i64,
    pub photo_id: i64,
    pub bbox: FaceBboxJson,
    pub detect_confidence: f32,
    pub person_tag_id: Option<i64>,
    pub person_name: Option<String>,
    pub state: String,
    pub match_confidence: Option<f32>,
    pub source: String,
}

/// Normalized bbox as a typed object for the frontend (`{x,y,w,h}`). The DB stores
/// this as a `[x,y,w,h]` JSON array; we deserialize then re-serialize as an object.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct FaceBboxJson {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl FaceBboxJson {
    /// Parse the `[x,y,w,h]` array stored in the DB.
    fn from_array_str(s: &str) -> Result<Self, String> {
        let arr: [f32; 4] = serde_json::from_str(s)
            .map_err(|e| format!("bbox parse error: {e}"))?;
        Ok(FaceBboxJson { x: arr[0], y: arr[1], w: arr[2], h: arr[3] })
    }

    /// Parse a DB-stored `[x,y,w,h]` JSON array, falling back to a full-frame bbox on error.
    /// Public convenience for callers outside this module (e.g. commands.rs summary queries).
    pub fn from_str(s: &str) -> Self {
        Self::from_array_str(s).unwrap_or(FaceBboxJson { x: 0.0, y: 0.0, w: 1.0, h: 1.0 })
    }
}

/// Return all face rows for a photo, joined to the tags table for the person name.
pub fn faces_for_photo(conn: &Connection, photo_id: i64) -> rusqlite::Result<Vec<FaceForPhoto>> {
    let mut stmt = conn.prepare(
        "SELECT f.id, f.photo_id, f.bbox, f.detect_confidence,
                f.person_tag_id, t.name, f.state, f.match_confidence, f.source
           FROM faces__faces f
           LEFT JOIN tags t ON t.id = f.person_tag_id
          WHERE f.photo_id = ?1
          ORDER BY f.id",
    )?;
    let rows = stmt.query_map([photo_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, f64>(3)?,
            r.get::<_, Option<i64>>(4)?,
            r.get::<_, Option<String>>(5)?,
            r.get::<_, String>(6)?,
            r.get::<_, Option<f64>>(7)?,
            r.get::<_, String>(8)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, pid, bbox_s, det_conf, tag_id, tag_name, state, match_conf, source) = row?;
        let bbox = FaceBboxJson::from_array_str(&bbox_s).unwrap_or(FaceBboxJson { x: 0.0, y: 0.0, w: 1.0, h: 1.0 });
        out.push(FaceForPhoto {
            id,
            photo_id: pid,
            bbox,
            detect_confidence: det_conf as f32,
            person_tag_id: tag_id,
            person_name: tag_name,
            state,
            match_confidence: match_conf.map(|v| v as f32),
            source,
        });
    }
    Ok(out)
}

/// Return every detected face's normalized `[x,y,w,h]` bbox for a photo, as
/// `sharpness_regions::NormBox`. Used by the sharpness scorer (H16c) to measure inside faces
/// — *all* detected faces count as subject regions (not just confirmed ones), since even an
/// unnamed detection marks where a person is in the frame. Malformed bboxes are skipped.
/// Returns an empty vec when the photo has no faces.
pub fn face_boxes_for_photo(
    conn: &Connection,
    photo_id: i64,
) -> rusqlite::Result<Vec<crate::sharpness_regions::NormBox>> {
    let mut stmt = conn.prepare("SELECT bbox FROM faces__faces WHERE photo_id = ?1")?;
    let rows = stmt.query_map([photo_id], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        let bbox_s = row?;
        if let Ok(arr) = serde_json::from_str::<[f32; 4]>(&bbox_s) {
            out.push(crate::sharpness_regions::NormBox {
                x: arr[0],
                y: arr[1],
                w: arr[2],
                h: arr[3],
            });
        }
    }
    Ok(out)
}

// ── Build the initial queue from the catalog ──────────────────────────────────

/// Populate `faces__queue` with every photo that has not yet been run through the detector
/// and is not already in the queue. Called once at the start of a fresh index run. Returns
/// the number of photos added to the queue.
///
/// Skip conditions (either is sufficient):
/// - Already in `faces__indexed` sentinel (ran through detector; may have found zero faces).
/// - Already has rows in `faces__faces` (has face data; covers rows from before this schema).
/// - Already in `faces__queue` (currently queued in this or a previous run).
pub fn populate_queue(conn: &Connection, now: i64) -> rusqlite::Result<usize> {
    let count = conn.execute(
        "INSERT OR IGNORE INTO faces__queue (photo_id, queued_at)
         SELECT p.id, ?1
           FROM photos p
          WHERE NOT EXISTS (
                    SELECT 1 FROM faces__indexed i WHERE i.photo_id = p.id
                )
            AND NOT EXISTS (
                    SELECT 1 FROM faces__faces f WHERE f.photo_id = p.id
                )
            AND NOT EXISTS (
                    SELECT 1 FROM faces__queue q WHERE q.photo_id = p.id
                )",
        rusqlite::params![now],
    )?;
    Ok(count as usize)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        // Minimal schema that faces__* depends on.
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS photos (
                 id INTEGER PRIMARY KEY,
                 uuid TEXT NOT NULL DEFAULT '',
                 path TEXT NOT NULL DEFAULT ''
             );
             CREATE TABLE IF NOT EXISTS tags (id INTEGER PRIMARY KEY);",
        )
        .unwrap();
        ensure_schema(&conn).unwrap();
        conn
    }

    // ── Embedding codec round-trip ────────────────────────────────────────────

    /// Vec<f32> → BLOB → Vec<f32> round-trip is lossless.
    #[test]
    fn embedding_blob_round_trip() {
        let orig: Vec<f32> = (0..512).map(|i| i as f32 * 0.001).collect();
        let blob = embedding_to_blob(&orig);
        assert_eq!(blob.len(), 512 * 4, "BLOB must be 2048 bytes");
        let back = blob_to_embedding(&blob).unwrap();
        assert_eq!(back.len(), orig.len());
        for (a, b) in orig.iter().zip(back.iter()) {
            assert!(
                (a - b).abs() < 1e-7,
                "round-trip mismatch at value {a} vs {b}"
            );
        }
    }

    /// L2-normalized unit vector survives the round-trip and remains unit.
    #[test]
    fn embedding_blob_round_trip_unit_vector() {
        let mut v: Vec<f32> = (0..512).map(|i| (i as f32 % 7.0) - 3.0).collect();
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        for x in &mut v {
            *x /= norm;
        }
        let blob = embedding_to_blob(&v);
        let back = blob_to_embedding(&blob).unwrap();
        let back_norm: f32 = back.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (back_norm - 1.0).abs() < 1e-5,
            "round-tripped unit vector must remain unit (norm={back_norm})"
        );
    }

    /// A slice whose length is not a multiple of 4 is rejected cleanly.
    #[test]
    fn blob_to_embedding_rejects_odd_length() {
        let bad = vec![0u8; 7];
        assert!(blob_to_embedding(&bad).is_err());
    }

    // ── Queue semantics ────────────────────────────────────────────────────────

    /// enqueue is idempotent.
    #[test]
    fn enqueue_is_idempotent() {
        let conn = mem_conn();
        conn.execute("INSERT INTO photos (id) VALUES (1)", []).unwrap();
        enqueue(&conn, 1, 100).unwrap();
        enqueue(&conn, 1, 200).unwrap(); // second call must not fail
        assert_eq!(queue_len(&conn).unwrap(), 1);
    }

    /// dequeue removes the row; queue becomes empty.
    #[test]
    fn dequeue_removes_row() {
        let conn = mem_conn();
        conn.execute("INSERT INTO photos (id) VALUES (2)", []).unwrap();
        enqueue(&conn, 2, 100).unwrap();
        assert_eq!(queue_len(&conn).unwrap(), 1);
        dequeue(&conn, 2).unwrap();
        assert_eq!(queue_len(&conn).unwrap(), 0);
    }

    // ── Queue resume after simulated abort ────────────────────────────────────

    /// Simulates an abort mid-run: half the queue is processed (dequeued), the other half
    /// stays. A second load_queue call resumes from where the job left off.
    #[test]
    fn queue_resumes_after_simulated_abort() {
        let conn = mem_conn();
        for id in 1i64..=6 {
            conn.execute("INSERT INTO photos (id) VALUES (?1)", [id]).unwrap();
            enqueue(&conn, id, id * 10).unwrap();
        }
        assert_eq!(queue_len(&conn).unwrap(), 6);

        // Simulate processing photos 1–3, then aborting before 4–6.
        let all = load_queue(&conn).unwrap();
        assert_eq!(all.len(), 6);
        for &id in &all[..3] {
            // "Process" by inserting a face row, marking indexed, and dequeueing.
            insert_face(&conn, id, "[0,0,0.1,0.1]", "[[0.05,0.05]]", 0.9, None, "detect", 999)
                .unwrap();
            mark_indexed(&conn, id, 1, 999).unwrap();
            dequeue(&conn, id).unwrap();
        }

        // After abort: 3 remain.
        assert_eq!(queue_len(&conn).unwrap(), 3);

        // Resume: load remaining queue and process.
        let remaining = load_queue(&conn).unwrap();
        assert_eq!(remaining.len(), 3);
        // Photos 4-6 are in the remaining queue.
        assert!(remaining.contains(&4));
        assert!(remaining.contains(&5));
        assert!(remaining.contains(&6));
    }

    // ── Skip-already-indexed ──────────────────────────────────────────────────

    /// A photo that already has a faces row is not re-indexed.
    #[test]
    fn skip_already_indexed() {
        let conn = mem_conn();
        conn.execute("INSERT INTO photos (id) VALUES (10)", []).unwrap();

        assert!(!photo_is_indexed(&conn, 10).unwrap());

        insert_face(
            &conn, 10,
            "[0.1,0.1,0.2,0.3]",
            "[[0.2,0.2],[0.3,0.2],[0.25,0.28],[0.22,0.34],[0.28,0.34]]",
            0.85, None, "detect", 12345,
        )
        .unwrap();
        mark_indexed(&conn, 10, 1, 12345).unwrap();

        assert!(photo_is_indexed(&conn, 10).unwrap());
    }

    /// A photo where zero faces were detected is marked via the sentinel and not
    /// considered indexed via faces__faces (no rows there), but photo_is_indexed
    /// returns true because of the sentinel row.
    #[test]
    fn skip_zero_face_photo_via_sentinel() {
        let conn = mem_conn();
        conn.execute("INSERT INTO photos (id) VALUES (11)", []).unwrap();

        assert!(!photo_is_indexed(&conn, 11).unwrap());

        // Zero faces → no insert_face call, but we mark indexed.
        mark_indexed(&conn, 11, 0, 99999).unwrap();

        // Must be considered indexed despite no faces__faces row.
        assert!(photo_is_indexed(&conn, 11).unwrap());
    }

    // ── populate_queue skips already-indexed ──────────────────────────────────

    /// populate_queue only enqueues photos without an existing faces row OR sentinel row.
    #[test]
    fn populate_queue_skips_indexed_photos() {
        let conn = mem_conn();
        for id in 1i64..=5 {
            conn.execute("INSERT INTO photos (id) VALUES (?1)", [id]).unwrap();
        }
        // Photo 1 has face rows (old-style — no sentinel).
        insert_face(&conn, 1, "[0,0,0.1,0.1]", "[]", 0.9, None, "detect", 0).unwrap();
        // Photo 2 has the sentinel only (zero-face result — the new case).
        mark_indexed(&conn, 2, 0, 0).unwrap();

        let added = populate_queue(&conn, 1000).unwrap();
        // Only photos 3, 4, 5 should be enqueued.
        assert_eq!(added, 3);
        let q = load_queue(&conn).unwrap();
        assert!(!q.contains(&1));
        assert!(!q.contains(&2));
        assert!(q.contains(&3));
        assert!(q.contains(&4));
        assert!(q.contains(&5));
    }

    // ── FaceBboxJson::from_str ────────────────────────────────────────────────

    /// from_str parses a valid [x,y,w,h] JSON array correctly.
    #[test]
    fn bbox_from_str_valid() {
        let b = FaceBboxJson::from_str("[0.1,0.2,0.3,0.4]");
        assert!((b.x - 0.1).abs() < 1e-6);
        assert!((b.y - 0.2).abs() < 1e-6);
        assert!((b.w - 0.3).abs() < 1e-6);
        assert!((b.h - 0.4).abs() < 1e-6);
    }

    /// from_str falls back to a full-frame bbox on malformed input.
    #[test]
    fn bbox_from_str_malformed_fallback() {
        let b = FaceBboxJson::from_str("not-json");
        assert_eq!(b.x, 0.0);
        assert_eq!(b.y, 0.0);
        assert_eq!(b.w, 1.0);
        assert_eq!(b.h, 1.0);
    }

    // ── People summary query pattern ──────────────────────────────────────────
    //
    // The faces_people_summary command runs a SQL query against faces__faces +
    // tags. Verify that the query pattern returns the expected aggregates.

    fn mem_conn_with_tags() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS photos (
                 id INTEGER PRIMARY KEY,
                 uuid TEXT NOT NULL DEFAULT '',
                 path TEXT NOT NULL DEFAULT ''
             );
             CREATE TABLE IF NOT EXISTS tags (
                 id INTEGER PRIMARY KEY,
                 name TEXT NOT NULL DEFAULT '',
                 full_path TEXT NOT NULL DEFAULT ''
             );",
        )
        .unwrap();
        ensure_schema(&conn).unwrap();
        conn
    }

    /// The people-summary query aggregates confirmed faces by person tag.
    #[test]
    fn people_summary_aggregates_confirmed_faces() {
        let conn = mem_conn_with_tags();
        conn.execute("INSERT INTO photos (id) VALUES (1)", []).unwrap();
        conn.execute("INSERT INTO photos (id) VALUES (2)", []).unwrap();
        conn.execute(
            "INSERT INTO tags (id, name, full_path) VALUES (10, 'Alice', 'People/Alice')",
            [],
        )
        .unwrap();

        // Two confirmed faces for Alice on two different photos.
        insert_face(&conn, 1, "[0.1,0.1,0.2,0.2]", "[]", 0.9, None, "seed", 100).unwrap();
        insert_face(&conn, 2, "[0.3,0.3,0.2,0.2]", "[]", 0.9, None, "seed", 100).unwrap();
        conn.execute(
            "UPDATE faces__faces SET person_tag_id = 10, state = 'confirmed'",
            [],
        )
        .unwrap();

        // Validate the query pattern used by faces_people_summary.
        let mut stmt = conn.prepare(
            "SELECT f.person_tag_id, t.name, t.full_path,
                    COUNT(f.id) AS face_count, COUNT(DISTINCT f.photo_id) AS photo_count,
                    MIN(f.id) AS rep_face_id
               FROM faces__faces f
               JOIN tags t ON t.id = f.person_tag_id
              WHERE f.state = 'confirmed' AND f.person_tag_id IS NOT NULL
              GROUP BY f.person_tag_id
              ORDER BY t.full_path",
        ).unwrap();
        let row: (i64, String, String, i64, i64, i64) = stmt.query_row([], |r| {
            Ok((
                r.get(0)?, r.get(1)?, r.get(2)?,
                r.get(3)?, r.get(4)?, r.get(5)?,
            ))
        }).unwrap();
        assert_eq!(row.0, 10, "tag_id");
        assert_eq!(row.1, "Alice", "name");
        assert_eq!(row.3, 2, "face_count");
        assert_eq!(row.4, 2, "photo_count (distinct)");
    }

    /// The cluster-summary query aggregates unassigned faces by cluster_id.
    #[test]
    fn cluster_summary_aggregates_cluster_members() {
        let conn = mem_conn_with_tags();
        for id in 1i64..=3 {
            conn.execute("INSERT INTO photos (id) VALUES (?1)", [id]).unwrap();
        }
        // Insert three faces all in cluster 42, state = unassigned.
        for pid in 1i64..=3 {
            let fid = insert_face(&conn, pid, "[0,0,0.1,0.1]", "[]", 0.9, None, "detect", 0).unwrap();
            conn.execute(
                "UPDATE faces__faces SET cluster_id = 42 WHERE id = ?1",
                [fid],
            ).unwrap();
        }

        // Validate the query pattern used by faces_cluster_summary.
        let row: (i64, i64, i64) = conn.query_row(
            "SELECT cluster_id, COUNT(*) AS cnt, MIN(id) AS rep_face_id
               FROM faces__faces
              WHERE cluster_id IS NOT NULL
                AND (state = 'unassigned' OR state = 'suggested')
              GROUP BY cluster_id",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        ).unwrap();
        assert_eq!(row.0, 42, "cluster_id");
        assert_eq!(row.1, 3, "member count");
    }

    /// The suggestion-list query returns suggested faces ordered by confidence.
    #[test]
    fn suggestion_list_ordered_by_confidence() {
        let conn = mem_conn_with_tags();
        for pid in 1i64..=2 {
            conn.execute("INSERT INTO photos (id) VALUES (?1)", [pid]).unwrap();
        }
        conn.execute(
            "INSERT INTO tags (id, name, full_path) VALUES (10, 'Alice', 'People/Alice')",
            [],
        ).unwrap();

        let f1 = insert_face(&conn, 1, "[0.1,0.1,0.2,0.2]", "[]", 0.9, None, "match", 0).unwrap();
        let f2 = insert_face(&conn, 2, "[0.2,0.2,0.2,0.2]", "[]", 0.8, None, "match", 0).unwrap();
        // f1: high confidence (0.9), f2: lower (0.5).
        conn.execute(
            "UPDATE faces__faces SET person_tag_id = 10, state = 'suggested', match_confidence = 0.9 WHERE id = ?1",
            [f1],
        ).unwrap();
        conn.execute(
            "UPDATE faces__faces SET person_tag_id = 10, state = 'suggested', match_confidence = 0.5 WHERE id = ?1",
            [f2],
        ).unwrap();

        let mut stmt = conn.prepare(
            "SELECT f.id, COALESCE(f.match_confidence, 0.0)
               FROM faces__faces f
               JOIN tags t ON t.id = f.person_tag_id
              WHERE f.state = 'suggested' AND f.person_tag_id IS NOT NULL
              ORDER BY f.match_confidence DESC NULLS LAST",
        ).unwrap();
        let rows: Vec<(i64, f64)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, f1, "highest confidence face first");
        assert!((rows[0].1 - 0.9).abs() < 1e-6);
        assert_eq!(rows[1].0, f2);
    }
}
