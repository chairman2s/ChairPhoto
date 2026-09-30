//! The Develop surface's source commands (docs/plans/raw-foundation): what the decoder
//! makes of a photo's file. Slice 1 is the probe alone; the working image, its job family
//! and the decode cache follow in later slices.

use super::AppState;
use tauri::{AppHandle, Manager};

/// What the Develop badge shows for a photo. Serialized with a `source` tag so the frontend
/// switches on one field. Which variants a build constructs depends on the `raw` feature
/// (`NoDecoder` only without it, `Raw`/`Unsupported` only with it) — the enum is the
/// contract, so the per-configuration dead-variant lint is silenced rather than split.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(tag = "source", rename_all = "lowercase")]
pub enum DevelopSource {
    /// The camera's embedded preview: while a RAW is being prepared (`preparing`), or when
    /// the RAW engine is switched off.
    Preview { preparing: bool },
    /// A RAW the vendored decoder identifies. `bits` is the working depth this engine will
    /// use (the decode is 16-bit linear); `megapixels` from the decoder's own dimensions.
    /// `token` is set once the working image is resident — it goes into every render URL.
    /// `camera_ev` rides with the token: the offset that matched the camera's JPEG of this
    /// frame, which the Darkroom stamps on new engine-2 records.
    Raw {
        camera: String,
        megapixels: f32,
        bits: u8,
        decoder: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        token: Option<String>,
        #[serde(rename = "cameraEv", skip_serializing_if = "Option::is_none")]
        camera_ev: Option<f32>,
        /// The as-shot light as Kelvin and tint (`linear::as_shot_kelvin`), when the
        /// camera gives what Kelvin white balance needs — the Kelvin slider's home.
        #[serde(rename = "asShotWb", skip_serializing_if = "Option::is_none")]
        as_shot_wb: Option<[f32; 2]>,
        /// Which lens corrections the camera wrote into this file, once the working image
        /// is resident (docs/plans/lens-corrections) — what the Darkroom's Lens switch offers.
        #[serde(skip_serializing_if = "Option::is_none")]
        lens: Option<LensInfo>,
    },
    /// A RAW the decoder does not support (yet): the Darkroom keeps working on the camera
    /// preview and says so.
    Unsupported { camera: Option<String>, reason: String },
    /// Not a RAW: the file's own pixels are its full quality.
    Jpeg,
    /// The `raw` feature is compiled out of this build.
    NoDecoder,
}

/// Identify a photo's file with the decoder (no pixels are read). Path resolution is a
/// brief catalog lock; the probe itself runs on a blocking worker.
#[tauri::command]
pub async fn raw_probe(app: AppHandle, photo_id: i64) -> Result<DevelopSource, String> {
    let state = app.state::<AppState>();
    let candidates = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_ref().ok_or("No catalog is open")?;
        catalog.photo_path_candidates(photo_id).map_err(|e| e.to_string())?
    };
    let health = state.volume_health.clone();
    crate::app::spawn_blocking(move || {
        let path = crate::volume_health::pick_existing(
            &candidates,
            &health,
            crate::catalog::ResolveMode::OriginalRequired,
        )
        .ok_or_else(|| format!("no reachable copy of photo {photo_id}"))?;
        if !crate::scanner::is_raw(&path) {
            return Ok(DevelopSource::Jpeg);
        }
        Ok(probe_source(&path))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The camera's lens tables for one photo, as the Darkroom shows them.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct LensInfo {
    /// Where the tables came from, e.g. "Sony built-in".
    pub source: String,
    pub vignetting: bool,
    pub distortion: bool,
    pub chromatic: bool,
}

#[cfg(feature = "raw")]
fn probe_source(path: &std::path::Path) -> DevelopSource {
    use crate::raw::{probe, RawSupport};
    match probe(path) {
        RawSupport::Supported(id) => DevelopSource::Raw {
            camera: format!("{} {}", id.make, id.model).trim().to_string(),
            megapixels: (id.width as f32 * id.height as f32) / 1_000_000.0,
            bits: 16,
            decoder: crate::raw::decoder_version().to_string(),
            token: None,
            camera_ev: None,
            as_shot_wb: None,
            lens: None,
        },
        RawSupport::Unsupported { camera, reason } => DevelopSource::Unsupported { camera, reason },
    }
}

#[cfg(not(feature = "raw"))]
fn probe_source(_path: &std::path::Path) -> DevelopSource {
    DevelopSource::NoDecoder
}

/// Resolve a photo's original path off the catalog lock (the same brief-lock-then-stat
/// shape every render command uses).
fn resolve_original(app: &AppHandle, photo_id: i64) -> Result<std::path::PathBuf, String> {
    let state = app.state::<AppState>();
    let candidates = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_ref().ok_or("No catalog is open")?;
        catalog.photo_path_candidates(photo_id).map_err(|e| e.to_string())?
    };
    crate::volume_health::pick_existing(
        &candidates,
        &state.volume_health,
        crate::catalog::ResolveMode::OriginalRequired,
    )
    .ok_or_else(|| format!("no reachable copy of photo {photo_id}"))
}

/// The Darkroom opened `photo_id`: claim the develop session and start preparing its
/// working image, then — at lower priority, silently — its `neighbours` (N+1 first).
/// Returns the state right now; changes arrive as `develop:source`. A neighbour that is not
/// a RAW or whose original is unreachable is simply not preloaded.
#[tauri::command]
pub async fn develop_open(app: AppHandle, photo_id: i64, neighbours: Vec<i64>) -> Result<DevelopSource, String> {
    crate::app::spawn_blocking(move || {
        let path = resolve_original(&app, photo_id)?;
        if !crate::scanner::is_raw(&path) {
            // Nothing to prepare for a JPEG; `session::open` is not reached, so a previous
            // photo's image is released by the next open or by Develop's close.
            return Ok(DevelopSource::Jpeg);
        }
        let probe = probe_source(&path);
        #[cfg(all(feature = "raw", feature = "edit"))]
        {
            let neighbours: Vec<(i64, std::path::PathBuf)> = neighbours
                .into_iter()
                .filter(|&n| n != photo_id)
                .filter_map(|n| resolve_original(&app, n).ok().map(|p| (n, p)))
                .filter(|(_, p)| crate::scanner::is_raw(p))
                .collect();
            return crate::develop::session::open(&app, photo_id, path, probe, neighbours);
        }
        #[cfg(not(all(feature = "raw", feature = "edit")))]
        {
            let _ = (path, neighbours);
            Ok(probe)
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The Darkroom closed: release the working images. Idempotent.
#[tauri::command]
pub async fn develop_close(app: AppHandle) -> Result<(), String> {
    #[cfg(all(feature = "raw", feature = "edit"))]
    {
        let state = app.state::<AppState>();
        return crate::develop::session::close(&state);
    }
    #[cfg(not(all(feature = "raw", feature = "edit")))]
    {
        let _ = app;
        Ok(())
    }
}

/// Bytes the `.rawf` decode cache holds right now (Preferences → Darkroom).
#[tauri::command]
pub async fn develop_cache_usage() -> Result<u64, String> {
    #[cfg(all(feature = "raw", feature = "edit"))]
    {
        return crate::app::spawn_blocking(crate::develop::cache::usage_bytes)
            .await
            .map_err(|e| e.to_string());
    }
    #[cfg(not(all(feature = "raw", feature = "edit")))]
    Ok(0)
}

/// Empty the `.rawf` decode cache. Returns the bytes freed. Photos open in Develop stay
/// open — their working images are in memory; only the next first open pays a decode.
#[tauri::command]
pub async fn develop_cache_clear() -> Result<u64, String> {
    #[cfg(all(feature = "raw", feature = "edit"))]
    {
        return crate::app::spawn_blocking(|| crate::develop::cache::trim_to(0))
            .await
            .map_err(|e| e.to_string());
    }
    #[cfg(not(all(feature = "raw", feature = "edit")))]
    Ok(0)
}

/// The develop source state right now (a remounted view re-attaching).
#[tauri::command]
pub async fn develop_source(app: AppHandle, photo_id: i64) -> Result<DevelopSource, String> {
    crate::app::spawn_blocking(move || {
        let path = resolve_original(&app, photo_id)?;
        if !crate::scanner::is_raw(&path) {
            return Ok(DevelopSource::Jpeg);
        }
        let probe = probe_source(&path);
        #[cfg(all(feature = "raw", feature = "edit"))]
        {
            let state = app.state::<AppState>();
            return Ok(crate::develop::session::current(&state, photo_id, probe));
        }
        #[cfg(not(all(feature = "raw", feature = "edit")))]
        {
            Ok(probe)
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_raw_path_is_jpeg_and_the_source_tag_serializes() {
        let json = serde_json::to_string(&DevelopSource::Jpeg).unwrap();
        assert_eq!(json, r#"{"source":"jpeg"}"#);
        let json = serde_json::to_string(&DevelopSource::Unsupported {
            camera: Some("ILCE-7RM6".into()),
            reason: "Unsupported file format or not RAW file".into(),
        })
        .unwrap();
        assert!(json.starts_with(r#"{"source":"unsupported","camera":"ILCE-7RM6""#), "{json}");
    }

    /// Runs only with a real RAW at `CHAIRPHOTO_RAW_FIXTURE`: prints the exact payload the
    /// Darkroom badge receives, and pins its shape.
    #[cfg(feature = "raw")]
    #[test]
    fn a_real_fixture_probes_as_raw_with_its_picture_size() {
        let Ok(fixture) = std::env::var("CHAIRPHOTO_RAW_FIXTURE") else {
            println!("SKIPPED: a_real_fixture_probes_as_raw_with_its_picture_size — set CHAIRPHOTO_RAW_FIXTURE");
            return;
        };
        let src = probe_source(std::path::Path::new(&fixture));
        println!("develop source: {}", serde_json::to_string(&src).unwrap());
        assert!(matches!(src, DevelopSource::Raw { bits: 16, .. }), "{src:?}");
    }

    #[cfg(not(feature = "raw"))]
    #[test]
    fn without_the_raw_feature_a_raw_probes_as_nodecoder() {
        assert_eq!(probe_source(std::path::Path::new("x.ARW")), DevelopSource::NoDecoder);
    }
}
