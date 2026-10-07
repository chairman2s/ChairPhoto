//! Resumable background perceptual-hash indexing job (H15a).
//!
//! [`run_index`] is the single entry point, mirroring [`crate::sharpness_indexer`]'s
//! Phase-B pattern:
//!
//! 1. Query all photos where `phash IS NULL` (unhashed) — the implicit resume queue.
//! 2. Resolve each photo to a reachable copy via the catalog resolver.
//! 3. Load the cached **thumbnail** bytes (512px) — a dHash crushes the image to a 9×8
//!    grid, so the small thumbnail is plenty and is the cheapest cached decode. (Unlike
//!    sharpness, which needs the 2048px preview to see micro-blur, the perceptual hash is
//!    resolution-invariant — see `phash::tests::resolution_invariant`.)
//! 4. Decode + compute the 64-bit dHash with [`crate::phash::dhash`].
//! 5. Write it to `photos.phash` via [`write_phash`].
//! 6. Emit `phash:progress {done, total}` after each hashed photo.
//! 7. Honour the abort flag between chunks and after each write — a tripped run leaves the
//!    rest of the queue as `phash IS NULL`, so the next run resumes automatically.
//!
//! ## Hash-on-import
//!
//! Newly imported photos are hashed for free via the I7b analyzer hook (registered in
//! `lib.rs`): the freshly decoded preview is handed to [`hash_and_store`], so the hash
//! rides the decode the thumbnail pipeline already paid for. This background job only
//! backfills photos imported before the hook was deployed.
//!
//! ## Parallelism
//!
//! Thumbnail load + decode + hash is CPU+I/O bound. Same shape as the sharpness indexer:
//! resolve sequentially (the `Connection` is not `Sync`), decode+hash in parallel with
//! rayon, then write results back on the single connection.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use rusqlite::Connection;
use serde::Serialize;

/// Progress event payload for `phash:progress`.
#[derive(Debug, Clone, Serialize)]
pub struct PhashProgress {
    pub done: usize,
    pub total: usize,
}

/// How an index run ended.
#[derive(Debug, Clone, Default, Serialize)]
pub struct IndexOutcome {
    /// Photos actually hashed in this run.
    pub done: usize,
    /// Photos that were unhashed when the run started (total queue length).
    pub total: usize,
    /// Photos whose thumbnail could not be generated/decoded.
    pub failed: usize,
    /// Photos skipped because no reachable copy exists right now (offline/NAS).
    pub offline: usize,
    /// True when the run stopped early because the abort flag tripped.
    pub aborted: bool,
}

/// Run the perceptual-hash indexing job.
///
/// - `conn`          — a *secondary* catalog connection (no lock contention with the UI).
/// - `resolve_path_fn` — closure that, given a photo_id, returns `Ok(Some(path))` when a
///                       reachable copy is found, `Ok(None)` when offline, `Err` on failure.
///                       In production this calls `catalog.resolve_photo_path(id)`.
/// - `thumb_fn`      — closure that, given `&Path`, returns cached JPEG thumbnail bytes.
///                     In production this calls `thumbnails::thumbnail_bytes`.
/// - `hash_fn`       — closure that, given JPEG bytes, returns the 64-bit dHash, or `None`
///                     when the bytes cannot be decoded. In production this is [`hash_jpeg`].
///                     Must be `Send + Sync` (called from rayon workers).
/// - `abort`         — shared abort flag; checked between chunks and after each write.
/// - `emit_progress` — callback receiving [`PhashProgress`] after each hashed photo.
pub fn run_index<ResolveFn, ThumbFn, HashFn, EmitFn>(
    conn: &Connection,
    resolve_path_fn: ResolveFn,
    thumb_fn: &ThumbFn,
    hash_fn: &HashFn,
    abort: &AtomicBool,
    mut emit_progress: EmitFn,
) -> Result<IndexOutcome, String>
where
    ResolveFn: Fn(i64) -> Result<Option<std::path::PathBuf>, String>,
    ThumbFn: Fn(&Path) -> Result<Vec<u8>, String> + Sync,
    HashFn: Fn(&[u8]) -> Option<u64> + Sync,
    EmitFn: FnMut(PhashProgress),
{
    use rayon::prelude::*;

    let queue = load_unhashed(conn)?;
    let total = queue.len();
    let mut outcome = IndexOutcome { total, ..IndexOutcome::default() };
    let mut done = 0usize;

    const CHUNK: usize = 128;
    for chunk in queue.chunks(CHUNK) {
        if abort.load(Ordering::Relaxed) {
            outcome.aborted = true;
            break;
        }

        // ── Step 1: sequential resolve pass ──────────────────────────────────────
        let mut to_hash: Vec<(i64, std::path::PathBuf)> = Vec::with_capacity(chunk.len());
        for &photo_id in chunk {
            match resolve_path_fn(photo_id) {
                Ok(Some(p)) => to_hash.push((photo_id, p)),
                Ok(None) => {
                    eprintln!("phash_index: photo {photo_id} is offline — skipping for this run");
                    outcome.offline += 1;
                }
                Err(e) => {
                    eprintln!("phash_index: resolve failed for photo {photo_id}: {e}");
                    outcome.offline += 1;
                }
            }
        }

        if to_hash.is_empty() {
            continue;
        }

        // ── Step 2: parallel thumbnail + hash pass ───────────────────────────────
        enum HashResult {
            Hashed(u64),
            ThumbError(String),
        }

        let results: Vec<(i64, HashResult)> = to_hash
            .par_iter()
            .map(|(photo_id, path)| {
                let jpeg = match thumb_fn(path) {
                    Ok(b) => b,
                    Err(e) => return (*photo_id, HashResult::ThumbError(e)),
                };
                let result = match hash_fn(&jpeg) {
                    Some(h) => HashResult::Hashed(h),
                    None => HashResult::ThumbError("image decode failed".to_string()),
                };
                (*photo_id, result)
            })
            .collect();

        // ── Step 3: sequential write pass ────────────────────────────────────────
        for (photo_id, result) in results {
            match result {
                HashResult::ThumbError(e) => {
                    eprintln!("phash_index: thumbnail/decode failed for photo {photo_id}: {e}");
                    outcome.failed += 1;
                    // Leave phash IS NULL so the next run retries.
                }
                HashResult::Hashed(h) => {
                    write_phash(conn, photo_id, h)?;
                    done += 1;
                    emit_progress(PhashProgress { done, total });
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

/// Decode JPEG bytes and compute the 64-bit dHash. Returns `None` when the image cannot
/// be decoded. Used by the indexer; the analyzer hook uses [`hash_and_store`] directly on
/// the already-decoded image (no re-decode).
pub fn hash_jpeg(jpeg: &[u8]) -> Option<u64> {
    let img = image::load_from_memory(jpeg).ok()?;
    Some(crate::phash::dhash(&img))
}

/// Compute the dHash of an already-decoded image and store it for `photo_id` **only if
/// the photo is not yet hashed** (`phash IS NULL` guard). Used by the I7b analyzer hook,
/// which holds the freshly decoded preview — so the hash rides that decode instead of
/// re-reading the file. The guard mirrors the sharpness hook: never overwrite a hash the
/// batch indexer may already have written, and never insert for a path with no photo row.
pub fn hash_and_store(conn: &Connection, photo_id: i64, img: &image::DynamicImage) {
    let h = crate::phash::dhash(img);
    let _ = conn.execute(
        "UPDATE photos SET phash = ?1 WHERE id = ?2 AND phash IS NULL",
        rusqlite::params![h as i64, photo_id],
    );
}

/// Load all photo IDs where `phash IS NULL`, ordered by `id` for stable resumption.
fn load_unhashed(conn: &Connection) -> Result<Vec<i64>, String> {
    let mut stmt = conn
        .prepare("SELECT id FROM photos WHERE phash IS NULL ORDER BY id ASC")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| r.get::<_, i64>(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(rows)
}

/// Write a perceptual hash to the catalog for one photo. Stores the `u64` bit pattern as
/// INTEGER (bit-preserving reinterpret; the hash is only ever Hamming-compared). Caller
/// holds the secondary catalog connection; no mutex around this call.
pub fn write_phash(conn: &Connection, photo_id: i64, phash: u64) -> Result<(), String> {
    conn.execute(
        "UPDATE photos SET phash = ?1 WHERE id = ?2",
        rusqlite::params![phash as i64, photo_id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
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
            "CREATE TABLE IF NOT EXISTS photos (
                 id    INTEGER PRIMARY KEY,
                 phash INTEGER
             );",
        )
        .unwrap();
        conn
    }

    fn insert_photo(conn: &Connection, id: i64) {
        conn.execute("INSERT INTO photos (id) VALUES (?1)", rusqlite::params![id])
            .unwrap();
    }

    fn get_phash(conn: &Connection, id: i64) -> Option<u64> {
        conn.query_row("SELECT phash FROM photos WHERE id = ?1", [id], |r| {
            r.get::<_, Option<i64>>(0)
        })
        .ok()
        .flatten()
        .map(|v| v as u64)
    }

    fn unhashed_count(conn: &Connection) -> usize {
        conn.query_row("SELECT COUNT(*) FROM photos WHERE phash IS NULL", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap_or(0) as usize
    }

    // A dummy hash_fn: derive a value from the byte length so different inputs differ.
    fn dummy_hash(jpeg: &[u8]) -> Option<u64> {
        Some(0xABCD_0000_0000_0000 | jpeg.len() as u64)
    }

    #[test]
    fn run_index_hashes_all_photos() {
        let conn = mem_conn();
        for id in 1i64..=3 {
            insert_photo(&conn, id);
        }
        let abort = Arc::new(AtomicBool::new(false));
        let mut events = Vec::new();

        let outcome = run_index(
            &conn,
            |id| Ok(Some(std::path::PathBuf::from(format!("/fake/{id}.jpg")))),
            &|_p| Ok(vec![0xFFu8, 0xD8, 0xFF]),
            &dummy_hash,
            &abort,
            |p| events.push(p),
        )
        .unwrap();

        assert_eq!(outcome.total, 3);
        assert_eq!(outcome.done, 3);
        assert_eq!(outcome.offline, 0);
        assert_eq!(outcome.failed, 0);
        assert!(!outcome.aborted);
        assert_eq!(unhashed_count(&conn), 0, "all photos must be hashed");
        assert_eq!(events.len(), 3);
        assert_eq!(events.last().unwrap().done, 3);
        for id in 1i64..=3 {
            assert!(get_phash(&conn, id).is_some(), "phash must be written");
        }
    }

    #[test]
    fn run_index_skips_already_hashed() {
        use std::sync::atomic::{AtomicUsize, Ordering as AO};
        let conn = mem_conn();
        for id in 1i64..=3 {
            insert_photo(&conn, id);
        }
        // Pre-hash photo 2.
        write_phash(&conn, 2, 0x1234).unwrap();

        let abort = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_c = calls.clone();

        let outcome = run_index(
            &conn,
            |id| Ok(Some(std::path::PathBuf::from(format!("/fake/{id}.jpg")))),
            &|_p| Ok(vec![0xFFu8, 0xD8, 0xFF]),
            &move |j: &[u8]| {
                calls_c.fetch_add(1, AO::Relaxed);
                dummy_hash(j)
            },
            &abort,
            |_| {},
        )
        .unwrap();

        assert_eq!(outcome.total, 2, "only unhashed photos are queued");
        assert_eq!(outcome.done, 2);
        assert_eq!(calls.load(AO::Relaxed), 2, "already-hashed photo is skipped");
        assert_eq!(get_phash(&conn, 2), Some(0x1234), "existing hash untouched");
    }

    #[test]
    fn run_index_resumes_after_abort() {
        let conn = mem_conn();
        for id in 1i64..=4 {
            insert_photo(&conn, id);
        }
        let abort = Arc::new(AtomicBool::new(true)); // tripped immediately

        let outcome1 = run_index(
            &conn,
            |id| Ok(Some(std::path::PathBuf::from(format!("/fake/{id}.jpg")))),
            &|_p| Ok(vec![0xFFu8, 0xD8, 0xFF]),
            &dummy_hash,
            &abort,
            |_| {},
        )
        .unwrap();
        assert!(outcome1.aborted);
        assert_eq!(unhashed_count(&conn), 4, "abort before first chunk: nothing hashed");

        let abort2 = Arc::new(AtomicBool::new(false));
        let outcome2 = run_index(
            &conn,
            |id| Ok(Some(std::path::PathBuf::from(format!("/fake/{id}.jpg")))),
            &|_p| Ok(vec![0xFFu8, 0xD8, 0xFF]),
            &dummy_hash,
            &abort2,
            |_| {},
        )
        .unwrap();
        assert_eq!(outcome2.done, 4);
        assert_eq!(unhashed_count(&conn), 0);
    }

    #[test]
    fn run_index_counts_thumb_failures() {
        let conn = mem_conn();
        for id in 1i64..=2 {
            insert_photo(&conn, id);
        }
        let abort = Arc::new(AtomicBool::new(false));

        let outcome = run_index(
            &conn,
            |id| Ok(Some(std::path::PathBuf::from(format!("/fake/{id}.jpg")))),
            &|path| {
                if path.ends_with("1.jpg") {
                    Err("unreadable".to_string())
                } else {
                    Ok(vec![0xFFu8, 0xD8, 0xFF])
                }
            },
            &dummy_hash,
            &abort,
            |_| {},
        )
        .unwrap();

        assert_eq!(outcome.done, 1);
        assert_eq!(outcome.failed, 1);
        assert_eq!(unhashed_count(&conn), 1, "failed photo remains unhashed");
    }

    #[test]
    fn run_index_offline_does_not_advance_done() {
        let conn = mem_conn();
        for id in 1i64..=2 {
            insert_photo(&conn, id);
        }
        let abort = Arc::new(AtomicBool::new(false));

        let outcome = run_index(
            &conn,
            |id| {
                if id == 1 {
                    Ok(None)
                } else {
                    Ok(Some(std::path::PathBuf::from(format!("/fake/{id}.jpg"))))
                }
            },
            &|_p| Ok(vec![0xFFu8, 0xD8, 0xFF]),
            &dummy_hash,
            &abort,
            |_| {},
        )
        .unwrap();

        assert_eq!(outcome.done, 1);
        assert_eq!(outcome.total, 2);
        assert_eq!(outcome.offline, 1);
        assert_eq!(unhashed_count(&conn), 1);
        assert!(get_phash(&conn, 2).is_some());
    }

    #[test]
    fn write_phash_preserves_high_bit() {
        // A hash with the top bit set must round-trip through the i64 store unchanged.
        let conn = mem_conn();
        insert_photo(&conn, 7);
        let h = 0xFEDC_BA98_7654_3210u64; // top bit set
        write_phash(&conn, 7, h).unwrap();
        assert_eq!(get_phash(&conn, 7), Some(h));
    }

    #[test]
    fn load_unhashed_excludes_hashed() {
        let conn = mem_conn();
        for id in 1i64..=5 {
            insert_photo(&conn, id);
        }
        write_phash(&conn, 2, 1).unwrap();
        write_phash(&conn, 4, 2).unwrap();
        assert_eq!(load_unhashed(&conn).unwrap(), vec![1, 3, 5]);
    }
}
