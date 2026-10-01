//! [`MachinePrefs`]: preferences that belong to this computer, not to the catalog — what the
//! React app kept in the webview's `localStorage` (parity.md, "Settings outside the
//! catalog"). They do not travel with a catalog between machines, and switching catalogs
//! does not change them.
//!
//! One small JSON object of string values, `machine-prefs.json` in the app data dir (beside
//! `recent_catalogs.json`), keyed exactly as React's `localStorage` keys so the meaning of
//! each value is the same: today `appearance.mode` (Preferences → Appearance,
//! [`crate::theme`]). The React layout keys (`panel.*`) are not stored yet.
//!
//! **Reads** happen once, in `run()` before the event loop (one small file, like the theme
//! read beside it). **Writes** never touch disk on the UI thread: [`MachinePrefs::set`]
//! updates the in-memory value and hands a snapshot to the [`Runner`]; each snapshot carries
//! a sequence number and an older snapshot never overwrites a newer one, however the workers
//! interleave. A missing or unreadable file is an empty store (logged), never an error: a
//! preference falls back to its default. Tests use [`MachinePrefs::in_memory`], which never
//! writes.

use crate::storage::Runner;
use gpui_kit::{App, Global};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The file in the app data dir.
pub const FILE_NAME: &str = "machine-prefs.json";

/// The per-machine preference store. A GPUI global, installed by `wire`.
#[derive(Clone, Default)]
pub struct MachinePrefs {
    /// Where it persists; `None` = memory only.
    path: Option<PathBuf>,
    values: BTreeMap<String, String>,
    /// The sequence number of the last snapshot handed out, and of the last one written.
    seq: u64,
    written: Arc<Mutex<u64>>,
}

impl Global for MachinePrefs {}

impl MachinePrefs {
    /// A store that never touches disk (tests, or no app data dir).
    pub fn in_memory() -> Self {
        MachinePrefs::default()
    }

    /// Read the store at `path`. Missing: empty. Unreadable or not a JSON object of strings:
    /// empty, logged — and the next [`set`](Self::set) replaces it.
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
        MachinePrefs { path: Some(path), values, ..Default::default() }
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

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    /// The installed store's value for `key`.
    pub fn read(cx: &App, key: &str) -> Option<String> {
        cx.try_global::<MachinePrefs>().and_then(|p| p.get(key).map(str::to_string))
    }

    /// Set `key` and persist the whole store off the UI thread. No store installed: nothing.
    pub fn set(cx: &mut App, key: &str, value: &str) {
        if !cx.has_global::<MachinePrefs>() {
            return;
        }
        let runner = Runner::get(cx);
        let prefs = cx.global_mut::<MachinePrefs>();
        if prefs.get(key) == Some(value) {
            return;
        }
        prefs.values.insert(key.to_string(), value.to_string());
        let Some(path) = prefs.path.clone() else { return };
        prefs.seq += 1;
        let (seq, snapshot, written) = (prefs.seq, prefs.values.clone(), prefs.written.clone());
        runner.spawn(move || {
            // Held across the write, so two snapshots never write at once and an older one
            // that arrives late is skipped rather than undoing a newer one.
            let mut last = written.lock().unwrap_or_else(|e| e.into_inner());
            if seq <= *last {
                return;
            }
            match write_atomically(&path, &snapshot) {
                Ok(()) => *last = seq,
                Err(e) => eprintln!("machine prefs: cannot write {}: {e}", path.display()),
            }
        });
    }
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

    /// A set is visible at once and on disk only once the runner ran; a fresh load reads it.
    #[gpui_kit::test]
    fn a_set_persists_off_the_ui_thread_and_loads_back(cx: &mut TestAppContext) {
        let dir = TempDir::new("roundtrip");
        let path = dir.0.join("sub").join(FILE_NAME);
        cx.update(|cx| {
            cx.set_global(Runner::manual());
            cx.set_global(MachinePrefs::load(path.clone()));
            MachinePrefs::set(cx, "appearance.mode", "standard");
            assert_eq!(MachinePrefs::read(cx, "appearance.mode").as_deref(), Some("standard"));
        });
        assert!(!path.exists(), "nothing written on the UI thread");
        assert_eq!(cx.update(|cx| Runner::get(cx).run_pending()), 1);
        assert_eq!(MachinePrefs::load(path.clone()).get("appearance.mode"), Some("standard"));
        // Setting the same value again writes nothing.
        cx.update(|cx| MachinePrefs::set(cx, "appearance.mode", "standard"));
        assert_eq!(cx.update(|cx| Runner::get(cx).pending()), 0);
    }

    /// **Forced interleaving.** Two sets queue two snapshots; the newer runs first, then the
    /// older: the file keeps the newer value.
    #[gpui_kit::test]
    fn an_older_snapshot_never_overwrites_a_newer_one(cx: &mut TestAppContext) {
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
        assert_eq!(MachinePrefs::load(path).get("appearance.mode"), Some("follow-omarchy"));
    }

    /// A broken file is an empty store, not an error; an in-memory store never writes.
    #[gpui_kit::test]
    fn a_broken_file_reads_as_empty_and_memory_never_writes(cx: &mut TestAppContext) {
        let dir = TempDir::new("broken");
        let path = dir.0.join(FILE_NAME);
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(MachinePrefs::load(path).get("appearance.mode"), None);
        cx.update(|cx| {
            cx.set_global(Runner::manual());
            cx.set_global(MachinePrefs::in_memory());
            MachinePrefs::set(cx, "appearance.mode", "standard");
            assert_eq!(MachinePrefs::read(cx, "appearance.mode").as_deref(), Some("standard"));
            assert_eq!(Runner::get(cx).pending(), 0);
        });
    }
}
