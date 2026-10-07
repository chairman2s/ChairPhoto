//! Tests never touch the developer's real cache or data directory.
//!
//! The cache (`thumbnails::cache_dir`: thumbnails, previews, offline thumbnails, the decode
//! cache, map tiles) and the data directory (`app::app_data_dir`: the default catalog, the
//! recent-catalogs list, LUTs, crash markers) default to `~/.cache` and `~/.local/share`. A
//! test that reaches either without pointing `XDG_CACHE_HOME` / `XDG_DATA_HOME` at its own
//! temp directory writes into the developer's real ones — the very catalog and caches the
//! app uses (found while fixing #258: tests that opened the default catalog set only
//! `XDG_DATA_HOME`).
//!
//! Two halves:
//! - [`isolate`] points both variables, once per test process, at a directory of that
//!   process's own under the temp dir, unless they already name somewhere else. In this
//!   crate's unit tests `cache_dir` and `app_data_dir` call it before they resolve anything,
//!   and the fixtures do (`test_support::TestTmpDir::new` and its copies, the app's test rig,
//!   `tests/common`'s copy), so every test is isolated by construction. A test that sets
//!   either variable itself still wins.
//! - [`check`], called by `cache_dir` and `app_data_dir`, panics when the app's directory
//!   they resolve is the real one: `chairphoto` under the real home's `.cache` or
//!   `.local/share` (not anything else there — a scratch area under `~/.local/share` is fine). Armed always in this crate's unit
//!   tests, and in a build with the `test-hooks` feature (the app's tests) once [`isolate`]
//!   ran — so a production build, which never has `test-hooks`, never panics, and a build
//!   that has it (`--all-features`) only after a test fixture armed it. A test that must see
//!   the real defaults sets `CHAIRPHOTO_TEST_REAL_HOME=1` ([`ALLOW_VAR`]).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;

/// Set (to anything) to let a test resolve paths under the real home.
pub const ALLOW_VAR: &str = "CHAIRPHOTO_TEST_REAL_HOME";

/// The home the guard protects, when not `HOME`: an audit run sets it, with `XDG_CACHE_HOME`
/// and `XDG_DATA_HOME` pointing under it, to see which tests would have reached the real
/// home's directories — they panic, or leave files there — without touching the real one.
pub const HOME_VAR: &str = "CHAIRPHOTO_TEST_HOME";

static ARMED: AtomicBool = AtomicBool::new(false);
static ISOLATED: Once = Once::new();

/// The directory [`isolate`] pointed `XDG_DATA_HOME` at, if it moved it ([`models_data_home`]).
static ISOLATED_DATA_HOME: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// The variables [`isolate`] points away from the real home, with the home-relative default
/// each falls back to.
const DIRS: [(&str, &str); 2] = [("XDG_CACHE_HOME", ".cache"), ("XDG_DATA_HOME", ".local/share")];

/// Point `XDG_CACHE_HOME` and `XDG_DATA_HOME` at this process's own directory under the temp
/// dir — each one that is unset, empty, relative, or would put the app's directory at the real
/// one ([`is_real_app_dir`]) — and arm [`check`]. Once per process; later calls do nothing.
pub fn isolate() {
    ISOLATED.call_once(|| {
        let own = std::env::temp_dir().join(format!("chairphoto-test-home-{}", std::process::id()));
        for (var, base) in DIRS {
            let current = std::env::var_os(var).map(PathBuf::from);
            let keep = current.is_some_and(|p| p.is_absolute() && !is_real_app_dir(&p.join(APP_DIR), base));
            if !keep {
                let dir = own.join(base.trim_start_matches('.').replace('/', "-"));
                let _ = std::fs::create_dir_all(&dir);
                std::env::set_var(var, &dir);
                if var == "XDG_DATA_HOME" {
                    let _ = ISOLATED_DATA_HOME.set(dir);
                }
            }
        }
        ARMED.store(true, Ordering::SeqCst);
    });
}

/// The data home the environment names now: an absolute `XDG_DATA_HOME`, else
/// `~/.local/share`.
fn data_home_now() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).filter(|p| p.is_absolute()).or_else(|| {
        std::env::var_os("HOME").filter(|h| !h.is_empty()).map(|h| PathBuf::from(h).join(".local/share"))
    })
}

/// Where a test looks for the downloaded face and Smart Tagging models: the data home it
/// would have used without [`isolate`] — `~/.local/share` when `XDG_DATA_HOME` is the
/// directory [`isolate`] set (it only moves one that is unset or under the home), else what
/// `XDG_DATA_HOME` names now — so a test that needs a model the developer downloaded still
/// finds it, and skips as before when there is none. Read from the environment on each call,
/// as `app_data_dir` is. Allowed past [`check`]: the models are read-only, and only an
/// explicit opt-in (`CHAIRPHOTO_TEST_DOWNLOAD_MODELS`) downloads into them, as before.
pub(crate) fn models_data_home() -> Option<PathBuf> {
    let now = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from);
    if now.is_some() && now.as_ref() == ISOLATED_DATA_HOME.get() {
        return std::env::var_os("HOME").filter(|h| !h.is_empty()).map(|h| PathBuf::from(h).join(".local/share"));
    }
    data_home_now()
}

/// `<home>/<base>`, the real default, or `None` with no home. The home is [`HOME_VAR`] if
/// set, else `HOME`.
fn real_default(base: &str) -> Option<PathBuf> {
    std::env::var_os(HOME_VAR)
        .or_else(|| std::env::var_os("HOME"))
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(base))
}

fn armed() -> bool {
    cfg!(test) || ARMED.load(Ordering::SeqCst)
}

/// The app's directory under each base (`<base>/chairphoto`).
const APP_DIR: &str = "chairphoto";

/// Whether `app_dir` is (or is inside) the app's real directory under the real home's `base`.
fn is_real_app_dir(app_dir: &Path, base: &str) -> bool {
    real_default(base).is_some_and(|real| app_dir.starts_with(real.join(APP_DIR)))
}

/// Panic if `app_dir` — the app's directory (`<base>/chairphoto`) that `var` (or its default)
/// resolved to — is the real one under the home's `base` (`.cache`, `.local/share`), while the
/// guard is armed and not allowed ([`ALLOW_VAR`]).
pub(crate) fn check(app_dir: &Path, base: &str, var: &str) {
    if !armed() || std::env::var_os(ALLOW_VAR).is_some() {
        return;
    }
    assert!(
        !is_real_app_dir(app_dir, base),
        "a test resolved {}, the app's real directory: point {var} at the test's temp dir \
         (test_home::isolate does, through the test fixtures), or set {ALLOW_VAR}=1",
        app_dir.display(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guard refuses the real home's directories and passes everything else.
    #[test]
    fn the_guard_refuses_only_the_real_homes_directories() {
        let Some(real) = real_default(".cache") else {
            println!("SKIPPED: the_guard_refuses_only_the_real_homes_directories — HOME is not set");
            return;
        };
        let refused = std::panic::catch_unwind(|| check(&real.join("chairphoto"), ".cache", "XDG_CACHE_HOME"));
        assert!(refused.is_err(), "the real cache is refused");
        let inside = std::panic::catch_unwind(|| check(&real.join("chairphoto/persist-v2"), ".cache", "XDG_CACHE_HOME"));
        assert!(inside.is_err(), "and anything inside it");
        check(&std::env::temp_dir().join("x").join("chairphoto"), ".cache", "XDG_CACHE_HOME");
        check(&real.join("scratch").join("chairphoto"), ".cache", "XDG_CACHE_HOME");
        check(&real.join("chairphoto-agent"), ".cache", "XDG_CACHE_HOME");
    }

    /// After `isolate`, the cache and data directories the crate resolves are this process's
    /// own, never the real home's (with nothing else setting them).
    #[test]
    fn isolate_points_both_directories_away_from_the_real_home() {
        let _env = crate::app::test_env_helpers::ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        isolate();
        for (var, base) in DIRS {
            let set = PathBuf::from(std::env::var_os(var).expect("set"));
            assert!(set.is_absolute(), "{var}={}", set.display());
            assert!(!is_real_app_dir(&set.join(APP_DIR), base), "{var}={}", set.display());
        }
        // Resolved through the real functions: passing the guard is the assertion.
        let _ = crate::thumbnails::cache_dir();
        let _ = crate::app::app_data_dir().unwrap();
    }
}
