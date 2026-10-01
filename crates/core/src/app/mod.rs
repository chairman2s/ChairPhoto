//! Application state and the services every frontend shares — Tauri-free.
//!
//! [`AppState`] (the open catalog, volume health, and every background job's ownership
//! state in [`jobs::JobRegistry`]) plus the helpers more than one domain needs. The Tauri
//! command layer (`commands/`) re-exports all of it; a native frontend links it directly.
//!
//! Startup is [`boot`] (crash markers, upload sweep, theme watcher, decode analyzers, the
//! image pool) and then [`open_default_catalog`]; every front end runs the same two.
//!
//! Blocking work goes through [`spawn_blocking`] on the runtime from [`runtime`], which the
//! Tauri shell also installs as its own async runtime, so there is one tokio runtime
//! whichever frontend is running.

use crate::catalog::Catalog;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};

mod boot;
pub mod catalogs;
pub mod bundles;
pub mod events;
pub mod identity;
pub mod jobs;
pub mod scans;
pub mod storage;

pub use boot::{boot, boot_with, Boot};
// `catalogs::switch_catalog` is deliberately not re-exported here: the Tauri shell re-exports
// this module flat beside a command of the same name.
pub use catalogs::{
    default_catalog_path, detach_catalog_and_trip_jobs, detach_catalog_and_trip_jobs_with,
    load_recent_catalogs, load_recent_catalogs_in, open_default_catalog,
    publish_catalog_and_reset_jobs, record_recent_catalog, record_recent_catalog_in, spawn_detached_phase_b, EnrichJob, RecentCatalog,
};
pub use events::*;

pub use jobs::JobRegistry;
// Every family with a queryable status slot needs these. Unconditional since #34: identity
// repair publishes one and is not feature-gated — see the "Status slots" section of `jobs`.
pub use jobs::{JobClaim, JobStatus};

/// The process's tokio runtime handle: the current one when called from inside a runtime
/// (a Tauri command, a test's `#[tokio::test]`), otherwise a process-wide multi-thread
/// runtime created on first use.
pub fn runtime() -> tokio::runtime::Handle {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    tokio::runtime::Handle::try_current().unwrap_or_else(|_| {
        RUNTIME
            .get_or_init(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .thread_name("chairphoto-rt")
                    .build()
                    .expect("build the tokio runtime")
            })
            .handle()
            .clone()
    })
}

/// Run blocking disk/SQLite/image work on the runtime's blocking pool.
pub fn spawn_blocking<F, R>(f: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    runtime().spawn_blocking(f)
}

/// Shared application state: the open catalog, volume health, every job's ownership
/// state, and where events go.
///
/// Cloning is cheap and **shares** everything (each field is an `Arc`), so a worker thread
/// holds a clone rather than reaching back through a UI toolkit for "the" state.
#[derive(Clone, Default)]
pub struct AppState {
    pub catalog: Arc<Mutex<Option<Catalog>>>,
    /// Short-TTL cache of per-volume reachability, so NAS stats happen off the catalog
    /// lock (on a blocking worker). See `volume_health`.
    pub volume_health: Arc<crate::volume_health::VolumeHealth>,
    /// Every background job's ownership state — abort generations, job-id sequences and
    /// queryable status slots — behind the one protocol they all run. Grouped rather than
    /// left as loose fields so a catalog switch cannot reach some families and miss others.
    /// **Declare new job families in [`JobRegistry`], never directly here.**
    pub jobs: Arc<JobRegistry>,
    /// The frontend's event sink, installed once at startup ([`AppState::set_events`]).
    /// Until then — and in tests that install none — events are dropped.
    events: Arc<OnceLock<Arc<dyn EventSink>>>,
}

impl AppState {
    /// Install the frontend's sink. Only the first call takes effect; returns whether this
    /// one did.
    pub fn set_events(&self, sink: Arc<dyn EventSink>) -> bool {
        self.events.set(sink).is_ok()
    }
}

/// Background work sends through the state it already holds.
impl EventSink for AppState {
    fn send(&self, event: CoreEvent) {
        if let Some(sink) = self.events.get() {
            sink.send(event);
        }
    }
}

/// The develop session's status slot lives with its worker (`develop::session`); it is
/// re-exported here so `JobRegistry` names every family's status in one place.
#[cfg(all(feature = "raw", feature = "edit"))]
pub use crate::develop::session::DevelopStatus;

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
pub fn begin_scan_generation(state: &AppState) -> Result<Arc<AtomicBool>, String> {
    state.jobs.scan.install_fresh()
}

/// Run `f` against the open catalog, holding the catalog lock for its duration.
///
/// **Never on the UI thread.** The lock is shared with every blocking worker; a UI-thread
/// caller that waits for it here parks every repaint for as long as the workers keep the
/// lock busy. That was a 2.2 s freeze on every Develop → Library switch in the Tauri app
/// (the Library mounts ~25 commands at once, and the `get_setting`s among them blocked the
/// window while the tag counts ran). Tauri commands are held to this by
/// `commands_that_take_the_catalog_lock_never_run_on_the_main_thread`.
pub fn with_catalog<T>(
    state: &AppState,
    f: impl FnOnce(&Catalog) -> crate::catalog::Result<T>,
) -> Result<T, String> {
    let guard = state.catalog.lock().map_err(|e| e.to_string())?;
    let catalog = guard.as_ref().ok_or("No catalog is open")?;
    f(catalog).map_err(|e| e.to_string())
}

/// Like `with_catalog`, but runs the closure on a blocking worker thread so the
/// UI thread and the async runtime are never stalled by SQLite work.
pub async fn with_catalog_blocking<T: Send + 'static>(
    state: &AppState,
    f: impl FnOnce(&Catalog) -> crate::catalog::Result<T> + Send + 'static,
) -> Result<T, String> {
    let catalog = state.catalog.clone();
    spawn_blocking(move || {
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
//
// The module lives in its own file so the Tauri shell's command tests can compile the same
// helper into their own test binary (`#[cfg(test)]` items are invisible across crates, and a
// separate process needs its own lock anyway).
#[cfg(test)]
pub(crate) mod test_env_helpers;

/// The bytes the Darkroom's `.rawf` decode cache holds (Preferences → Darkroom): 0 in a build
/// without the decoder and the engine (`raw` + `edit`), which decodes nothing into it.
/// **Blocking** (walks the cache directory): run it off the UI thread.
pub fn decode_cache_usage() -> u64 {
    #[cfg(all(feature = "raw", feature = "edit"))]
    return crate::develop::cache::usage_bytes();
    #[cfg(not(all(feature = "raw", feature = "edit")))]
    0
}

/// Preferences → Darkroom → Clear: empty the decode cache. Returns the bytes freed; 0 in a
/// build without `raw` + `edit`. Photos open in Develop stay open — their working images are
/// in memory; only the next first open pays a decode. **Blocking**: off the UI thread.
pub fn decode_cache_clear() -> u64 {
    #[cfg(all(feature = "raw", feature = "edit"))]
    return crate::develop::cache::trim_to(0);
    #[cfg(not(all(feature = "raw", feature = "edit")))]
    0
}

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
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Expand a leading "~" or "~/" to $HOME. Other paths pass through unchanged.
pub fn expand_home(path: &str) -> PathBuf {
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
mod tests {
    use super::*;

    struct Count(std::sync::atomic::AtomicUsize);

    impl EventSink for Count {
        fn send(&self, _event: CoreEvent) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn ev() -> CoreEvent {
        CoreEvent::CatalogSwitched("x".into())
    }

    /// A worker holds a clone taken before the sink was installed; its events must still
    /// reach the frontend, because a clone shares the state rather than copying it.
    #[test]
    fn a_clone_shares_the_sink_installed_after_it_was_taken() {
        let state = AppState::default();
        let worker = state.clone();
        worker.send(ev()); // before install: dropped, not an error
        let sink = Arc::new(Count(Default::default()));
        assert!(state.set_events(sink.clone()));
        worker.send(ev());
        state.send(ev());
        assert_eq!(sink.0.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn only_the_first_sink_is_installed() {
        let state = AppState::default();
        let first = Arc::new(Count(Default::default()));
        let second = Arc::new(Count(Default::default()));
        assert!(state.set_events(first.clone()));
        assert!(!state.set_events(second.clone()));
        state.send(ev());
        assert_eq!(first.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(second.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
