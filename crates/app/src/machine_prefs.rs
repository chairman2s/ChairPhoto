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
//! **Reads** happen once, in `run()` before the event loop (one small file, like the theme
//! read beside it). **Writes** never touch disk on the UI thread: [`MachinePrefs::set`]
//! updates the in-memory value and hands a snapshot to the [`Runner`]; each snapshot carries
//! a sequence number and an older snapshot never overwrites a newer one, however the workers
//! interleave. A missing or unreadable file is an empty store (logged), never an error: a
//! preference falls back to its default. Tests use [`MachinePrefs::in_memory`], which never
//! writes.
//!
//! **Visibility of an unconfirmed value** (#214). `set` and `set_then` are optimistic: the
//! candidate goes into `values` — readable by [`get`](MachinePrefs::get)/
//! [`read`](MachinePrefs::read), and included in the snapshot of any *other* key's write that
//! happens to follow it — before the write is even attempted, and it stays there even if that
//! write fails; a setting the user just changed (a theme, a panel layout, or the Map
//! module's own `set_consent` — "applied at once" by its own doc, deliberately not held back
//! the way the legacy migration is) should neither disappear nor flicker back just because a
//! transient write failed, and a later retry (of the same value, or of anything else) is what
//! finally lands it. [`MachinePrefs::set_confirmed_then`] is for the one caller
//! that must not act on an unconfirmed value at all — the Map module's legacy-consent
//! migration, which may not re-admit a host the user never actually got to durably allow: its
//! candidate is kept out of `values` (so neither a plain read nor a later write of a different
//! key can see or persist it) until this exact write is confirmed durable, at which point it
//! lands in `values` too, back on the UI thread. A write that fails simply never inserts it —
//! isolated until confirmed, nothing to roll back.

use crate::storage::Runner;
use gpui_kit::{App, Global};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Told whether a write reached the disk ([`MachinePrefs::set_then`]).
type Saved = Box<dyn FnOnce(Result<(), String>) + Send>;

/// [`MachinePrefs::set`]/[`MachinePrefs::set_then`] vs.
/// [`MachinePrefs::set_confirmed_then`]'s choice of when a candidate value becomes visible to
/// `get`/`read`, and able to ride along on another key's write (#214; see the module docs).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Visibility {
    /// `set`/`set_then`: in `values` before the write is even attempted, and left there even if it
    /// fails.
    Immediate,
    /// `set_then`: in `values` only once this exact write is confirmed durable.
    ConfirmedOnly,
}

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

    /// Set `key` and persist the whole store off the UI thread, visible through
    /// [`get`](Self::get)/[`read`](Self::read) at once (see the module docs). No store
    /// installed: nothing.
    pub fn set(cx: &mut App, key: &str, value: &str) {
        if cx.try_global::<MachinePrefs>().is_none_or(|p| p.get(key) == Some(value)) {
            return;
        }
        Self::persist(cx, key, value, Visibility::Immediate, None);
    }

    /// [`set`](Self::set), then `then(saved)` off the UI thread once the write is over:
    /// `Ok` when the file holds this value (this snapshot, or a newer one, was written);
    /// `Err` when the write failed, or nothing persists (no store installed, or memory
    /// only). The candidate is visible at once, exactly like [`set`](Self::set) — for a
    /// caller whose own change is already applied at once and only needs to know whether it
    /// durably saved (the Map module's `set_consent`: "applied at once... unlike the legacy
    /// merge"). A caller that must not act on its own value until it is confirmed durable
    /// wants [`set_confirmed_then`](Self::set_confirmed_then) instead (#214). Writes even an
    /// unchanged value: an earlier write of it may have failed.
    pub fn set_then(cx: &mut App, key: &str, value: &str, then: impl FnOnce(Result<(), String>) + Send + 'static) {
        Self::persist(cx, key, value, Visibility::Immediate, Some(Box::new(then)));
    }

    /// [`set_then`](Self::set_then), but the candidate stays out of `values` — unreadable,
    /// and never persisted by a later write of a different key — until this write is
    /// confirmed durable (#214; see the module docs), at which point it lands in `values`
    /// too, back on the UI thread. `then` keeps running off the UI thread exactly as
    /// `set_then`'s does. For the one caller that may discard another copy of this value
    /// only once this one is durable (the Map module's consent migration) — unlike
    /// `set_then`, whose callers apply their own change at once and merely want to know
    /// whether it saved.
    pub fn set_confirmed_then(cx: &mut App, key: &str, value: &str, then: impl FnOnce(Result<(), String>) + Send + 'static) {
        Self::persist(cx, key, value, Visibility::ConfirmedOnly, Some(Box::new(then)));
    }

    fn persist(cx: &mut App, key: &str, value: &str, visibility: Visibility, then: Option<Saved>) {
        let runner = Runner::get(cx);
        let fail = |runner: &Runner, then: Option<Saved>, why: &'static str| {
            if let Some(then) = then {
                runner.spawn(move || then(Err(why.into())));
            }
        };
        if !cx.has_global::<MachinePrefs>() {
            return fail(&runner, then, "no preference store is installed");
        }
        let prefs = cx.global_mut::<MachinePrefs>();
        // The snapshot this write serializes: every value already in `values`, plus this
        // one — computed without mutating `values` itself, so a `ConfirmedOnly` candidate
        // can be left out of it (#214).
        let mut snapshot = prefs.values.clone();
        snapshot.insert(key.to_string(), value.to_string());
        if visibility == Visibility::Immediate {
            prefs.values = snapshot.clone();
        }
        let Some(path) = prefs.path.clone() else {
            return fail(&runner, then, "this machine's preferences are kept in memory only");
        };
        prefs.seq += 1;
        let (seq, written) = (prefs.seq, prefs.written.clone());
        // `ConfirmedOnly` lands its candidate in `values` once this write is known to have
        // actually reached disk — a hop back onto the UI thread, since the runner thread
        // below has no `cx`. `then` itself keeps running off the UI thread, same as before
        // #214: a caller's own callback may block (the consent migration's catalog write
        // does).
        let confirm = (visibility == Visibility::ConfirmedOnly).then(|| {
            let (tx, rx) = futures::channel::oneshot::channel::<bool>();
            let (key, value) = (key.to_string(), value.to_string());
            cx.spawn(async move |cx| {
                if rx.await == Ok(true) {
                    let _ = cx.update_global(|prefs: &mut MachinePrefs, _| {
                        prefs.values.insert(key, value);
                    });
                }
            })
            .detach();
            tx
        });
        runner.spawn(move || {
            // Held across the write, so two snapshots never write at once and an older one
            // that arrives late is skipped rather than undoing a newer one.
            let mut last = written.lock().unwrap_or_else(|e| e.into_inner());
            let saved = if seq <= *last {
                Ok(()) // a newer snapshot is on disk: this value, or what replaced it
            } else {
                match write_atomically(&path, &snapshot) {
                    Ok(()) => {
                        *last = seq;
                        Ok(())
                    }
                    Err(e) => {
                        eprintln!("machine prefs: cannot write {}: {e}", path.display());
                        Err(format!("cannot write {}: {e}", path.display()))
                    }
                }
            };
            drop(last);
            if let Some(tx) = confirm {
                let _ = tx.send(saved.is_ok());
            }
            if let Some(then) = then {
                then(saved);
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

    // --- set_then is immediate, like `set`; set_confirmed_then is not (#214 M1) -----------

    /// `set_then` is `set` plus a completion callback — visible at once, not held back —
    /// for a caller that applies its own change optimistically already and only wants to
    /// know whether the write landed (`MapState::set_consent`).
    #[gpui_kit::test]
    fn a_set_then_candidate_is_visible_at_once_like_set(cx: &mut TestAppContext) {
        let dir = TempDir::new("set-then-immediate");
        let path = dir.0.join(FILE_NAME);
        cx.update(|cx| {
            cx.set_global(Runner::manual());
            cx.set_global(MachinePrefs::load(path.clone()));
            MachinePrefs::set_then(cx, "map.tileHosts", "merged", |saved| assert!(saved.is_ok()));
            assert_eq!(
                MachinePrefs::read(cx, "map.tileHosts").as_deref(),
                Some("merged"),
                "visible before the write is even attempted, like `set`"
            );
        });
        assert_eq!(cx.update(|cx| Runner::get(cx).run_pending()), 1);
        assert_eq!(MachinePrefs::load(path).get("map.tileHosts"), Some("merged"));
    }

    // --- set_confirmed_then's candidate is confirm-gated, not optimistic (#214 M1) ---------

    /// Unlike `set` (and `set_then`), a `set_confirmed_then` candidate is not readable before
    /// its write is attempted, nor while it is in flight — only once the runner's write
    /// actually confirms it, which lands it in `values` on a hop back to the UI thread.
    #[gpui_kit::test]
    fn a_set_confirmed_then_candidate_is_invisible_until_confirmed(cx: &mut TestAppContext) {
        let dir = TempDir::new("confirm-visible");
        let path = dir.0.join(FILE_NAME);
        cx.update(|cx| {
            cx.set_global(Runner::manual());
            cx.set_global(MachinePrefs::load(path.clone()));
            MachinePrefs::set_confirmed_then(cx, "map.tileHosts", "merged", |saved| assert!(saved.is_ok()));
            assert_eq!(MachinePrefs::read(cx, "map.tileHosts"), None, "not visible before the write even starts");
        });
        assert!(!path.exists(), "nothing written on the UI thread");
        assert_eq!(cx.update(|cx| Runner::get(cx).run_pending()), 1);
        cx.run_until_parked(); // the confirm hop back onto the UI thread
        cx.update(|cx| {
            assert_eq!(MachinePrefs::read(cx, "map.tileHosts").as_deref(), Some("merged"), "confirmed: now visible");
        });
        assert_eq!(MachinePrefs::load(path).get("map.tileHosts"), Some("merged"));
    }

    /// P214b: a `set_confirmed_then` write that fails leaves nothing in `values` to roll back,
    /// so a later `set` of an unrelated key does not carry the failed candidate to disk with it.
    #[gpui_kit::test]
    fn a_failed_set_confirmed_then_is_never_persisted_by_a_later_set_of_another_key(cx: &mut TestAppContext) {
        let dir = TempDir::new("confirm-fail");
        let blocker = dir.0.join("blocker");
        std::fs::write(&blocker, "a file where the store's directory should be").unwrap();
        let path = blocker.join(FILE_NAME); // every write under here fails
        cx.update(|cx| {
            cx.set_global(Runner::manual());
            cx.set_global(MachinePrefs::load(path.clone()));
            MachinePrefs::set_confirmed_then(cx, "map.tileHosts", "merged", |saved| assert!(saved.is_err()));
        });
        assert_eq!(cx.update(|cx| Runner::get(cx).run_pending()), 1);
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(MachinePrefs::read(cx, "map.tileHosts"), None, "the failed candidate was never inserted");
        });

        std::fs::remove_file(&blocker).unwrap();
        cx.update(|cx| MachinePrefs::set(cx, "appearance.mode", "standard"));
        assert_eq!(cx.update(|cx| Runner::get(cx).run_pending()), 1);
        cx.run_until_parked();
        assert_eq!(MachinePrefs::load(path.clone()).get("map.tileHosts"), None, "the failed merge never reached disk");
        assert_eq!(MachinePrefs::load(path).get("appearance.mode"), Some("standard"), "the unrelated key still saved");
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
