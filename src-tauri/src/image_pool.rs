//! Bounded LIFO worker pool for image rendering with in-flight deduplication.
//!
//! # Design
//!
//! A fast grid scroll can fire hundreds of `thumb://` requests within milliseconds.
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
//!    the result bytes.
//!
//! # Safety invariants
//!
//! * A job key lives in `jobs` from the moment it is first submitted until *after*
//!   the runner completes and every responder has been called. This window is wider
//!   than the time the key lives in `stack` (which ends when a worker pops it), which
//!   is what makes mid-render attach work.
//!
//! * Every responder is *always* called — success, error, or panic. Dropping a
//!   `UriSchemeResponder` without calling `respond()` hangs the webview request
//!   forever.
//!
//! * A panicking runner maps to `Err("render panicked")` via `catch_unwind`; the
//!   worker thread itself is never killed.

use std::collections::HashMap;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex};

use crate::protocol::ImageKind;

/// Key identifying a unique image job: `(photo_id, kind)`.
pub type JobKey = (i64, ImageKind);

/// A one-shot callback that receives the rendered bytes (or an error string).
pub type Respond = Box<dyn FnOnce(Result<Vec<u8>, String>) + Send>;

/// The render function. Must be `Send + Sync` so it can be shared across workers.
pub type Runner = Arc<dyn Fn(JobKey) -> Result<Vec<u8>, String> + Send + Sync>;

struct PoolInner {
    /// LIFO stack of keys waiting to be picked up by a worker.
    stack: Vec<JobKey>,
    /// All pending or in-flight jobs, keyed by `JobKey`.
    /// A key is present here from first submit until *after* the runner finishes.
    jobs: HashMap<JobKey, Vec<Respond>>,
}

/// Bounded image-render pool. Create with [`ImagePool::start_with_runner`] and
/// submit work with [`ImagePool::submit`].
pub struct ImagePool {
    inner: Mutex<PoolInner>,
    cond: Condvar,
}

impl ImagePool {
    /// Spawn `n_threads` worker threads backed by `runner` and return the pool.
    pub fn start_with_runner(n_threads: usize, runner: Runner) -> Arc<Self> {
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
    pub fn submit(&self, key: JobKey, respond: Respond) {
        let mut guard = self.inner.lock().expect("image pool lock poisoned");
        if let Some(responders) = guard.jobs.get_mut(&key) {
            // Job already exists (queued or mid-render) — attach and return.
            responders.push(respond);
        } else {
            guard.jobs.insert(key, vec![respond]);
            guard.stack.push(key); // LIFO: newest at top
            self.cond.notify_one();
        }
    }

    // --- internals -----------------------------------------------------------

    fn worker_loop(&self, runner: Runner) {
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
            let result: Result<Vec<u8>, String> =
                panic::catch_unwind(AssertUnwindSafe(|| runner(key)))
                    .unwrap_or_else(|_| Err("render panicked".into()));

            // Collect all responders for this key, removing the job entry *after*
            // render so that any attach arriving mid-render still sees the key.
            let responders: Vec<Respond> = {
                let mut guard = self.inner.lock().expect("image pool lock poisoned");
                guard.jobs.remove(&key).unwrap_or_default()
            };

            // Call every responder with its own clone (or move for the last one).
            let n = responders.len();
            for (i, respond) in responders.into_iter().enumerate() {
                if i + 1 < n {
                    // Clone the result for every responder except the last.
                    let r = match &result {
                        Ok(bytes) => Ok(bytes.clone()),
                        Err(e) => Err(e.clone()),
                    };
                    respond(r);
                } else {
                    respond(result.clone().map_err(|e| e));
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

        let runner2: Runner = Arc::new(move |_key| {
            run_count4.fetch_add(1, Ordering::SeqCst);
            let _ = entered_tx.send(());
            gate_wait4.wait(); // block
            Ok(b"dedup-result".to_vec())
        });

        let pool2 = ImagePool::start_with_runner(1, runner2);

        let (result_tx, result_rx) = std::sync::mpsc::channel::<Result<Vec<u8>, String>>();

        let key: JobKey = (42, ImageKind::Thumb);
        // First submit — enters the runner (which then blocks).
        {
            let tx = result_tx.clone();
            pool2.submit(key, Box::new(move |r| { let _ = tx.send(r); }));
        }

        // Wait until the runner is actually executing (entered the barrier wait).
        entered_rx.recv().unwrap();

        // Now submit 4 more identical keys — these should attach to the in-flight job.
        for _ in 0..4 {
            let tx = result_tx.clone();
            pool2.submit(key, Box::new(move |r| { let _ = tx.send(r); }));
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

        let key_a: JobKey = (1, ImageKind::Thumb);
        let key_b: JobKey = (2, ImageKind::Thumb);
        let key_c: JobKey = (3, ImageKind::Thumb);

        let runner: Runner = Arc::new(move |key| {
            if key == key_a {
                let _ = a_entered_tx.send(());
                gate2.wait(); // block until released
            }
            Ok(vec![key.0 as u8])
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
        let panic_key: JobKey = (999, ImageKind::Thumb);
        let ok_key:    JobKey = (1,   ImageKind::Thumb);

        let runner: Runner = Arc::new(move |key| {
            if key == panic_key {
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

        let key: JobKey = (77, ImageKind::Thumb);

        let runner: Runner = Arc::new(move |_key| {
            let _ = entered_tx.send(());
            gate2.wait();
            Ok(b"shared-bytes".to_vec())
        });

        let pool = ImagePool::start_with_runner(1, runner);
        let (tx, rx) = std::sync::mpsc::channel::<Result<Vec<u8>, String>>();

        // First submit.
        let tx1 = tx.clone();
        pool.submit(key, Box::new(move |r| { let _ = tx1.send(r); }));

        // Wait until runner has started (is inside the gate).
        entered_rx.recv().unwrap();

        // Late attach while runner is blocked.
        let tx2 = tx.clone();
        pool.submit(key, Box::new(move |r| { let _ = tx2.send(r); }));

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
}
