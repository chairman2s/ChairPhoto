//! Availability probe for the system ONNX Runtime, shared by the `faces` and `smarttags`
//! backends.
//!
//! Both link ONNX Runtime dynamically (`ort/load-dynamic`), so the library is an optional
//! runtime dependency rather than a link-time one — matching how `exiftool`, `ffmpeg`, and
//! ImageMagick are treated. Missing it must degrade those two modules and nothing else.
//!
//! ort cannot be asked politely whether the runtime is present: its lazy `setup_api()` loads
//! the dylib on first use, and when that fails the call **hangs indefinitely with no error**
//! rather than returning one. A hang is worse than a crash here — no message, no recovery,
//! and a wedged worker — so nothing may reach ort until we know the library is usable.
//!
//! [`ensure_available`] therefore performs the load itself, first, and caches the verdict.
//! It deliberately mirrors ort's own resolution (see `ort::load_dylib_from_path`), because a
//! probe that looked somewhere else would answer a different question than the one that
//! matters:
//!
//! 1. `ORT_DYLIB_PATH` when set and non-empty, else the platform default name;
//! 2. a relative name is tried against the executable's directory first, then left bare for
//!    the dynamic loader's own search path;
//! 3. `OrtGetApiBase` must be present, the runtime's minor version must be at least
//!    [`MIN_MINOR`], and `GetApi(MIN_MINOR)` must return non-null — the last because that is
//!    the call ort actually makes and unwraps, so a runtime can report a new enough version
//!    and still panic there. Refusing all three here means an explicit error instead of that
//!    same hang or panic.

use std::ffi::{c_char, c_void, CStr};
use std::path::PathBuf;
use std::sync::OnceLock;

/// The ONNX Runtime minor version ort requires, taken from ort itself rather than restated
/// here. `ort::MINOR_VERSION` is `ort_sys::ORT_API_VERSION`, which is computed from whichever
/// `api-*` feature is enabled, so raising that feature raises this floor with it and the two
/// cannot drift. A hand-written constant could sit at 24 while the feature moved to 25,
/// leaving the probe to accept runtimes ort then rejects — which is the hang this module
/// exists to prevent.
const MIN_MINOR: u32 = ort::MINOR_VERSION;

/// The two entry points every ONNX Runtime exports. This is the C API's stable entry
/// struct — the one part whose layout cannot change without breaking every consumer — so
/// declaring it here avoids taking a direct dependency on `ort-sys` just to run this check.
/// Only the leading two members are declared because only they are called.
#[repr(C)]
struct OrtApiBase {
    get_api: unsafe extern "C" fn(u32) -> *const c_void,
    get_version_string: unsafe extern "C" fn() -> *const c_char,
}

/// The library name/path ort will use, resolved the way ort resolves it.
fn dylib_path() -> PathBuf {
    let name = match std::env::var("ORT_DYLIB_PATH") {
        Ok(s) if !s.is_empty() => s,
        _ => default_dylib_name().to_owned(),
    };
    let path = PathBuf::from(&name);
    if path.is_absolute() {
        return path;
    }
    // Mirror ort: prefer a copy sitting next to the executable, otherwise hand the bare name
    // to the loader so it searches the usual system paths.
    match std::env::current_exe().ok().and_then(|exe| {
        let candidate = exe.parent()?.join(&path);
        candidate.exists().then_some(candidate)
    }) {
        Some(next_to_exe) => next_to_exe,
        None => path,
    }
}

const fn default_dylib_name() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "onnxruntime.dll"
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        "libonnxruntime.dylib"
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "ios")))]
    {
        "libonnxruntime.so"
    }
}

/// Load the runtime, read its version, and confirm it serves the API level ort will ask for.
fn probe() -> Result<String, String> {
    probe_at(&dylib_path())
}

/// [`probe`] against an explicit path, so tests can exercise the failure path without
/// mutating `ORT_DYLIB_PATH` — which would race every other test in the binary.
///
/// On success the [`libloading::Library`] handle is deliberately leaked. ort dlopens the same
/// file moments later; keeping this handle makes that a refcount bump instead of an unload
/// followed by a fresh map. Dropping it here would take the refcount to zero — nothing else
/// holds a reference at probe time — and unloading a library that registers atexit handlers
/// and thread-local destructors, as ONNX Runtime does, only to immediately reload it is risk
/// taken for nothing. One handle per process, and only when the runtime is usable.
fn probe_at(path: &std::path::Path) -> Result<String, String> {
    // Deliberately worded to avoid the "not available"/"no file" phrasing the model managers
    // use: a runtime failure and a missing-model failure must stay tellable apart, including
    // by the tests that assert on each.
    //
    // SAFETY: loading a shared library runs its initialisers, which is exactly what ort will
    // do moments later.
    let lib = unsafe { libloading::Library::new(path) }.map_err(|e| {
        format!(
            "ONNX Runtime could not be loaded from `{}` ({e}). \
             Install onnxruntime (Arch: onnxruntime-cpu, or onnxruntime-cuda for GPU), \
             or set ORT_DYLIB_PATH.",
            path.display()
        )
    })?;

    // Scoped so the symbol's borrow of `lib` ends before the handle is leaked below.
    let probed = (|| -> Result<String, String> {
        // SAFETY: `OrtGetApiBase` and the two members read from it are the ONNX Runtime C
        // API's documented entry points, and their layout is fixed by that contract.
        let base_getter: libloading::Symbol<unsafe extern "C" fn() -> *const OrtApiBase> =
            unsafe { lib.get(b"OrtGetApiBase") }.map_err(|_| {
                format!(
                    "`{}` is not an ONNX Runtime library (no OrtGetApiBase symbol).",
                    path.display()
                )
            })?;
        let base = unsafe { base_getter() };
        if base.is_null() {
            return Err(format!("`{}` returned a null OrtApiBase.", path.display()));
        }

        let version = unsafe { CStr::from_ptr(((*base).get_version_string)()) }
            .to_string_lossy()
            .into_owned();
        let Some(minor) = version.split('.').nth(1).and_then(|m| m.parse::<u32>().ok()) else {
            // Distinct from "too old": an unparseable version is not evidence of an old
            // runtime, and telling the user to upgrade a current one wastes their time.
            return Err(format!(
                "could not read a version from the ONNX Runtime at `{}` (got {version:?}).",
                path.display()
            ));
        };
        if minor < MIN_MINOR {
            return Err(format!(
                "ONNX Runtime {version} at `{}` is too old; 1.{MIN_MINOR} or newer is required.",
                path.display()
            ));
        }

        // The version string is not the check that matters. ort calls
        // `GetApi(ORT_API_VERSION)` and unwraps the result, so a runtime that reports a new
        // enough version but declines that API level would pass every check above and then
        // panic inside ort. Ask the same question ort asks.
        if unsafe { ((*base).get_api)(MIN_MINOR) }.is_null() {
            return Err(format!(
                "ONNX Runtime {version} at `{}` does not provide API version {MIN_MINOR}.",
                path.display()
            ));
        }

        Ok(version)
    })();

    match probed {
        Ok(version) => {
            std::mem::forget(lib);
            Ok(version)
        }
        // Unusable: let the handle drop, since nothing will load it.
        Err(e) => Err(e),
    }
}

/// `Ok(version)` when the system ONNX Runtime is present and new enough, `Err(reason)`
/// otherwise. Probed once and cached, so a machine without the runtime pays one failed
/// `dlopen` rather than one per session.
///
/// Call this before touching any `ort` API. Returning its error keeps a missing runtime a
/// reported failure of one module instead of a hang.
pub fn ensure_available() -> Result<&'static str, String> {
    static CELL: OnceLock<Result<String, String>> = OnceLock::new();
    match CELL.get_or_init(probe) {
        Ok(version) => Ok(version.as_str()),
        Err(e) => Err(e.clone()),
    }
}

/// Build one ONNX session against the model at `model_path`.
///
/// **This is the only supported way to obtain an `ort` session.** Constructing one directly
/// skips [`ensure_available`], and ort's loader does not fail when the runtime is missing —
/// it hangs indefinitely with no error. A module that forgot the preflight would not fail
/// loudly during review; it would ship, and then wedge for any user without the runtime
/// installed. `ort_sessions_are_only_built_through_this_module` fails the build rather than
/// relying on the next author knowing that.
///
/// `register_ep` is where a caller adds an execution provider, because that part genuinely
/// differs per module: the CUDA feature gates (`faces-cuda`, `smarttags-cuda`) and the
/// force-CPU settings are owned by each plugin. It returns whether its provider was
/// registered, which is returned alongside the session so callers can report GPU-vs-CPU.
/// Registering must never be fatal — a provider that fails to attach leaves the builder on
/// CPU.
#[cfg(any(feature = "faces", feature = "smarttags"))]
pub fn build_session<F>(
    model_path: &std::path::Path,
    intra_threads: usize,
    register_ep: F,
) -> Result<(ort::session::Session, bool), String>
where
    F: FnOnce(&mut ort::session::builder::SessionBuilder) -> bool,
{
    ensure_available()?;
    let mut builder = ort::session::Session::builder()
        .map_err(|e| e.to_string())?
        .with_intra_threads(intra_threads.max(1))
        .map_err(|e| e.to_string())?;
    let registered = register_ep(&mut builder);
    let session = builder
        .commit_from_file(model_path)
        .map_err(|e| e.to_string())?;
    Ok((session, registered))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// When a runtime *is* installed the probe must accept it and report a version at or above
    /// the floor — this is the only test that can prove the probe does not reject a good
    /// runtime. CI runners have no ONNX Runtime, so an absent one auto-skips (printing why),
    /// matching `tests/faces_engine.rs`; the failure path is covered unconditionally below.
    #[test]
    fn probe_accepts_an_installed_runtime() {
        // The floor is derived from ort's enabled api-* feature, so it is not readable from
        // this file. Print it: a wrong derivation would otherwise be invisible here, since
        // too low a floor accepts everything and asserts nothing.
        eprintln!("probe floor: ONNX Runtime 1.{MIN_MINOR} or newer");
        assert!(
            MIN_MINOR >= 17,
            "derived floor {MIN_MINOR} is below the oldest API ort supports; \
             ort::MINOR_VERSION did not resolve to a real API level"
        );
        match ensure_available() {
            Ok(version) => {
                let minor: u32 = version
                    .split('.')
                    .nth(1)
                    .and_then(|m| m.parse().ok())
                    .unwrap_or_else(|| panic!("unparseable ONNX Runtime version {version:?}"));
                assert!(
                    minor >= MIN_MINOR,
                    "probe accepted ONNX Runtime {version}, below the 1.{MIN_MINOR} floor"
                );
            }
            Err(e) => eprintln!("skipping: no usable ONNX Runtime on this machine ({e})"),
        }
    }

    /// A missing runtime must produce an error rather than the hang ort exhibits when it
    /// loads the dylib itself. This is the whole reason the probe exists.
    #[test]
    fn missing_runtime_reports_an_error() {
        let err = probe_at(std::path::Path::new("/nonexistent/libonnxruntime.so"))
            .expect_err("a nonexistent dylib must not probe as available");
        assert!(
            err.contains("could not be loaded"),
            "error should explain the runtime is missing, got: {err}"
        );
    }

    /// No module may construct an ort session itself; [`build_session`] is the only route,
    /// because it is the only one that runs the preflight.
    ///
    /// This is the point of the facade. A future plugin — a car detector, anything — that
    /// calls `Session::builder()` directly compiles cleanly, passes review, and then hangs
    /// forever for every user without ONNX Runtime installed. Nothing else catches that: the
    /// author gets a working build because *their* machine has the runtime, exactly as
    /// happened when this change was first written. So the rule is enforced here instead of
    /// documented and hoped for.
    ///
    /// Scans source rather than relying on visibility because Rust has no way to restrict a
    /// dependency to one module within a crate.
    #[test]
    fn ort_sessions_are_only_built_through_this_module() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let facade = src.join("plugins").join("onnx.rs");

        let offenders: Vec<String> = walkdir::WalkDir::new(&src)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .filter(|e| e.path().extension().is_some_and(|x| x == "rs"))
            .filter(|e| e.path() != facade)
            .filter(|e| {
                std::fs::read_to_string(e.path())
                    .is_ok_and(|text| text.contains("Session::builder"))
            })
            .map(|e| e.path().display().to_string())
            .collect();

        assert!(
            offenders.is_empty(),
            "these build an ort session directly and so skip the runtime preflight, which \
             means they hang instead of erroring when ONNX Runtime is absent — use \
             plugins::onnx::build_session instead: {offenders:?}"
        );
    }

    /// A real library that is not ONNX Runtime must be rejected on the missing symbol rather
    /// than loaded and handed to ort, which would fail far less clearly.
    #[test]
    fn non_onnx_library_is_rejected() {
        let libc = std::path::Path::new("/usr/lib/libc.so.6");
        if !libc.exists() {
            return; // not a glibc system; nothing to assert against
        }
        let err = probe_at(libc).expect_err("libc is not an ONNX Runtime");
        assert!(
            err.contains("OrtGetApiBase"),
            "error should name the missing symbol, got: {err}"
        );
    }
}
