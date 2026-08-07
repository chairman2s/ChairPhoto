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
//! 3. `OrtGetApiBase` must be present, and the runtime's minor version must be at least
//!    [`MIN_MINOR`] — ort refuses an older runtime, and refusing it here means an explicit
//!    error instead of that same hang.

use std::ffi::{c_char, c_void, CStr};
use std::path::PathBuf;
use std::sync::OnceLock;

/// The ONNX Runtime minor version ort targets, from its `api-24` feature (`1.24.x`). ort
/// rejects anything older, so the probe must apply the same floor.
const MIN_MINOR: u32 = 24;

/// The two entry points every ONNX Runtime exports. This is the C API's stable entry
/// struct — the one part whose layout cannot change without breaking every consumer — so
/// declaring it here avoids taking a direct dependency on `ort-sys` just to read a version.
/// Only the leading two members are declared because only they are read.
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

/// Load the runtime and read its version. The [`libloading::Library`] is dropped on the way
/// out: this only answers "would ort succeed?", and ort keeps its own handle. Unloading and
/// reloading the same shared object is cheap because the OS keeps it mapped for the process.
fn probe() -> Result<String, String> {
    probe_at(&dylib_path())
}

/// [`probe`] against an explicit path, so tests can exercise the failure path without
/// mutating `ORT_DYLIB_PATH` — which would race every other test in the binary.
fn probe_at(path: &std::path::Path) -> Result<String, String> {
    // SAFETY: loading a shared library runs its initialisers, which is exactly what ort will
    // do moments later. Reading `OrtGetApiBase` and calling `GetVersionString` matches the
    // documented C API; both are infallible once the symbol resolves.
    unsafe {
        // Deliberately worded to avoid the "not available"/"no file" phrasing the model
        // managers use: a runtime failure and a missing-model failure must stay tellable
        // apart, including by the tests that assert on each.
        let lib = libloading::Library::new(path).map_err(|e| {
            format!(
                "ONNX Runtime could not be loaded from `{}` ({e}). \
                 Install onnxruntime (Arch: onnxruntime-cpu), or set ORT_DYLIB_PATH.",
                path.display()
            )
        })?;
        let base_getter: libloading::Symbol<unsafe extern "C" fn() -> *const OrtApiBase> =
            lib.get(b"OrtGetApiBase").map_err(|_| {
                format!(
                    "`{}` is not an ONNX Runtime library (no OrtGetApiBase symbol).",
                    path.display()
                )
            })?;
        let base = base_getter();
        if base.is_null() {
            return Err(format!("`{}` returned a null OrtApiBase.", path.display()));
        }
        let version = CStr::from_ptr(((*base).get_version_string)())
            .to_string_lossy()
            .into_owned();
        let minor = version
            .split('.')
            .nth(1)
            .and_then(|m| m.parse::<u32>().ok())
            .unwrap_or(0);
        if minor < MIN_MINOR {
            return Err(format!(
                "ONNX Runtime {version} at `{}` is too old; 1.{MIN_MINOR} or newer is required.",
                path.display()
            ));
        }
        Ok(version)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// When a runtime *is* installed the probe must accept it and report a version at or above
    /// the floor — this is the only test that can prove the probe does not reject a good
    /// runtime. CI runners have no ONNX Runtime, so an absent one auto-skips (printing why),
    /// matching `tests/faces_engine.rs`; the failure path is covered unconditionally below.
    #[test]
    fn probe_accepts_an_installed_runtime() {
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
