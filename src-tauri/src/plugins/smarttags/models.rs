//! Smart Tagging model manager: locate (and optionally download) the CLIP vision-encoder
//! ONNX model the embedding engine needs, and expose a clean "is the model present?" state.
//!
//! Mirrors `plugins/faces/models.rs` (the reference for I7c/I7d model infrastructure), with
//! two differences that matter for H7a:
//!
//! - **Configurable path.** The model location is not fixed. The `smarttags.model_path`
//!   setting may point the engine at any CLIP-family vision-encoder ONNX file the user
//!   supplies. When the setting is blank/unset the engine falls back to the pinned default
//!   under `<app_data_dir>/models/smarttags/`, which can be fetched on first use.
//! - **Checksum only for the pinned default.** A user-supplied model has an unknown checksum,
//!   so it is verified by presence + non-empty size only; the pinned default is additionally
//!   verified against its baked-in SHA-256 (same integrity guarantee as the faces models).
//!
//! A missing or unreadable model is always a *typed error state* ([`ModelError`]), never a
//! panic — the module reports "model not available" and stays inert, in the same spirit as
//! the optional external binaries and the faces models.

use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

/// The pinned default CLIP vision-encoder model, with its source + checksum. This is the
/// model fetched on first use when `smarttags.model_path` is unset.
///
/// CLIP ViT-B/32 vision tower exported to ONNX (Xenova/clip-vit-base-patch32, Apache-2.0 —
/// a Transformers.js ONNX export). Input: a `[1, 3, 224, 224]` float `pixel_values` tensor
/// (CLIP mean/std normalized); output: a `[1, 512]` image embedding. ~350 MB.
///
/// NOTE: `sha256` is the pinned integrity check for the default download. If the default
/// source is ever re-pinned, update this hash to match the exact bytes served.
pub const CLIP_DEFAULT: ModelSpec = ModelSpec {
    key: "clip-vit-b32",
    filename: "clip_vit_b32_vision.onnx",
    url: "https://huggingface.co/Xenova/clip-vit-base-patch32/resolve/main/onnx/vision_model.onnx",
    // Verified 2026-07-22 against the exact bytes the URL serves.
    sha256: "fd6e1402a588279d1723c7534d4bcba5bc0b14b47dfab0e46f8c47b8270d7d40",
    size: 351_685_709,
};

/// A model the embedding engine can load, with an optional pinned source + checksum.
pub struct ModelSpec {
    /// Stable key used in status reports and as the on-disk filename stem.
    pub key: &'static str,
    /// On-disk filename under the models dir (for the pinned default).
    pub filename: &'static str,
    /// Pinned official download URL (for the default; a user path bypasses this).
    pub url: &'static str,
    /// Lowercase hex SHA-256 of the exact file the URL serves (checked for the default only).
    pub sha256: &'static str,
    /// Expected byte length, or `0` to skip the size gate.
    pub size: u64,
}

/// Typed failure modes for the smarttags model manager. Surfaced to the UI instead of panicking.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("models directory unavailable: {0}")]
    Dir(String),
    #[error("smart-tagging model not available: {0}")]
    Missing(String),
    #[error("smart-tagging model has wrong size (expected {expected} bytes, got {got} bytes)")]
    Size { expected: u64, got: u64 },
    #[error("smart-tagging model failed checksum (expected {expected}, got {got})")]
    Checksum { expected: String, got: String },
    #[error("download of smart-tagging model failed: {msg}")]
    Download { msg: String },
    #[error("io error on smart-tagging model: {msg}")]
    Io { msg: String },
}

/// Per-model presence, reported to the frontend so it can offer a download / point at a file.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelReport {
    pub key: String,
    /// Present on disk and loadable.
    pub present: bool,
    /// Absolute path where the model lives (or would live once downloaded).
    pub path: String,
    /// True when `path` came from the `smarttags.model_path` setting (a user-supplied model).
    pub custom: bool,
    /// When absent/unreadable, a short human-readable reason.
    pub detail: Option<String>,
}

/// The overall model-manager status for the frontend.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelStatus {
    /// True when the resolved model is present and loadable — the engine is usable.
    pub ready: bool,
    pub model: ModelReport,
}

/// Setting key: an absolute path to a user-supplied CLIP vision-encoder ONNX model. Blank/unset
/// → use the pinned default under the app data dir.
pub const MODEL_PATH_SETTING: &str = "smarttags.model_path";

/// The directory holding the pinned default model (`<app_data_dir>/models/smarttags`).
/// Created lazily.
pub fn models_dir() -> Result<PathBuf, ModelError> {
    let base = crate::commands::app_data_dir().map_err(ModelError::Dir)?;
    let dir = base.join("models").join("smarttags");
    std::fs::create_dir_all(&dir).map_err(|e| ModelError::Dir(e.to_string()))?;
    Ok(dir)
}

/// Absolute on-disk path for the pinned default model (whether or not it exists yet).
pub fn default_model_path() -> Result<PathBuf, ModelError> {
    Ok(models_dir()?.join(CLIP_DEFAULT.filename))
}

/// Resolve the effective model path: the `smarttags.model_path` override if non-blank, else the
/// pinned default under the app data dir. Returns `(path, is_custom)`.
///
/// `model_path_setting` is the raw setting value (pass `None` when unset). Kept as a pure
/// function of the setting so the command layer and tests resolve identically.
pub fn resolve_model_path(
    model_path_setting: Option<&str>,
) -> Result<(PathBuf, bool), ModelError> {
    match model_path_setting.map(str::trim).filter(|s| !s.is_empty()) {
        Some(custom) => Ok((PathBuf::from(custom), true)),
        None => Ok((default_model_path()?, false)),
    }
}

/// Lowercase hex SHA-256 of a file, streamed so a large model doesn't sit in RAM twice.
fn file_sha256(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_lower(&hasher.finalize()))
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Verify the model at `path` is present and (for the pinned default) matches its checksum,
/// returning the path. This is the *load-time* integrity check.
///
/// - A **custom** model (`is_custom = true`) is trusted once it exists and is non-empty — its
///   checksum is unknown, so we cannot pin it.
/// - The **default** model is additionally verified against [`CLIP_DEFAULT`]'s size (when a
///   non-zero size is pinned) and SHA-256.
pub fn verify(path: &Path, is_custom: bool) -> Result<PathBuf, ModelError> {
    if !path.exists() {
        return Err(ModelError::Missing(format!("no file at {}", path.display())));
    }
    let len = std::fs::metadata(path)
        .map(|m| m.len())
        .map_err(|e| ModelError::Io { msg: e.to_string() })?;
    if len == 0 {
        return Err(ModelError::Missing(format!("empty file at {}", path.display())));
    }
    if is_custom {
        // User-supplied model: presence + non-empty is all we can assert.
        return Ok(path.to_path_buf());
    }
    // Pinned default: size gate (if pinned) + full checksum.
    if CLIP_DEFAULT.size != 0 && len != CLIP_DEFAULT.size {
        return Err(ModelError::Size {
            expected: CLIP_DEFAULT.size,
            got: len,
        });
    }
    let got = file_sha256(path).map_err(|e| ModelError::Io { msg: e.to_string() })?;
    if got != CLIP_DEFAULT.sha256 {
        return Err(ModelError::Checksum {
            expected: CLIP_DEFAULT.sha256.to_string(),
            got,
        });
    }
    Ok(path.to_path_buf())
}

/// A non-failing status report for the UI. Cheap: it does NOT re-hash a custom model on every
/// poll — only presence + (for the default) size are checked here; full checksum is enforced
/// once at load/download time by [`verify`]/[`ensure`].
pub fn status(model_path_setting: Option<&str>) -> ModelStatus {
    let (path, custom) = match resolve_model_path(model_path_setting) {
        Ok(v) => v,
        Err(e) => {
            return ModelStatus {
                ready: false,
                model: ModelReport {
                    key: CLIP_DEFAULT.key.to_string(),
                    present: false,
                    path: String::new(),
                    custom: false,
                    detail: Some(e.to_string()),
                },
            };
        }
    };
    let path_str = path.to_string_lossy().into_owned();
    let (present, detail) = match presence_check(&path, custom) {
        Ok(()) => (true, None),
        Err(e) => (false, Some(e.to_string())),
    };
    ModelStatus {
        ready: present,
        model: ModelReport {
            key: CLIP_DEFAULT.key.to_string(),
            present,
            path: path_str,
            custom,
            detail,
        },
    }
}

/// Cheap presence check for [`status`]: exists, non-empty, and (default only, when pinned) the
/// right size. Never hashes — that would hitch the UI on every poll.
fn presence_check(path: &Path, is_custom: bool) -> Result<(), ModelError> {
    if !path.exists() {
        return Err(ModelError::Missing(format!("no file at {}", path.display())));
    }
    let len = std::fs::metadata(path)
        .map(|m| m.len())
        .map_err(|e| ModelError::Io { msg: e.to_string() })?;
    if len == 0 {
        return Err(ModelError::Missing(format!("empty file at {}", path.display())));
    }
    if !is_custom && CLIP_DEFAULT.size != 0 && len != CLIP_DEFAULT.size {
        return Err(ModelError::Size {
            expected: CLIP_DEFAULT.size,
            got: len,
        });
    }
    Ok(())
}

/// Ensure the effective model is present and verified, downloading the pinned default once if
/// it is missing (a **custom** model is never downloaded — a missing custom path is an error the
/// user must fix). Returns the verified path.
///
/// `progress` is called approximately every 1 MiB (or 1% of total when total is known) with
/// `(bytes_done, total_bytes)`. Pass `None` when the caller does not need progress reporting.
/// The callback is `Send + Sync` so it can be an `AppHandle`-based emitter; `models.rs` itself
/// stays free of Tauri types.
pub async fn ensure(
    model_path_setting: Option<&str>,
    progress: Option<&(dyn Fn(u64, Option<u64>) + Send + Sync)>,
) -> Result<PathBuf, ModelError> {
    let (path, custom) = resolve_model_path(model_path_setting)?;
    // Fast path: already present and correct.
    if let Ok(p) = verify(&path, custom) {
        return Ok(p);
    }
    if custom {
        // We do not fetch a user-supplied model — the user pointed us at a path.
        return Err(ModelError::Missing(format!(
            "configured model path {} is missing or unreadable",
            path.display()
        )));
    }
    download_to(&CLIP_DEFAULT, &path, progress).await?;
    verify(&path, false)
}

/// Download the pinned default model to `dest` (temp file + atomic rename so a crash mid-download
/// never leaves a truncated file that would later fail verification).
///
/// `progress` receives `(bytes_done, total_bytes_opt)` throttled to roughly every 1 MiB or 1%
/// of the total (whichever is smaller) so the UI doesn't receive thousands of events. Pass
/// `None` for no reporting.
async fn download_to(
    spec: &ModelSpec,
    dest: &Path,
    progress: Option<&(dyn Fn(u64, Option<u64>) + Send + Sync)>,
) -> Result<(), ModelError> {
    let resp = reqwest::Client::builder()
        .build()
        .map_err(|e| ModelError::Download { msg: e.to_string() })?
        .get(spec.url)
        .header(reqwest::header::USER_AGENT, "chairphoto-smarttags/1.0")
        .send()
        .await
        .map_err(|e| ModelError::Download { msg: e.to_string() })?;
    if !resp.status().is_success() {
        return Err(ModelError::Download {
            msg: format!("HTTP {}", resp.status()),
        });
    }
    // Read Content-Length for percentage-based throttling (may be absent).
    let total: Option<u64> = resp.content_length();
    // Throttle: fire at most every 1 MiB, or every 1% of total if smaller.
    let throttle_bytes: u64 = match total {
        Some(t) => (t / 100).max(1).min(1 << 20), // 1% but ≥1 B and ≤1 MiB
        None => 1 << 20,                            // 1 MiB when total is unknown
    };
    let tmp = dest.with_extension("part");
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp).map_err(|e| ModelError::Io { msg: e.to_string() })?;
        let mut resp = resp;
        let mut bytes_done: u64 = 0;
        let mut last_reported: u64 = 0;
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| ModelError::Download { msg: e.to_string() })?
        {
            file.write_all(&chunk)
                .map_err(|e| ModelError::Io { msg: e.to_string() })?;
            bytes_done += chunk.len() as u64;
            if let Some(cb) = progress {
                if bytes_done.saturating_sub(last_reported) >= throttle_bytes {
                    cb(bytes_done, total);
                    last_reported = bytes_done;
                }
            }
        }
        file.flush().map_err(|e| ModelError::Io { msg: e.to_string() })?;
        // Final progress tick so the UI reaches 100% cleanly.
        if let Some(cb) = progress {
            cb(bytes_done, total);
        }
    }
    std::fs::rename(&tmp, dest).map_err(|e| ModelError::Io { msg: e.to_string() })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_prefers_custom_path() {
        let (p, custom) = resolve_model_path(Some("/tmp/my-clip.onnx")).unwrap();
        assert!(custom);
        assert_eq!(p, PathBuf::from("/tmp/my-clip.onnx"));
    }

    #[test]
    fn resolve_blank_falls_back_to_default() {
        // Blank / whitespace-only is treated as unset (uses the default under app data dir).
        let (_, custom) = resolve_model_path(Some("   ")).unwrap();
        assert!(!custom, "blank override must fall back to the default (non-custom)");
        let (_, custom2) = resolve_model_path(None).unwrap();
        assert!(!custom2);
    }

    #[test]
    fn verify_missing_is_typed_error_not_panic() {
        let missing = PathBuf::from("/nonexistent/definitely/not/here.onnx");
        let err = verify(&missing, true).unwrap_err();
        assert!(matches!(err, ModelError::Missing(_)), "got {err:?}");
    }

    #[test]
    fn verify_empty_custom_is_missing() {
        let dir = std::env::temp_dir().join(format!("st-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("empty.onnx");
        std::fs::write(&f, b"").unwrap();
        let err = verify(&f, true).unwrap_err();
        assert!(matches!(err, ModelError::Missing(_)), "got {err:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn custom_nonempty_verifies_without_checksum() {
        // A user-supplied model is trusted once present + non-empty (its hash is unknown).
        let dir = std::env::temp_dir().join(format!("st-custom-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("model.onnx");
        std::fs::write(&f, b"not really onnx but non-empty").unwrap();
        assert!(verify(&f, true).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
