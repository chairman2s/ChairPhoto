//! [`MachinePrefs`]: preferences that belong to this computer, not to the catalog — what the
//! React app kept in the webview's `localStorage` (parity.md, "Settings outside the
//! catalog"). They do not travel with a catalog between machines, and switching catalogs
//! does not change them.
//!
//! One small JSON object of string values, `machine-prefs.json` in the app data dir (beside
//! `recent_catalogs.json`), keyed exactly as React's `localStorage` keys so the meaning of
//! each value is the same: today `appearance.mode` (Preferences → Appearance,
//! [`crate::theme`]) and the Map module's per-host tile answers, `map.tileHosts` (a JSON
//! object; no React counterpart, decision #118), the shell layout (`panel.*`,
//! `panel.section.*`, [`crate::shell::layout_prefs`]), Compare's `panel.compareMode` and the
//! Photo inspector's `inspector.section.*`.
//!
//! **Reads** of the file happen once, in `run()` before the event loop (one small file, like
//! the theme read beside it). A missing or unreadable file is an empty store (logged), never
//! an error: a preference falls back to its default. Tests use [`MachinePrefs::in_memory`],
//! which never writes.
//!
//! # The write model (#214)
//!
//! There is **one intended map** — the values every reader sees — and the file is a copy of
//! it that lags behind. Nothing else holds a value: no snapshot taken at call time, no
//! candidate waiting for a confirmation on the side.
//!
//! - **An immediate change** ([`set`](MachinePrefs::set),
//!   [`modify_then`](MachinePrefs::modify_then)) changes the intended map at once, on the UI
//!   thread, bumps its generation, and queues a write on the [`Runner`].
//! - **A write** serializes the intended map **as it is when the write runs**, never as it was
//!   when it was queued. Writes take the disk lock one at a time and record the generation
//!   they wrote, so a write whose change an earlier-running write already carried skips the
//!   disk. Whatever order the workers run them in, the last write to finish holds the newest
//!   value of every key.
//! - **A durable change** ([`modify_durably`](MachinePrefs::modify_durably)) is a function of
//!   the key's current value, not a value. It runs only inside a write, under the disk lock:
//!   it is applied to the key's newest intended value, the whole map with its result is
//!   written, and only once that file is on disk does the result enter the intended map —
//!   provided the key still holds the value the function saw. If an immediate change to the
//!   key landed while the file was being written, the function is applied again to that
//!   newer value and written again. A store that cannot write (memory only, a failing disk)
//!   never applies it at all.
//! - **A callback** (`then`) is told `Ok` only when the file holds the change: for an
//!   immediate change, this value or a newer value of the key; for a durable change, its
//!   result, applied to the newest value, and visible.
//!
//! The properties this gives, each pinned by a test below:
//!
//! 1. **The newest intent per key wins**, in memory and on disk, whatever order the writes
//!    complete in. An immediate change is visible at once; a durable change never replaces a
//!    newer value, because it is computed from the newest one when it is applied.
//! 2. **No write loses a value.** Every write serializes the whole intended map at the
//!    moment it runs, and a durable change is in that map before the disk lock is released,
//!    so no later write can leave it out.
//! 3. **A durable change is never visible, or written by anyone else, before it is on disk.**
//!    It is not in the intended map until its own write succeeds, so neither a read nor
//!    another key's write can see it; on a store that cannot write, it never applies.
//! 4. **No false success.** `then(Ok)` means the file holds it (see above).
//!
//! An immediate change is **optimistic**: if its write fails, the value stays in memory (a
//! theme, a panel layout, or the user's own tile answer should not flicker back) and the
//! next write that succeeds — of any key — carries it. The one exception to "newest on disk"
//! is a disk that starts failing mid-change: if an immediate change to a key lands while a
//! durable change's write of it is under way, and the re-applied write then fails, the file
//! keeps the durable result (never made visible) until a write succeeds. The immediate
//! change's own write fails too, and its caller is told `Err`.
//!
//! **Lock order:** the disk lock (held across each write, on a worker only), then the state
//! lock (held only to read or change the intended map, never across I/O — the UI thread
//! takes it, never the disk lock). A durable change's function runs with the disk lock held
//! and the state lock released.

use crate::storage::Runner;
use gpui_kit::{App, Global};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

/// Told whether a change reached the disk.
type Saved = Box<dyn FnOnce(Result<(), String>) + Send>;

/// The file in the app data dir.
pub const FILE_NAME: &str = "machine-prefs.json";

/// How often a durable change re-applies itself to a key that changed under its write before
/// it gives up (`Err`). Each round needs another immediate change to land during one small
/// file write.
const DURABLE_ROUNDS: usize = 8;

/// The per-machine preference store. A GPUI global, installed by `wire`. Clones share one
/// store.
#[derive(Clone, Default)]
pub struct MachinePrefs {
    /// Where it persists; `None` = memory only.
    path: Option<PathBuf>,
    shared: Arc<Shared>,
}

#[derive(Default)]
struct Shared {
    /// The intended map. Never held across I/O.
    state: Mutex<State>,
    /// Held across each write: the generation of the intended map the file holds — every
    /// change at or below it is in the file, or replaced there by a newer value of its key.
    disk: Mutex<u64>,
}

#[derive(Default)]
struct State {
    values: BTreeMap<String, String>,
    /// Bumped on every change to `values`.
    generation: u64,
}

impl Global for MachinePrefs {}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl MachinePrefs {
    /// A store that never touches disk (tests, or no app data dir).
    pub fn in_memory() -> Self {
        MachinePrefs::default()
    }

    /// Read the store at `path`. Missing: empty. Unreadable or not a JSON object of strings:
    /// empty, logged — and the next write replaces it.
    pub fn load(path: PathBuf) -> Self {
        let values = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<BTreeMap<String, String>>(&text).unwrap_or_else(|e| {
                eprintln!("machine prefs: ignoring {}: {e}", path.display());
                BTreeMap::new()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => {
                eprintln!("machine prefs: cannot read {}: {e}", path.display());
                BTreeMap::new()
            }
        };
        let shared = Shared { state: Mutex::new(State { values, generation: 0 }), disk: Mutex::new(0) };
        MachinePrefs { path: Some(path), shared: Arc::new(shared) }
    }

    /// The store in the app data dir, or memory only when there is none (logged).
    pub fn load_default() -> Self {
        match chairphoto_core::app::app_data_dir() {
            Ok(dir) => Self::load(dir.join(FILE_NAME)),
            Err(e) => {
                eprintln!("machine prefs: no app data dir ({e}); preferences last this session only");
                Self::in_memory()
            }
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The intended value of `key`.
    pub fn get(&self, key: &str) -> Option<String> {
        lock(&self.shared.state).values.get(key).cloned()
    }

    /// The installed store's intended value for `key`.
    pub fn read(cx: &App, key: &str) -> Option<String> {
        cx.try_global::<MachinePrefs>().and_then(|p| p.get(key))
    }

    /// Set `key` at once and persist the store off the UI thread (an immediate change; see the
    /// module docs). An unchanged value writes nothing. No store installed: nothing.
    pub fn set(cx: &mut App, key: &str, value: &str) {
        if cx.try_global::<MachinePrefs>().is_none_or(|p| p.get(key).as_deref() == Some(value)) {
            return;
        }
        let value = value.to_string();
        Self::modify(cx, key, move |_| value, None);
    }

    /// An immediate change computed from the key's current value, atomically (no other
    /// change to the key can land between the read and the write of it): `key` becomes
    /// `change(current)` at once, and `then(saved)` runs off the UI thread once the write is
    /// over — `Ok` when the file holds this value or a newer one of the key; `Err` when the
    /// write failed, or nothing persists (no store installed, or memory only). Writes even an
    /// unchanged value: an earlier write of it may have failed. Returns the new value (`None`:
    /// no store installed).
    pub fn modify_then(
        cx: &mut App,
        key: &str,
        change: impl FnOnce(Option<&str>) -> String,
        then: impl FnOnce(Result<(), String>) + Send + 'static,
    ) -> Option<String> {
        Self::modify(cx, key, change, Some(Box::new(then)))
    }

    fn modify(cx: &mut App, key: &str, change: impl FnOnce(Option<&str>) -> String, then: Option<Saved>) -> Option<String> {
        let runner = Runner::get(cx);
        let Some(prefs) = cx.try_global::<MachinePrefs>().cloned() else {
            fail(&runner, then, "no preference store is installed");
            return None;
        };
        let (value, generation) = {
            let mut state = lock(&prefs.shared.state);
            let value = change(state.values.get(key).map(String::as_str));
            state.values.insert(key.to_string(), value.clone());
            state.generation += 1;
            (value, state.generation)
        };
        let Some(path) = prefs.path.clone() else {
            fail(&runner, then, "this machine's preferences are kept in memory only");
            return Some(value);
        };
        runner.spawn(move || {
            let saved = prefs.shared.write_covering(&path, generation);
            if let Some(then) = then {
                then(saved);
            }
        });
        Some(value)
    }

    /// A durable change (see the module docs): `change` is applied to `key`'s newest value
    /// inside a write, off the UI thread, and its result becomes visible only once the file
    /// holds it. If an immediate change to the key lands while that file is being written,
    /// `change` is applied again to the newer value and written again — so it must be a
    /// function of the value it is given that respects what is already there (the Map
    /// module's legacy-consent merge, which never overrides an answer the user gave meanwhile).
    /// `then` runs off the UI thread after the write, and may block: `Ok` only once the result
    /// is on disk and visible; `Err` when the write failed or nothing persists (no store
    /// installed, memory only), and then nothing changed.
    pub fn modify_durably(
        cx: &mut App,
        key: &str,
        change: impl Fn(Option<&str>) -> String + Send + 'static,
        then: impl FnOnce(Result<(), String>) + Send + 'static,
    ) {
        let runner = Runner::get(cx);
        let then: Saved = Box::new(then);
        let Some(prefs) = cx.try_global::<MachinePrefs>().cloned() else {
            return fail(&runner, Some(then), "no preference store is installed");
        };
        let Some(path) = prefs.path.clone() else {
            return fail(&runner, Some(then), "this machine's preferences are kept in memory only");
        };
        let key = key.to_string();
        runner.spawn(move || {
            let saved = prefs.shared.write_durably(&path, &key, &change);
            then(saved);
        });
    }
}

/// Tell `then`, off the UI thread, that nothing was saved.
fn fail(runner: &Runner, then: Option<Saved>, why: &'static str) {
    if let Some(then) = then {
        runner.spawn(move || then(Err(why.into())));
    }
}

impl Shared {
    /// Make the file hold every change up to `generation`: nothing to do if a write that ran
    /// meanwhile already did; otherwise write the intended map as it is now.
    fn write_covering(&self, path: &Path, generation: u64) -> Result<(), String> {
        let mut on_disk = lock(&self.disk);
        if *on_disk >= generation {
            return Ok(()); // a write that ran after this change carried it, or a newer value
        }
        let (values, now) = {
            let state = lock(&self.state);
            (state.values.clone(), state.generation)
        };
        write_atomically(path, &values).map_err(|e| failed(path, e))?;
        *on_disk = (*on_disk).max(now);
        Ok(())
    }

    /// Apply `change` to `key`'s newest value, write the whole map with its result, and only
    /// then make the result visible — if the key still holds what `change` saw; otherwise do
    /// it again from the newer value.
    fn write_durably(&self, path: &Path, key: &str, change: &dyn Fn(Option<&str>) -> String) -> Result<(), String> {
        let mut on_disk = lock(&self.disk);
        for _ in 0..DURABLE_ROUNDS {
            let (mut values, seen) = {
                let state = lock(&self.state);
                (state.values.clone(), state.generation)
            };
            let base = values.get(key).cloned();
            let result = change(base.as_deref()); // state lock released: may take its own
            values.insert(key.to_string(), result.clone());
            write_atomically(path, &values).map_err(|e| failed(path, e))?;
            let mut state = lock(&self.state);
            if state.values.get(key) != base.as_ref() {
                continue; // an immediate change to the key landed during the write: newer intent
            }
            // The file holds the map as of `seen` with the result in place of `base`.
            let exact = state.generation == seen;
            if base.as_ref() != Some(&result) {
                state.values.insert(key.to_string(), result);
                state.generation += 1;
            }
            *on_disk = (*on_disk).max(if exact { state.generation } else { seen });
            return Ok(());
        }
        Err(format!("{key} kept changing while it was being saved"))
    }
}

fn failed(path: &Path, e: std::io::Error) -> String {
    eprintln!("machine prefs: cannot write {}: {e}", path.display());
    format!("cannot write {}: {e}", path.display())
}

/// Write `values` to `path` through a temporary file renamed into place, so a crash leaves
/// the old file or the new one, never half of one.
fn write_atomically(path: &Path, values: &BTreeMap<String, String>) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(values).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(test)]
impl MachinePrefs {
    /// Tests: what an immediate change does to the intended map, without its write — for a
    /// durable change's function to simulate one landing in the middle of its write.
    fn change_now(&self, key: &str, value: &str) {
        let mut state = lock(&self.shared.state);
        state.values.insert(key.to_string(), value.to_string());
        state.generation += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::TestAppContext;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let dir = std::env::temp_dir().join(format!("cp-mprefs-{tag}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// What a fresh launch over `path` reads for `key`.
    fn disk(path: &Path, key: &str) -> Option<String> {
        MachinePrefs::load(path.to_path_buf()).get(key)
    }

    fn read(cx: &mut TestAppContext, key: &str) -> Option<String> {
        cx.update(|cx| MachinePrefs::read(cx, key))
    }

    /// Every `then` result, by label, in the order they arrived.
    #[derive(Clone, Default)]
    struct Told(Arc<Mutex<Vec<(&'static str, Result<(), String>)>>>);

    impl Told {
        fn cb(&self, label: &'static str) -> impl FnOnce(Result<(), String>) + Send + 'static {
            let told = self.0.clone();
            move |saved| told.lock().unwrap().push((label, saved))
        }

        fn get(&self, label: &str) -> Option<Result<(), String>> {
            self.0.lock().unwrap().iter().find(|(l, _)| *l == label).map(|(_, s)| s.clone())
        }
    }

    /// A store whose every write fails (its directory is a file) until `unblock`.
    fn failing_store(dir: &TempDir) -> (PathBuf, PathBuf) {
        let blocker = dir.0.join("blocker");
        std::fs::write(&blocker, "a file where the store's directory should be").unwrap();
        (blocker.join(FILE_NAME), blocker)
    }

    /// The legacy-merge shape of a durable change: fills the key only if it has no value, so
    /// it never replaces a newer intent.
    fn fill(value: &'static str) -> impl Fn(Option<&str>) -> String + Send + 'static {
        move |current| current.unwrap_or(value).to_string()
    }

    fn install(cx: &mut TestAppContext, prefs: MachinePrefs) {
        cx.update(|cx| {
            cx.set_global(Runner::manual());
            cx.set_global(prefs);
        });
    }

    fn run(cx: &mut TestAppContext) -> usize {
        cx.update(|cx| Runner::get(cx).run_pending())
    }

    fn run_reversed(cx: &mut TestAppContext) -> usize {
        cx.update(|cx| Runner::get(cx).run_pending_reversed())
    }

    // --- immediate changes ---------------------------------------------------------------

    /// A set is visible at once and on disk only once the runner ran; a fresh load reads it.
    #[gpui_kit::test]
    fn a_set_persists_off_the_ui_thread_and_loads_back(cx: &mut TestAppContext) {
        let dir = TempDir::new("roundtrip");
        let path = dir.0.join("sub").join(FILE_NAME);
        install(cx, MachinePrefs::load(path.clone()));
        cx.update(|cx| MachinePrefs::set(cx, "appearance.mode", "standard"));
        assert_eq!(read(cx, "appearance.mode").as_deref(), Some("standard"));
        assert!(!path.exists(), "nothing written on the UI thread");
        assert_eq!(run(cx), 1);
        assert_eq!(disk(&path, "appearance.mode").as_deref(), Some("standard"));
        // Setting the same value again writes nothing.
        cx.update(|cx| MachinePrefs::set(cx, "appearance.mode", "standard"));
        assert_eq!(cx.update(|cx| Runner::get(cx).pending()), 0);
    }

    /// **Forced interleaving.** Two sets queue two writes; the newer runs first, then the
    /// older: the file keeps the newer value.
    #[gpui_kit::test]
    fn an_older_write_never_overwrites_a_newer_one(cx: &mut TestAppContext) {
        let dir = TempDir::new("order");
        let path = dir.0.join(FILE_NAME);
        let (first, second) = (Runner::manual(), Runner::manual());
        cx.update(|cx| {
            cx.set_global(MachinePrefs::load(path.clone()));
            cx.set_global(first.clone());
            MachinePrefs::set(cx, "appearance.mode", "standard");
            cx.set_global(second.clone());
            MachinePrefs::set(cx, "appearance.mode", "follow-omarchy");
        });
        assert_eq!(second.run_pending(), 1);
        assert_eq!(first.run_pending(), 1);
        assert_eq!(disk(&path, "appearance.mode").as_deref(), Some("follow-omarchy"));
    }

    /// Property 2, immediate changes: two keys' writes run newest first. Each write carries
    /// the whole map as it is when it runs, so the file has both, and each `then(Ok)` is
    /// true.
    #[gpui_kit::test]
    fn writes_to_different_keys_completing_out_of_order_keep_both(cx: &mut TestAppContext) {
        let dir = TempDir::new("two-keys");
        let path = dir.0.join(FILE_NAME);
        install(cx, MachinePrefs::load(path.clone()));
        let told = Told::default();
        cx.update(|cx| {
            MachinePrefs::modify_then(cx, "a", |_| "1".into(), told.cb("a"));
            MachinePrefs::modify_then(cx, "b", |_| "2".into(), told.cb("b"));
        });
        assert_eq!(run_reversed(cx), 2);
        assert_eq!(told.get("a"), Some(Ok(())));
        assert_eq!(told.get("b"), Some(Ok(())));
        assert_eq!(disk(&path, "a").as_deref(), Some("1"));
        assert_eq!(disk(&path, "b").as_deref(), Some("2"));
    }

    /// An immediate change is optimistic: a failed write keeps the value in memory, says
    /// `Err`, and the next write that succeeds — of another key — carries it.
    #[gpui_kit::test]
    fn a_failed_immediate_write_stays_in_memory_says_err_and_rides_the_next_write(cx: &mut TestAppContext) {
        let dir = TempDir::new("immediate-fail");
        let (path, blocker) = failing_store(&dir);
        install(cx, MachinePrefs::load(path.clone()));
        let told = Told::default();
        cx.update(|cx| MachinePrefs::modify_then(cx, "panel.leftW", |_| "300".into(), told.cb("left")));
        assert_eq!(read(cx, "panel.leftW").as_deref(), Some("300"), "visible at once");
        run(cx);
        assert!(matches!(told.get("left"), Some(Err(_))), "no false success");
        assert_eq!(read(cx, "panel.leftW").as_deref(), Some("300"), "kept in memory");

        std::fs::remove_file(&blocker).unwrap();
        cx.update(|cx| MachinePrefs::set(cx, "appearance.mode", "standard"));
        run(cx);
        assert_eq!(disk(&path, "panel.leftW").as_deref(), Some("300"), "carried by the next write");
    }

    // --- durable changes: the #214 matrix ------------------------------------------------

    /// Property 3: a durable change is not visible before its write runs, nor while it waits.
    /// Once the write lands, it is on disk and visible, and `then` says `Ok`.
    #[gpui_kit::test]
    fn a_durable_change_is_invisible_until_its_write_lands(cx: &mut TestAppContext) {
        let dir = TempDir::new("durable-visible");
        let path = dir.0.join(FILE_NAME);
        install(cx, MachinePrefs::load(path.clone()));
        let told = Told::default();
        cx.update(|cx| MachinePrefs::modify_durably(cx, "map.tileHosts", fill("merged"), told.cb("merge")));
        assert_eq!(read(cx, "map.tileHosts"), None, "not visible before its write runs");
        assert!(!path.exists(), "nothing written on the UI thread");
        assert_eq!(run(cx), 1);
        assert_eq!(told.get("merge"), Some(Ok(())));
        assert_eq!(read(cx, "map.tileHosts").as_deref(), Some("merged"), "visible once on disk");
        assert_eq!(disk(&path, "map.tileHosts").as_deref(), Some("merged"));
    }

    /// Property 3 (P214b at this level): a durable change whose write fails never applies —
    /// not readable, and not carried to disk by a later write of another key that succeeds.
    #[gpui_kit::test]
    fn a_failed_durable_change_never_applies_nor_rides_a_later_write(cx: &mut TestAppContext) {
        let dir = TempDir::new("durable-fail");
        let (path, blocker) = failing_store(&dir);
        install(cx, MachinePrefs::load(path.clone()));
        let told = Told::default();
        cx.update(|cx| MachinePrefs::modify_durably(cx, "map.tileHosts", fill("merged"), told.cb("merge")));
        run(cx);
        assert!(matches!(told.get("merge"), Some(Err(_))));
        assert_eq!(read(cx, "map.tileHosts"), None, "never applied");

        std::fs::remove_file(&blocker).unwrap();
        cx.update(|cx| MachinePrefs::set(cx, "appearance.mode", "standard"));
        run(cx);
        assert_eq!(disk(&path, "map.tileHosts"), None, "the failed change never reached disk");
        assert_eq!(disk(&path, "appearance.mode").as_deref(), Some("standard"), "the other key saved");
    }

    /// Property 3, memory only: a durable change says `Err` and never applies; an immediate
    /// change applies (and says `Err`: it will not survive a restart).
    #[gpui_kit::test]
    fn an_in_memory_store_never_applies_a_durable_change(cx: &mut TestAppContext) {
        install(cx, MachinePrefs::in_memory());
        let told = Told::default();
        cx.update(|cx| {
            MachinePrefs::modify_durably(cx, "map.tileHosts", fill("merged"), told.cb("merge"));
            MachinePrefs::modify_then(cx, "appearance.mode", |_| "standard".into(), told.cb("mode"));
        });
        run(cx);
        assert!(matches!(told.get("merge"), Some(Err(_))));
        assert!(matches!(told.get("mode"), Some(Err(_))));
        assert_eq!(read(cx, "map.tileHosts"), None, "never applied");
        assert_eq!(read(cx, "appearance.mode").as_deref(), Some("standard"));
    }

    /// Property 3, the other half: another key's write that runs *before* a queued durable
    /// change does not carry it — it is not in the intended map yet.
    #[gpui_kit::test]
    fn another_keys_write_running_first_does_not_carry_a_queued_durable_change(cx: &mut TestAppContext) {
        let dir = TempDir::new("durable-not-carried");
        let path = dir.0.join(FILE_NAME);
        install(cx, MachinePrefs::load(path.clone()));
        cx.update(|cx| MachinePrefs::modify_durably(cx, "map.tileHosts", fill("merged"), |_| {}));
        let held = cx.update(|cx| Runner::get(cx).hold_pending());
        cx.update(|cx| MachinePrefs::set(cx, "appearance.mode", "standard"));
        assert_eq!(run(cx), 1, "the other key's write runs first");
        assert_eq!(disk(&path, "map.tileHosts"), None, "it did not write the pending change");
        assert_eq!(disk(&path, "appearance.mode").as_deref(), Some("standard"));
        cx.update(|cx| Runner::get(cx).release(held));
        run(cx);
        assert_eq!(disk(&path, "map.tileHosts").as_deref(), Some("merged"));
        assert_eq!(disk(&path, "appearance.mode").as_deref(), Some("standard"), "and it kept the other key");
    }

    /// P214d, properties 2 and 4: a durable change, then a set of another key, run in call
    /// order. `then(Ok)` is true: the file holds the change after both writes.
    #[gpui_kit::test]
    fn p214d_a_set_of_another_key_after_a_durable_change_keeps_it_on_disk(cx: &mut TestAppContext) {
        p214d(false, cx);
    }

    /// P214d, run newest first.
    #[gpui_kit::test]
    fn p214d_reversed(cx: &mut TestAppContext) {
        p214d(true, cx);
    }

    fn p214d(reversed: bool, cx: &mut TestAppContext) {
        let dir = TempDir::new("p214d");
        let path = dir.0.join(FILE_NAME);
        install(cx, MachinePrefs::load(path.clone()));
        let told = Told::default();
        cx.update(|cx| {
            MachinePrefs::modify_durably(cx, "map.tileHosts", fill("merged"), told.cb("merge"));
            MachinePrefs::set(cx, "appearance.mode", "standard");
        });
        if reversed { run_reversed(cx) } else { run(cx) };
        assert_eq!(told.get("merge"), Some(Ok(())));
        assert_eq!(disk(&path, "map.tileHosts").as_deref(), Some("merged"), "then said Ok: on disk");
        assert_eq!(disk(&path, "appearance.mode").as_deref(), Some("standard"));
        assert_eq!(read(cx, "map.tileHosts").as_deref(), Some("merged"), "memory agrees with disk");
    }

    /// P214e, property 1: an immediate change of the key after a queued durable change — the
    /// user's newer answer. The durable change is applied to it, not instead of it: memory and
    /// disk keep the newer value, in either run order.
    #[gpui_kit::test]
    fn p214e_a_later_set_of_the_key_is_never_overwritten_by_an_earlier_durable_change(cx: &mut TestAppContext) {
        for reversed in [false, true] {
            let dir = TempDir::new("p214e");
            let path = dir.0.join(FILE_NAME);
            install(cx, MachinePrefs::load(path.clone()));
            let told = Told::default();
            cx.update(|cx| {
                MachinePrefs::modify_durably(cx, "map.tileHosts", fill("old-merged-allow"), told.cb("merge"));
                MachinePrefs::modify_then(cx, "map.tileHosts", |_| "user-block".into(), told.cb("user"));
            });
            if reversed { run_reversed(cx) } else { run(cx) };
            assert_eq!(read(cx, "map.tileHosts").as_deref(), Some("user-block"), "reversed={reversed}");
            assert_eq!(disk(&path, "map.tileHosts").as_deref(), Some("user-block"), "reversed={reversed}");
            assert_eq!(told.get("merge"), Some(Ok(())));
            assert_eq!(told.get("user"), Some(Ok(())));
        }
    }

    /// Property 1, the tightest window: an immediate change of the key lands *during* the
    /// durable change's write (after it read the key, before it could apply). The durable
    /// change does not apply its stale result; it re-applies itself to the newer value and
    /// writes again, so memory and disk both end on the newer intent, and `Ok` is true.
    #[gpui_kit::test]
    fn an_immediate_change_landing_during_a_durable_write_wins_and_the_change_reapplies(cx: &mut TestAppContext) {
        let dir = TempDir::new("mid-write");
        let path = dir.0.join(FILE_NAME);
        let prefs = MachinePrefs::load(path.clone());
        install(cx, prefs.clone());
        let seen = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let told = Told::default();
        {
            let seen = seen.clone();
            cx.update(|cx| {
                MachinePrefs::modify_durably(
                    cx,
                    "k",
                    move |current| {
                        let first = seen.lock().unwrap().is_empty();
                        seen.lock().unwrap().push(current.map(str::to_string));
                        if first {
                            prefs.change_now("k", "user"); // lands while this write is under way
                        }
                        format!("{}+merged", current.unwrap_or("none"))
                    },
                    told.cb("merge"),
                )
            });
        }
        run(cx);
        assert_eq!(*seen.lock().unwrap(), vec![None, Some("user".to_string())], "re-applied to the newer value");
        assert_eq!(read(cx, "k").as_deref(), Some("user+merged"), "the stale result never applied");
        assert_eq!(disk(&path, "k").as_deref(), Some("user+merged"), "and disk matches memory");
        assert_eq!(told.get("merge"), Some(Ok(())));
    }

    /// Property 4, the skip: a durable write records only what its file holds. A change to
    /// another key that lands during it (after it read the map) is not in that file, so the
    /// change's own write must still run — not be skipped as "already on disk" and told `Ok`.
    #[gpui_kit::test]
    fn a_durable_write_does_not_claim_a_change_that_landed_during_it(cx: &mut TestAppContext) {
        let dir = TempDir::new("no-overclaim");
        let path = dir.0.join(FILE_NAME);
        let prefs = MachinePrefs::load(path.clone());
        install(cx, prefs.clone());
        let during = Arc::new(Mutex::new(None::<u64>));
        {
            let (prefs, during) = (prefs.clone(), during.clone());
            cx.update(|cx| {
                MachinePrefs::modify_durably(
                    cx,
                    "map.tileHosts",
                    move |current| {
                        if during.lock().unwrap().is_none() {
                            prefs.change_now("appearance.mode", "standard");
                            *during.lock().unwrap() = Some(lock(&prefs.shared.state).generation);
                        }
                        current.unwrap_or("merged").to_string()
                    },
                    |_| {},
                )
            });
        }
        run(cx);
        assert_eq!(disk(&path, "map.tileHosts").as_deref(), Some("merged"));
        assert_eq!(disk(&path, "appearance.mode"), None, "it landed after the durable write read the map");
        // The write queued for that change now runs (here, directly): it must write.
        let generation = during.lock().unwrap().unwrap();
        assert_eq!(prefs.shared.write_covering(&path, generation), Ok(()));
        assert_eq!(disk(&path, "appearance.mode").as_deref(), Some("standard"), "its own write was not skipped");
        assert_eq!(disk(&path, "map.tileHosts").as_deref(), Some("merged"));
    }

    /// P214f, properties 2 and 4: two durable changes to different keys whose writes run
    /// newest first. Both say `Ok`, and both are on disk and visible.
    #[gpui_kit::test]
    fn p214f_two_durable_changes_to_different_keys_out_of_order(cx: &mut TestAppContext) {
        let dir = TempDir::new("p214f");
        let path = dir.0.join(FILE_NAME);
        install(cx, MachinePrefs::load(path.clone()));
        let told = Told::default();
        cx.update(|cx| {
            MachinePrefs::modify_durably(cx, "a", fill("1"), told.cb("a"));
            MachinePrefs::modify_durably(cx, "b", fill("2"), told.cb("b"));
        });
        assert_eq!(run_reversed(cx), 2);
        for (key, value) in [("a", "1"), ("b", "2")] {
            assert_eq!(told.get(key), Some(Ok(())), "{key}");
            assert_eq!(disk(&path, key).as_deref(), Some(value), "{key}: then said Ok, so on disk");
            assert_eq!(read(cx, key).as_deref(), Some(value), "{key}: and visible");
        }
    }

    /// Two durable changes to the same key, out of order: each is applied to the result of
    /// the other, so neither is lost.
    #[gpui_kit::test]
    fn two_durable_changes_to_the_same_key_out_of_order_compose(cx: &mut TestAppContext) {
        let dir = TempDir::new("same-key");
        let path = dir.0.join(FILE_NAME);
        install(cx, MachinePrefs::load(path.clone()));
        let add = |part: &'static str| {
            move |current: Option<&str>| {
                let mut parts: Vec<&str> = current.unwrap_or("").split(',').filter(|p| !p.is_empty()).collect();
                parts.push(part);
                parts.sort();
                parts.join(",")
            }
        };
        let told = Told::default();
        cx.update(|cx| {
            MachinePrefs::modify_durably(cx, "k", add("x"), told.cb("x"));
            MachinePrefs::modify_durably(cx, "k", add("y"), told.cb("y"));
        });
        run_reversed(cx);
        assert_eq!(read(cx, "k").as_deref(), Some("x,y"));
        assert_eq!(disk(&path, "k").as_deref(), Some("x,y"));
        assert_eq!((told.get("x"), told.get("y")), (Some(Ok(())), Some(Ok(()))));
    }

    /// A restart: a new store over the same file reads every value the last writes left —
    /// immediate and durable alike.
    #[gpui_kit::test]
    fn a_restart_reads_back_immediate_and_durable_changes(cx: &mut TestAppContext) {
        let dir = TempDir::new("restart");
        let path = dir.0.join(FILE_NAME);
        install(cx, MachinePrefs::load(path.clone()));
        cx.update(|cx| {
            MachinePrefs::modify_durably(cx, "map.tileHosts", fill("merged"), |_| {});
            MachinePrefs::set(cx, "appearance.mode", "standard");
        });
        run_reversed(cx);
        install(cx, MachinePrefs::load(path.clone()));
        assert_eq!(read(cx, "map.tileHosts").as_deref(), Some("merged"));
        assert_eq!(read(cx, "appearance.mode").as_deref(), Some("standard"));
    }

    /// A broken file is an empty store, not an error; an in-memory store never writes.
    #[gpui_kit::test]
    fn a_broken_file_reads_as_empty_and_memory_never_writes(cx: &mut TestAppContext) {
        let dir = TempDir::new("broken");
        let path = dir.0.join(FILE_NAME);
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(disk(&path, "appearance.mode"), None);
        install(cx, MachinePrefs::in_memory());
        cx.update(|cx| MachinePrefs::set(cx, "appearance.mode", "standard"));
        assert_eq!(read(cx, "appearance.mode").as_deref(), Some("standard"));
        assert_eq!(cx.update(|cx| Runner::get(cx).pending()), 0);
    }
}
