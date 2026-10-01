//! [`Runner`]: where the storage jobs' blocking work runs.
//!
//! Imports, scans, catalog switches, identity repair, reconcile and emptying the trash are
//! long and lock-holding (file copies, a SQLite open, sidecar IO per copy on a NAS). In the
//! app they run on the core runtime's blocking pool, as the Tauri commands do, never on the
//! UI thread or GPUI's background executor (whose threads a long job would park).
//!
//! GPUI's deterministic test scheduler rejects wakeups from foreign threads, so a job on the
//! core runtime cannot be awaited in a headless test. Tests install [`Runner::manual`]
//! instead: work queues up and runs only when the test calls [`Runner::run_pending`], on the
//! test thread. That is also what lets a test force an interleaving — start a job, switch the
//! catalog, *then* let the job finish.

use futures::channel::oneshot;
use gpui_kit::{App, Global};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

type Work = Box<dyn FnOnce() + Send + 'static>;

/// Runs blocking work off the UI thread. A GPUI global; absent means the core runtime.
#[derive(Clone, Default)]
pub struct Runner {
    manual: Option<Arc<Mutex<VecDeque<Work>>>>,
}

impl Global for Runner {}

impl Runner {
    /// Work queues until [`run_pending`](Self::run_pending) — tests only.
    pub fn manual() -> Self {
        Runner { manual: Some(Arc::default()) }
    }

    /// The installed runner, or the core runtime's blocking pool.
    pub fn get(cx: &App) -> Runner {
        cx.try_global::<Runner>().cloned().unwrap_or_default()
    }

    /// Run `work` off the UI thread.
    pub fn spawn(&self, work: impl FnOnce() + Send + 'static) {
        match &self.manual {
            Some(queue) => queue.lock().unwrap().push_back(Box::new(work)),
            None => {
                chairphoto_core::app::spawn_blocking(work);
            }
        }
    }

    /// Run `work` off the UI thread; its result arrives on the returned channel (`Err` only
    /// if the work panicked).
    pub fn run<R: Send + 'static>(&self, work: impl FnOnce() -> R + Send + 'static) -> oneshot::Receiver<R> {
        let (tx, rx) = oneshot::channel();
        self.spawn(move || {
            let _ = tx.send(work());
        });
        rx
    }

    /// Manual runner: run everything queued so far, and whatever that work queues in turn,
    /// in order, on this thread. Returns how many items ran. No-op for the core runtime.
    pub fn run_pending(&self) -> usize {
        let Some(queue) = &self.manual else { return 0 };
        let mut ran = 0;
        loop {
            let next = queue.lock().unwrap().pop_front();
            match next {
                Some(work) => {
                    work();
                    ran += 1;
                }
                None => return ran,
            }
        }
    }

    /// Manual runner: run what is queued right now **newest first** — the order a pool of
    /// workers may pick tasks up in — and nothing those queue. Returns how many ran.
    #[cfg(test)]
    pub fn run_pending_reversed(&self) -> usize {
        let Some(queue) = &self.manual else { return 0 };
        let batch: Vec<Work> = queue.lock().unwrap().drain(..).collect();
        let ran = batch.len();
        for work in batch.into_iter().rev() {
            work();
        }
        ran
    }

    /// Manual runner: how many items wait.
    pub fn pending(&self) -> usize {
        self.manual.as_ref().map_or(0, |q| q.lock().unwrap().len())
    }
}
