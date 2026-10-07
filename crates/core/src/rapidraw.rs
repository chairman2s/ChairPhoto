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

use crate::app::{identity_of, AppState, CatalogIdentity, CoreEvent, EventSink, CATALOG_CHANGED};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::catalog::Catalog;

/// Settings key namespace, mirroring `editor.<key>.<which>` from `external_edit.rs`.
const BIN_SETTING: &str = "editor.rapidraw.bin";
const FORMAT_SETTING: &str = "editor.rapidraw.format";
const DEFAULT_BIN: &str = "RapidRAW";
const DEFAULT_FORMAT: &str = "tiff";

/// How often the forwarded-case watcher polls for the output file to appear, and how long the
/// size-stability check waits between the two size samples. Kept modest so cancellation and
/// completion feel responsive without busy-spinning.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// One in-flight round-trip in the cancel registry: its job id and its abandon flag.
struct InFlight {
    job_id: u64,
    flag: Arc<AtomicBool>,
}

/// The cancel registry: per (catalog, photo), the round-trip in flight and its abandon flag;
/// per job id, the flag of a round-trip queued but not started yet ([`queue_job`]). One mutex
/// for both, so a cancel always finds a job's flag: a starting worker registers it in flight
/// before it leaves the queue. A `cancel_rapidraw` command trips the flag; the worker checks it
/// before launching and the watcher loop each tick. Kept module-local (rather than on
/// `AppState`) so this feature is mostly self-contained — the one exception is
/// [`trip_catalog`], which a catalog switch/re-root calls so a wait with no timeout does not
/// outlive the catalog it was registered under (#188).
///
/// Keyed by `(CatalogIdentity, photo_id)`, not `photo_id` alone: photo ids collide across
/// catalogs, so two catalogs' same-id photos would otherwise share one slot — a false "already
/// being edited" refusal in the second catalog, and an unscoped Cancel able to trip the first
/// catalog's round-trip from the second (#188).
#[derive(Default)]
struct Registry {
    in_flight: HashMap<(CatalogIdentity, i64), InFlight>,
    queued: HashMap<u64, Arc<AtomicBool>>,
}

fn registry() -> std::sync::MutexGuard<'static, Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(Mutex::default).lock().unwrap_or_else(|e| e.into_inner())
}

/// A fresh job id for [`edit_in_rapidraw_as`]. Ids are process-wide and never reused, so a
/// round-trip's events and its cancel can be told apart from any other's — including one
/// started on the same photo id in another catalog.
pub fn next_job_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A round-trip queued for a worker, under a fresh job id ([`next_job_id`]) whose cancel flag
/// is registered from now on: [`cancel_rapidraw_job`] before the worker starts makes
/// [`edit_in_rapidraw_as`] end as cancelled without launching RapidRAW. Dropped unstarted
/// (the queue discarded it), it unregisters itself.
pub struct QueuedJob {
    id: u64,
    flag: Arc<AtomicBool>,
}

impl QueuedJob {
    pub fn id(&self) -> u64 {
        self.id
    }
}

impl Drop for QueuedJob {
    fn drop(&mut self) {
        registry().queued.remove(&self.id);
    }
}

/// Queue a round-trip: its job id, cancellable from this moment.
pub fn queue_job() -> QueuedJob {
    let (id, flag) = (next_job_id(), Arc::new(AtomicBool::new(false)));
    registry().queued.insert(id, flag.clone());
    QueuedJob { id, flag }
}

/// Register `flag` as the cancel flag of `identity`'s `photo_id` round-trip (owned by
/// `job_id`) and return it, or `None` if an edit for this (catalog, photo) is already in
/// flight. Rejecting the second edit (rather than replacing the entry) keeps each worker's
/// `clear_cancel` unambiguous — otherwise the first worker's clear would drop the second's
/// flag, leaving the second watcher uncancellable. It also matches RapidRAW's single-instance
/// nature: a second launch on the same photo forwards into the first anyway.
fn register_cancel(identity: CatalogIdentity, photo_id: i64, job_id: u64, flag: Arc<AtomicBool>) -> Option<Arc<AtomicBool>> {
    use std::collections::hash_map::Entry;
    match registry().in_flight.entry((identity, photo_id)) {
        Entry::Occupied(_) => None,
        Entry::Vacant(v) => {
            v.insert(InFlight { job_id, flag: flag.clone() });
            Some(flag)
        }
    }
}

/// Remove the cancel flag for `identity`'s `photo_id` once its workflow has ended (on every
/// path).
fn clear_cancel(identity: CatalogIdentity, photo_id: i64) {
    registry().in_flight.remove(&(identity, photo_id));
}

/// Trip the cancel flag of every round-trip in flight for `identity`'s catalog. Called when
/// that catalog is detached by a switch or a re-root (`catalogs::switch_catalog_in`,
/// `catalogs::reroot`), so a wait with no timeout (the forwarded/closed-without-Done case)
/// does not linger forever unreachable once the front end drops its own per-photo tracking on
/// the switch — the watcher ends on its next poll and `clear_cancel`s itself, same as a user
/// Cancel (#188). A round-trip queued but not yet running has no identity yet: it is
/// unaffected here, and `resolve` fails closed for a bound queue if it runs against a
/// different catalog once its worker starts.
pub(crate) fn trip_catalog(identity: CatalogIdentity) {
    let registry = registry();
    for ((entry_identity, _), in_flight) in registry.in_flight.iter() {
        if *entry_identity == identity {
            in_flight.flag.store(true, Ordering::Relaxed);
        }
    }
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
pub fn rapidraw_available(state: &AppState) -> Result<RapidRawStatus, String> {
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
pub struct RapidRawProgress {
    pub photo_id: i64,
    /// The round-trip this event belongs to ([`next_job_id`]): a front end drops events of a
    /// job it does not follow (a superseded one, or one from before a catalog switch).
    pub job_id: u64,
    /// editing | waiting | importing | done | error | cancelled
    pub phase: String,
    /// Human-readable detail (error text, or the output path being watched).
    pub message: String,
}

fn emit(state: &AppState, photo_id: i64, job_id: u64, phase: &str, message: &str) {
    state.send(CoreEvent::RapidRawProgress(RapidRawProgress {
        photo_id,
        job_id,
        phase: phase.into(),
        message: message.into(),
    }));
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

/// `expected`: the catalog the photo id was read from (`None` = whichever is open, the
/// unbound form the former Tauri command used), checked under the same lock hold as the path lookup — once
/// another catalog is open this fails closed with [`CATALOG_CHANGED`] and nothing launches.
/// Also returns the resolved catalog's identity (even when `expected` was `None`), captured
/// under that same lock hold, so the in-flight registry entry is always scoped to the actual
/// catalog this round-trip runs against (#188).
///
/// Also claims the round-trip's registry entry ([`register_cancel`]) **before releasing the
/// catalog lock** (#188 M3): a catalog switch or re-root trips every round-trip registered for
/// the catalog it is leaving from inside its own catalog-lock hold
/// (`detach_catalog_and_trip_jobs_with`'s `before_drop`, see the lock-order table in
/// `app::jobs`). Registering on a separate, later lock acquisition left a window where that
/// trip could run — finding nothing yet, a no-op — strictly between this function reading the
/// identity and the caller actually inserting the registry entry; the entry then existed,
/// untripped, for a catalog that had already gone. Holding one lock across both halves closes
/// that window: whichever of this call and the switch/re-root acquires the catalog lock first
/// runs its whole critical section before the other starts.
fn resolve(
    state: &AppState,
    expected: Option<CatalogIdentity>,
    photo_id: i64,
    job_id: u64,
    flag: Arc<AtomicBool>,
) -> Result<(Resolved, CatalogIdentity, Arc<AtomicBool>), String> {
    let guard = state.catalog.lock().map_err(|e| e.to_string())?;
    let catalog = guard.as_ref().ok_or("No catalog is open")?;
    if expected.is_some_and(|e| !e.is(catalog)) {
        return Err(CATALOG_CHANGED.into());
    }
    let identity = identity_of(catalog);
    let source = catalog.require_photo_path(photo_id).map_err(|e| e.to_string())?;
    let bin = resolved_bin(catalog)
        .ok_or("RapidRAW is not configured — set its path in Preferences → Editors")?;
    let resolved = Resolved {
        db_path: catalog.db_path().to_path_buf(),
        root: catalog.root().to_path_buf(),
        source,
        source_photo_id: photo_id,
        bin,
        format: resolved_format(catalog),
    };
    let cancel = register_cancel(identity, photo_id, job_id, flag)
        .ok_or("This photo is already being edited in RapidRAW — finish or cancel that edit first")?;
    Ok((resolved, identity, cancel))
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

/// The blocking core of the round-trip, decoupled from any front end so it's directly testable with a
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
    if cancel.load(Ordering::Relaxed) {
        return Ok(None); // cancelled before the launch
    }
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
pub async fn edit_in_rapidraw(state: AppState, photo_id: i64) -> Result<Option<i64>, String> {
    edit(state, None, photo_id, queue_job()).await
}

/// [`edit_in_rapidraw`] of a photo read from the catalog `expected` names, under a job id the
/// caller queued ([`queue_job`]), so it can follow the job's `rapidraw:progress` events from
/// the first one and cancel exactly this job ([`cancel_rapidraw_job`]) — also before this
/// starts, which then launches nothing and ends as cancelled. Once another catalog
/// is open it fails closed with [`CATALOG_CHANGED`]: RapidRAW is not launched on, and nothing
/// is imported into, the new catalog's photo with the same id.
pub async fn edit_in_rapidraw_as(
    state: AppState,
    expected: CatalogIdentity,
    photo_id: i64,
    job: QueuedJob,
) -> Result<Option<i64>, String> {
    edit(state, Some(expected), photo_id, job).await
}

async fn edit(
    state: AppState,
    expected: Option<CatalogIdentity>,
    photo_id: i64,
    job: QueuedJob,
) -> Result<Option<i64>, String> {
    let job_id = job.id;
    if job.flag.load(Ordering::Relaxed) {
        // Cancelled while queued: nothing was launched.
        emit(&state, photo_id, job_id, "cancelled", "");
        return Ok(None);
    }
    let (r, identity, cancel) = resolve(&state, expected, photo_id, job_id, job.flag.clone())?;
    // In flight now (same flag), so it leaves the queue: a cancel finds it either way.
    drop(job);
    let state2 = state.clone();

    let joined = crate::app::spawn_blocking(move || {
        run_roundtrip(&r, &cancel, &|phase, message| emit(&state2, photo_id, job_id, phase, message))
    })
    .await;

    // Whatever happened — including a panic inside the worker (JoinError) — the photo must
    // not stay "editing": clear the flag BEFORE propagating any error, and on the terminal
    // non-"done" outcomes emit the matching phase so the UI drops its indicator.
    clear_cancel(identity, photo_id);
    let result = joined.map_err(|e| e.to_string())?;
    match &result {
        Ok(None) => emit(&state, photo_id, job_id, "cancelled", ""),
        Ok(Some(_)) => {} // "done" already emitted from the worker
        Err(e) => emit(&state, photo_id, job_id, "error", e),
    }
    result
}

/// Cancel an in-flight RapidRAW wait for `photo_id`, scoped to the catalog open right now (the
/// former Tauri/React command had no identity of its own to pass — #188): trips the watcher's cancel
/// flag so it abandons the wait (used both to give up on a forwarded session and to resolve
/// the "closed without Done" case, which the app can't distinguish from forwarding). A switch
/// since the round-trip started means this call's identity no longer matches that entry's, so
/// it is left alone — Cancel from the newly open catalog never reaches another catalog's
/// round-trip. No catalog open: a no-op, as a missing entry always was.
pub fn cancel_rapidraw(state: &AppState, photo_id: i64) -> Result<(), String> {
    let Ok(identity) = crate::app::catalog_identity(state) else { return Ok(()) };
    cancel_rapidraw_in(identity, photo_id);
    Ok(())
}

fn cancel_rapidraw_in(identity: CatalogIdentity, photo_id: i64) {
    if let Some(in_flight) = registry().in_flight.get(&(identity, photo_id)) {
        in_flight.flag.store(true, Ordering::Relaxed);
    }
}

/// [`cancel_rapidraw`], only if `job_id` is the round-trip in flight for `identity`'s
/// `photo_id` — or one still queued ([`queue_job`]), which then never launches: a front end
/// that followed one job cannot cancel another (one started later, or one on the same photo id
/// in another catalog, #188).
pub fn cancel_rapidraw_job(identity: CatalogIdentity, photo_id: i64, job_id: u64) -> Result<(), String> {
    let registry = registry();
    let flag = match registry.in_flight.get(&(identity, photo_id)) {
        Some(in_flight) if in_flight.job_id == job_id => Some(&in_flight.flag),
        _ => registry.queued.get(&job_id),
    };
    if let Some(flag) = flag {
        flag.store(true, Ordering::Relaxed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_output_path_bumps_on_collision() {
        let dir = crate::test_support::TestTmpDir::new("rapidraw-unique");
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
        let dir = crate::test_support::TestTmpDir::new("rapidraw-stable");
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

    /// A catalog with a brief, unique identity, for tests that only need one to key the
    /// registry with (not a real photo or round-trip).
    fn identity_only(tag: &str) -> CatalogIdentity {
        let dir = crate::test_support::TestTmpDir::new(&format!("rapidraw-registry-{tag}"));
        let c = Catalog::open(&dir.join("c.chairphoto"), &dir.join("photos")).unwrap();
        let state = AppState::default();
        *state.catalog.lock().unwrap() = Some(c);
        crate::app::catalog_identity(&state).unwrap()
    }

    #[test]
    fn register_cancel_rejects_a_second_edit_and_frees_after_clear() {
        let dir = crate::test_support::TestTmpDir::new("rapidraw-registry-single");
        let c = Catalog::open(&dir.join("c.chairphoto"), &dir.join("photos")).unwrap();
        let state = AppState::default();
        *state.catalog.lock().unwrap() = Some(c);
        let identity = crate::app::catalog_identity(&state).unwrap();
        // Use a photo id unlikely to collide with any other test in this module.
        let pid = 987_654_321;
        clear_cancel(identity, pid); // ensure a clean slate regardless of test order

        // First edit registers a flag.
        let first = register_cancel(identity, pid, 1, Arc::default()).expect("first edit registers a cancel flag");
        // A second edit for the SAME (catalog, photo) is rejected — it must NOT overwrite the
        // entry (otherwise the first worker's clear_cancel would drop the second's flag).
        assert!(register_cancel(identity, pid, 2, Arc::default()).is_none(), "a second in-flight edit must be rejected");

        // cancel_rapidraw, scoped to the catalog open now, still targets the in-flight flag.
        cancel_rapidraw(&state, pid).unwrap();
        assert!(first.load(Ordering::Relaxed), "cancel must trip the registered flag");

        // Once the first worker clears its entry, a fresh edit can register again.
        clear_cancel(identity, pid);
        assert!(register_cancel(identity, pid, 3, Arc::default()).is_some(), "after clear, a new edit registers");
        clear_cancel(identity, pid);
    }

    #[test]
    fn cancel_by_job_trips_only_the_job_in_flight() {
        let identity = identity_only("job");
        let pid = 987_654_322;
        clear_cancel(identity, pid);
        let (old, new) = (next_job_id(), next_job_id());
        assert_ne!(old, new, "job ids are never reused");
        let flag = register_cancel(identity, pid, new, Arc::default()).expect("registers");

        cancel_rapidraw_job(identity, pid, old).unwrap();
        assert!(!flag.load(Ordering::Relaxed), "another job's cancel must not trip this one");
        cancel_rapidraw_job(identity, pid, new).unwrap();
        assert!(flag.load(Ordering::Relaxed), "its own job id cancels it");
        clear_cancel(identity, pid);
    }

    /// Two catalogs whose same-id photo is being edited at once (#188): the second catalog's
    /// edit is not refused by the first's entry, and cancelling the second never trips the
    /// first's flag — the registry key is `(CatalogIdentity, photo_id)`, not `photo_id` alone.
    /// (Mutation-checked: keying `in_flight` by `photo_id` alone, as before #188, makes the
    /// second `register_cancel` return `None` and this fails.)
    #[test]
    fn two_catalogs_with_the_same_photo_id_do_not_collide() {
        let (a, b) = (identity_only("collide-a"), identity_only("collide-b"));
        assert_ne!(a, b);
        let pid = 987_654_323; // shared id, as real catalogs' colliding ids are
        clear_cancel(a, pid);
        clear_cancel(b, pid);

        let a_flag = register_cancel(a, pid, 1, Arc::default()).expect("A's edit registers");
        let b_flag = register_cancel(b, pid, 2, Arc::default()).expect("B's edit on the same id is unaffected by A's");

        cancel_rapidraw_in(b, pid);
        assert!(b_flag.load(Ordering::Relaxed), "cancel in B trips B's flag");
        assert!(!a_flag.load(Ordering::Relaxed), "cancel in B must never trip A's flag");

        clear_cancel(a, pid);
        clear_cancel(b, pid);
    }

    /// A catalog switch trips every round-trip still in flight for the catalog left behind, so
    /// a wait with no timeout does not outlive it; a round-trip of another catalog (even one
    /// sharing the photo id) is untouched (#188).
    /// (Mutation-checked: an empty `trip_catalog` body leaves `a_flag` untripped and this fails.)
    #[test]
    fn trip_catalog_trips_only_that_catalogs_round_trips() {
        let (a, b) = (identity_only("trip-a"), identity_only("trip-b"));
        let pid = 987_654_324;
        clear_cancel(a, pid);
        clear_cancel(b, pid);
        let a_flag = register_cancel(a, pid, 1, Arc::default()).expect("A's edit registers");
        let b_flag = register_cancel(b, pid, 2, Arc::default()).expect("B's edit registers");

        trip_catalog(a);
        assert!(a_flag.load(Ordering::Relaxed), "A's round-trip is tripped on A's switch-away");
        assert!(!b_flag.load(Ordering::Relaxed), "B's round-trip is untouched");

        clear_cancel(a, pid);
        clear_cancel(b, pid);
    }

    /// #188, through the real API (adapts the batch1 review's probe P1): catalog A's photo 7
    /// is registered in flight (what `edit()` does before spawning its watcher); the app
    /// switches to catalog B, whose photo 7 collides. Before #188 the registry was keyed by
    /// photo id alone, so B's `edit_in_rapidraw_as` was refused with "already being edited" by
    /// A's entry — `resolve` now captures B's own identity under the same lock as the path
    /// lookup, so B's `register_cancel` is a distinct entry and the edit proceeds (to a real
    /// launch of `/bin/false`, which then fails — the point here is the refusal that used to
    /// happen before any launch, not the launch itself).
    /// (Mutation-checked: hard-coding `resolve`'s returned identity to `a_identity` instead of
    /// the resolved catalog's own reproduces the refusal and this fails.)
    #[test]
    fn a_switched_to_catalogs_colliding_photo_is_not_refused_by_the_old_catalogs_entry() {
        let dir = crate::test_support::TestTmpDir::new("rapidraw-switch-collide");
        let open = |name: &str| {
            let root = dir.join(name);
            let c = Catalog::open(&root.join(format!("{name}.chairphoto")), &root).unwrap();
            c.set_setting(BIN_SETTING, "/bin/false").unwrap();
            let mut id = 0;
            for i in 0..7 {
                let o = root.join(format!("{name}{i}.ARW"));
                std::fs::write(&o, b"raw").unwrap();
                id = c.upsert_photo(&o, None, 0, 1).unwrap().id;
            }
            (c, id)
        };
        let ((a, a_id), (b, b_id)) = (open("a"), open("b"));
        assert_eq!(a_id, b_id, "the ids collide, as real catalogs' do");

        let state = AppState::default();
        *state.catalog.lock().unwrap() = Some(a);
        let a_identity = crate::app::catalog_identity(&state).unwrap();
        register_cancel(a_identity, a_id, 424242, Arc::default()).expect("A registers");

        crate::app::detach_catalog_and_trip_jobs(&state).unwrap();
        crate::app::publish_catalog_and_reset_jobs(&state, b).unwrap();
        let b_identity = crate::app::catalog_identity(&state).unwrap();
        let rt = crate::app::runtime();

        let r = rt.block_on(edit_in_rapidraw_as(state.clone(), b_identity, b_id, queue_job()));
        assert!(
            r.as_ref().err().map_or(true, |e| !e.contains("already being edited")),
            "B's edit must not be refused by A's in-flight entry at the same photo id: {r:?}"
        );
        // Confirms the edit actually reached the launch (so the above is a real negative, not
        // an early return for an unrelated reason): `/bin/false` always exits nonzero.
        assert!(r.as_ref().err().is_some_and(|e| e.contains("RapidRAW exited with an error")), "{r:?}");
        clear_cancel(a_identity, a_id);
    }

    /// #188, end-to-end: a real `switch_catalog` — not just `trip_catalog` in isolation — trips
    /// a round-trip registered for the catalog being left, through the
    /// `detach_catalog_and_trip_jobs_with` hook `catalogs::switch_catalog_in` calls it under.
    /// (Mutation-checked: reverting that call site to the plain `detach_catalog_and_trip_jobs`
    /// — dropping the `before_drop` closure that calls `trip_catalog` — leaves `flag`
    /// untripped and this fails.)
    #[test]
    fn a_real_catalog_switch_trips_a_waiting_round_trip() {
        let dir = crate::test_support::TestTmpDir::new("rapidraw-switch-trip");
        let a_root = dir.join("a");
        std::fs::create_dir_all(&a_root).unwrap();
        let a = Catalog::open(&dir.join("a.chairphoto"), &a_root).unwrap();
        let state = AppState::default();
        *state.catalog.lock().unwrap() = Some(a);
        let a_identity = crate::app::catalog_identity(&state).unwrap();
        let pid = 987_654_330;
        clear_cancel(a_identity, pid);
        let flag = register_cancel(a_identity, pid, 1, Arc::default()).expect("registers");

        crate::app::catalogs::switch_catalog(&state, &dir.join("b.chairphoto"), &dir.join("b"), true, None)
            .expect("switch to a fresh catalog");

        assert!(flag.load(Ordering::Relaxed), "switching away from A must trip its waiting round-trip");
        clear_cancel(a_identity, pid);
    }

    /// #188 M3, functional: `resolve` itself — not `register_cancel` called separately — ends
    /// up registering the round-trip, and a real, later `switch_catalog` still finds and trips
    /// exactly that entry. Sequential (no race needed for this part: the switch runs strictly
    /// after `resolve` has already returned), so it exercises `resolve`'s wiring — that the
    /// flag it returns really is the one `register_cancel` stored — without depending on any
    /// timing.
    /// (Mutation-checked: skipping the `register_cancel` call and returning a fresh, untracked
    /// flag instead — nothing registered — leaves the switch's `trip_catalog` with no entry to
    /// find, and this fails: `flag` stays untripped.)
    #[test]
    fn resolve_registers_the_round_trip_and_a_later_switch_trips_it() {
        let dir = crate::test_support::TestTmpDir::new("rapidraw-resolve-then-switch");
        let a_root = dir.join("a");
        std::fs::create_dir_all(&a_root).unwrap();
        let a = Catalog::open(&dir.join("a.chairphoto"), &a_root).unwrap();
        let original = a_root.join("p.ARW");
        std::fs::write(&original, b"raw").unwrap();
        let pid = a.upsert_photo(&original, None, 0, 1).unwrap().id;
        // `resolve` launches nothing; any existing path configures the editor, so the test
        // does not depend on RapidRAW being installed (CI runners have none).
        a.set_setting(BIN_SETTING, "/bin/false").unwrap();
        let state = AppState::default();
        *state.catalog.lock().unwrap() = Some(a);
        let a_identity = crate::app::catalog_identity(&state).unwrap();
        clear_cancel(a_identity, pid);

        let (_resolved, identity, flag) =
            resolve(&state, Some(a_identity), pid, 1, Arc::new(AtomicBool::new(false))).expect("A is open; nothing else holds this slot");
        assert_eq!(identity, a_identity);
        assert!(!flag.load(Ordering::Relaxed), "not tripped yet — no switch has happened");

        crate::app::catalogs::switch_catalog(&state, &dir.join("b.chairphoto"), &dir.join("b"), true, None)
            .expect("switch to a fresh catalog");
        assert!(flag.load(Ordering::Relaxed), "switching away from A must trip the round-trip resolve just registered");
        clear_cancel(a_identity, pid);
    }

    /// #188 M3, the hazard this fix closes: before it, `resolve` read the identity, released
    /// the catalog lock, and `edit` called `register_cancel` as its own, later, separate
    /// statement. That left a window in which a catalog switch's own catalog-lock hold could
    /// run `trip_catalog` strictly in between — finding no entry yet (a no-op) — after which
    /// the late `register_cancel` call inserted one, untripped, for a catalog that had already
    /// gone (review `agent-notes/reviews/claude-fixes-r4.log`, probe P6: "the entry registers
    /// AFTER trip_catalog ran, flag untripped").
    ///
    /// This reconstructs that exact sequence deterministically — not through `resolve` itself
    /// (its current shape, read below, makes the sequence unreachable through the real call
    /// path: there is no longer any way to call it and then separately call `register_cancel`
    /// afterwards, because `resolve` now does both, under one lock hold, before returning) —
    /// but through the same primitives `resolve`/`edit` used before this fix
    /// (`identity_of` read under the catalog lock, then `register_cancel` as its own call),
    /// channel-synchronized against a *real* `switch_catalog` so the switch's `trip_catalog` is
    /// guaranteed to land in the gap, with no timing luck involved. It pins the hazard's
    /// mechanism and would catch a regression that reintroduces a separate, unsynchronized
    /// `register_cancel` call anywhere in `edit`'s neighbourhood.
    ///
    /// Why not force this through `resolve` with a race instead: the actual gap a reverted fix
    /// would reopen is a couple of machine instructions wide (a lock `drop` immediately
    /// followed by a non-blocking `HashMap` insert) — far narrower than realistic OS thread
    /// scheduling reliably lands in, even with deliberate contention (measured: 50 iterations
    /// of racing a real `resolve` against a real `switch_catalog`, with both threads already
    /// parked on the shared catalog mutex before release, did not reproduce it once). Treat
    /// `resolve`'s atomicity itself as verified by inspection — the function below never drops
    /// its `guard` before calling `register_cancel` — backed by this deterministic
    /// reconstruction of what happens when that is not true.
    #[test]
    fn registering_after_a_switch_already_tripped_leaves_a_stale_untripped_entry() {
        let dir = crate::test_support::TestTmpDir::new("rapidraw-resolve-switch-sequence");
        let a_root = dir.join("a");
        std::fs::create_dir_all(&a_root).unwrap();
        let a = Catalog::open(&dir.join("a.chairphoto"), &a_root).unwrap();
        let state = AppState::default();
        *state.catalog.lock().unwrap() = Some(a);
        let a_identity = crate::app::catalog_identity(&state).unwrap();
        let pid = 987_654_335;
        clear_cancel(a_identity, pid);

        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let state_for_thread = state.clone();
        let late_register = std::thread::spawn(move || {
            // Step 1: read the identity under the catalog lock, then drop it — `resolve`'s
            // pre-#188-M3 shape.
            let identity = {
                let guard = state_for_thread.catalog.lock().unwrap();
                let catalog = guard.as_ref().unwrap();
                identity_of(catalog)
            };
            // Step 2: let the real switch run to completion, and wait for it to finish.
            ready_tx.send(()).unwrap();
            go_rx.recv().unwrap();
            // Step 3: register only now — `edit`'s pre-fix, separate, later call.
            register_cancel(identity, pid, 1, Arc::new(AtomicBool::new(false)))
        });

        ready_rx.recv().unwrap();
        crate::app::catalogs::switch_catalog(&state, &dir.join("b.chairphoto"), &dir.join("b"), true, None)
            .expect("switch to a fresh catalog");
        go_tx.send(()).unwrap();

        let flag = late_register.join().unwrap().expect("nothing else holds this (catalog, photo) slot");
        assert!(
            !flag.load(Ordering::Relaxed),
            "the switch's trip ran before this entry existed, so a register call made strictly after it can never be \
             reached by that trip — exactly the hazard #188 M3 closes by keeping resolve's identity read and its \
             registration under one uninterrupted catalog-lock hold"
        );
        clear_cancel(a_identity, pid);
    }

    /// #188 L2: a re-root that trips RapidRAW's round-trips *before* persisting the new root
    /// leaves them cancelled even when the persist then fails and the whole re-root aborts
    /// with the catalog still open (`before_drop`'s `?` skips `job_guards.trip_and_clear_all()`
    /// and `*cat_guard = None` on that path, per `reroot`'s own "A failed persist leaves the
    /// catalog open and nothing tripped" comment) — RapidRAW was the one exception to that
    /// comment being true. Forces the failure by dropping the `settings` table out from under
    /// the open connection (mirrors the identity-repair tests' `DROP TRIGGER` technique) so
    /// `set_setting` fails; a round-trip registered beforehand must still be untripped
    /// afterwards, and the catalog must still be open.
    /// (Mutation-checked: moving `crate::rapidraw::trip_catalog` back before `set_setting` in
    /// `catalogs::reroot` — the pre-fix order — trips the flag even though the re-root as a
    /// whole failed; this test then fails.)
    #[test]
    fn a_reroot_whose_persist_fails_does_not_trip_rapidraws_round_trips() {
        let dir = crate::test_support::TestTmpDir::new("rapidraw-reroot-persist-fails");
        let a_root = dir.join("a");
        std::fs::create_dir_all(&a_root).unwrap();
        let a = Catalog::open(&dir.join("a.chairphoto"), &a_root).unwrap();
        a.conn().execute_batch("DROP TABLE settings;").unwrap();
        let state = AppState::default();
        *state.catalog.lock().unwrap() = Some(a);
        let a_identity = crate::app::catalog_identity(&state).unwrap();
        let pid = 987_654_336;
        clear_cancel(a_identity, pid);
        let flag = register_cancel(a_identity, pid, 1, Arc::default()).expect("registers");

        let err = crate::app::catalogs::reroot_open_catalog(&state, dir.join("new")).unwrap_err();
        assert!(err.contains("settings"), "expected the forced set_setting failure: {err}");

        assert!(!flag.load(Ordering::Relaxed), "a re-root that failed to persist must not trip RapidRAW's round-trips");
        assert!(state.catalog.lock().unwrap().is_some(), "the catalog stays open on a failed persist");
        clear_cancel(a_identity, pid);
    }

    /// A job cancelled while queued (#108 gate): `cancel_rapidraw_job` before the worker
    /// starts makes `edit_in_rapidraw_as` end as cancelled (`Ok(None)`) without launching the
    /// binary (a fake script that logs its arguments — never RapidRAW), and the job leaves the
    /// registry. A queued job that is not cancelled does launch it (the positive control).
    /// (Mutation-checked: without the `queued` lookup in `cancel_rapidraw_job`, the cancelled
    /// job launches the fake and this fails.)
    #[test]
    fn a_job_cancelled_before_it_starts_launches_nothing() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = crate::test_support::TestTmpDir::new("rapidraw-cancel-queued");
        let (script, log) = (dir.join("fake-rapidraw.sh"), dir.join("fake-rapidraw.log"));
        std::fs::write(&script, format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 1\n", log.display())).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let c = Catalog::open(&dir.join("c.chairphoto"), &dir.join("photos")).unwrap();
        c.set_setting(BIN_SETTING, script.to_str().unwrap()).unwrap();
        // Photo 3: just needs its own catalog's photo — the registry is per (catalog, photo).
        let photo = (0..3)
            .map(|i| {
                let original = dir.join(format!("photos/p{i}.ARW"));
                std::fs::create_dir_all(original.parent().unwrap()).unwrap();
                std::fs::write(&original, b"raw").unwrap();
                c.upsert_photo(&original, None, 0, 1).unwrap().id
            })
            .last()
            .unwrap();
        let state = AppState::default();
        *state.catalog.lock().unwrap() = Some(c);
        let from = crate::app::catalog_identity(&state).unwrap();
        let rt = crate::app::runtime();

        let job = queue_job();
        let id = job.id();
        cancel_rapidraw_job(from, photo, id).unwrap();
        assert_eq!(rt.block_on(edit_in_rapidraw_as(state.clone(), from, photo, job)), Ok(None));
        assert!(!log.exists(), "launched after its cancel: {:?}", std::fs::read_to_string(&log));
        assert!(!registry().queued.contains_key(&id), "the job left the queue");

        assert!(rt.block_on(edit_in_rapidraw_as(state.clone(), from, photo, queue_job())).is_err(), "the fake exits 1");
        assert_eq!(std::fs::read_to_string(&log).expect("an uncancelled job launches").lines().count(), 1);
        let dropped = queue_job();
        let dropped_id = dropped.id();
        drop(dropped);
        assert!(!registry().queued.contains_key(&dropped_id), "a job dropped unstarted unregisters");
    }

    #[test]
    fn wait_for_stable_output_bails_on_cancel() {
        let dir = crate::test_support::TestTmpDir::new("rapidraw-cancel");
        let out = dir.join("never.tiff"); // never created

        let cancel = Arc::new(AtomicBool::new(false));
        let c2 = cancel.clone();
        let waiter = std::thread::spawn(move || wait_for_stable_output(&out, &c2));
        std::thread::sleep(Duration::from_millis(150));
        cancel.store(true, Ordering::Relaxed);
        assert!(!waiter.join().unwrap(), "cancel must abandon the wait");
    }
}
