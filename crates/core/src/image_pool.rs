//! Bounded LIFO worker pool for image rendering with in-flight deduplication.
//!
//! # Design
//!
//! A fast grid scroll can ask for hundreds of thumbnails within milliseconds.
//! The old approach spawned one OS thread per request, potentially launching hundreds
//! of threads and exiv2/exiftool subprocesses in parallel, all racing each other with
//! no priority ordering.
//!
//! This pool fixes three problems:
//!
//! 1. **Bounded concurrency** — at most N worker threads run simultaneously (2–6,
//!    based on available parallelism / 3).
//!
//! 2. **LIFO ordering** — jobs are popped from a stack, so the *newest* submitted
//!    request (the tile the user just scrolled to) is rendered first, not last.
//!
//! 3. **In-flight deduplication** — if the same `(photo_id, kind)` key is submitted
//!    while it is already queued or actively rendering, the new responder is attached
//!    to the existing job. The runner is called exactly once and all responders share
//!    the result.
//!
//! # Safety invariants
//!
//! * A job key lives in `jobs` from the moment it is first submitted until *after*
//!   the runner completes and every responder has been called. This window is wider
//!   than the time the key lives in `stack` (which ends when a worker pops it), which
//!   is what makes mid-render attach work.
//!
//! * Every responder is *always* called — success, error, or panic. A responder dropped
//!   without being called leaves its requester waiting forever.
//!
//! * A panicking runner maps to `Err("render panicked")` via `catch_unwind`; the
//!   worker thread itself is never killed.
//!
//! # What a job returns
//!
//! The pool is generic over its result `T`: the GPUI app's renders decoded pixels
//! (`media::render_image`, #101). The pool never looks at `T`; it only clones it for a
//! job's extra responders, so a cheap-to-clone `T` (an `Arc`) is what a multi-responder job
//! wants.
//!
//! # Priority batches and cancellation (the GPUI app, #101)
//!
//! [`ImagePool::submit_batch`] submits several keys under one lock, most urgent first, so no
//! worker can pop between them: the first key ends on top of the stack. A key that is still
//! queued moves to the top instead of staying where it was — the batch is the newest request.
//! [`ImagePool::cancel`] takes a queued (not yet started) job off the stack and answers every
//! responder with [`CANCELLED`]; a job already rendering runs to completion.

use std::collections::HashMap;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex};

/// Which cached tier of a photo a job renders. Lives here, beside the key that carries it,
/// so the pool needs nothing from a front end.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ImageKind {
    Thumb,
    Preview,
    Zoom,
}

/// Key identifying a unique image job. A photo tier is `(photo_id, kind)`; an edit render
/// (the `edit` feature) carries its whole request, so two identical requests coalesce into
/// one render and any difference is a different job. The pool never
/// inspects a key — it only hashes and compares it.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum JobKey {
    Photo { id: i64, kind: ImageKind },
    #[cfg(feature = "edit")]
    Edit(EditJob),
    #[cfg(feature = "faces")]
    Avatar(AvatarJob),
}

impl JobKey {
    pub fn photo(id: i64, kind: ImageKind) -> Self {
        JobKey::Photo { id, kind }
    }

    /// The photo a job renders, whichever kind it is.
    pub fn photo_id(&self) -> i64 {
        match self {
            JobKey::Photo { id, .. } => *id,
            #[cfg(feature = "edit")]
            JobKey::Edit(job) => job.photo_id,
            #[cfg(feature = "faces")]
            JobKey::Avatar(job) => job.photo_id,
        }
    }
}

/// One avatar-crop render request (the Faces module's People view, #223 F1): a small square
/// cut from a face in its photo's original frame (never a cover render — #152), rendered on
/// the pool like any tier. People-view avatars used to claim the whole `Preview` tier
/// (≤2048px, tens of MB) just to show a 72px circle; many covered avatars on screen could
/// exceed the image budget and never settle. This key renders only the avatar's own pixels,
/// so the budget holds hundreds of them.
#[cfg(feature = "faces")]
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct AvatarJob {
    pub photo_id: i64,
    /// For the key's identity only — the render itself only needs `bbox` and `size` (below).
    pub face_id: i64,
    /// The face's normalized box `(x, y, w, h)`, already turned by the photo's user rotation
    /// (`modules::faces::logic::rotate_box`, the app crate) — the frame `ImageKind::Preview`
    /// decodes into. Stored as the four `f32`s' bits so the key hashes and compares exactly
    /// (`f32` is not `Eq`); a rotation change turns the box, so it is naturally a different
    /// job, no separate version needed here.
    bbox_bits: [u32; 4],
    /// Output edge, pixels — the avatar's native size (e.g. 144 for a 72px avatar at 2x).
    pub size: u32,
}

#[cfg(feature = "faces")]
impl AvatarJob {
    pub fn new(photo_id: i64, face_id: i64, bbox: (f32, f32, f32, f32), size: u32) -> Self {
        Self { photo_id, face_id, bbox_bits: [bbox.0.to_bits(), bbox.1.to_bits(), bbox.2.to_bits(), bbox.3.to_bits()], size }
    }

    /// The box `new` was given, back as floats.
    pub fn bbox(&self) -> (f32, f32, f32, f32) {
        let [x, y, w, h] = self.bbox_bits;
        (f32::from_bits(x), f32::from_bits(y), f32::from_bits(w), f32::from_bits(h))
    }
}

/// One edit render request (the Darkroom's frame), rendered by `media::render_edit_image`.
#[cfg(feature = "edit")]
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct EditJob {
    pub photo_id: i64,
    /// The edit record, verbatim.
    pub edit_json: String,
    /// Longest output edge; 0 = full size.
    pub max_edge: u32,
    /// Render from the native-size zoom tier instead of the 2048 px proxy.
    pub hi_res: bool,
    /// Geometry only, no look.
    pub base_only: bool,
    /// Which pixels to render from: the camera preview, or a resident RAW working image by
    /// token.
    pub source: crate::plugins::edit::SourceToken,
    /// The sensor-clipping overlay instead of the render: a transparent image with the same
    /// geometry and size, marked where the RAW itself clipped. Engine 2 only.
    pub clip: bool,
    /// Which catalog the caller had open when it asked (the GPUI app's
    /// `AppModel::catalog_epoch`). Photo ids from different catalogs are different photos, so
    /// two otherwise identical requests across a catalog switch are different jobs and must
    /// never merge into one render. Neither the pool nor the renderer reads it.
    pub catalog_epoch: u64,
}

/// A one-shot callback that receives the rendered result (or an error string).
pub type Respond<T> = Box<dyn FnOnce(Result<T, String>) + Send>;

/// The render function. Must be `Send + Sync` so it can be shared across workers.
pub type Runner<T> = Arc<dyn Fn(JobKey) -> Result<T, String> + Send + Sync>;

/// The error every responder of a [cancelled](ImagePool::cancel) job receives.
pub const CANCELLED: &str = "cancelled";

struct PoolInner<T> {
    /// LIFO stack of keys waiting to be picked up by a worker.
    stack: Vec<JobKey>,
    /// All pending or in-flight jobs, keyed by `JobKey`.
    /// A key is present here from first submit until *after* the runner finishes.
    jobs: HashMap<JobKey, Vec<Respond<T>>>,
}

/// Bounded image-render pool. Create with [`ImagePool::start_with_runner`] and
/// submit work with [`ImagePool::submit`].
pub struct ImagePool<T> {
    inner: Mutex<PoolInner<T>>,
    cond: Condvar,
}

impl<T: Clone + Send + 'static> ImagePool<T> {
    /// Spawn `n_threads` worker threads backed by `runner` and return the pool.
    pub fn start_with_runner(n_threads: usize, runner: Runner<T>) -> Arc<Self> {
        let pool = Arc::new(ImagePool {
            inner: Mutex::new(PoolInner {
                stack: Vec::new(),
                jobs: HashMap::new(),
            }),
            cond: Condvar::new(),
        });

        for _ in 0..n_threads {
            let pool_ref = Arc::clone(&pool);
            let runner_ref = Arc::clone(&runner);
            std::thread::Builder::new()
                .name("image-pool-worker".into())
                .spawn(move || pool_ref.worker_loop(runner_ref))
                .expect("failed to spawn image pool worker");
        }

        pool
    }

    /// Submit a render request for `key`.
    ///
    /// * If a job for `key` is already queued or rendering, `respond` is attached to
    ///   it and will be called when the ongoing render completes (dedup).
    /// * Otherwise a new job is created, pushed to the top of the LIFO stack, and a
    ///   worker is woken.
    pub fn submit(&self, key: JobKey, respond: Respond<T>) {
        let mut guard = self.inner.lock().expect("image pool lock poisoned");
        if let Some(responders) = guard.jobs.get_mut(&key) {
            // Job already exists (queued or mid-render) — attach and return.
            responders.push(respond);
        } else {
            guard.stack.push(key.clone()); // LIFO: newest at top
            guard.jobs.insert(key, vec![respond]);
            self.cond.notify_one();
        }
    }

    /// Submit several requests at once, **most urgent first**, under one lock: when the call
    /// returns, `batch[0]` is on top of the stack, then `batch[1]`, and so on — no worker can
    /// pop one before the rest are in place. What the loupe needs: the current photo, then
    /// N+1, then N−1.
    ///
    /// Per key, as [`submit`](Self::submit) — an in-flight or queued job gets the responder
    /// attached — except that a key still **queued** moves to the top in batch order rather
    /// than keeping its old, lower place: a preload queued earlier becomes the current photo.
    pub fn submit_batch(&self, batch: Vec<(JobKey, Respond<T>)>) {
        let mut guard = self.inner.lock().expect("image pool lock poisoned");
        let mut new_jobs = 0;
        // Least urgent first, so the most urgent ends on top.
        for (key, respond) in batch.into_iter().rev() {
            if let Some(responders) = guard.jobs.get_mut(&key) {
                responders.push(respond);
                if let Some(at) = guard.stack.iter().position(|k| *k == key) {
                    let k = guard.stack.remove(at);
                    guard.stack.push(k);
                }
            } else {
                guard.stack.push(key.clone());
                guard.jobs.insert(key, vec![respond]);
                new_jobs += 1;
            }
        }
        drop(guard);
        for _ in 0..new_jobs {
            self.cond.notify_one();
        }
    }

    /// Take `key` off the stack if no worker has started it, and answer every responder
    /// attached to it with `Err(CANCELLED)`. Returns whether it did. A job already rendering
    /// is left alone (it cannot be interrupted); its result reaches its responders as usual,
    /// and a caller that no longer wants it drops it.
    pub fn cancel(&self, key: &JobKey) -> bool {
        let responders = {
            let mut guard = self.inner.lock().expect("image pool lock poisoned");
            let Some(at) = guard.stack.iter().position(|k| k == key) else {
                return false;
            };
            guard.stack.remove(at);
            guard.jobs.remove(key).unwrap_or_default()
        };
        // Outside the lock, like the worker's answers: a responder may submit again.
        for respond in responders {
            respond(Err(CANCELLED.into()));
        }
        true
    }

    /// How many jobs are waiting for a worker (not counting those rendering).
    pub fn queued(&self) -> usize {
        self.inner.lock().expect("image pool lock poisoned").stack.len()
    }

    // --- internals -----------------------------------------------------------

    fn worker_loop(&self, runner: Runner<T>) {
        loop {
            // Acquire a key from the top of the stack.
            let key = {
                let mut guard = self.inner.lock().expect("image pool lock poisoned");
                loop {
                    if let Some(k) = guard.stack.pop() {
                        break k;
                    }
                    guard = self
                        .cond
                        .wait(guard)
                        .expect("image pool condvar poisoned");
                }
            }; // lock released here

            // Run the renderer outside the lock.  Catch panics so the thread lives.
            let result: Result<T, String> =
                panic::catch_unwind(AssertUnwindSafe(|| runner(key.clone())))
                    .unwrap_or_else(|_| Err("render panicked".into()));

            // Collect all responders for this key, removing the job entry *after*
            // render so that any attach arriving mid-render still sees the key.
            let responders: Vec<Respond<T>> = {
                let mut guard = self.inner.lock().expect("image pool lock poisoned");
                guard.jobs.remove(&key).unwrap_or_default()
            };

            // Call every responder with its own clone (or move for the last one).
            let n = responders.len();
            for (i, respond) in responders.into_iter().enumerate() {
                if i + 1 < n {
                    // Clone the result for every responder except the last.
                    respond(result.clone());
                } else {
                    respond(result);
                    break;
                }
            }
        }
    }
}

/// Compute the number of worker threads: `available_parallelism / 3`, clamped to 2..=6.
pub fn default_thread_count() -> usize {
    let par = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    ((par / 3) as usize).clamp(2, 6)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

    // -----------------------------------------------------------------------
    // 1. Dedup: runner blocks; submit same key 5×; runner called exactly once.
    // -----------------------------------------------------------------------
    #[test]
    fn test_dedup() {
        let gate_wait3 = Arc::new(Barrier::new(2));
        let gate_wait4 = Arc::clone(&gate_wait3);

        let run_count3 = Arc::new(AtomicUsize::new(0));
        let run_count4 = Arc::clone(&run_count3);

        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();

        let runner2: Runner<Vec<u8>> = Arc::new(move |_key| {
            run_count4.fetch_add(1, Ordering::SeqCst);
            let _ = entered_tx.send(());
            gate_wait4.wait(); // block
            Ok(b"dedup-result".to_vec())
        });

        let pool2 = ImagePool::start_with_runner(1, runner2);

        let (result_tx, result_rx) = std::sync::mpsc::channel::<Result<Vec<u8>, String>>();

        let key: JobKey = JobKey::photo(42, ImageKind::Thumb);
        // First submit — enters the runner (which then blocks).
        {
            let tx = result_tx.clone();
            pool2.submit(key.clone(), Box::new(move |r| { let _ = tx.send(r); }));
        }

        // Wait until the runner is actually executing (entered the barrier wait).
        entered_rx.recv().unwrap();

        // Now submit 4 more identical keys — these should attach to the in-flight job.
        for _ in 0..4 {
            let tx = result_tx.clone();
            pool2.submit(key.clone(), Box::new(move |r| { let _ = tx.send(r); }));
        }

        // Release the runner.
        gate_wait3.wait();

        // Collect all 5 results.
        let mut results = Vec::new();
        for _ in 0..5 {
            results.push(result_rx.recv().expect("no result"));
        }

        assert_eq!(run_count3.load(Ordering::SeqCst), 1, "runner should be called exactly once");
        for r in &results {
            assert!(r.is_ok(), "expected Ok result");
            assert_eq!(r.as_deref().unwrap(), b"dedup-result");
        }
    }

    // -----------------------------------------------------------------------
    // 2. LIFO: pool with 1 thread; block on job A; submit B then C; order = A, C, B.
    // -----------------------------------------------------------------------
    #[test]
    fn test_lifo_ordering() {
        let (a_entered_tx, a_entered_rx) = std::sync::mpsc::channel::<()>();
        let gate = Arc::new(Barrier::new(2));
        let gate2 = Arc::clone(&gate);

        let key_a: JobKey = JobKey::photo(1, ImageKind::Thumb);
        let key_b: JobKey = JobKey::photo(2, ImageKind::Thumb);
        let key_c: JobKey = JobKey::photo(3, ImageKind::Thumb);

        let key_a_in = key_a.clone();
        let runner: Runner<Vec<u8>> = Arc::new(move |key| {
            if key == key_a_in {
                let _ = a_entered_tx.send(());
                gate2.wait(); // block until released
            }
            Ok(vec![key.photo_id() as u8])
        });

        let pool = ImagePool::start_with_runner(1, runner);
        let (tx, rx) = std::sync::mpsc::channel::<i64>();

        // Submit A first; worker picks it up immediately.
        let tx1 = tx.clone();
        pool.submit(key_a, Box::new(move |r| {
            if let Ok(b) = r { let _ = tx1.send(b[0] as i64); }
        }));

        // Wait until A is inside the runner (blocking at the gate).
        a_entered_rx.recv().unwrap();

        // Now submit B then C — both land in the stack. LIFO means C is on top.
        let tx2 = tx.clone();
        pool.submit(key_b, Box::new(move |r| {
            if let Ok(b) = r { let _ = tx2.send(b[0] as i64); }
        }));
        let tx3 = tx.clone();
        pool.submit(key_c, Box::new(move |r| {
            if let Ok(b) = r { let _ = tx3.send(b[0] as i64); }
        }));

        // Release A.
        gate.wait();

        let first  = rx.recv().unwrap(); // A completes
        let second = rx.recv().unwrap(); // LIFO: C next
        let third  = rx.recv().unwrap(); // then B

        assert_eq!(first,  1, "A should complete first");
        assert_eq!(second, 3, "C should be second (LIFO)");
        assert_eq!(third,  2, "B should be last");
    }

    // -----------------------------------------------------------------------
    // 3. Panic safety: runner panics for one key; responders get Err;
    //    subsequent submit on the same pool completes normally.
    // -----------------------------------------------------------------------
    #[test]
    fn test_panic_safety() {
        let panic_key: JobKey = JobKey::photo(999, ImageKind::Thumb);
        let ok_key:    JobKey = JobKey::photo(1, ImageKind::Thumb);

        let panic_key_in = panic_key.clone();
        let runner: Runner<Vec<u8>> = Arc::new(move |key| {
            if key == panic_key_in {
                panic!("deliberate test panic");
            }
            Ok(b"ok".to_vec())
        });

        let pool = ImagePool::start_with_runner(2, runner);
        let (tx, rx) = std::sync::mpsc::channel::<Result<Vec<u8>, String>>();

        let tx1 = tx.clone();
        pool.submit(panic_key, Box::new(move |r| { let _ = tx1.send(r); }));

        let panic_result = rx.recv().unwrap();
        assert!(panic_result.is_err(), "panic should produce Err");

        // Worker must have survived — submit a normal key.
        let tx2 = tx.clone();
        pool.submit(ok_key, Box::new(move |r| { let _ = tx2.send(r); }));

        let ok_result = rx.recv().unwrap();
        assert!(ok_result.is_ok(), "subsequent job should succeed");
    }

    // -----------------------------------------------------------------------
    // 4. Concurrent attach: submit while mid-render; late responder gets result.
    // -----------------------------------------------------------------------
    #[test]
    fn test_concurrent_attach() {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let gate = Arc::new(Barrier::new(2));
        let gate2 = Arc::clone(&gate);

        let key: JobKey = JobKey::photo(77, ImageKind::Thumb);

        let runner: Runner<Vec<u8>> = Arc::new(move |_key| {
            let _ = entered_tx.send(());
            gate2.wait();
            Ok(b"shared-bytes".to_vec())
        });

        let pool = ImagePool::start_with_runner(1, runner);
        let (tx, rx) = std::sync::mpsc::channel::<Result<Vec<u8>, String>>();

        // First submit.
        let tx1 = tx.clone();
        pool.submit(key.clone(), Box::new(move |r| { let _ = tx1.send(r); }));

        // Wait until runner has started (is inside the gate).
        entered_rx.recv().unwrap();

        // Late attach while runner is blocked.
        let tx2 = tx.clone();
        pool.submit(key.clone(), Box::new(move |r| { let _ = tx2.send(r); }));

        // Release the runner.
        gate.wait();

        // Both responders should receive the bytes.
        let r1 = rx.recv().unwrap();
        let r2 = rx.recv().unwrap();
        assert!(r1.is_ok());
        assert!(r2.is_ok());
        assert_eq!(r1.unwrap(), b"shared-bytes");
        assert_eq!(r2.unwrap(), b"shared-bytes");
    }

    /// A 1-thread pool whose runner blocks on `gate` for photo 0 and records every photo it
    /// renders, in order.
    fn gated_pool() -> (Arc<ImagePool<Vec<u8>>>, Arc<Barrier>, std::sync::mpsc::Receiver<i64>, std::sync::mpsc::Receiver<()>) {
        let gate = Arc::new(Barrier::new(2));
        let (ran_tx, ran_rx) = std::sync::mpsc::channel::<i64>();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let g = Arc::clone(&gate);
        let ran_tx = std::sync::Mutex::new(ran_tx);
        let entered_tx = std::sync::Mutex::new(entered_tx);
        let runner: Runner<Vec<u8>> = Arc::new(move |key| {
            if key.photo_id() == 0 {
                let _ = entered_tx.lock().unwrap().send(());
                g.wait();
            }
            let _ = ran_tx.lock().unwrap().send(key.photo_id());
            Ok(vec![key.photo_id() as u8])
        });
        (ImagePool::start_with_runner(1, runner), gate, ran_rx, entered_rx)
    }

    fn noop() -> Respond<Vec<u8>> {
        Box::new(|_| {})
    }

    // -----------------------------------------------------------------------
    // 5. Batch order: the first key of a batch renders first, then the rest in order —
    //    even with older jobs queued underneath.
    // -----------------------------------------------------------------------
    #[test]
    fn a_batch_renders_most_urgent_first_above_older_queued_jobs() {
        let (pool, gate, ran, entered) = gated_pool();
        pool.submit(JobKey::photo(0, ImageKind::Thumb), noop());
        entered.recv().unwrap(); // the only worker is now busy with photo 0
        pool.submit(JobKey::photo(9, ImageKind::Thumb), noop()); // an older grid request
        pool.submit_batch(vec![
            (JobKey::photo(5, ImageKind::Preview), noop()), // current
            (JobKey::photo(6, ImageKind::Preview), noop()), // N+1
            (JobKey::photo(4, ImageKind::Preview), noop()), // N-1
        ]);
        assert_eq!(pool.queued(), 4);
        gate.wait();
        let order: Vec<i64> = (0..5).map(|_| ran.recv().unwrap()).collect();
        assert_eq!(order, vec![0, 5, 6, 4, 9]);
    }

    // -----------------------------------------------------------------------
    // 6. A key a batch names that is already queued moves to the top and keeps one job:
    //    both responders get the one render.
    // -----------------------------------------------------------------------
    #[test]
    fn a_batch_promotes_a_queued_key_and_merges_its_responders() {
        let (pool, gate, ran, entered) = gated_pool();
        pool.submit(JobKey::photo(0, ImageKind::Thumb), noop());
        entered.recv().unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<Result<Vec<u8>, String>>();
        let tx1 = tx.clone();
        pool.submit(JobKey::photo(6, ImageKind::Preview), Box::new(move |r| { let _ = tx1.send(r); }));
        pool.submit(JobKey::photo(7, ImageKind::Preview), noop());
        pool.submit(JobKey::photo(8, ImageKind::Preview), noop());
        // 6 was queued at the bottom; the batch makes it the most urgent.
        pool.submit_batch(vec![(JobKey::photo(6, ImageKind::Preview), Box::new(move |r| { let _ = tx.send(r); }))]);
        assert_eq!(pool.queued(), 3, "a merged key is still one job");
        gate.wait();
        let order: Vec<i64> = (0..4).map(|_| ran.recv().unwrap()).collect();
        assert_eq!(order, vec![0, 6, 8, 7]);
        assert_eq!(rx.recv().unwrap().unwrap(), vec![6]);
        assert_eq!(rx.recv().unwrap().unwrap(), vec![6]);
    }

    // -----------------------------------------------------------------------
    // 7. Cancel: a queued job never renders and every responder hears CANCELLED; a job
    //    already rendering cannot be cancelled and still answers.
    // -----------------------------------------------------------------------
    #[test]
    fn cancel_answers_a_queued_job_and_leaves_a_rendering_one() {
        let (pool, gate, ran, entered) = gated_pool();
        let (tx, rx) = std::sync::mpsc::channel::<(i64, Result<Vec<u8>, String>)>();
        let respond = |id: i64| -> Respond<Vec<u8>> {
            let tx = tx.clone();
            Box::new(move |r| { let _ = tx.send((id, r)); })
        };
        pool.submit(JobKey::photo(0, ImageKind::Thumb), respond(0));
        entered.recv().unwrap();
        pool.submit(JobKey::photo(3, ImageKind::Thumb), respond(3));
        pool.submit(JobKey::photo(3, ImageKind::Thumb), respond(3));
        pool.submit(JobKey::photo(4, ImageKind::Thumb), respond(4));

        assert!(!pool.cancel(&JobKey::photo(0, ImageKind::Thumb)), "photo 0 is rendering");
        assert!(pool.cancel(&JobKey::photo(3, ImageKind::Thumb)));
        assert!(!pool.cancel(&JobKey::photo(3, ImageKind::Thumb)), "already gone");
        for _ in 0..2 {
            assert_eq!(rx.recv().unwrap(), (3, Err(CANCELLED.to_string())));
        }
        gate.wait();
        let order: Vec<i64> = (0..2).map(|_| ran.recv().unwrap()).collect();
        assert_eq!(order, vec![0, 4], "the cancelled job never reached the runner");
        let mut answered: Vec<i64> = (0..2).map(|_| rx.recv().unwrap().0).collect();
        answered.sort();
        assert_eq!(answered, vec![0, 4]);
    }

    // -----------------------------------------------------------------------
    // 8. Generic result: a pool of `Arc`s hands every merged responder the same `Arc`.
    // -----------------------------------------------------------------------
    #[test]
    fn a_pool_of_arcs_shares_one_result_between_merged_responders() {
        let gate = Arc::new(Barrier::new(2));
        let g = Arc::clone(&gate);
        let runner: Runner<Arc<String>> = Arc::new(move |_| {
            g.wait();
            Ok(Arc::new("pixels".to_string()))
        });
        let pool = ImagePool::start_with_runner(1, runner);
        let (tx, rx) = std::sync::mpsc::channel::<Arc<String>>();
        for _ in 0..2 {
            let tx = tx.clone();
            pool.submit(JobKey::photo(1, ImageKind::Zoom), Box::new(move |r| { let _ = tx.send(r.unwrap()); }));
        }
        gate.wait();
        let (a, b) = (rx.recv().unwrap(), rx.recv().unwrap());
        assert!(Arc::ptr_eq(&a, &b));
    }
}
