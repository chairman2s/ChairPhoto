//! Tauri commands — the bridge between the React frontend and the Rust catalog.
//!
//! Every command takes the shared [`AppState`] (a mutex-guarded open catalog) and
//! returns a serializable value or a string error. Errors are stringified here so
//! the frontend gets a plain message; richer typing can come later if needed.
//!
//! The commands themselves live in the domain submodules below; this file keeps only
//! the state and the helpers more than one domain needs. The imports here are the
//! shared surface the submodules pick up through their `use super::*` — keep them
//! broad enough to serve the submodules, not just this file's own code.

use crate::catalog::{
    Album, Catalog, IptcFields, MetadataEntry, Photo, PhotoLocation, PhotoVersion, PickState,
    Publication, SmartAlbum, Tag, TagGroup, TagTerm, TagWithCount, Volume,
};
use crate::scanner::ScanResult;
use crate::thumbnails::{preview_bytes, thumbnail_bytes};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tauri::State;

// ── Domain submodules ────────────────────────────────────────────────────────
//
// Each holds one domain's `#[tauri::command]`s plus the helpers only that domain
// uses. Anything shared by two or more domains stays in this file (`with_catalog`,
// `AppState`, `app_data_dir`, …). Commands are re-exported flat, so `lib.rs` keeps
// referring to them as `commands::<name>` and the frontend is unaffected.

mod ai;
mod albums;
mod burst;
mod catalog;
#[cfg(feature = "collage")]
mod collage;
mod editing;
mod export;
#[cfg(feature = "faces")]
mod faces;
#[cfg(feature = "flickr")]
mod flickr;
mod graph;
mod images;
mod indexing;
#[cfg(feature = "instagram")]
mod instagram;
#[cfg(feature = "localsend")]
mod localsend;
#[cfg(feature = "map")]
mod map;
mod photos;
mod publications;
// Shared publish/transfer helpers. Compiled for LocalSend and Instagram too: they render
// through the same job-scoped temp directories (`publishing::JobTempDir`).
#[cfg(any(feature = "flickr", feature = "smugmug", feature = "instagram", feature = "localsend"))]
pub(crate) mod publishing;
mod scan;
mod settings;
#[cfg(feature = "slideshow")]
mod slideshow;
#[cfg(feature = "smarttags")]
mod smarttags;
#[cfg(feature = "smugmug")]
mod smugmug;
mod storage;
mod tags;

pub use ai::*;
pub use albums::*;
pub use burst::*;
pub use catalog::*;
#[cfg(feature = "collage")]
pub use collage::*;
pub use editing::*;
pub use export::*;
#[cfg(feature = "faces")]
pub use faces::*;
#[cfg(feature = "flickr")]
pub use flickr::*;
pub use graph::*;
pub use images::*;
pub use indexing::*;
#[cfg(feature = "instagram")]
pub use instagram::*;
#[cfg(feature = "localsend")]
pub use localsend::*;
#[cfg(feature = "map")]
pub use map::*;
pub use photos::*;
pub use publications::*;
pub use scan::*;
pub use settings::*;
#[cfg(feature = "slideshow")]
pub use slideshow::*;
#[cfg(feature = "smarttags")]
pub use smarttags::*;
#[cfg(feature = "smugmug")]
pub use smugmug::*;
pub use storage::*;
pub use tags::*;

/// Shared application state: the currently open catalog, if any.
#[derive(Default)]
pub struct AppState {
    pub catalog: Arc<Mutex<Option<Catalog>>>,
    /// Short-TTL cache of per-volume reachability, so NAS stats happen off the catalog
    /// lock (on a blocking worker). See `volume_health`.
    pub volume_health: Arc<crate::volume_health::VolumeHealth>,
    /// Abort flag for the currently-running scan generation (I4b/I6c). Held as a
    /// **swappable** `Arc<AtomicBool>`: a catalog switch trips the current flag so an
    /// in-flight scan (Phase A *or* a detached Phase B) stops writing before the old
    /// catalog is torn down. Each new scan installs a *fresh* flag via
    /// [`begin_scan_generation`], which also trips the previous one — so a second scan
    /// aborts the earlier scan's still-running detached Phase B instead of racing it on
    /// the same catalog. The worker threads each hold a clone of their generation's flag.
    pub scan_abort: Mutex<Arc<AtomicBool>>,
    /// Abort flag for any in-flight face-indexing job (H13b). Same swappable-Arc pattern
    /// as `scan_abort`: `faces_index_photos` installs a fresh flag, `faces_index_cancel`
    /// trips it, and a catalog switch trips it under the catalog lock (both phases). The
    /// worker holds a clone of its generation's flag so a cancel never races a subsequent
    /// re-index.
    #[cfg(feature = "faces")]
    pub faces_abort: Mutex<Arc<AtomicBool>>,
    /// Live status of the face-indexing job, `None` when idle. Written by the worker
    /// (created at job start, updated on each progress event, cleared on completion —
    /// only if a newer job hasn't overwritten it). Lets the UI re-attach to a running
    /// job after a panel remount instead of believing it's idle (`faces_index_status`).
    #[cfg(feature = "faces")]
    pub faces_job: Arc<Mutex<Option<FacesJobStatus>>>,
    /// Monotonic id source for face-indexing jobs. Progress/done events carry the job id
    /// so the UI can ignore events from a superseded job.
    #[cfg(feature = "faces")]
    pub faces_job_seq: std::sync::atomic::AtomicU64,
    /// Abort flag for the face **matching** job (H13c), separate from `faces_abort` so
    /// cancelling a match does not stop an index and vice versa. Same swappable-Arc
    /// pattern: `faces_run_matching` installs a fresh flag, `faces_match_cancel` trips it,
    /// and a catalog switch trips it under the catalog lock in both phases.
    #[cfg(feature = "faces")]
    pub faces_match_abort: Mutex<Arc<AtomicBool>>,
    /// Live status of the face-matching job, `None` when idle. Same ownership rules as
    /// `faces_job`: the worker clears it on the way out, and only if it still owns it.
    #[cfg(feature = "faces")]
    pub faces_match_job: Arc<Mutex<Option<FacesMatchJobStatus>>>,
    /// Monotonic id source for face-matching jobs, independent of `faces_job_seq` so the
    /// two job families never collide on an id.
    #[cfg(feature = "faces")]
    pub faces_match_job_seq: std::sync::atomic::AtomicU64,
    /// Abort flag for the sharpness-indexing job (H16b). Same swappable-Arc pattern as
    /// `scan_abort` and `faces_abort`: `index_sharpness` installs a fresh flag,
    /// `sharpness_index_cancel` trips it.
    pub sharpness_abort: Mutex<Arc<AtomicBool>>,
    /// Monotonic id source for sharpness-indexing jobs. Progress/done events carry the
    /// job id so the UI can ignore events from a superseded run.
    pub sharpness_job_seq: std::sync::atomic::AtomicU64,
    /// Abort flag for the perceptual-hash indexing job (H15a). Same swappable-Arc pattern
    /// as `sharpness_abort`: `index_phashes` installs a fresh flag, `phash_index_cancel`
    /// trips it.
    pub phash_abort: Mutex<Arc<AtomicBool>>,
    /// Monotonic id source for perceptual-hash indexing jobs. Progress/done events carry
    /// the job id so the UI can ignore events from a superseded run.
    pub phash_job_seq: std::sync::atomic::AtomicU64,
    /// Abort flag for the Smart Tagging embedding-index job (H7b). Same swappable-Arc
    /// pattern as `faces_abort`: `smarttags_index_photos` installs a fresh flag,
    /// `smarttags_index_cancel` trips it, and `switch_catalog` trips it under the catalog
    /// lock so a job can never keep indexing a catalog the user has left.
    #[cfg(feature = "smarttags")]
    pub smarttags_abort: Mutex<Arc<AtomicBool>>,
    /// Monotonic id source for Smart Tagging index jobs. Progress/done events carry the
    /// job id so the UI can ignore events from a superseded run.
    #[cfg(feature = "smarttags")]
    pub smarttags_job_seq: std::sync::atomic::AtomicU64,
    /// Live status of the Smart Tagging indexing job, `None` when idle. Written by the worker
    /// (created at job start, updated on each progress event, cleared on completion —
    /// only if a newer job hasn't overwritten it). Lets the UI re-attach to a running
    /// job after a panel remount instead of believing it's idle (`smarttags_index_status`).
    #[cfg(feature = "smarttags")]
    pub smarttags_job: Arc<Mutex<Option<SmarttagsJobStatus>>>,
}

/// Snapshot of the running face-indexing job (`faces_index_status`).
#[cfg(feature = "faces")]
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct FacesJobStatus {
    pub job: u64,
    pub done: usize,
    pub total: usize,
}

/// Snapshot of the running face-matching job (`faces_match_status`).
///
/// Carries the pipeline step as well as the counters, because matching's `total` restarts
/// at each phase — a bare `done`/`total` would look like the job was going backwards.
#[cfg(feature = "faces")]
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct FacesMatchJobStatus {
    pub job: u64,
    pub done: usize,
    pub total: usize,
    pub phase: &'static str,
}

/// Snapshot of the running Smart Tagging embedding-index job (`smarttags_index_status`).
#[cfg(feature = "smarttags")]
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct SmarttagsJobStatus {
    pub job: u64,
    pub done: usize,
    pub total: usize,
}

const _: () = {
    #[allow(dead_code)]
    fn assert_send_sync<T: Send + Sync>() {}
    #[allow(dead_code)]
    fn check() {
        assert_send_sync::<AppState>();
    }
};

/// Begin a new scan generation (I6c). Trips the *previous* generation's abort flag — so
/// any in-flight scan from an earlier call (its Phase A, or a still-running detached
/// Phase B) stops writing — then installs and returns a fresh, un-tripped flag for this
/// scan. Because the previous scan's workers hold a clone of the *old* Arc (now tripped),
/// and this scan holds the fresh one, a second scan cannot race the earlier scan's
/// detached Phase B on the same catalog.
fn begin_scan_generation(state: &AppState) -> Result<Arc<AtomicBool>, String> {
    let mut guard = state.scan_abort.lock().map_err(|e| e.to_string())?;
    // Trip the previous generation so its workers (Phase A / detached Phase B) bail.
    guard.store(true, Ordering::Relaxed);
    let fresh = Arc::new(AtomicBool::new(false));
    *guard = fresh.clone();
    Ok(fresh)
}

/// Run a closure against the open catalog, or return an error if none is open.
fn with_catalog<T>(
    state: &State<'_, AppState>,
    f: impl FnOnce(&Catalog) -> crate::catalog::Result<T>,
) -> Result<T, String> {
    let guard = state.catalog.lock().map_err(|e| e.to_string())?;
    let catalog = guard.as_ref().ok_or("No catalog is open")?;
    f(catalog).map_err(|e| e.to_string())
}

/// Like `with_catalog`, but runs the closure on a blocking worker thread so the
/// main thread and the async runtime are never stalled by SQLite work.
async fn with_catalog_blocking<T: Send + 'static>(
    state: &AppState,
    f: impl FnOnce(&Catalog) -> crate::catalog::Result<T> + Send + 'static,
) -> Result<T, String> {
    let catalog = state.catalog.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let guard = catalog.lock().map_err(|e| e.to_string())?;
        let c = guard.as_ref().ok_or("No catalog is open")?;
        f(c).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

// ── Shared test helpers (env-var serialization) ───────────────────────────────
//
// Several test modules in this file mutate process-wide environment variables
// (e.g. XDG_DATA_HOME) and therefore need a *single* process-wide Mutex so that
// mutations from different test modules are serialized even when cargo runs them
// concurrently on multiple threads.
//
// Rules:
//   • Every test module that calls `std::env::set_var` / `remove_var` on any key
//     that affects `app_data_dir()` MUST acquire `test_env_helpers::ENV_LOCK`
//     (via `EnvGuard::set`) before mutating and hold it until the test ends.
//   • Use `test_env_helpers::EnvGuard::set(key, value)` — do NOT declare a
//     separate `static ENV_LOCK` inside individual test modules; separate statics
//     are independent instances and provide no cross-module exclusion.
#[cfg(test)]
mod test_env_helpers {
    use std::sync::Mutex;

    /// Process-wide lock for env-var mutations. **One static for the whole crate.**
    pub static ENV_LOCK: Mutex<()> = Mutex::new(());

    pub struct EnvGuard {
        pub key: &'static str,
        pub original: Option<std::ffi::OsString>,
        pub _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        pub fn set(key: &'static str, value: &str) -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let original = std::env::var_os(key);
            std::env::set_var(key, value);
            EnvGuard { key, original, _lock: lock }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

// ── Tests for list_external_modules (H8b) ─────────────────────────────────────

/// The app's data dir (`$XDG_DATA_HOME/chairphoto` or `~/.local/share/chairphoto`) —
/// home of the default catalog DB and the user's LUT folder.
pub fn app_data_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME is not set")?;
    Ok(std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"))
        .join("chairphoto"))
}

/// The folder holding user-supplied `.cube` LUTs (created on first use). Edit records
/// reference LUTs by bare filename resolved against this folder.
pub fn luts_dir() -> Result<PathBuf, String> {
    let dir = app_data_dir()?.join("luts");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

/// Unix seconds, for stamping rows written by the command layer (AI/Smart Tagging
/// suggestion timestamps, face matcher rejection memory and cluster creation).
///
/// Saturates to 0 rather than panicking if the system clock is before the epoch — a
/// bad timestamp is not worth taking the app down for.
#[cfg(any(feature = "ai", feature = "smarttags", feature = "faces"))]
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Expand a leading "~" or "~/" to $HOME. Other paths pass through unchanged.
fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    } else if path == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(path)
}
