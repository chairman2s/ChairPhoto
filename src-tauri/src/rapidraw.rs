//! "Edit in RapidRAW" round-trip (Lightroom → Photoshop style).
//!
//! Unlike the sidecar-based external editors in `external_edit.rs` (darktable / RawTherapee /
//! ART), the user's custom **RapidRAW** build supports an explicit request/response protocol:
//!
//! ```text
//! RapidRAW --edit <absolute-source> --output <absolute-output> [--format tiff|png|jpg] [--quality N]
//! ```
//!
//! - Opens the editor on the source; shows a "Done — saves to <output>" button.
//! - Done  → writes exactly `--output`, exits code 0.
//! - Close without Done → exits WITHOUT creating the output file.
//! - SINGLE-INSTANCE: if a RapidRAW window is already open, the spawned process forwards the
//!   session to it and exits immediately (code 0, **no** output yet); the output appears later
//!   when the user clicks Done in the other window.
//!
//! Because a clean exit with no output is ambiguous (forwarded to another window *or* closed
//! without Done), we treat exit-0-without-output as a **watch** state: we poll for the output
//! file to appear, and expose a `cancel` command so the user can abandon the wait (which also
//! resolves the "closed without Done" case).
//!
//! We follow `external_edit.rs` conventions: the `editor.rapidraw.*` settings namespace,
//! PATH/override detection, an off-UI-thread worker (`spawn_blocking`), progress events, and
//! adopting the result as a **stacked child** of the original via `upsert_external_one` +
//! `set_stack_parent` — the same association mechanism the develop round-trip uses.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::catalog::Catalog;
use crate::commands::AppState;
use tauri::{AppHandle, Emitter, Manager, Runtime, State};

/// Settings key namespace, mirroring `editor.<key>.<which>` from `external_edit.rs`.
const BIN_SETTING: &str = "editor.rapidraw.bin";
const FORMAT_SETTING: &str = "editor.rapidraw.format";
const DEFAULT_BIN: &str = "RapidRAW";
const DEFAULT_FORMAT: &str = "tiff";

/// How often the forwarded-case watcher polls for the output file to appear, and how long the
/// size-stability check waits between the two size samples. Kept modest so cancellation and
/// completion feel responsive without busy-spinning.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Registry of per-photo cancel flags for in-flight RapidRAW watchers. A `cancel_rapidraw`
/// command trips the flag for a photo; the watcher loop checks it each tick. Kept module-local
/// (rather than on `AppState`) so this feature is self-contained.
fn cancels() -> &'static Mutex<HashMap<i64, Arc<AtomicBool>>> {
    static CANCELS: OnceLock<Mutex<HashMap<i64, Arc<AtomicBool>>>> = OnceLock::new();
    CANCELS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Install a fresh cancel flag for `photo_id` and return it, or `None` if an edit for this
/// photo is already in flight. Rejecting the second edit (rather than replacing the entry)
/// keeps each worker's `clear_cancel` unambiguous — otherwise the first worker's clear would
/// drop the second's flag, leaving the second watcher uncancellable. It also matches RapidRAW's
/// single-instance nature: a second launch on the same photo forwards into the first anyway.
fn register_cancel(photo_id: i64) -> Option<Arc<AtomicBool>> {
    use std::collections::hash_map::Entry;
    match cancels().lock().unwrap().entry(photo_id) {
        Entry::Occupied(_) => None,
        Entry::Vacant(v) => {
            let flag = Arc::new(AtomicBool::new(false));
            v.insert(flag.clone());
            Some(flag)
        }
    }
}

/// Remove the cancel flag for `photo_id` once its workflow has ended (on every path).
fn clear_cancel(photo_id: i64) {
    cancels().lock().unwrap().remove(&photo_id);
}

/// Whether a command is runnable: an absolute/relative path is checked directly; a bare name
/// is looked up with `which` (same approach as `external_edit::on_path`).
fn on_path(cmd: &str) -> bool {
    if cmd.contains('/') {
        return Path::new(cmd).exists();
    }
    Command::new("which")
        .arg(cmd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Resolve the RapidRAW binary: an explicit Preferences setting wins; otherwise the default
/// `RapidRAW` if it's on `PATH`. `None` = not available (don't offer it / can't run it).
fn resolved_bin(catalog: &Catalog) -> Option<String> {
    if let Ok(Some(v)) = catalog.get_setting(BIN_SETTING) {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return on_path(&v).then_some(v);
        }
    }
    on_path(DEFAULT_BIN).then(|| DEFAULT_BIN.to_string())
}

/// The configured output format (tiff default). Only tiff/png/jpg are accepted; anything else
/// falls back to tiff. TIFF/PNG are 16-bit; jpg takes a quality.
fn resolved_format(catalog: &Catalog) -> String {
    let fmt = catalog
        .get_setting(FORMAT_SETTING)
        .ok()
        .flatten()
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_default();
    match fmt.as_str() {
        "png" => "png".into(),
        "jpg" | "jpeg" => "jpg".into(),
        _ => DEFAULT_FORMAT.into(),
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RapidRawStatus {
    /// Whether the binary is detected (PATH or override) — gates offering the action.
    pub available: bool,
    /// The output format that will be used (tiff | png | jpg).
    pub format: String,
}

/// Whether RapidRAW is configured/available, for the inspector "Edit in…" action + Preferences.
#[tauri::command]
pub fn rapidraw_available(state: State<'_, AppState>) -> Result<RapidRawStatus, String> {
    let guard = state.catalog.lock().map_err(|e| e.to_string())?;
    let catalog = guard.as_ref().ok_or("No catalog is open")?;
    Ok(RapidRawStatus {
        available: resolved_bin(catalog).is_some(),
        format: resolved_format(catalog),
    })
}

/// Per-photo progress of a RapidRAW round-trip, streamed on `rapidraw:progress`.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct RapidRawProgress {
    photo_id: i64,
    /// editing | waiting | importing | done | error | cancelled
    phase: String,
    /// Human-readable detail (error text, or the output path being watched).
    message: String,
}

fn emit<R: Runtime>(app: &AppHandle<R>, photo_id: i64, phase: &str, message: &str) {
    let _ = app.emit(
        "rapidraw:progress",
        RapidRawProgress { photo_id, phase: phase.into(), message: message.into() },
    );
}

/// Everything needed to run a round-trip, resolved under a brief catalog lock so the long
/// GUI/watch work never holds the shared connection. Public (with a public constructor) so the
/// integration test can drive [`run_roundtrip`] with a mock binary against a temp catalog.
pub struct Resolved {
    pub db_path: PathBuf,
    pub root: PathBuf,
    pub source: PathBuf,
    pub source_photo_id: i64,
    pub bin: String,
    pub format: String,
}

impl Resolved {
    /// Build a `Resolved` directly (for tests): the source photo's resolved path + id, the
    /// catalog's db path + root, and the RapidRAW binary + output format to use.
    pub fn for_test(
        db_path: PathBuf,
        root: PathBuf,
        source: PathBuf,
        source_photo_id: i64,
        bin: String,
        format: String,
    ) -> Self {
        Self { db_path, root, source, source_photo_id, bin, format }
    }
}

fn resolve(app: &AppHandle, photo_id: i64) -> Result<Resolved, String> {
    let state = app.state::<AppState>();
    let guard = state.catalog.lock().map_err(|e| e.to_string())?;
    let catalog = guard.as_ref().ok_or("No catalog is open")?;
    let source = catalog.require_photo_path(photo_id).map_err(|e| e.to_string())?;
    let bin = resolved_bin(catalog)
        .ok_or("RapidRAW is not configured — set its path in Preferences → Editors")?;
    Ok(Resolved {
        db_path: catalog.db_path().to_path_buf(),
        root: catalog.root().to_path_buf(),
        source,
        source_photo_id: photo_id,
        bin,
        format: resolved_format(catalog),
    })
}

/// `<parent>/<stem>-rapidraw.<ext>`, bumping ` (2)`, ` (3)`… on collision (never overwrite).
fn unique_output_path(source: &Path, ext: &str) -> Result<PathBuf, String> {
    let parent = source.parent().ok_or("photo has no parent folder")?;
    let stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or("photo has no filename")?;
    let mut candidate = parent.join(format!("{stem}-rapidraw.{ext}"));
    let mut n = 2;
    while candidate.exists() {
        candidate = parent.join(format!("{stem}-rapidraw ({n}).{ext}"));
        n += 1;
    }
    Ok(candidate)
}

/// Poll until `out` exists and its size is stable across two consecutive checks (nonzero), or
/// the cancel flag trips. Returns `true` when a stable, nonzero file is present; `false` if
/// cancelled first.
fn wait_for_stable_output(out: &Path, cancel: &AtomicBool) -> bool {
    // Phase 1: wait for the file to appear at all (the forwarded / Done-later case).
    while !out.is_file() {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    // Phase 2: wait for the size to settle (RapidRAW may still be writing it).
    let mut last = file_len(out);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL);
        let now = file_len(out);
        if now > 0 && now == last {
            return true;
        }
        last = now;
    }
}

fn file_len(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// Index the finished output as a stacked child of the original and copy the original's EXIF
/// across if the export carries none. Mirrors `external_edit::render_and_stack` for the import
/// half. Runs on a secondary catalog connection so the shared one keeps serving reads.
fn import_and_stack(
    catalog: &Catalog,
    source: &Path,
    out: &Path,
    source_photo_id: i64,
) -> Result<i64, String> {
    // Best-effort: give the export the original's EXIF when it lacks any (e.g. a bare TIFF).
    copy_exif_if_missing(source, out);

    // Index the rendered file on the source's volume (writes a UUID sidecar, merge-safe).
    let (id, _created, _unchanged) = crate::scanner::upsert_external_one(catalog, out)?;
    // Fill in EXIF/dimensions from the (now possibly EXIF-carrying) output.
    let mut meta = crate::metadata::extract_batch(&[out.to_path_buf()]);
    if let Some(m) = meta.remove(out) {
        let _ = catalog.set_photo_metadata(id, &m.promoted, &m.entries);
    }
    // Attribute the edit to RapidRAW so the "edited" filter picks it up.
    let _ = catalog.set_external_editors(id, "RapidRAW");
    // Group it under the original.
    catalog.set_stack_parent(id, source_photo_id).map_err(|e| e.to_string())?;
    Ok(id)
}

/// If `out` has no camera-model EXIF, copy tags from `source` with exiftool (best-effort,
/// non-fatal — follows the scanner's exiftool usage). The `.tiff`/`.png` RapidRAW writes may
/// omit the source's shooting metadata; carrying it over keeps the derived file sortable by
/// capture date alongside the original.
fn copy_exif_if_missing(source: &Path, out: &Path) {
    // Cheap probe: does the output already carry a capture date / camera model?
    let mut probe = crate::metadata::extract_batch(&[out.to_path_buf()]);
    let has_exif = probe
        .remove(out)
        .map(|m| m.promoted.camera_model.is_some() || m.promoted.capture_time.is_some())
        .unwrap_or(false);
    if has_exif {
        return;
    }
    // exiftool -TagsFromFile <source> -all:all <out> -overwrite_original: copy every tag —
    // EXCEPT Orientation, which is forced to 1 (upright). RapidRAW bakes the rotation into
    // the exported pixels (a portrait ARW yields portrait pixel rows) and writes no
    // Orientation tag of its own; copying the RAW's rotate-90 tag onto already-rotated
    // pixels makes every orientation-honoring viewer rotate the image a second time.
    // The later assignment wins over the -all:all copy; `#` writes the numeric value.
    let status = Command::new("exiftool")
        .arg("-TagsFromFile")
        .arg(source)
        .args(["-m", "-all:all", "-Orientation#=1", "-overwrite_original"])
        .arg(out)
        .status();
    if let Err(e) = status {
        eprintln!("rapidraw: couldn't copy EXIF from {}: {e}", source.display());
    }
}

/// The blocking core of the round-trip, decoupled from Tauri so it's directly testable with a
/// mock binary (see `tests/rapidraw_roundtrip.rs`). Launches RapidRAW with the request/response
/// protocol, then runs the completion state machine:
///   - nonzero exit / spawn failure → `Err`;
///   - exit 0 with the output present → import + stack, `Ok(Some(id))`;
///   - exit 0 **without** the output (forwarded / closed-without-Done) → poll for the file
///     (cancellable) then import, or `Ok(None)` if cancelled first.
///
/// `progress` receives (phase, message) so the caller can emit UI events; `cancel` is the
/// watcher's abandon flag. This never touches the UI thread — the command wraps it in
/// `spawn_blocking`.
pub fn run_roundtrip(
    r: &Resolved,
    cancel: &AtomicBool,
    progress: &dyn Fn(&str, &str),
) -> Result<Option<i64>, String> {
    let ext = match r.format.as_str() {
        "jpg" => "jpg",
        "png" => "png",
        _ => "tiff",
    };
    let source_photo_id = r.source_photo_id;
    let out = unique_output_path(&r.source, ext)?;
    progress("editing", &out.to_string_lossy());

    // Launch and wait for THIS process to exit. In the single-instance forwarding case it
    // returns immediately (code 0) having handed the session to the open window.
    let mut cmd = Command::new(&r.bin);
    cmd.arg("--edit")
        .arg(&r.source)
        .arg("--output")
        .arg(&out)
        .args(["--format", &r.format]);
    if r.format == "jpg" {
        cmd.args(["--quality", "95"]);
    }
    let status = cmd
        .status()
        .map_err(|e| format!("couldn't launch RapidRAW ({}): {e}", r.bin))?;

    if !status.success() {
        return Err(format!(
            "RapidRAW exited with an error{}",
            status.code().map(|c| format!(" (code {c})")).unwrap_or_default()
        ));
    }

    // Exit 0. If the output isn't there yet, we're in the forwarded / Done-later case:
    // watch for it (cancellable). If it's already there, import straight away.
    if !out.is_file() {
        progress("waiting", &out.to_string_lossy());
    }
    if !wait_for_stable_output(&out, cancel) {
        // Cancelled before the file appeared/stabilised — abandon without importing.
        return Ok(None);
    }

    progress("importing", &out.to_string_lossy());
    let catalog = Catalog::open_secondary(&r.db_path, &r.root).map_err(|e| e.to_string())?;
    let id = import_and_stack(&catalog, &r.source, &out, source_photo_id)?;
    progress("done", &out.to_string_lossy());
    Ok(Some(id))
}

/// Launch RapidRAW on a photo's original with the request/response protocol, then run the
/// completion state machine off the UI thread (see [`run_roundtrip`]). The photo is never
/// left "editing" — every path clears the state and emits a terminal `rapidraw:progress`.
///
/// Returns the new stacked child's id on success, or `None` if the wait was cancelled.
#[tauri::command]
pub async fn edit_in_rapidraw(app: AppHandle, photo_id: i64) -> Result<Option<i64>, String> {
    let r = resolve(&app, photo_id)?;
    let cancel = register_cancel(photo_id)
        .ok_or("This photo is already being edited in RapidRAW — finish or cancel that edit first")?;
    let app2 = app.clone();

    let joined = tauri::async_runtime::spawn_blocking(move || {
        run_roundtrip(&r, &cancel, &|phase, message| emit(&app2, photo_id, phase, message))
    })
    .await;

    // Whatever happened — including a panic inside the worker (JoinError) — the photo must
    // not stay "editing": clear the flag BEFORE propagating any error, and on the terminal
    // non-"done" outcomes emit the matching phase so the UI drops its indicator.
    clear_cancel(photo_id);
    let result = joined.map_err(|e| e.to_string())?;
    match &result {
        Ok(None) => emit(&app, photo_id, "cancelled", ""),
        Ok(Some(_)) => {} // "done" already emitted from the worker
        Err(e) => emit(&app, photo_id, "error", e),
    }
    result
}

/// Cancel an in-flight RapidRAW wait for `photo_id`. Trips the watcher's cancel flag so it
/// abandons the wait (used both to give up on a forwarded session and to resolve the
/// "closed without Done" case, which the app can't distinguish from forwarding).
#[tauri::command]
pub fn cancel_rapidraw(photo_id: i64) -> Result<(), String> {
    if let Some(flag) = cancels().lock().map_err(|e| e.to_string())?.get(&photo_id) {
        flag.store(true, Ordering::Relaxed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_output_path_bumps_on_collision() {
        let dir = std::env::temp_dir().join("chairphoto-rapidraw-unique");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("DSC1.ARW");
        std::fs::write(&source, b"raw").unwrap();

        let first = unique_output_path(&source, "tiff").unwrap();
        assert_eq!(first, dir.join("DSC1-rapidraw.tiff"));
        std::fs::write(&first, b"x").unwrap();

        let second = unique_output_path(&source, "tiff").unwrap();
        assert_eq!(second, dir.join("DSC1-rapidraw (2).tiff"));
    }

    #[test]
    fn wait_for_stable_output_returns_when_size_settles() {
        let dir = std::env::temp_dir().join("chairphoto-rapidraw-stable");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out.tiff");

        // Writer thread: create the file, grow it once, then leave it stable.
        let out_w = out.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            std::fs::write(&out_w, b"chunk-one").unwrap();
            std::thread::sleep(POLL_INTERVAL + Duration::from_millis(200));
            // Append: grows the size once, then never again.
            let mut f = std::fs::OpenOptions::new().append(true).open(&out_w).unwrap();
            use std::io::Write;
            f.write_all(b"chunk-two").unwrap();
        });

        let cancel = AtomicBool::new(false);
        assert!(wait_for_stable_output(&out, &cancel));
        writer.join().unwrap();
        // Both chunks present → we only returned after the final append settled.
        assert_eq!(std::fs::read(&out).unwrap(), b"chunk-onechunk-two");
    }

    #[test]
    fn register_cancel_rejects_a_second_edit_and_frees_after_clear() {
        // Use a photo id unlikely to collide with any other test in this module.
        let pid = 987_654_321;
        clear_cancel(pid); // ensure a clean slate regardless of test order

        // First edit registers a flag.
        let first = register_cancel(pid).expect("first edit registers a cancel flag");
        // A second edit for the SAME photo is rejected — it must NOT overwrite the entry
        // (otherwise the first worker's clear_cancel would drop the second's flag).
        assert!(register_cancel(pid).is_none(), "a second in-flight edit must be rejected");

        // cancel_rapidraw still targets the (single) in-flight flag.
        cancel_rapidraw(pid).unwrap();
        assert!(first.load(Ordering::Relaxed), "cancel must trip the registered flag");

        // Once the first worker clears its entry, a fresh edit can register again.
        clear_cancel(pid);
        assert!(register_cancel(pid).is_some(), "after clear, a new edit registers");
        clear_cancel(pid);
    }

    #[test]
    fn wait_for_stable_output_bails_on_cancel() {
        let dir = std::env::temp_dir().join("chairphoto-rapidraw-cancel");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("never.tiff"); // never created

        let cancel = Arc::new(AtomicBool::new(false));
        let c2 = cancel.clone();
        let waiter = std::thread::spawn(move || wait_for_stable_output(&out, &c2));
        std::thread::sleep(Duration::from_millis(150));
        cancel.store(true, Ordering::Relaxed);
        assert!(!waiter.join().unwrap(), "cancel must abandon the wait");
    }
}
