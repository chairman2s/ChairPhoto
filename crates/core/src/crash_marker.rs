//! Crash markers: remember what the process was doing when it died, so a native crash in
//! code Rust cannot catch — LibRaw today, a GPU driver if a GPU backend ever lands — does
//! not turn into a crash loop.
//!
//! A panic unwinds and is caught; a segfault inside C code or a driver kills the process
//! with no chance to clean up. The only defence is to write down *before* the risky call
//! what it is about to do, and look for that note on the next launch:
//!
//! 1. [`enter`] writes a small in-flight marker file naming a `kind` (e.g.
//!    `"libraw-decode"`) and a `subject` (a key for the thing being worked on) and returns
//!    a [`Guard`].
//! 2. The guard's `Drop` deletes the marker. A normal return **and an error return** both
//!    drop it: an error is the library surviving the input, which is the property being
//!    tracked. A panic unwinds, so it drops it too.
//! 3. A marker still on disk at the next [`recover`] means the process died inside the
//!    call. The subject earns a strike. After [`BLOCK_AFTER`] strikes, [`blocked`] reports
//!    it and callers skip the risky call and take their fallback (the embedded preview,
//!    or — for a GPU — the CPU path).
//! 4. Surviving the call once clears the subject's strikes. A clean quit ([`clean_exit`])
//!    clears this process's in-flight markers, so closing the app during a 3-second RAW
//!    decode is not mistaken for a crash.
//!
//! Callers choose the subject key so that a *change* earns a fresh chance: the LibRaw
//! keys include the decoder version and the file's size and mtime, so upgrading the
//! decoder or replacing the file retries it without any user action.
//!
//! Two strikes, not one: a power cut or a `kill -9` mid-call leaves a marker exactly like
//! a crash does. One such accident must not block a good file; a genuine crash loop is
//! broken on its second round. Several markers for the same subject from one dead process
//! (two decodes of the same file in flight at once) count as one strike.
//!
//! Markers are plain writes without `fsync`: a crashing process loses nothing already
//! handed to the kernel, and an `fsync` per decode would cost milliseconds for the
//! power-cut case that two strikes already tolerates.
//!
//! State lives in the app data dir (`crash-markers/`), not the catalog: it is about this
//! machine's decoder and driver, is needed before any catalog is open, and must survive a
//! catalog switch.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// Strikes after which a subject is skipped.
pub const BLOCK_AFTER: u32 = 2;

const INFLIGHT: &str = "inflight";
const STRIKES: &str = "strikes.json";

/// One in-flight marker, as written to disk.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct Marker {
    kind: String,
    subject: String,
    /// Human-readable name of the subject for messages (a file path, an adapter name).
    label: String,
    pid: u32,
    started_at: i64,
}

/// A subject's crash history.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Strikes {
    pub kind: String,
    pub subject: String,
    pub label: String,
    pub strikes: u32,
    pub last_at: i64,
}

impl Strikes {
    pub fn is_blocked(&self) -> bool {
        self.strikes >= BLOCK_AFTER
    }
}

/// The crash-marker store rooted at one directory. The app uses one global instance
/// ([`init`]); tests build their own.
pub struct Markers {
    dir: PathBuf,
    pid: u32,
    seq: AtomicU64,
    /// `strikes.json`, loaded once and written through.
    strikes: Mutex<BTreeMap<String, Strikes>>,
}

fn key(kind: &str, subject: &str) -> String {
    format!("{kind}\u{1f}{subject}")
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Markers {
    /// Open (creating if needed) the store at `dir`. Never fails: a store that cannot be
    /// written just cannot protect, and must not stop the app from starting.
    pub fn open(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let _ = std::fs::create_dir_all(dir.join(INFLIGHT));
        let strikes = std::fs::read(dir.join(STRIKES))
            .ok()
            .and_then(|b| serde_json::from_slice::<Vec<Strikes>>(&b).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|s| (key(&s.kind, &s.subject), s))
            .collect();
        Markers {
            dir,
            pid: std::process::id(),
            seq: AtomicU64::new(0),
            strikes: Mutex::new(strikes),
        }
    }

    fn inflight_dir(&self) -> PathBuf {
        self.dir.join(INFLIGHT)
    }

    fn save(&self, strikes: &BTreeMap<String, Strikes>) {
        let list: Vec<&Strikes> = strikes.values().collect();
        let Ok(bytes) = serde_json::to_vec_pretty(&list) else { return };
        // Write-then-rename so a crash mid-save cannot leave half a file.
        let tmp = self.dir.join(format!("{STRIKES}.tmp{}", self.pid));
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, self.dir.join(STRIKES));
        }
    }

    /// Mark `kind`/`subject` in flight until the returned guard drops.
    pub fn enter(&self, kind: &str, subject: &str, label: &str) -> Guard<'_> {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        let path = self.inflight_dir().join(format!("{}-{n}.json", self.pid));
        let marker = Marker {
            kind: kind.into(),
            subject: subject.into(),
            label: label.into(),
            pid: self.pid,
            started_at: now(),
        };
        let written = serde_json::to_vec(&marker)
            .ok()
            .is_some_and(|bytes| std::fs::write(&path, bytes).is_ok());
        Guard {
            markers: Some(self),
            path: written.then_some(path),
            key: key(kind, subject),
        }
    }

    /// The subject's history if it has crashed enough times to be skipped.
    pub fn blocked(&self, kind: &str, subject: &str) -> Option<Strikes> {
        let strikes = self.strikes.lock().ok()?;
        strikes.get(&key(kind, subject)).filter(|s| s.is_blocked()).cloned()
    }

    /// Survived: forget the subject's strikes (only writes when there were any).
    fn survived(&self, key: &str) {
        if let Ok(mut strikes) = self.strikes.lock() {
            if strikes.remove(key).is_some() {
                self.save(&strikes);
            }
        }
    }

    /// Turn leftover markers into strikes. Call once at startup, before any risky work.
    /// Returns every subject that earned a strike this time, blocked or not.
    pub fn recover(&self) -> Vec<Strikes> {
        let Ok(entries) = std::fs::read_dir(self.inflight_dir()) else { return Vec::new() };
        let mut leftovers: Vec<(PathBuf, Marker)> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let parsed = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Marker>(&b).ok());
            match parsed {
                Some(m) if m.pid != self.pid && process_is_running_us(m.pid) => {
                    // Another live instance's work in progress — not ours to judge.
                }
                Some(m) => leftovers.push((path, m)),
                // Unreadable: the process died while writing it. Nothing to blame.
                None => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        if leftovers.is_empty() {
            return Vec::new();
        }
        let mut struck = Vec::new();
        let mut seen: HashSet<(u32, String)> = HashSet::new();
        if let Ok(mut strikes) = self.strikes.lock() {
            for (path, m) in &leftovers {
                let k = key(&m.kind, &m.subject);
                // One dead process, one strike per subject, however many markers it left.
                if seen.insert((m.pid, k.clone())) {
                    let entry = strikes.entry(k).or_insert_with(|| Strikes {
                        kind: m.kind.clone(),
                        subject: m.subject.clone(),
                        label: m.label.clone(),
                        strikes: 0,
                        last_at: 0,
                    });
                    entry.strikes += 1;
                    entry.last_at = m.started_at;
                    entry.label = m.label.clone();
                    struck.push(entry.clone());
                }
                let _ = std::fs::remove_file(path);
            }
            self.save(&strikes);
        }
        struck
    }

    /// The app is quitting on purpose: whatever is still in flight was cut short, not
    /// crashed. Remove this process's markers.
    pub fn clean_exit(&self) {
        let prefix = format!("{}-", self.pid);
        let Ok(entries) = std::fs::read_dir(self.inflight_dir()) else { return };
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Keeps a marker on disk while the risky call runs. Dropping it — by return, error or
/// panic — records survival.
pub struct Guard<'a> {
    markers: Option<&'a Markers>,
    path: Option<PathBuf>,
    key: String,
}

impl Guard<'_> {
    /// A guard that protects nothing (no store initialised).
    fn inert() -> Guard<'static> {
        Guard { markers: None, path: None, key: String::new() }
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_file(path);
        }
        if let Some(markers) = self.markers {
            markers.survived(&self.key);
        }
    }
}

/// Whether `pid` is a live process running this same executable. Linux only (via
/// `/proc`); elsewhere every leftover is treated as dead, which at worst strikes a
/// subject that a second instance was still working on.
fn process_is_running_us(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        let Ok(theirs) = std::fs::read_link(format!("/proc/{pid}/exe")) else { return false };
        std::env::current_exe().is_ok_and(|ours| ours == theirs)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        false
    }
}

// ── The process-wide store ────────────────────────────────────────────────────────────

static GLOBAL: OnceLock<Markers> = OnceLock::new();

/// Open the app's store at `dir` and turn the previous run's leftovers into strikes.
/// Call once at startup, before anything that takes a guard. Later calls are no-ops.
pub fn init(dir: &Path) -> Vec<Strikes> {
    let mut struck = Vec::new();
    GLOBAL.get_or_init(|| {
        let m = Markers::open(dir);
        struck = m.recover();
        m
    });
    struck
}

/// [`Markers::enter`] on the app's store; an inert guard before [`init`] (tests, tools).
pub fn enter(kind: &str, subject: &str, label: &str) -> Guard<'static> {
    match GLOBAL.get() {
        Some(m) => m.enter(kind, subject, label),
        None => Guard::inert(),
    }
}

/// [`Markers::blocked`] on the app's store; `None` before [`init`].
pub fn blocked(kind: &str, subject: &str) -> Option<Strikes> {
    GLOBAL.get()?.blocked(kind, subject)
}

/// [`Markers::clean_exit`] on the app's store.
pub fn clean_exit() {
    if let Some(m) = GLOBAL.get() {
        m.clean_exit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> (PathBuf, Markers) {
        let dir = std::env::temp_dir().join(format!("chairphoto-crash-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let m = Markers::open(&dir);
        (dir, m)
    }

    fn inflight_count(dir: &Path) -> usize {
        std::fs::read_dir(dir.join(INFLIGHT)).map(|d| d.count()).unwrap_or(0)
    }

    /// A crash, simulated: the guard is leaked, so its marker stays on disk — exactly what
    /// a process that dies inside the call leaves behind.
    fn crash_inside(m: &Markers, kind: &str, subject: &str) {
        std::mem::forget(m.enter(kind, subject, subject));
    }

    #[test]
    fn a_call_that_returns_leaves_nothing_behind() {
        let (dir, m) = store("returns");
        {
            let _g = m.enter("libraw-decode", "a.ARW", "a.ARW");
            assert_eq!(inflight_count(&dir), 1, "the marker is on disk during the call");
        }
        assert_eq!(inflight_count(&dir), 0);
        assert!(m.recover().is_empty());
        assert!(m.blocked("libraw-decode", "a.ARW").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_second_crash_blocks_and_the_block_survives_a_restart() {
        let (dir, m) = store("blocks");
        crash_inside(&m, "libraw-decode", "bad.ARW");
        let struck = m.recover();
        assert_eq!(struck.len(), 1);
        assert_eq!(struck[0].strikes, 1);
        assert!(m.blocked("libraw-decode", "bad.ARW").is_none(), "one strike must not block");

        crash_inside(&m, "libraw-decode", "bad.ARW");
        let struck = m.recover();
        assert!(struck[0].is_blocked());
        assert!(m.blocked("libraw-decode", "bad.ARW").is_some());
        assert_eq!(inflight_count(&dir), 0, "recovery consumes the markers");

        // A fresh process reads the block back from disk.
        let reopened = Markers::open(&dir);
        assert!(reopened.blocked("libraw-decode", "bad.ARW").is_some());
        // Kinds are separate: a file that crashes the decode may still be probed.
        assert!(reopened.blocked("libraw-probe", "bad.ARW").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn surviving_once_clears_the_strikes() {
        let (dir, m) = store("survives");
        crash_inside(&m, "libraw-decode", "flaky.ARW");
        m.recover();
        {
            // The same subject goes through the call and returns — even with an error.
            let _g = m.enter("libraw-decode", "flaky.ARW", "flaky.ARW");
        }
        crash_inside(&m, "libraw-decode", "flaky.ARW");
        let struck = m.recover();
        assert_eq!(struck[0].strikes, 1, "the survival in between reset the count");
        assert!(m.blocked("libraw-decode", "flaky.ARW").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn two_markers_for_one_subject_from_one_crash_are_one_strike() {
        let (dir, m) = store("twice");
        // Develop and an export decoding the same file when the process died.
        crash_inside(&m, "libraw-decode", "same.ARW");
        crash_inside(&m, "libraw-decode", "same.ARW");
        let struck = m.recover();
        assert_eq!(struck.len(), 1);
        assert_eq!(struck[0].strikes, 1);
        assert!(m.blocked("libraw-decode", "same.ARW").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_clean_quit_mid_call_is_not_a_crash() {
        let (dir, m) = store("quit");
        crash_inside(&m, "libraw-decode", "slow.ARW"); // still decoding when the user quits
        m.clean_exit();
        assert_eq!(inflight_count(&dir), 0);
        assert!(m.recover().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unreadable_markers_are_dropped_without_blame() {
        let (dir, m) = store("torn");
        std::fs::write(dir.join(INFLIGHT).join("999999-0.json"), b"{\"kind\":\"lib").unwrap();
        assert!(m.recover().is_empty());
        assert_eq!(inflight_count(&dir), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_dead_processs_marker_is_recovered() {
        let (dir, m) = store("deadpid");
        let marker = Marker {
            kind: "gpu-init".into(),
            subject: "vulkan:NVIDIA GeForce RTX 3080".into(),
            label: "NVIDIA GeForce RTX 3080".into(),
            pid: u32::MAX - 7, // not a running process
            started_at: 1,
        };
        std::fs::write(dir.join(INFLIGHT).join("x.json"), serde_json::to_vec(&marker).unwrap()).unwrap();
        let struck = m.recover();
        assert_eq!(struck.len(), 1);
        assert_eq!(struck[0].kind, "gpu-init");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The real thing, not a leaked guard: a child process dies inside a guarded call —
    /// no unwinding, no `Drop` — and the parent, playing the next launch, finds the marker.
    /// The child is this same test binary re-run on this one test with an env var set.
    /// It ends with `process::exit`, which, like a segfault, runs no destructors; `abort()`
    /// would do the same but leaves a core dump (and a desktop crash notification) per run.
    #[test]
    fn a_process_that_dies_inside_the_call_leaves_its_marker() {
        const CHILD: &str = "CHAIRPHOTO_CRASH_MARKER_CHILD_DIR";
        if let Ok(dir) = std::env::var(CHILD) {
            let m = Markers::open(dir);
            let _g = m.enter("libraw-decode", "poison.ARW", "poison.ARW");
            std::process::exit(86); // dies here: `_g` is never dropped
        }
        let (dir, _) = store("abort");
        for round in 1..=BLOCK_AFTER {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "crash_marker::tests::a_process_that_dies_inside_the_call_leaves_its_marker"])
                .args(["--test-threads", "1", "--nocapture"])
                .env(CHILD, &dir)
                .output()
                .unwrap()
                .status;
            assert_eq!(status.code(), Some(86), "the child must die inside the call, not return");
            let next_launch = Markers::open(&dir);
            let struck = next_launch.recover();
            assert_eq!(struck.len(), 1, "round {round}: the death left exactly one marker");
            assert_eq!(struck[0].strikes, round);
        }
        assert!(Markers::open(&dir).blocked("libraw-decode", "poison.ARW").is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn without_a_store_guards_are_inert() {
        // The global is never initialised in unit tests.
        let g = enter("libraw-decode", "x", "x");
        assert!(g.path.is_none());
        assert!(blocked("libraw-decode", "x").is_none());
    }
}
