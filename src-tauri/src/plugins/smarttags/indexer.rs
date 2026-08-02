//! Resumable background embedding-index job for Smart Tagging (H7b).
//!
//! [`run_index`] is the single entry point. It:
//!
//! 1. Calls [`store::ensure_schema`] on the secondary connection to create
//!    `smarttags__embeddings` if it does not exist yet.
//! 2. Loads all un-embedded photos via a LEFT JOIN against `smarttags__embeddings`
//!    (`photo_id IS NULL`) — this is the implicit resume queue.
//! 3. Resolves each photo to a reachable path via the catalog resolver.
//! 4. Loads the cached 2048 px preview bytes (`thumbnails::preview_bytes`).
//! 5. Calls the embed closure (in production: `embed::encode_jpeg`) and upserts the
//!    resulting f32-LE BLOB into `smarttags__embeddings`.
//! 6. Emits `smarttags:progress {done, total}` events after each embedded photo.
//! 7. Checks the `abort` flag after each photo and stops cleanly when tripped.
//!
//! ## Parallelism
//!
//! ONNX inference (CLIP) is CPU-heavy. The pattern mirrors [`crate::sharpness_indexer`]
//! and [`crate::phash_indexer`]: resolve sequentially (the `Connection` is not `Sync`),
//! run preview-load + embed in **rayon** parallel, then collect results back to a
//! sequential loop for the SQLite writes. Workers are bounded by `parallelism` from the
//! [`crate::plugins::faces::indexer::IndexingPlan`] (shared `indexing.speed` setting).
//!
//! ## Resume safety
//!
//! The LEFT JOIN queue is re-derived on every run from the current `photos` table, so a
//! crash mid-run simply leaves the affected photo without a row in `smarttags__embeddings`
//! and it is re-picked up next time. An `INSERT OR REPLACE` upsert is used so a partial
//! failure never leaves a corrupted BLOB row.
//!
//! ## Testability without models
//!
//! The embed closure is injected rather than called directly. In tests it returns a
//! synthetic unit vector (no model download required).

use std::sync::atomic::{AtomicBool, Ordering};

use rusqlite::Connection;
use serde::Serialize;

pub use crate::plugins::smarttags::store;

// ── Progress / outcome types ──────────────────────────────────────────────────

/// Payload for the `smarttags:progress` Tauri event.
#[derive(Debug, Clone, Serialize)]
pub struct SmarttagsProgress {
    pub done: usize,
    pub total: usize,
}

/// How an index run ended.
#[derive(Debug, Clone, Default, Serialize)]
pub struct IndexOutcome {
    /// Photos actually embedded in this run.
    pub done: usize,
    /// Photos that were un-embedded when the run started.
    pub total: usize,
    /// Photos whose preview could not be loaded or decoded.
    pub failed: usize,
    /// Photos skipped because no reachable copy exists right now (offline/NAS).
    pub offline: usize,
    /// True when the run stopped early because the abort flag tripped.
    pub aborted: bool,
}

// ── Core indexing logic ───────────────────────────────────────────────────────

/// Run the Smart Tagging embedding-index job.
///
/// - `conn`            — a *secondary* catalog connection (no lock contention with the UI).
/// - `parallelism`     — number of concurrent embed workers (from the resolved
///                       [`crate::plugins::faces::indexer::IndexingPlan`]).
/// - `resolve_path_fn` — closure: `photo_id → Ok(Some(path))` when reachable, `Ok(None)`
///                       when offline, `Err` on failure. Calls `catalog.resolve_photo_path`.
/// - `preview_fn`      — closure: `&Path → Ok(JPEG bytes)`. Loads the 2048 px cached
///                       preview; must be `Send + Sync` (called from rayon workers).
/// - `embed_fn`        — closure: `&[u8] → Option<Vec<f32>>`. Returns the L2-normalized
///                       embedding or `None` on failure. Must be `Send + Sync`.
/// - `abort`           — shared abort flag; checked after each photo.
/// - `emit_progress`   — callback receiving [`SmarttagsProgress`] after each embedded photo.
///
/// The function is not `async` — it runs inside `tauri::async_runtime::spawn_blocking`.
#[allow(clippy::too_many_arguments)]
pub fn run_index<ResolveFn, PreviewFn, EmbedFn, EmitFn>(
    conn: &Connection,
    parallelism: usize,
    resolve_path_fn: ResolveFn,
    preview_fn: PreviewFn,
    embed_fn: EmbedFn,
    abort: &AtomicBool,
    mut emit_progress: EmitFn,
) -> Result<IndexOutcome, String>
where
    ResolveFn: Fn(i64) -> Result<Option<std::path::PathBuf>, String>,
    PreviewFn: Fn(&std::path::Path) -> Result<Vec<u8>, String> + Sync,
    EmbedFn: Fn(&[u8]) -> Option<Vec<f32>> + Sync,
    EmitFn: FnMut(SmarttagsProgress),
{
    use rayon::prelude::*;

    store::ensure_schema(conn).map_err(|e| e.to_string())?;

    let queue = store::load_unembedded(conn)?;
    let total = queue.len();
    let mut outcome = IndexOutcome { total, ..IndexOutcome::default() };
    let mut done = 0usize;

    // Process in chunks of `parallelism` (at least 1). For each chunk:
    //   Step 1 — sequential resolve pass (conn not Sync).
    //   Step 2 — parallel preview-load + embed pass (no conn access).
    //   Step 3 — sequential upsert pass.
    let parallelism = parallelism.max(1);

    for chunk in queue.chunks(parallelism) {
        if abort.load(Ordering::Relaxed) {
            outcome.aborted = true;
            break;
        }

        // ── Step 1: sequential resolve ────────────────────────────────────────
        let mut to_embed: Vec<(i64, std::path::PathBuf)> = Vec::with_capacity(chunk.len());
        for &photo_id in chunk {
            match resolve_path_fn(photo_id) {
                Ok(Some(p)) => to_embed.push((photo_id, p)),
                Ok(None) => {
                    eprintln!(
                        "smarttags_index: photo {photo_id} is offline — skipping for this run"
                    );
                    outcome.offline += 1;
                }
                Err(e) => {
                    eprintln!("smarttags_index: resolve failed for photo {photo_id}: {e}");
                    outcome.offline += 1;
                }
            }
        }

        if to_embed.is_empty() {
            continue;
        }

        // ── Step 2: parallel preview-load + embed ────────────────────────────
        enum EmbedResult {
            Embedded(Vec<f32>),
            PreviewError(String),
            EmbedError,
        }

        let results: Vec<(i64, EmbedResult)> = to_embed
            .par_iter()
            .map(|(photo_id, path)| {
                let jpeg = match preview_fn(path) {
                    Ok(b) => b,
                    Err(e) => return (*photo_id, EmbedResult::PreviewError(e)),
                };
                let emb = match embed_fn(&jpeg) {
                    Some(v) => v,
                    None => return (*photo_id, EmbedResult::EmbedError),
                };
                (*photo_id, EmbedResult::Embedded(emb))
            })
            .collect();

        // ── Step 3: sequential upsert ────────────────────────────────────────
        for (photo_id, result) in results {
            match result {
                EmbedResult::PreviewError(e) => {
                    eprintln!(
                        "smarttags_index: preview failed for photo {photo_id}: {e}"
                    );
                    outcome.failed += 1;
                    // Leave un-embedded; next run will retry.
                }
                EmbedResult::EmbedError => {
                    eprintln!(
                        "smarttags_index: embed failed for photo {photo_id} (model error or decode failed)"
                    );
                    outcome.failed += 1;
                }
                EmbedResult::Embedded(emb) => {
                    let blob = store::embedding_to_blob(&emb);
                    store::upsert_embedding(conn, photo_id, &blob)
                        .map_err(|e| e.to_string())?;
                    done += 1;
                    emit_progress(SmarttagsProgress { done, total });
                }
            }

            if abort.load(Ordering::Relaxed) {
                outcome.aborted = true;
                break;
            }
        }

        if outcome.aborted {
            break;
        }
    }

    outcome.done = done;
    Ok(outcome)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

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
        store::ensure_schema(&conn).unwrap();
        conn
    }

    fn insert_photo(conn: &Connection, id: i64) {
        conn.execute("INSERT INTO photos (id) VALUES (?1)", [id]).unwrap();
    }

    /// A synthetic unit embedding of dimension `dim`.
    fn unit_embedding(dim: usize, seed: usize) -> Vec<f32> {
        let v: Vec<f32> = (0..dim).map(|i| ((seed * dim + i) as f32).sin()).collect();
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter().map(|x| x / norm).collect()
    }

    fn unembedded_count(conn: &Connection) -> usize {
        // Photos not yet in smarttags__embeddings.
        conn.query_row(
            "SELECT COUNT(*) FROM photos p
              WHERE NOT EXISTS (
                  SELECT 1 FROM smarttags__embeddings e WHERE e.photo_id = p.id
              )",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0) as usize
    }

    // ── Basic full-run ────────────────────────────────────────────────────────

    #[test]
    fn run_index_embeds_all_photos() {
        let conn = mem_conn();
        for id in 1i64..=3 {
            insert_photo(&conn, id);
        }
        let abort = Arc::new(AtomicBool::new(false));
        let mut events = Vec::new();

        let outcome = run_index(
            &conn,
            2,
            |id| Ok(Some(std::path::PathBuf::from(format!("/fake/{id}.jpg")))),
            |_path| Ok(vec![0xFFu8, 0xD8, 0xFF, 0xE0]),
            |_jpeg| Some(unit_embedding(512, 1)),
            &abort,
            |p| events.push(p),
        )
        .unwrap();

        assert_eq!(outcome.total, 3);
        assert_eq!(outcome.done, 3);
        assert_eq!(outcome.offline, 0);
        assert_eq!(outcome.failed, 0);
        assert!(!outcome.aborted);
        assert_eq!(unembedded_count(&conn), 0, "all photos must be embedded");
        assert_eq!(events.len(), 3);
        assert_eq!(events.last().unwrap().done, 3);
    }

    // ── Skip already-embedded ─────────────────────────────────────────────────

    #[test]
    fn run_index_skips_already_embedded() {
        use std::sync::atomic::{AtomicUsize, Ordering as AO};
        let conn = mem_conn();
        for id in 1i64..=3 {
            insert_photo(&conn, id);
        }
        // Pre-embed photo 2 so it should be skipped.
        let blob = store::embedding_to_blob(&unit_embedding(512, 99));
        store::upsert_embedding(&conn, 2, &blob).unwrap();

        let abort = Arc::new(AtomicBool::new(false));
        let embed_calls = Arc::new(AtomicUsize::new(0));
        let embed_calls_c = embed_calls.clone();

        let outcome = run_index(
            &conn,
            2,
            |id| Ok(Some(std::path::PathBuf::from(format!("/fake/{id}.jpg")))),
            |_path| Ok(vec![0xFFu8, 0xD8, 0xFF, 0xE0]),
            move |_jpeg| {
                embed_calls_c.fetch_add(1, AO::Relaxed);
                Some(unit_embedding(512, 1))
            },
            &abort,
            |_| {},
        )
        .unwrap();

        // Only photos 1 and 3 were un-embedded.
        assert_eq!(outcome.total, 2, "only unembedded photos are queued");
        assert_eq!(outcome.done, 2);
        assert_eq!(
            embed_calls.load(AO::Relaxed),
            2,
            "already-embedded photo must be skipped"
        );
        assert_eq!(unembedded_count(&conn), 0);
    }

    // ── Abort safety ──────────────────────────────────────────────────────────

    #[test]
    fn run_index_aborts_cleanly() {
        use std::sync::atomic::{AtomicUsize, Ordering as AO};
        let conn = mem_conn();
        for id in 1i64..=6 {
            insert_photo(&conn, id);
        }
        let abort = Arc::new(AtomicBool::new(false));
        let abort_clone = abort.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_c = calls.clone();

        let outcome = run_index(
            &conn,
            2,
            |id| Ok(Some(std::path::PathBuf::from(format!("/fake/{id}.jpg")))),
            |_path| Ok(vec![0xFFu8, 0xD8, 0xFF, 0xE0]),
            move |_jpeg| {
                let n = calls_c.fetch_add(1, AO::Relaxed) + 1;
                if n >= 3 {
                    abort_clone.store(true, AO::Relaxed);
                }
                Some(unit_embedding(512, 1))
            },
            &abort,
            |_| {},
        )
        .unwrap();

        assert!(outcome.aborted, "aborted run must report aborted=true");
        let remaining = unembedded_count(&conn);
        assert!(remaining > 0, "some photos must remain unembedded after abort");
        assert!(remaining < 6, "some photos must have been embedded before abort");
    }

    // ── Resume after abort ────────────────────────────────────────────────────

    #[test]
    fn run_index_resumes_after_abort() {
        let conn = mem_conn();
        for id in 1i64..=4 {
            insert_photo(&conn, id);
        }
        // Trip the abort immediately — nothing gets embedded.
        let abort = Arc::new(AtomicBool::new(true));
        let outcome1 = run_index(
            &conn,
            2,
            |id| Ok(Some(std::path::PathBuf::from(format!("/fake/{id}.jpg")))),
            |_path| Ok(vec![0xFFu8, 0xD8, 0xFF, 0xE0]),
            |_jpeg| Some(unit_embedding(512, 1)),
            &abort,
            |_| {},
        )
        .unwrap();
        assert!(outcome1.aborted);
        assert_eq!(unembedded_count(&conn), 4);

        // Second run: no abort.
        let abort2 = Arc::new(AtomicBool::new(false));
        let outcome2 = run_index(
            &conn,
            2,
            |id| Ok(Some(std::path::PathBuf::from(format!("/fake/{id}.jpg")))),
            |_path| Ok(vec![0xFFu8, 0xD8, 0xFF, 0xE0]),
            |_jpeg| Some(unit_embedding(512, 1)),
            &abort2,
            |_| {},
        )
        .unwrap();
        assert_eq!(outcome2.done, 4);
        assert_eq!(unembedded_count(&conn), 0);
    }

    // ── Offline photos ────────────────────────────────────────────────────────

    #[test]
    fn run_index_offline_does_not_advance_done() {
        let conn = mem_conn();
        for id in 1i64..=2 {
            insert_photo(&conn, id);
        }
        let abort = Arc::new(AtomicBool::new(false));
        let mut events = Vec::new();

        let outcome = run_index(
            &conn,
            2,
            |id| {
                if id == 1 {
                    Ok(None) // photo 1 is offline
                } else {
                    Ok(Some(std::path::PathBuf::from(format!("/fake/{id}.jpg"))))
                }
            },
            |_path| Ok(vec![0xFFu8, 0xD8, 0xFF, 0xE0]),
            |_jpeg| Some(unit_embedding(512, 1)),
            &abort,
            |p| events.push(p),
        )
        .unwrap();

        assert_eq!(outcome.done, 1);
        assert_eq!(outcome.total, 2);
        assert_eq!(outcome.offline, 1);
        assert!(!outcome.aborted);
        // Photo 1 must remain unembedded.
        assert_eq!(unembedded_count(&conn), 1);
        // Only one progress event (for photo 2).
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].done, 1);
    }

    // ── Preview failures ──────────────────────────────────────────────────────

    #[test]
    fn run_index_counts_preview_failures() {
        let conn = mem_conn();
        for id in 1i64..=2 {
            insert_photo(&conn, id);
        }
        let abort = Arc::new(AtomicBool::new(false));

        let outcome = run_index(
            &conn,
            2,
            |id| Ok(Some(std::path::PathBuf::from(format!("/fake/{id}.jpg")))),
            |path| {
                if path.ends_with("1.jpg") {
                    Err("unreadable".to_string())
                } else {
                    Ok(vec![0xFFu8, 0xD8, 0xFF, 0xE0])
                }
            },
            |_jpeg| Some(unit_embedding(512, 1)),
            &abort,
            |_| {},
        )
        .unwrap();

        assert_eq!(outcome.done, 1);
        assert_eq!(outcome.failed, 1);
        assert_eq!(unembedded_count(&conn), 1, "failed photo must remain unembedded");
    }

    // ── Embed failures ────────────────────────────────────────────────────────

    #[test]
    fn run_index_counts_embed_failures() {
        let conn = mem_conn();
        for id in 1i64..=2 {
            insert_photo(&conn, id);
        }
        let abort = Arc::new(AtomicBool::new(false));

        let outcome = run_index(
            &conn,
            2,
            |id| Ok(Some(std::path::PathBuf::from(format!("/fake/{id}.jpg")))),
            |_path| Ok(vec![0xFFu8, 0xD8, 0xFF, 0xE0]),
            |_jpeg| None, // embed always fails
            &abort,
            |_| {},
        )
        .unwrap();

        assert_eq!(outcome.done, 0);
        assert_eq!(outcome.failed, 2);
        assert_eq!(unembedded_count(&conn), 2);
    }

    // ── Blob round-trip ───────────────────────────────────────────────────────

    /// The stored embedding blob decodes back losslessly to the original f32 values.
    #[test]
    fn run_index_stores_embedding_blob_roundtrip() {
        let conn = mem_conn();
        insert_photo(&conn, 42);
        let abort = Arc::new(AtomicBool::new(false));

        // Inject a known embedding.
        let known = unit_embedding(512, 7);
        let known_clone = known.clone();

        let _ = run_index(
            &conn,
            1,
            |_| Ok(Some(std::path::PathBuf::from("/fake/42.jpg"))),
            |_path| Ok(vec![0xFFu8, 0xD8, 0xFF, 0xE0]),
            move |_jpeg| Some(known_clone.clone()),
            &abort,
            |_| {},
        )
        .unwrap();

        // Read the blob back from the table.
        let blob: Vec<u8> = conn
            .query_row(
                "SELECT vector FROM smarttags__embeddings WHERE photo_id = 42",
                [],
                |r| r.get(0),
            )
            .unwrap();

        let recovered = store::blob_to_embedding(&blob).unwrap();
        assert_eq!(recovered.len(), known.len());

        let norm: f32 = recovered.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "recovered embedding must be unit, norm={norm}");

        for (a, b) in known.iter().zip(recovered.iter()) {
            assert!(
                (a - b).abs() < 1e-7,
                "blob round-trip mismatch: {a} vs {b}"
            );
        }
    }

    /// Direct codec round-trip (embedding_to_blob / blob_to_embedding).
    #[test]
    fn embedding_codec_roundtrip() {
        let orig: Vec<f32> = (0..512).map(|i| i as f32 * 0.001).collect();
        let blob = store::embedding_to_blob(&orig);
        assert_eq!(blob.len(), 512 * 4, "BLOB must be 2048 bytes");
        let back = store::blob_to_embedding(&blob).unwrap();
        assert_eq!(back.len(), orig.len());
        for (a, b) in orig.iter().zip(back.iter()) {
            assert!((a - b).abs() < 1e-7, "round-trip mismatch: {a} vs {b}");
        }
    }

    /// A blob whose length is not a multiple of 4 is rejected cleanly.
    #[test]
    fn blob_to_embedding_rejects_odd_length() {
        let bad = vec![0u8; 7];
        assert!(store::blob_to_embedding(&bad).is_err());
    }

    /// A unit vector survives the codec round-trip and remains unit.
    #[test]
    fn blob_roundtrip_unit_vector_stays_unit() {
        let v = unit_embedding(512, 3);
        let blob = store::embedding_to_blob(&v);
        let back = store::blob_to_embedding(&blob).unwrap();
        let norm: f32 = back.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "round-tripped unit vector must stay unit, norm={norm}");
    }
}
