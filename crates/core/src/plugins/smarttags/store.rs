//! `smarttags__embeddings` table — schema, BLOB codec, and queue helpers (H7b).
//!
//! Schema is created lazily on first use via [`ensure_schema`], following the same
//! pattern as `plugins/ai/mod.rs`, `plugins/faces/store.rs`, and `plugins/map/mod.rs`.
//! The connection passed in is always the secondary catalog connection opened by the
//! indexer — the primary connection is never touched here.
//!
//! ## Table design
//!
//! `smarttags__embeddings` — one row per photo that has been embedded. `photo_id` is the
//!   primary key; an `INSERT OR REPLACE` upsert keeps the row current if a re-index is
//!   needed (e.g. after a model change). The `vector` column stores the L2-normalized
//!   f32 embedding as a little-endian BLOB; `indexed_at` is a unix timestamp.
//!
//! ## Implicit queue
//!
//! Unlike the faces indexer (which has an explicit `faces__queue` table), the embedding
//! queue is implicit: `SELECT id FROM photos WHERE NOT EXISTS (SELECT 1 FROM
//! smarttags__embeddings WHERE photo_id = photos.id)`. This is the same pattern as
//! sharpness (`sharpness IS NULL`) and pHash (`phash IS NULL`) — a crash mid-run simply
//! leaves the photo without a row, and the next run picks it up. No separate bookkeeping.
//!
//! ## BLOB layout
//!
//! Dimension × 4 bytes (little-endian IEEE-754 f32). For CLIP ViT-B/32 that is
//! 512 × 4 = 2048 bytes. The kNN engine (H7c) reads this format directly.

use rusqlite::Connection;

// ── Schema ────────────────────────────────────────────────────────────────────

/// Create the plugin-owned table if it does not already exist. Idempotent — safe to
/// call on every connection open (the same "lazy `ensure_schema`" pattern used by
/// `plugins/ai/mod.rs` and `plugins/faces/store.rs`).
pub fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS smarttags__embeddings (
            photo_id   INTEGER PRIMARY KEY,
            vector     BLOB    NOT NULL,
            indexed_at INTEGER NOT NULL,
            FOREIGN KEY (photo_id) REFERENCES photos(id) ON DELETE CASCADE
        );
        ",
    )?;
    Ok(())
}

// ── Queue helper ──────────────────────────────────────────────────────────────

/// Return all photo IDs that do not yet have a row in `smarttags__embeddings`, ordered
/// by `id` for stable resumption. This is the implicit "un-embedded" queue.
pub fn load_unembedded(conn: &Connection) -> Result<Vec<i64>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT p.id FROM photos p
              WHERE NOT EXISTS (
                  SELECT 1 FROM smarttags__embeddings e WHERE e.photo_id = p.id
              )
              ORDER BY p.id ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| r.get::<_, i64>(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(rows)
}

// ── Upsert ────────────────────────────────────────────────────────────────────

/// Insert or replace the embedding for `photo_id`. The `INSERT OR REPLACE` semantics
/// mean a re-index (after a model change) updates the existing row rather than failing.
///
/// `blob` is the raw f32-LE bytes (from [`embedding_to_blob`]).
pub fn upsert_embedding(conn: &Connection, photo_id: i64, blob: &[u8]) -> rusqlite::Result<()> {
    let now = unix_now();
    conn.execute(
        "INSERT OR REPLACE INTO smarttags__embeddings (photo_id, vector, indexed_at)
         VALUES (?1, ?2, ?3)",
        rusqlite::params![photo_id, blob, now],
    )?;
    Ok(())
}

// ── BLOB codec ────────────────────────────────────────────────────────────────

/// Encode a `Vec<f32>` embedding as a little-endian byte BLOB.
pub fn embedding_to_blob(embedding: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(embedding.len() * 4);
    for &v in embedding {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Decode a little-endian BLOB back into `Vec<f32>`. Returns an error when the slice
/// length is not a multiple of 4.
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

// ── Helpers ───────────────────────────────────────────────────────────────────

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS photos (
                 id   INTEGER PRIMARY KEY,
                 uuid TEXT NOT NULL DEFAULT '',
                 path TEXT NOT NULL DEFAULT ''
             );",
        )
        .unwrap();
        ensure_schema(&conn).unwrap();
        conn
    }

    // ── Schema idempotency ─────────────────────────────────────────────────────

    #[test]
    fn ensure_schema_is_idempotent() {
        let conn = mem_conn();
        // Calling twice must not fail.
        ensure_schema(&conn).unwrap();
        ensure_schema(&conn).unwrap();
    }

    // ── BLOB codec ─────────────────────────────────────────────────────────────

    #[test]
    fn embedding_blob_roundtrip() {
        let orig: Vec<f32> = (0..512).map(|i| i as f32 * 0.001).collect();
        let blob = embedding_to_blob(&orig);
        assert_eq!(blob.len(), 512 * 4, "BLOB must be 2048 bytes for a 512-d embedding");
        let back = blob_to_embedding(&blob).unwrap();
        assert_eq!(back.len(), orig.len());
        for (a, b) in orig.iter().zip(back.iter()) {
            assert!((a - b).abs() < 1e-7, "round-trip mismatch: {a} vs {b}");
        }
    }

    #[test]
    fn embedding_blob_roundtrip_unit_vector() {
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
            "unit vector must remain unit after round-trip, norm={back_norm}"
        );
    }

    #[test]
    fn blob_to_embedding_rejects_odd_length() {
        let bad = vec![0u8; 7];
        assert!(blob_to_embedding(&bad).is_err());
    }

    // ── Queue helper ───────────────────────────────────────────────────────────

    #[test]
    fn load_unembedded_excludes_already_embedded() {
        let conn = mem_conn();
        for id in 1i64..=5 {
            conn.execute("INSERT INTO photos (id) VALUES (?1)", [id]).unwrap();
        }
        // Pre-embed photos 2 and 4.
        let blob = embedding_to_blob(&[1.0f32]);
        upsert_embedding(&conn, 2, &blob).unwrap();
        upsert_embedding(&conn, 4, &blob).unwrap();

        let unembedded = load_unembedded(&conn).unwrap();
        assert_eq!(unembedded, vec![1, 3, 5]);
    }

    // ── Upsert semantics ───────────────────────────────────────────────────────

    #[test]
    fn upsert_embedding_is_idempotent() {
        let conn = mem_conn();
        conn.execute("INSERT INTO photos (id) VALUES (1)", []).unwrap();

        let blob1 = embedding_to_blob(&[1.0f32, 0.0]);
        upsert_embedding(&conn, 1, &blob1).unwrap();

        // Second upsert with a different blob — must update, not fail.
        let blob2 = embedding_to_blob(&[0.0f32, 1.0]);
        upsert_embedding(&conn, 1, &blob2).unwrap();

        // The row must hold the second blob.
        let stored: Vec<u8> = conn
            .query_row(
                "SELECT vector FROM smarttags__embeddings WHERE photo_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, blob2, "upsert must replace the existing row");
    }

    /// After upsert, load_unembedded must not return the embedded photo.
    #[test]
    fn upsert_removes_photo_from_queue() {
        let conn = mem_conn();
        for id in 1i64..=3 {
            conn.execute("INSERT INTO photos (id) VALUES (?1)", [id]).unwrap();
        }
        assert_eq!(load_unembedded(&conn).unwrap().len(), 3);

        let blob = embedding_to_blob(&[1.0f32]);
        upsert_embedding(&conn, 2, &blob).unwrap();

        let queue = load_unembedded(&conn).unwrap();
        assert_eq!(queue, vec![1, 3]);
        assert!(!queue.contains(&2));
    }
}
