//! Resumable background face-indexing job (H13b).
//!
//! [`run_index`] is the single entry point. It:
//!
//! 1. Calls [`store::ensure_schema`] on the secondary connection.
//! 2. Calls [`store::populate_queue`] to enqueue every un-indexed photo.
//! 3. Drains the queue with bounded parallelism via rayon, resolving photo paths via
//!    the catalog resolver (never `photos.path`), loading the 2048 px preview, and
//!    calling the detection/embedding closure.
//! 4. Emits `faces:progress {done, total}` Tauri events after each photo that was
//!    *actually processed* (indexed or already-indexed sentinel). Offline and error
//!    cases do NOT advance `done` — they are left in the queue for retry.
//! 5. Checks the `abort` flag after each photo and stops cleanly when tripped.
//!
//! ## Bounded parallelism
//!
//! Detection is CPU-heavy (ONNX inference). Workers are bounded to avoid saturating all
//! cores. The detect closure is called from rayon worker threads in parallel; the SQLite
//! writes (which require `&Connection`, not `Send`) are serialised on the caller's thread.
//! Concretely: rayon's `par_iter` with a bounded pool (see `DETECT_PARALLELISM`) runs
//! the resolve + preview + detect steps, then results are collected back to a sequential
//! loop for the DB writes.
//!
//! ## Testability without models
//!
//! Detection and embedding are injected as a closure (`impl Fn`) rather than called
//! directly. In production the closure calls the engine; in tests it can inject synthetic
//! results (fake faces with known embeddings). This keeps the test suite green on machines
//! that have no ONNX models.
//!
//! The module itself carries no `#[cfg(feature = "faces")]` on the function signatures so
//! unit tests compile without the feature. The *commands* that wire in the real engine are
//! gated, but the queue/store/progress logic is always testable.

use std::sync::atomic::{AtomicBool, Ordering};

use rusqlite::Connection;
use serde::Serialize;

use super::store;

// ── Source constant ────────────────────────────────────────────────────────────

/// The `source` value written for faces discovered by the background indexer.
/// H13c reads this to distinguish how a person assignment was made (seed / match /
/// manual / xmp / **detect** — freshly detected, not yet assigned to a person).
pub const SOURCE_DETECT: &str = "detect";

/// Setting key forcing CPU face inference even in a `faces-cuda` (GPU-capable) build. `"true"`
/// (case-insensitive) = force CPU; anything else / unset = let the engine try GPU first and
/// fall back on its own. Inert in a non-CUDA build (there is no GPU EP to skip). Read by
/// [`load_force_cpu`] into [`super::engine::configure_force_cpu`] before the first pool build.
pub const FORCE_CPU_SETTING: &str = "faces.force_cpu";

/// Whether the user has forced CPU inference (`faces.force_cpu = "true"`). Missing/blank/other
/// values → `false` (let the engine choose GPU-or-fallback). Parsing is here so both the
/// command layer and tests read the setting the same way.
pub fn load_force_cpu(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT value FROM settings WHERE key = ?1",
        [FORCE_CPU_SETTING],
        |r| r.get::<_, String>(0),
    )
    .ok()
    .map(|s| s.trim().eq_ignore_ascii_case("true"))
    .unwrap_or(false)
}

// ── Progress event ─────────────────────────────────────────────────────────────

/// Payload for the `faces:progress` Tauri event.
#[derive(Debug, Clone, Serialize)]
pub struct FacesProgress {
    pub done: usize,
    pub total: usize,
}

/// How an index run ended — the honest breakdown the UI needs to say *why* `done < total`
/// instead of guessing ("cancelled or some photos offline"). All skipped photos stay in
/// the queue for the next run.
#[derive(Debug, Clone, Default, Serialize)]
pub struct IndexOutcome {
    /// Photos actually processed (indexed, or already-indexed sentinel cleaned up).
    pub done: usize,
    /// Photos that were in the queue when the run started.
    pub total: usize,
    /// Photos skipped because no reachable copy exists right now (NAS offline, …).
    pub offline: usize,
    /// Photos whose preview could not be generated (unreadable/undecodable file).
    pub failed: usize,
    /// True when the run stopped early because the abort flag tripped (user cancel,
    /// a newer run superseding this one, or a catalog switch).
    pub aborted: bool,
}

// ── Detected face (engine output, injected via closure) ──────────────────────

/// One face as returned by the detection/embedding closure.
pub struct IndexedFace {
    /// `(x, y, w, h)` normalized 0–1.
    pub bbox: (f32, f32, f32, f32),
    /// 5 landmarks `(x, y)` normalized 0–1.
    pub landmarks: [(f32, f32); 5],
    /// Detection confidence.
    pub confidence: f32,
    /// 512-d L2-normalized embedding, `None` if embedding failed.
    pub embedding: Option<Vec<f32>>,
}

// ── Core indexing logic ────────────────────────────────────────────────────────

/// Run the face-indexing job.
///
/// - `conn`            — a *secondary* catalog connection (no lock contention with the UI).
/// - `resolve_path_fn` — closure that, given a photo_id, returns `Ok(Some(path))` when a
///                       reachable copy is found, `Ok(None)` when offline, `Err` on failure.
///                       In production this calls `catalog.resolve_photo_path(id)`.
/// - `preview_fn`      — closure that, given a `&std::path::Path`, returns JPEG bytes. In
///                       production this calls `thumbnails::preview_bytes`.
/// - `detect_fn`       — closure that, given raw JPEG bytes, returns a `Vec<IndexedFace>`. In
///                       production this calls the engine; in tests it returns synthetic data.
///                       Must be `Send + Sync` because it is called from rayon worker threads.
/// - `abort`           — shared abort flag; checked after each processed photo.
/// - `emit_progress`   — callback that receives a `FacesProgress` after each photo that was
///                       *actually processed* (indexed or already-indexed sentinel). Offline
///                       and error cases do **not** advance `done` — they stay in the queue
///                       for the next run.
///
/// Returns an [`IndexOutcome`] — processed/skipped counts and whether the run aborted.
///
/// The function is not `async` because it runs inside `tauri::async_runtime::spawn_blocking`.
///
/// ## Parallelism
///
/// Resolve + preview + detect are the expensive steps. They are batched in windows of
/// `parallelism` (from the resolved [`IndexingPlan`]) and run in parallel via rayon. Each
/// batch's results are then collected back to the caller's thread for the SQLite writes
/// (which require `&Connection`, not `Send`). This keeps inference parallelism bounded while
/// keeping DB writes sequential and safe.
pub fn run_index<ResolveFn, PreviewFn, DetectFn, EmitFn>(
    conn: &Connection,
    parallelism: usize,
    resolve_path_fn: ResolveFn,
    preview_fn: PreviewFn,
    detect_fn: DetectFn,
    abort: &AtomicBool,
    emit_progress: EmitFn,
) -> Result<IndexOutcome, String>
where
    ResolveFn: Fn(i64) -> Result<Option<std::path::PathBuf>, String>,
    PreviewFn: Fn(&std::path::Path) -> Result<Vec<u8>, String> + Sync,
    DetectFn: Fn(&[u8]) -> Vec<IndexedFace> + Sync,
    EmitFn: FnMut(FacesProgress),
{
    // Delegate to the full form with a no-op post-index hook.
    run_index_with_hook(
        conn,
        parallelism,
        resolve_path_fn,
        preview_fn,
        detect_fn,
        abort,
        emit_progress,
        |_conn, _photo_id, _path| {},
    )
}

/// Like [`run_index`] but with an `on_indexed` hook fired **after** each photo's detected faces
/// are written and the photo is dequeued. The hook runs on the sequential DB-write thread with
/// `(&Connection, photo_id, &path)`, so it may safely touch the connection — used by the MWG
/// region **import** pass (H13f), which reads the photo's existing sidecar regions and confirms
/// matching detections. Errors inside the hook are the hook's own concern (it should log, not
/// panic); the indexing loop is unaffected.
#[allow(clippy::too_many_arguments)]
pub fn run_index_with_hook<ResolveFn, PreviewFn, DetectFn, EmitFn, HookFn>(
    conn: &Connection,
    parallelism: usize,
    resolve_path_fn: ResolveFn,
    preview_fn: PreviewFn,
    detect_fn: DetectFn,
    abort: &AtomicBool,
    mut emit_progress: EmitFn,
    mut on_indexed: HookFn,
) -> Result<IndexOutcome, String>
where
    // ResolveFn is only called from the sequential Step 1 pass on the calling thread —
    // no Sync requirement.
    // PreviewFn requires Sync because it is called from rayon worker threads in the
    // Step 2 parallel detect pass (alongside DetectFn).
    ResolveFn: Fn(i64) -> Result<Option<std::path::PathBuf>, String>,
    PreviewFn: Fn(&std::path::Path) -> Result<Vec<u8>, String> + Sync,
    // DetectFn is called from rayon worker threads (Step 2 parallel pass) — must be Sync.
    DetectFn: Fn(&[u8]) -> Vec<IndexedFace> + Sync,
    EmitFn: FnMut(FacesProgress),
    // HookFn runs on the sequential DB-write thread after each photo is indexed.
    HookFn: FnMut(&Connection, i64, &std::path::Path),
{
    use rayon::prelude::*;

    let now = unix_now();

    store::ensure_schema(conn).map_err(|e| e.to_string())?;
    store::populate_queue(conn, now).map_err(|e| e.to_string())?;

    let queue = store::load_queue(conn).map_err(|e| e.to_string())?;
    let mut outcome = IndexOutcome {
        total: queue.len(),
        ..IndexOutcome::default()
    };
    let total = outcome.total;
    let mut done = 0usize;

    // Process the queue in chunks of `parallelism` (from the resolved IndexingPlan; at
    // least one so an empty/zero plan still makes progress).
    let parallelism = parallelism.max(1);
    //
    // For each chunk:
    //  1. **Sequential DB pass** (this thread): filter out already-indexed photos and
    //     resolve each photo to a path. `rusqlite::Connection` is not `Sync`, so DB
    //     calls must stay on the calling thread.
    //  2. **Parallel detect pass** (rayon): for each resolved path, load the preview and
    //     run the CPU-heavy inference. The closures are pure I/O + compute — no conn
    //     reference crosses thread boundaries.
    //  3. **Sequential DB write pass** (this thread): write face rows + sentinel + dequeue.
    for chunk in queue.chunks(parallelism) {
        if abort.load(Ordering::Relaxed) {
            outcome.aborted = true;
            break;
        }

        // ── Step 1: sequential DB pass — filter + resolve ──────────────────────
        // Each element is `(photo_id, Option<PathBuf>)`.
        // `None` means the photo was already indexed or is offline; we handle those cases here.
        let mut to_detect: Vec<(i64, std::path::PathBuf)> = Vec::with_capacity(chunk.len());

        for &photo_id in chunk {
            // Already-indexed check (sentinel OR face rows from a prior run).
            match store::photo_is_indexed(conn, photo_id) {
                Ok(true) => {
                    // Sentinel exists but photo is still in queue (crash/abort between
                    // mark_indexed and dequeue in a prior run). Clean it up now.
                    store::dequeue(conn, photo_id).map_err(|e| e.to_string())?;
                    done += 1;
                    emit_progress(FacesProgress { done, total });
                    continue;
                }
                Err(e) => {
                    eprintln!("faces_index: photo_is_indexed failed for {photo_id}: {e}");
                    // Treat as offline — leave in queue.
                    outcome.offline += 1;
                    continue;
                }
                Ok(false) => {}
            }

            // Resolve path.
            match resolve_path_fn(photo_id) {
                Ok(Some(p)) => to_detect.push((photo_id, p)),
                Ok(None) => {
                    // Photo is offline; leave in queue for next run. Do NOT advance done.
                    eprintln!(
                        "faces_index: photo {photo_id} is offline — skipping for this run"
                    );
                    outcome.offline += 1;
                }
                Err(e) => {
                    eprintln!("faces_index: resolve failed for {photo_id}: {e}");
                    // Treat as offline.
                    outcome.offline += 1;
                }
            }
        }

        if to_detect.is_empty() {
            continue;
        }

        // ── Step 2: parallel detect pass — no conn access ──────────────────────
        // Each element is `(photo_id, path, DetectOutcome)`.
        let detected: Vec<(i64, std::path::PathBuf, DetectOutcome)> = to_detect
            .par_iter()
            .map(|(photo_id, path)| {
                // Load 2048 px preview.
                let jpeg_bytes = match preview_fn(path) {
                    Ok(b) => b,
                    Err(e) => return (*photo_id, path.clone(), DetectOutcome::PreviewError(e)),
                };
                // Detect + embed.
                let faces = detect_fn(&jpeg_bytes);
                (*photo_id, path.clone(), DetectOutcome::Detected(faces))
            })
            .collect();

        // ── Step 3: sequential DB write pass ──────────────────────────────────
        for (photo_id, path, detect_result) in detected {
            match detect_result {
                DetectOutcome::PreviewError(e) => {
                    // Leave in queue for retry. Do NOT advance done.
                    eprintln!("faces_index: preview failed for photo {photo_id}: {e}");
                    outcome.failed += 1;
                }
                DetectOutcome::Detected(faces) => {
                    let face_count = faces.len();
                    // Write face rows in a single savepoint transaction.
                    write_faces(conn, photo_id, &faces, now)?;
                    // Mark indexed BEFORE dequeue: if we crash between these two the
                    // next run will find the sentinel and dequeue without re-detecting.
                    store::mark_indexed(conn, photo_id, face_count, now)
                        .map_err(|e| e.to_string())?;
                    store::dequeue(conn, photo_id).map_err(|e| e.to_string())?;
                    // Post-index hook (H13f MWG region import): now that the detected faces
                    // exist, let the caller import any existing sidecar regions for this photo.
                    on_indexed(conn, photo_id, &path);
                    done += 1;
                    emit_progress(FacesProgress { done, total });
                }
            }

            if abort.load(Ordering::Relaxed) {
                outcome.aborted = true;
                break;
            }
        }
    }

    outcome.done = done;
    Ok(outcome)
}

/// Outcome of the parallel preview-load + detect step for one photo.
enum DetectOutcome {
    /// Loading the 2048 px preview failed; leave in queue for retry.
    PreviewError(String),
    /// Detection ran successfully (may have found zero faces).
    Detected(Vec<IndexedFace>),
}

/// Write a slice of [`IndexedFace`]s for one photo into `faces__faces`.
///
/// All inserts are wrapped in a single savepoint so either all face rows for this photo
/// are committed together or none are. Without the transaction, a mid-photo failure (e.g.
/// `SQLITE_FULL`) would leave a partial set of face rows with no recovery path — the
/// photo would be dequeued on resume (because `photo_is_indexed` returns true) but would
/// hold an incomplete embedding set.
fn write_faces(
    conn: &Connection,
    photo_id: i64,
    faces: &[IndexedFace],
    now: i64,
) -> Result<(), String> {
    if faces.is_empty() {
        return Ok(());
    }
    conn.execute_batch("SAVEPOINT write_faces").map_err(|e| e.to_string())?;
    let result = (|| {
        for face in faces {
            let bbox_json = format!(
                "[{},{},{},{}]",
                face.bbox.0, face.bbox.1, face.bbox.2, face.bbox.3
            );
            let lm_json = {
                let parts: Vec<String> = face
                    .landmarks
                    .iter()
                    .map(|(x, y)| format!("[{x},{y}]"))
                    .collect();
                format!("[{}]", parts.join(","))
            };
            let embedding_blob: Option<Vec<u8>> =
                face.embedding.as_ref().map(|e| store::embedding_to_blob(e));
            store::insert_face(
                conn,
                photo_id,
                &bbox_json,
                &lm_json,
                face.confidence,
                embedding_blob.as_deref(),
                SOURCE_DETECT,
                now,
            )
            .map_err(|e| e.to_string())?;
        }
        Ok::<(), String>(())
    })();
    if result.is_ok() {
        conn.execute_batch("RELEASE write_faces").map_err(|e| e.to_string())?;
    } else {
        let _ = conn.execute_batch("ROLLBACK TO write_faces");
        let _ = conn.execute_batch("RELEASE write_faces");
    }
    result
}

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
    use std::sync::Arc;
    use store::{blob_to_embedding, embedding_to_blob, queue_len, photo_is_indexed};

    fn mem_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
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
        store::ensure_schema(&conn).unwrap();
        conn
    }

    fn insert_photo(conn: &Connection, id: i64) {
        conn.execute("INSERT INTO photos (id) VALUES (?1)", [id]).unwrap();
    }

    fn synthetic_faces(count: usize) -> Vec<IndexedFace> {
        (0..count)
            .map(|i| {
                let e: Vec<f32> = (0..512).map(|j| ((i * 512 + j) as f32).sin()).collect();
                // L2-normalize.
                let norm: f32 = e.iter().map(|x| x * x).sum::<f32>().sqrt();
                let emb = e.iter().map(|x| x / norm).collect::<Vec<_>>();
                IndexedFace {
                    bbox: (0.1, 0.1, 0.2, 0.3),
                    landmarks: [(0.2, 0.2), (0.3, 0.2), (0.25, 0.28), (0.22, 0.34), (0.28, 0.34)],
                    confidence: 0.9,
                    embedding: Some(emb),
                }
            })
            .collect()
    }

    // ── run_index basic flow ──────────────────────────────────────────────────

    /// All photos are indexed; queue drains to zero; progress reports match.
    #[test]
    fn run_index_processes_all_photos() {
        let conn = mem_conn();
        for id in 1i64..=3 {
            insert_photo(&conn, id);
        }
        let abort = Arc::new(AtomicBool::new(false));
        let mut progress_events = Vec::new();

        let result = run_index(
            &conn,
            2,
            // Resolver: every photo has a fake path that "exists" — we don't actually
            // check existence in the preview_fn so we can use any string.
            |id| Ok(Some(std::path::PathBuf::from(format!("/fake/{id}.jpg")))),
            // Preview: always returns dummy bytes.
            |_path| Ok(vec![0xFFu8, 0xD8, 0xFF, 0xE0]),
            // Detector: inject 1 synthetic face per photo.
            |_jpeg| synthetic_faces(1),
            &abort,
            |p| progress_events.push(p),
        );

        let outcome = result.unwrap();
        assert_eq!(outcome.total, 3);
        assert_eq!(outcome.done, 3);
        assert_eq!(outcome.offline, 0);
        assert_eq!(outcome.failed, 0);
        assert!(!outcome.aborted);
        assert_eq!(queue_len(&conn).unwrap(), 0, "queue must be empty after full run");
        assert_eq!(progress_events.len(), 3);
        assert_eq!(progress_events.last().unwrap().done, 3);
    }

    /// The post-index hook (H13f region import) fires once per indexed photo, AFTER the
    /// photo's face rows are written and it is dequeued, with the resolved path.
    #[test]
    fn run_index_with_hook_fires_after_write() {
        let conn = mem_conn();
        for id in 1i64..=2 {
            insert_photo(&conn, id);
        }
        let abort = Arc::new(AtomicBool::new(false));
        let mut hook_calls: Vec<(i64, std::path::PathBuf, i64)> = Vec::new();

        run_index_with_hook(
            &conn,
            2,
            |id| Ok(Some(std::path::PathBuf::from(format!("/fake/{id}.jpg")))),
            |_path| Ok(vec![0xFFu8, 0xD8, 0xFF, 0xE0]),
            |_jpeg| synthetic_faces(1),
            &abort,
            |_| {},
            |hook_conn, photo_id, path| {
                // By the time the hook runs, the photo's face row must already exist.
                let n: i64 = hook_conn
                    .query_row(
                        "SELECT COUNT(*) FROM faces__faces WHERE photo_id = ?1",
                        [photo_id],
                        |r| r.get(0),
                    )
                    .unwrap();
                hook_calls.push((photo_id, path.to_path_buf(), n));
            },
        )
        .unwrap();

        assert_eq!(hook_calls.len(), 2, "hook fires once per indexed photo");
        for (photo_id, path, face_count) in &hook_calls {
            assert_eq!(*face_count, 1, "faces written before hook runs");
            assert_eq!(path, &std::path::PathBuf::from(format!("/fake/{photo_id}.jpg")));
        }
    }

    // ── Abort safety ─────────────────────────────────────────────────────────

    /// When the abort flag is tripped mid-run, the job stops and the queue retains the
    /// unprocessed photos (resumable).
    #[test]
    fn run_index_aborts_cleanly() {
        use std::sync::atomic::AtomicUsize;
        let conn = mem_conn();
        for id in 1i64..=6 {
            insert_photo(&conn, id);
        }
        let abort = Arc::new(AtomicBool::new(false));
        let abort_clone = abort.clone();
        // Use an atomic so the closure is Sync (required by the new parallel detect signature).
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = calls.clone();

        let outcome = run_index(
            &conn,
            2,
            |id| Ok(Some(std::path::PathBuf::from(format!("/fake/{id}.jpg")))),
            |_path| Ok(vec![0xFFu8, 0xD8, 0xFF, 0xE0]),
            move |_jpeg| {
                let n = calls_clone.fetch_add(1, Ordering::Relaxed) + 1;
                // Trip after the 3rd photo.
                if n >= 3 {
                    abort_clone.store(true, Ordering::Relaxed);
                }
                synthetic_faces(1)
            },
            &abort,
            |_| {},
        );

        // Some photos were indexed (at least some), some remain in the queue.
        // With DETECT_PARALLELISM=2 and abort tripped on 3rd call, the exact split
        // depends on scheduling — but at least one must remain and at least one processed.
        assert!(
            outcome.unwrap().aborted,
            "a tripped abort flag must be reported in the outcome"
        );
        let remaining = queue_len(&conn).unwrap();
        assert!(remaining > 0, "queue must retain unprocessed photos after abort");
        assert!(
            remaining < 6,
            "at least some photos should have been processed before abort"
        );
    }

    // ── Skip-already-indexed ──────────────────────────────────────────────────

    /// Photos that already have face rows (or a sentinel) are skipped (no duplicate insertion).
    #[test]
    fn run_index_skips_already_indexed() {
        use std::sync::atomic::AtomicUsize;
        let conn = mem_conn();
        for id in 1i64..=3 {
            insert_photo(&conn, id);
        }
        // Pre-index photo 2 with face row + sentinel (simulating a prior complete run).
        store::insert_face(&conn, 2, "[0,0,0.1,0.1]", "[]", 0.9, None, "detect", 0).unwrap();
        store::mark_indexed(&conn, 2, 1, 0).unwrap();
        // Manually enqueue all three as if a previous run started.
        let now = 1000i64;
        store::enqueue(&conn, 1, now).unwrap();
        store::enqueue(&conn, 2, now).unwrap();
        store::enqueue(&conn, 3, now).unwrap();

        let abort = Arc::new(AtomicBool::new(false));
        // AtomicUsize so the closure is Fn (Sync), required by the parallel detect signature.
        let detect_calls = Arc::new(AtomicUsize::new(0));
        let detect_calls_clone = detect_calls.clone();

        let outcome = run_index(
            &conn,
            2,
            |id| Ok(Some(std::path::PathBuf::from(format!("/fake/{id}.jpg")))),
            |_| Ok(vec![0xFFu8, 0xD8, 0xFF, 0xE0]),
            move |_jpeg| {
                detect_calls_clone.fetch_add(1, Ordering::Relaxed);
                synthetic_faces(1)
            },
            &abort,
            |_| {},
        )
        .unwrap();

        // 3 photos in queue; photo 2 was already indexed → detect called for 1 and 3 only.
        assert_eq!(outcome.total, 3);
        assert_eq!(outcome.done, 3);
        assert_eq!(
            detect_calls.load(Ordering::Relaxed),
            2,
            "already-indexed photo must skip detection"
        );
        assert_eq!(queue_len(&conn).unwrap(), 0);
    }

    // ── Embedding stored and retrievable ─────────────────────────────────────

    /// Face rows written by run_index carry the expected embedding BLOB, which decodes back
    /// losslessly.
    #[test]
    fn run_index_stores_embedding_blob() {
        let conn = mem_conn();
        insert_photo(&conn, 99);
        let abort = Arc::new(AtomicBool::new(false));
        // Inject a known embedding.
        let known: Vec<f32> = (0..512).map(|i| i as f32 / 512.0).collect();
        let norm: f32 = known.iter().map(|x| x * x).sum::<f32>().sqrt();
        let known_unit: Vec<f32> = known.iter().map(|x| x / norm).collect();
        let known_unit_clone = known_unit.clone();

        let _ = run_index(
            &conn,
            2,
            |_| Ok(Some(std::path::PathBuf::from("/fake/99.jpg"))),
            |_| Ok(vec![0xFFu8, 0xD8, 0xFF, 0xE0]),
            move |_jpeg| {
                vec![IndexedFace {
                    bbox: (0.1, 0.1, 0.2, 0.3),
                    landmarks: [(0.2, 0.2), (0.3, 0.2), (0.25, 0.28), (0.22, 0.34), (0.28, 0.34)],
                    confidence: 0.92,
                    embedding: Some(known_unit_clone.clone()),
                }]
            },
            &abort,
            |_| {},
        );

        // Read the blob back.
        let blob: Vec<u8> = conn
            .query_row(
                "SELECT embedding FROM faces__faces WHERE photo_id = 99",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let recovered = blob_to_embedding(&blob).unwrap();
        assert_eq!(recovered.len(), 512);
        let norm2: f32 = recovered.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm2 - 1.0).abs() < 1e-5,
            "recovered embedding must be unit (norm={norm2})"
        );
        for (a, b) in known_unit.iter().zip(recovered.iter()) {
            assert!(
                (a - b).abs() < 1e-7,
                "embedding value mismatch: {a} vs {b}"
            );
        }

        // Also round-trip through the codec helpers directly.
        let blob2 = embedding_to_blob(&known_unit);
        let back2 = blob_to_embedding(&blob2).unwrap();
        assert_eq!(back2.len(), 512);
    }

    // ── No faces → photo is dequeued and sentinel written ────────────────────

    /// When no faces are detected the photo still leaves the queue (a blank result is valid),
    /// and a sentinel row in faces__indexed prevents it from being re-enqueued on future runs.
    #[test]
    fn run_index_dequeues_on_zero_faces() {
        let conn = mem_conn();
        insert_photo(&conn, 77);
        let abort = Arc::new(AtomicBool::new(false));

        let outcome = run_index(
            &conn,
            2,
            |_| Ok(Some(std::path::PathBuf::from("/fake/77.jpg"))),
            |_| Ok(vec![0xFFu8, 0xD8, 0xFF, 0xE0]),
            |_jpeg| vec![], // no faces
            &abort,
            |_| {},
        )
        .unwrap();

        assert_eq!(outcome.total, 1);
        assert_eq!(outcome.done, 1);
        assert_eq!(queue_len(&conn).unwrap(), 0, "queue must be empty after zero-face run");
        // photo_is_indexed must return true via the sentinel, even though there are no face rows.
        assert!(
            photo_is_indexed(&conn, 77).unwrap(),
            "zero-face photo must be marked indexed via sentinel"
        );
        // A second populate_queue call must NOT re-enqueue photo 77.
        let added = store::populate_queue(&conn, 9999).unwrap();
        assert_eq!(added, 0, "zero-face photo must not be re-enqueued");
    }

    // ── Preview failures are counted and stay in the queue ───────────────────

    /// A photo whose preview can't be generated is counted as `failed`, stays in the
    /// queue, and does not advance `done`.
    #[test]
    fn run_index_counts_preview_failures() {
        let conn = mem_conn();
        for id in 1i64..=2 {
            insert_photo(&conn, id);
        }
        let abort = Arc::new(AtomicBool::new(false));

        // Photo 1's preview fails; photo 2 decodes fine.
        let outcome = run_index(
            &conn,
            2,
            |id| Ok(Some(std::path::PathBuf::from(format!("/fake/{id}.jpg")))),
            |path| {
                if path.ends_with("1.jpg") {
                    Err("undecodable".to_string())
                } else {
                    Ok(vec![0xFFu8, 0xD8, 0xFF, 0xE0])
                }
            },
            |_jpeg| synthetic_faces(1),
            &abort,
            |_| {},
        )
        .unwrap();

        assert_eq!(outcome.done, 1);
        assert_eq!(outcome.failed, 1, "preview failure must be counted in the outcome");
        assert_eq!(outcome.offline, 0);
        assert_eq!(queue_len(&conn).unwrap(), 1, "failed photo must remain in queue");
    }

    // ── Offline photos do not advance done ───────────────────────────────────

    /// Offline photos stay in the queue and do not advance the done counter.
    #[test]
    fn run_index_offline_does_not_advance_done() {
        let conn = mem_conn();
        for id in 1i64..=2 {
            insert_photo(&conn, id);
        }
        let abort = Arc::new(AtomicBool::new(false));
        let mut progress_events = Vec::new();

        // Photo 1 is offline (resolver returns None), photo 2 is reachable.
        let outcome = run_index(
            &conn,
            2,
            |id| {
                if id == 1 {
                    Ok(None)
                } else {
                    Ok(Some(std::path::PathBuf::from(format!("/fake/{id}.jpg"))))
                }
            },
            |_| Ok(vec![0xFFu8, 0xD8, 0xFF, 0xE0]),
            |_jpeg| synthetic_faces(1),
            &abort,
            |p| progress_events.push(p),
        )
        .unwrap();

        // Only photo 2 was actually processed.
        assert_eq!(outcome.done, 1, "offline photo must not advance done counter");
        assert_eq!(outcome.total, 2);
        assert_eq!(outcome.offline, 1, "offline photo must be counted in the outcome");
        assert!(!outcome.aborted);
        // Photo 1 must still be in the queue (left for next run).
        assert_eq!(queue_len(&conn).unwrap(), 1, "offline photo must remain in queue");
        // Only 1 progress event (for photo 2).
        assert_eq!(progress_events.len(), 1);
        assert_eq!(progress_events[0].done, 1);
    }
}
