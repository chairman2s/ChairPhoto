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
mod appearance;
mod burst;
mod catalog;
#[cfg(feature = "collage")]
mod collage;
mod culling;
mod editing;
mod export;
#[cfg(feature = "faces")]
mod faces;
#[cfg(feature = "flickr")]
mod flickr;
mod graph;
mod images;
mod indexing;
// Not a command module: the shared job-ownership protocol (abort generations, job ids,
// status slots, and the start/switch transitions) every background job family runs.
pub mod jobs;
#[cfg(feature = "instagram")]
mod instagram;
#[cfg(feature = "localsend")]
mod localsend;
#[cfg(feature = "map")]
mod map;
// The host-mediated `api.fetch` proxy (#49). Feature-gated because it is the only core
// command needing an HTTP client, and `reqwest` is an optional dependency.
#[cfg(feature = "module-fetch")]
mod net;
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
pub use appearance::*;
pub use burst::*;
pub use culling::*;
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
// Re-exported flat like the command modules so the domain submodules pick these up through
// their `use super::*`. The rest of the ownership vocabulary (`AbortGeneration`, `JobFamily`,
// `JobSlot`) is reached through the registry, so it stays namespaced under `jobs::`.
pub use jobs::JobRegistry;
// Every family with a queryable status slot needs these. Unconditional since #34: identity
// repair publishes one and is not feature-gated — see the "Status slots" section of `jobs`.
pub use jobs::{JobClaim, JobStatus};
#[cfg(feature = "instagram")]
pub use instagram::*;
#[cfg(feature = "localsend")]
pub use localsend::*;
#[cfg(feature = "map")]
pub use map::*;
#[cfg(feature = "module-fetch")]
pub use net::*;
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
    /// Every background job's ownership state — abort generations, job-id sequences and
    /// queryable status slots — behind the one protocol they all run. Grouped rather than
    /// left as loose fields so a catalog switch cannot reach some families and miss others.
    /// **Declare new job families in [`JobRegistry`], never directly here.**
    pub jobs: JobRegistry,
}

/// Snapshot of the running face-indexing job (`faces_index_status`).
#[cfg(feature = "faces")]
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct FacesJobStatus {
    pub job: u64,
    pub done: usize,
    pub total: usize,
}

#[cfg(feature = "faces")]
impl JobStatus for FacesJobStatus {
    fn job_id(&self) -> u64 {
        self.job
    }
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

#[cfg(feature = "faces")]
impl JobStatus for FacesMatchJobStatus {
    fn job_id(&self) -> u64 {
        self.job
    }
}

/// Snapshot of the running Smart Tagging embedding-index job (`smarttags_index_status`).
#[cfg(feature = "smarttags")]
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct SmarttagsJobStatus {
    pub job: u64,
    pub done: usize,
    pub total: usize,
}

#[cfg(feature = "smarttags")]
impl JobStatus for SmarttagsJobStatus {
    fn job_id(&self) -> u64 {
        self.job
    }
}

/// Snapshot of the running sidecar-identity repair pass (`identity_repair_status`), #34.
///
/// `total` is the un-dismissed queue ROWS when the pass started, not copies: the pass
/// retries each owed field, so a copy owing both `identifier` and `import_batch` is two
/// units of work here while the debt panel's header counts it as one copy. The two answer
/// different questions and are deliberately not the same number.
///
/// Unlike every other status here this is not feature-gated — identity debt is core.
#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentityRepairJobStatus {
    pub job: u64,
    pub done: usize,
    pub total: usize,
}

impl JobStatus for IdentityRepairJobStatus {
    fn job_id(&self) -> u64 {
        self.job
    }
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
///
/// The scan publishes no queryable status slot (its progress is event-only), so the whole
/// transition is [`jobs::AbortGeneration::install_fresh`] — the slot-less half of what
/// `jobs::JobFamily::begin` does for Faces and Smart Tagging.
fn begin_scan_generation(state: &AppState) -> Result<Arc<AtomicBool>, String> {
    state.jobs.scan.install_fresh()
}

/// Run a closure against the open catalog, or return an error if none is open.
/// Run `f` against the open catalog, holding the catalog lock for its duration.
///
/// **Never on the main thread.** A command that calls this must be `async fn` or carry
/// `#[tauri::command(async)]`, so its body runs on the async runtime instead of the GTK
/// main thread. The lock is shared with every blocking worker; a plain sync command that
/// waits for it here parks the UI thread, and with it every repaint and every IPC
/// response, for as long as the workers keep the lock busy. That was a 2.2 s freeze on
/// every Develop → Library switch (the Library mounts ~25 commands at once, and the
/// `get_setting`s among them blocked the window while the tag counts ran). The rule is
/// enforced by `commands_that_take_the_catalog_lock_never_run_on_the_main_thread`.
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
// Several test modules in this crate (this file's submodules, and `appearance`)
// mutate process-wide environment variables (e.g. XDG_DATA_HOME, XDG_STATE_HOME)
// and therefore need a *single* process-wide Mutex so that
// mutations from different test modules are serialized even when cargo runs them
// concurrently on multiple threads.
//
// Rules:
//   • Every test module that calls `std::env::set_var` / `remove_var` on any key
//     that affects an env-derived path (`app_data_dir()`, the Omarchy state root, …)
//     MUST acquire `test_env_helpers::ENV_LOCK`
//     (via `EnvGuard::set`) before mutating and hold it until the test ends.
//   • Use `test_env_helpers::EnvGuard::set(key, value)` — do NOT declare a
//     separate `static ENV_LOCK` inside individual test modules; separate statics
//     are independent instances and provide no cross-module exclusion.
#[cfg(test)]
pub(crate) mod test_env_helpers {
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
/// suggestion timestamps, face matcher rejection memory and cluster creation, tag-merge
/// tombstones).
///
/// Ungated: tag maintenance is core, so this is reachable in a build with every plugin
/// feature compiled out.
///
/// Saturates to 0 rather than panicking if the system clock is before the epoch — a
/// bad timestamp is not worth taking the app down for.
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

#[cfg(test)]
mod thread_rules {
    /// Source scan: every `#[tauri::command]` in `commands/` whose body takes the catalog
    /// lock (`with_catalog(`, `with_catalog_blocking(`, `state.catalog.lock()`) must not run on
    /// the main thread — it is either an `async fn` or marked `#[tauri::command(async)]`.
    /// See `with_catalog`'s doc for the freeze this prevents. The scan is deliberately
    /// syntactic: a bare `#[tauri::command]` directly above a `pub fn` whose body (up to
    /// the next line that is exactly `}`) mentions the lock is a failure.
    #[test]
    fn commands_that_take_the_catalog_lock_never_run_on_the_main_thread() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/commands");
        let mut offenders = Vec::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let src = std::fs::read_to_string(&path).unwrap();
            let lines: Vec<&str> = src.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                if line.trim() != "#[tauri::command]" {
                    continue;
                }
                // Skip attribute/doc lines to the signature.
                let mut j = i + 1;
                while j < lines.len() && (lines[j].starts_with("#[") || lines[j].starts_with("///")) {
                    j += 1;
                }
                let Some(sig) = lines.get(j) else { continue };
                if !sig.starts_with("pub fn ") {
                    continue; // async fn: fine
                }
                let name = sig["pub fn ".len()..].split('(').next().unwrap_or("?");
                // Body: up to the first line that is exactly "}".
                let end = lines[j..].iter().position(|l| *l == "}").map(|k| j + k).unwrap_or(lines.len());
                let body = lines[j..end].join("\n");
                if body.contains("with_catalog(") || body.contains("with_catalog_blocking(") || body.contains(".catalog.lock()") {
                    offenders.push(format!("{}:{} {name}", path.file_name().unwrap().to_string_lossy(), i + 1));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "sync commands taking the catalog lock on the main thread — mark them \
             #[tauri::command(async)] or make them async fn:\n{}",
            offenders.join("\n")
        );
    }
}
