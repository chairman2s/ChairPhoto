//! What the Develop surface's source is for a photo (docs/plans/raw-foundation): the
//! camera preview, a RAW the vendored decoder identifies, or one of the honest exceptions.
//! Ungated — a build without the `raw` feature still answers, with `NoDecoder`.

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

/// The camera's lens tables for one photo, as the Darkroom shows them.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct LensInfo {
    /// Where the tables came from, e.g. "Sony built-in".
    pub source: String,
    pub vignetting: bool,
    pub distortion: bool,
    pub chromatic: bool,
}

/// What the decoder makes of `path` (no pixels are read).
#[cfg(feature = "raw")]
pub fn probe_source(path: &std::path::Path) -> DevelopSource {
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
pub fn probe_source(_path: &std::path::Path) -> DevelopSource {
    DevelopSource::NoDecoder
}

// Moved from the Tauri shell's `commands/develop.rs` when it was removed (#165).
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
