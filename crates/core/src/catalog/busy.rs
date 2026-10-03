//! A transient "database is locked" is a reason to wait, not to give up (#182).
//!
//! Every connection waits out another writer for `busy_timeout` (5 s) before SQLite reports
//! `SQLITE_BUSY`. Some writers hold the lock longer than that: the bundle importer keeps one
//! write transaction open across its whole index phase, so a long-running job — the identity
//! repair pass, the bulk conflict resolution — that meets one used to end on its first busy
//! row, throwing away the rest of a 74k-row queue. Such a job retries the statement that
//! met the lock a few times, with a growing pause, and when the lock still holds it leaves
//! that one row as it was (still queued, still owed) and carries on with the next. The next
//! pass finds the row where it left it.
//!
//! Only the catalog step is retried, never the file IO before it: a sidecar the pass has
//! already written is not written twice, and the record that follows is the same
//! compare-and-set the first attempt would have made.

use super::{CatalogError, Result};
use rusqlite::ErrorCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The pauses between attempts, each on top of the connection's own `busy_timeout`. With the
/// 5 s timeout one stubborn row costs about 20 s before it is left queued.
#[cfg(not(test))]
const BACKOFF: [Duration; 3] =
    [Duration::from_millis(250), Duration::from_millis(1000), Duration::from_millis(2000)];
/// Tests shorten the connection's `busy_timeout` instead of waiting out 5 s; the pauses
/// shrink with it.
#[cfg(test)]
const BACKOFF: [Duration; 3] =
    [Duration::from_millis(1), Duration::from_millis(2), Duration::from_millis(4)];

/// How often a pause looks at the abort flag.
const ABORT_POLL: Duration = Duration::from_millis(50);

/// True when `e` is SQLite saying another connection holds the lock (`SQLITE_BUSY`, in any of
/// its extended forms, or `SQLITE_LOCKED`) — a state that passes, unlike every other error.
pub(crate) fn is_busy(e: &CatalogError) -> bool {
    matches!(
        e,
        CatalogError::Sqlite(err)
            if matches!(
                err.sqlite_error_code(),
                Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
            )
    )
}

/// Run `op`, and again after each pause in [`BACKOFF`] while it fails with [`is_busy`]. Returns
/// the first result that is not busy, or the last busy error once the pauses run out or
/// `abort` trips (a stopped job does not sit out a lock). Any other error returns at once.
///
/// `op` must be safe to repeat: a single statement, or a transaction that rolled back whole.
pub(crate) fn retry_busy<T>(abort: &AtomicBool, mut op: impl FnMut() -> Result<T>) -> Result<T> {
    let mut pauses = BACKOFF.iter();
    loop {
        match op() {
            Err(e) if is_busy(&e) => {
                let Some(pause) = pauses.next() else { return Err(e) };
                if abort.load(Ordering::Relaxed) {
                    return Err(e);
                }
                #[cfg(test)]
                hook::on_retry();
                pause_unless_aborted(*pause, abort);
            }
            other => return other,
        }
    }
}

fn pause_unless_aborted(pause: Duration, abort: &AtomicBool) {
    let until = std::time::Instant::now() + pause;
    loop {
        let now = std::time::Instant::now();
        if now >= until || abort.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep((until - now).min(ABORT_POLL));
    }
}

/// Where a test acts between a busy attempt and its retry — releasing the lock it holds, say —
/// so the interleaving is forced rather than timed.
#[cfg(test)]
pub(crate) mod hook {
    use std::cell::RefCell;

    thread_local! {
        static ON_RETRY: RefCell<Option<Box<dyn FnMut()>>> = RefCell::new(None);
    }

    /// Call `f` before each retry on this thread, until [`clear`].
    pub(crate) fn set(f: impl FnMut() + 'static) {
        ON_RETRY.with(|h| *h.borrow_mut() = Some(Box::new(f)));
    }

    pub(crate) fn clear() {
        ON_RETRY.with(|h| *h.borrow_mut() = None);
    }

    pub(super) fn on_retry() {
        // Taken out while it runs, so the hook may itself touch the catalog.
        let taken = ON_RETRY.with(|h| h.borrow_mut().take());
        if let Some(mut f) = taken {
            f();
            ON_RETRY.with(|h| {
                let mut slot = h.borrow_mut();
                if slot.is_none() {
                    *slot = Some(f);
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn busy() -> CatalogError {
        CatalogError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("database is locked".into()),
        ))
    }

    // --- retry_busy ---------------------------------------------------------------------

    #[test]
    fn a_busy_statement_is_retried_until_it_goes_through() {
        let calls = Cell::new(0);
        let out = retry_busy(&AtomicBool::new(false), || {
            calls.set(calls.get() + 1);
            if calls.get() < 3 { Err(busy()) } else { Ok(7) }
        });
        assert_eq!(out.unwrap(), 7);
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn a_lock_that_outlasts_every_pause_is_returned_as_busy() {
        let calls = Cell::new(0);
        let out: Result<()> = retry_busy(&AtomicBool::new(false), || {
            calls.set(calls.get() + 1);
            Err(busy())
        });
        assert!(out.as_ref().is_err_and(is_busy), "{out:?}");
        assert_eq!(calls.get(), BACKOFF.len() + 1);
    }

    #[test]
    fn any_other_error_and_a_stopped_job_are_not_retried() {
        let calls = Cell::new(0);
        let out: Result<()> = retry_busy(&AtomicBool::new(false), || {
            calls.set(calls.get() + 1);
            Err(CatalogError::Validation("no".into()))
        });
        assert!(matches!(out, Err(CatalogError::Validation(_))));
        assert_eq!(calls.get(), 1);

        calls.set(0);
        let out: Result<()> = retry_busy(&AtomicBool::new(true), || {
            calls.set(calls.get() + 1);
            Err(busy())
        });
        assert!(out.as_ref().is_err_and(is_busy));
        assert_eq!(calls.get(), 1, "an aborted job does not wait out the lock");
    }
}
