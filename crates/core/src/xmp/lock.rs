//! Per-sidecar serialisation (issue #149).
//!
//! Two writers on one sidecar used to interleave: each read the file, changed its own
//! properties and wrote the whole document back, so one change was lost — and two in-place
//! writes overlapping could leave a malformed file. Two keyed FIFO locks fix that:
//!
//! * **The file lock** ([`FILE_TURNS`]) — taken by `SidecarDocument::open` before the sidecar
//!   is read and released when the document is committed or dropped, so every writer's whole
//!   read-modify-write is one turn. Every sidecar writer goes through `SidecarDocument`, so
//!   every writer takes it by construction.
//! * **The write order** ([`WriteOrder`]) — for a writer that stores its change in the
//!   catalog, releases the catalog lock and only then writes the sidecar (the IPTC save and
//!   the geocoder's fill). It reserves a place in line under the catalog lock, waits for its
//!   turn with no lock held, then stores and writes while holding the turn, so stores and
//!   sidecar writes run in one order: the catalog's newest value is also the sidecar's. The
//!   store resolves the original again and checks it still maps to the turn's sidecar
//!   ([`WriteOrder::moved_to`]); a photo whose reachable copy changed meanwhile takes the
//!   new sidecar's turn before it stores (`app::iptc::store_in_turn`, #155 R1).
//!
//! # Lock order
//!
//! Both are listed in the table in `app/jobs.rs`.
//!
//! * The file lock is a **leaf**. Some writers take it while holding the catalog lock (face
//!   regions, GPS, an identity Overwrite); nothing is acquired while it is held — the XML
//!   work and the file I/O only.
//! * [`WriteOrder::reserve`] never blocks, so it may be called under the catalog lock.
//!   [`WriteOrder::wait`] blocks, and is called with **no lock held**, never across an
//!   `.await`, and **never on an async runtime's worker thread**: on a blocking thread
//!   (`spawn_blocking`, the GPUI blocking runner). A tokio worker blocked in it can stall
//!   the runtime's timers (#148: a geocode fill waiting there left a 20 ms sleep elsewhere
//!   on the runtime unfired for as long as the wait lasted, with the other workers idle,
//!   until a newly spawned task woke one of them). The turn it returns is held for one
//!   store, its sidecar write and that write's settle: the turn holder takes the catalog
//!   lock (the store), releases it, takes the file lock (the write), then the catalog lock
//!   again (the settle). That is safe because no catalog holder ever waits on a turn: a
//!   waiter holds nothing another thread needs, and the earliest ticket's holder is always
//!   running towards its write, so the line always moves.
//!
//! Both are process-wide and keyed by the sidecar's resolved path ([`key`]); an entry exists
//! only while some ticket for that path is outstanding. Neither protects against a second
//! process writing the same sidecar.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};

/// One sidecar's line: tickets `serving..next` are outstanding; `done` holds the ones
/// released out of turn (a ticket dropped before it was served).
#[derive(Default)]
struct Line {
    next: u64,
    serving: u64,
    done: BTreeSet<u64>,
}

/// A process-wide family of FIFO locks keyed by path.
pub(super) struct Turns {
    lines: Mutex<BTreeMap<PathBuf, Line>>,
    turn_changed: Condvar,
}

impl Turns {
    const fn new() -> Self {
        Self { lines: Mutex::new(BTreeMap::new()), turn_changed: Condvar::new() }
    }

    fn lines(&self) -> std::sync::MutexGuard<'_, BTreeMap<PathBuf, Line>> {
        // Every critical section below leaves the map consistent, so a poisoned guard (a
        // panic elsewhere while holding it is impossible, but be safe) is still usable.
        self.lines.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Take the next place in `key`'s line. Never blocks.
    pub(super) fn reserve(&'static self, key: PathBuf) -> Ticket {
        let mut lines = self.lines();
        let line = lines.entry(key.clone()).or_default();
        let n = line.next;
        line.next += 1;
        Ticket { turns: self, key, n }
    }

    /// Take the next place in `key`'s line and wait for it.
    pub(super) fn lock(&'static self, key: PathBuf) -> Ticket {
        let ticket = self.reserve(key);
        ticket.wait_turn();
        ticket
    }

    fn release(&self, key: &Path, n: u64) {
        let mut lines = self.lines();
        let Some(line) = lines.get_mut(key) else { return };
        line.done.insert(n);
        while line.done.remove(&line.serving) {
            line.serving += 1;
        }
        if line.serving == line.next {
            lines.remove(key);
        }
        drop(lines);
        self.turn_changed.notify_all();
    }

    #[cfg(test)]
    fn outstanding(&self) -> usize {
        self.lines().len()
    }
}

/// A place in one sidecar's line. Dropping it — served or not — releases the place.
pub(super) struct Ticket {
    turns: &'static Turns,
    key: PathBuf,
    n: u64,
}

impl Ticket {
    /// Block until every earlier ticket for this path has been released.
    fn wait_turn(&self) {
        let mut lines = self.turns.lines();
        loop {
            match lines.get(&self.key) {
                Some(line) if line.serving < self.n => {}
                _ => return,
            }
            lines = self.turns.turn_changed.wait(lines).unwrap_or_else(|e| e.into_inner());
        }
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.turns.release(&self.key, self.n);
    }
}

/// The file lock: one turn per sidecar read-modify-write (see the module docs).
pub(super) static FILE_TURNS: Turns = Turns::new();
static ORDER_TURNS: Turns = Turns::new();

/// The lock key for `sidecar`: its resolved path when it (or its directory) exists, so two
/// spellings of one file — a symlink, `..` — share a lock; the path as given otherwise.
pub(super) fn key(sidecar: &Path) -> PathBuf {
    if let Ok(resolved) = std::fs::canonicalize(sidecar) {
        return resolved;
    }
    match (sidecar.parent(), sidecar.file_name()) {
        (Some(dir), Some(name)) => std::fs::canonicalize(dir)
            .map(|d| d.join(name))
            .unwrap_or_else(|_| sidecar.to_path_buf()),
        _ => sidecar.to_path_buf(),
    }
}

/// A reserved place for one store-and-sidecar-write (see the module docs for where it may
/// be reserved and waited on).
pub struct WriteOrder(Ticket);

impl WriteOrder {
    /// Reserve the next place in line for `photo_path`'s sidecar. Never blocks, so it may be
    /// called under the catalog lock (where the path was resolved).
    pub fn reserve(photo_path: &Path) -> Self {
        Self(ORDER_TURNS.reserve(key(&super::sidecar_path(photo_path))))
    }

    /// `None` while `photo_path` — the original as resolved again where this turn is used —
    /// still maps to the sidecar the turn was reserved for; otherwise a fresh reservation for
    /// the sidecar it maps to now. Never blocks.
    ///
    /// A photo with two locations resolves to whichever copy is reachable, so the copy can
    /// change while a writer waits (its primary volume comes back). A turn for the old
    /// sidecar does not order writes to the new one, so a writer that finds itself moved must
    /// give this turn up and wait for the new one before it stores (#155 R1).
    pub fn moved_to(&self, photo_path: &Path) -> Option<Self> {
        let now = key(&super::sidecar_path(photo_path));
        (now != self.0.key).then(|| Self(ORDER_TURNS.reserve(now)))
    }

    /// Wait (holding no lock) until every write reserved before this one is done, then
    /// return the turn. Hold it across the store and the sidecar write; dropping it lets the
    /// next go.
    pub fn wait(self) -> Self {
        self.0.wait_turn();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    static TEST_TURNS: Turns = Turns::new();

    /// Turns are served in reservation order, whatever order the waiters arrive in, and a
    /// ticket dropped unserved does not stall the line.
    #[test]
    fn turns_are_served_in_reservation_order() {
        let key = PathBuf::from("/turns/order");
        let first = TEST_TURNS.reserve(key.clone());
        let abandoned = TEST_TURNS.reserve(key.clone());
        let third = TEST_TURNS.reserve(key.clone());
        let log = Arc::new(Mutex::new(Vec::new()));

        let l = log.clone();
        let waiter = std::thread::spawn(move || {
            third.wait_turn();
            l.lock().unwrap().push(3);
            drop(third);
        });
        std::thread::sleep(Duration::from_millis(50));
        assert!(!waiter.is_finished(), "the third ticket must wait for the first");
        drop(abandoned); // released out of turn: must not let the third through yet
        std::thread::sleep(Duration::from_millis(50));
        assert!(!waiter.is_finished(), "the first ticket still holds the line");
        first.wait_turn();
        log.lock().unwrap().push(1);
        drop(first);
        waiter.join().unwrap();
        assert_eq!(*log.lock().unwrap(), vec![1, 3]);
    }

    /// Entries are removed once their last ticket is released, so the map does not grow
    /// with every sidecar ever written.
    #[test]
    fn lines_are_cleaned_up_after_the_last_ticket() {
        static CLEAN: Turns = Turns::new();
        let counter = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let c = counter.clone();
                std::thread::spawn(move || {
                    for j in 0..50 {
                        let _t = CLEAN.lock(PathBuf::from(format!("/clean/{}", (i + j) % 3)));
                        c.fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(counter.load(Ordering::Relaxed), 400);
        assert_eq!(CLEAN.outstanding(), 0, "every line must be removed once empty");
        // An unserved ticket dropped alone also cleans up.
        drop(CLEAN.reserve(PathBuf::from("/clean/x")));
        assert_eq!(CLEAN.outstanding(), 0);
    }

    /// Two spellings of one sidecar share a key.
    #[test]
    fn key_resolves_spellings_of_one_file() {
        let dir = crate::test_support::TestTmpDir::new("xmp-lock-key");
        std::fs::create_dir_all(dir.join("a")).unwrap();
        let direct = dir.join("A.ARW.xmp");
        let dotted = dir.join("a").join("..").join("A.ARW.xmp");
        assert_eq!(key(&direct), key(&dotted), "absent file, existing directory");
        std::fs::write(&direct, "x").unwrap();
        assert_eq!(key(&direct), key(&dotted), "existing file");
    }
}
