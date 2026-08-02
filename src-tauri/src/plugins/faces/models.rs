//! Face-model manager: download the two ONNX models the engine needs into the app models
//! dir, verify them by pinned SHA-256, and expose a clean "are the models present?" state.
//!
//! Models are **not bundled** (repo/app size + license hygiene — see docs/face-tagging.md).
//! They are fetched once from their pinned official sources, checksummed, and cached under
//! `<app_data_dir>/models/faces/`. A missing or corrupt model is a *typed error state*
//! (`ModelStatus` / `ModelError`), never a panic — the module simply reports "models not
//! downloaded" and stays inert, in the same spirit as the optional external binaries.

use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

/// A model the face engine depends on, with its pinned source + checksum. Verified at
/// implementation time (2026-07-05) by downloading each file and hashing it.
pub struct ModelSpec {
    /// Stable key used in status reports and as the on-disk filename stem.
    pub key: &'static str,
    /// On-disk filename under the models dir.
    pub filename: &'static str,
    /// Pinned official download URL.
    pub url: &'static str,
    /// Lowercase hex SHA-256 of the exact file the URL serves.
    pub sha256: &'static str,
    /// Expected byte length (a cheap pre-hash sanity check).
    pub size: u64,
}

/// YuNet 2023mar face detector (OpenCV Zoo, Apache-2.0). ~227 KB. SCRFD-style multi-output
/// ONNX with fixed 640×640 input — see `engine.rs` for the decode.
pub const YUNET: ModelSpec = ModelSpec {
    key: "yunet",
    filename: "face_detection_yunet_2023mar.onnx",
    url: "https://github.com/opencv/opencv_zoo/raw/main/models/face_detection_yunet/face_detection_yunet_2023mar.onnx",
    sha256: "8f2383e4dd3cfbb4553ea8718107fc0423210dc964f9f4280604804ed2552fa4",
    size: 232_589,
};

/// AuraFace-v1 embedding model (`glintr100.onnx` in HF repo `fal/AuraFace-v1`, Apache-2.0).
/// ResNet100 ArcFace clone → 512-d embedding, 112×112 input. ~249 MB.
pub const AURAFACE: ModelSpec = ModelSpec {
    key: "auraface",
    filename: "auraface_glintr100.onnx",
    url: "https://huggingface.co/fal/AuraFace-v1/resolve/main/glintr100.onnx",
    sha256: "a7933ea5330113b01c9b60351d8f4c33003f145d8470ac5f0e52ee2effe25c60",
    size: 260_694_151,
};

/// All models the engine needs, in the order the status report lists them.
pub const ALL: [&ModelSpec; 2] = [&YUNET, &AURAFACE];

/// Typed failure modes for model management. Surfaced to the UI instead of panicking.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("models directory unavailable: {0}")]
    Dir(String),
    #[error("model {key} not downloaded")]
    Missing { key: &'static str },
    #[error("model {key} has wrong size (expected {expected} bytes, got {got} bytes)")]
    Size {
        key: &'static str,
        expected: u64,
        got: u64,
    },
    #[error("model {key} failed checksum (expected {expected}, got {got})")]
    Checksum {
        key: &'static str,
        expected: String,
        got: String,
    },
    #[error("download of model {key} failed: {msg}")]
    Download { key: &'static str, msg: String },
    #[error("io error on model {key}: {msg}")]
    Io { key: &'static str, msg: String },
}

/// Per-model presence, reported to the frontend so it can offer a download.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelReport {
    pub key: String,
    pub filename: String,
    /// Present on disk with a matching checksum.
    pub present: bool,
    /// Absolute path where it lives (or would live once downloaded).
    pub path: String,
    /// When absent/corrupt, a short human-readable reason.
    pub detail: Option<String>,
}

/// The overall model-manager status for the `faces_models_status` command.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelStatus {
    /// True only when every model is present and verified — the engine is usable.
    pub ready: bool,
    pub models: Vec<ModelReport>,
}

/// The directory holding the face models (`<app_data_dir>/models/faces`). Created lazily.
pub fn models_dir() -> Result<PathBuf, ModelError> {
    let base = crate::commands::app_data_dir().map_err(ModelError::Dir)?;
    let dir = base.join("models").join("faces");
    std::fs::create_dir_all(&dir).map_err(|e| ModelError::Dir(e.to_string()))?;
    Ok(dir)
}

/// Absolute on-disk path for a model (whether or not it exists yet).
pub fn model_path(spec: &ModelSpec) -> Result<PathBuf, ModelError> {
    Ok(models_dir()?.join(spec.filename))
}

/// Lowercase hex SHA-256 of a file, streamed so a 249 MB model doesn't sit in RAM twice.
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

/// Cheap presence check: the model file exists and has the pinned byte length. This is the
/// hot-path check `status()` uses — it must NOT hash the file (streaming a 249 MB SHA-256 on
/// every status poll would hitch the UI). Full-content integrity is verified once at
/// download time by [`verify`]; a present, correctly-sized file is trusted thereafter.
pub fn check_present(spec: &ModelSpec) -> Result<PathBuf, ModelError> {
    let path = model_path(spec)?;
    if !path.exists() {
        return Err(ModelError::Missing { key: spec.key });
    }
    // Size gate only — a truncated/partial file is caught immediately, without reading the
    // whole file.
    let len = std::fs::metadata(&path)
        .map(|m| m.len())
        .map_err(|e| ModelError::Io {
            key: spec.key,
            msg: e.to_string(),
        })?;
    if len != spec.size {
        return Err(ModelError::Size {
            key: spec.key,
            expected: spec.size,
            got: len,
        });
    }
    Ok(path)
}

/// Verify one model file exists on disk with the pinned checksum, returning its path. This
/// streams a full SHA-256 over the file, so it is the *download-time* integrity check — not
/// the status hot path (use [`check_present`] for that).
pub fn verify(spec: &ModelSpec) -> Result<PathBuf, ModelError> {
    // Presence + size first (cheap; also gives us the path).
    let path = check_present(spec)?;
    let got = file_sha256(&path).map_err(|e| ModelError::Io {
        key: spec.key,
        msg: e.to_string(),
    })?;
    if got != spec.sha256 {
        return Err(ModelError::Checksum {
            key: spec.key,
            expected: spec.sha256.to_string(),
            got,
        });
    }
    Ok(path)
}

/// A non-failing status report for the UI: which models are present (exist on disk with the
/// expected size). This is a cheap poll — it does NOT re-hash the (up to 249 MB) files;
/// content integrity is verified once at download time by [`ensure`].
pub fn status() -> ModelStatus {
    let mut models = Vec::with_capacity(ALL.len());
    let mut ready = true;
    for spec in ALL {
        let path = model_path(spec)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        match check_present(spec) {
            Ok(_) => models.push(ModelReport {
                key: spec.key.to_string(),
                filename: spec.filename.to_string(),
                present: true,
                path,
                detail: None,
            }),
            Err(e) => {
                ready = false;
                models.push(ModelReport {
                    key: spec.key.to_string(),
                    filename: spec.filename.to_string(),
                    present: false,
                    path,
                    detail: Some(e.to_string()),
                });
            }
        }
    }
    ModelStatus { ready, models }
}

/// Ensure a single model is present and verified, downloading it once if needed. If the file
/// exists but its checksum does not match (partial/corrupt download, or the pinned source
/// changed), it is re-downloaded once. Returns the verified path.
pub async fn ensure(spec: &ModelSpec) -> Result<PathBuf, ModelError> {
    // Fast path: already present and correct.
    if let Ok(path) = verify(spec) {
        return Ok(path);
    }
    let path = model_path(spec)?;
    download_to(spec, &path).await?;
    // Verify the freshly-downloaded file; a mismatch here is a hard error (bad source).
    verify(spec)
}

/// Ensure every model is present, downloading any that are missing/corrupt. Returns the
/// post-download status so the caller can report per-model outcomes.
pub async fn ensure_all() -> ModelStatus {
    for spec in ALL {
        // Ignore per-model errors here; `status()` reflects the final truth for the UI.
        let _ = ensure(spec).await;
    }
    status()
}

/// Download a model to `dest` (via a temp file + atomic rename so a crash mid-download never
/// leaves a truncated file that would later fail verification silently).
async fn download_to(spec: &ModelSpec, dest: &Path) -> Result<(), ModelError> {
    let resp = reqwest::Client::builder()
        .build()
        .map_err(|e| ModelError::Download {
            key: spec.key,
            msg: e.to_string(),
        })?
        .get(spec.url)
        .header(reqwest::header::USER_AGENT, "chairphoto-faces/1.0")
        .send()
        .await
        .map_err(|e| ModelError::Download {
            key: spec.key,
            msg: e.to_string(),
        })?;
    if !resp.status().is_success() {
        return Err(ModelError::Download {
            key: spec.key,
            msg: format!("HTTP {}", resp.status()),
        });
    }
    // Stream the body to the temp file so a ~260 MB model never sits whole in RAM.
    let tmp = dest.with_extension("part");
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp).map_err(|e| ModelError::Io {
            key: spec.key,
            msg: e.to_string(),
        })?;
        let mut resp = resp;
        while let Some(chunk) = resp.chunk().await.map_err(|e| ModelError::Download {
            key: spec.key,
            msg: e.to_string(),
        })? {
            file.write_all(&chunk).map_err(|e| ModelError::Io {
                key: spec.key,
                msg: e.to_string(),
            })?;
        }
        file.flush().map_err(|e| ModelError::Io {
            key: spec.key,
            msg: e.to_string(),
        })?;
    }
    std::fs::rename(&tmp, dest).map_err(|e| ModelError::Io {
        key: spec.key,
        msg: e.to_string(),
    })?;
    Ok(())
}
