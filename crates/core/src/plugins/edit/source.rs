//! What a render reads from (docs/plans/raw-foundation): the camera's preview JPEG, an
//! already-decoded 8-bit image (export, tests), or the RAW **working image** — the
//! full-resolution linear decode the Develop session holds. The engine dispatches on the
//! record's engine id and refuses a mismatch instead of substituting: an engine-2 record
//! never renders from a JPEG, an engine-1 record never from the working image.

use image::{DynamicImage, Rgb32FImage};
use std::sync::Arc;

/// Names the pixels a URL renders from. `Preview` is the camera JPEG (the path that always
/// existed); `Working` is the RAW working image, valid only while its generation is the
/// develop claim's current one — a stale token renders nothing rather than something else.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum SourceToken {
    Preview,
    Working { photo_id: i64, generation: u64 },
}

impl SourceToken {
    /// The URL form: `p`, or `w:<photo>:<generation>`.
    pub fn parse(s: &str) -> Option<Self> {
        if s.is_empty() || s == "p" {
            return Some(SourceToken::Preview);
        }
        let mut parts = s.split(':');
        if parts.next()? != "w" {
            return None;
        }
        let photo_id = parts.next()?.parse().ok()?;
        let generation = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(SourceToken::Working { photo_id, generation })
    }

    pub fn to_query(&self) -> String {
        match self {
            SourceToken::Preview => "p".to_string(),
            SourceToken::Working { photo_id, generation } => format!("w:{photo_id}:{generation}"),
        }
    }
}

/// The RAW working image: scene-linear RGB (0.0 = black, 1.0 = sensor white; values are
/// clipped there — docs/plans/raw-foundation decision 3), Rec.709/sRGB primaries, as-shot
/// white balance applied, the camera's visible rectangle, display orientation. The
/// as-shot multipliers and the camera matrix ride along for the Kelvin slice.
pub struct WorkingImage {
    pub width: u32,
    pub height: u32,
    pub linear: Rgb32FImage,
    pub cam_mul: [f32; 4],
    /// The daylight multipliers `rgb_cam` is normalized to (`raw::LinearDecode::pre_mul`).
    pub pre_mul: [f32; 4],
    pub rgb_cam: [[f32; 3]; 3],
    /// The camera's white-balance table (`raw::LinearDecode::wbct`).
    pub wbct: Vec<[f32; 4]>,
    pub decoder: &'static str,
    /// The exposure offset that matched the camera's own JPEG of this frame
    /// (`linear::camera_match_ev`), measured when the image was prepared. New engine-2
    /// records carry it as `cameraEv`; the image never applies it by itself.
    pub camera_ev: Option<f32>,
    /// The camera's lens-correction tables (`raw::LinearDecode::lens`). Radial about this
    /// image's centre, so orientation does not move them.
    #[cfg(feature = "raw")]
    pub lens: Option<crate::lens::LensCorrection>,
}

impl WorkingImage {
    /// A field-for-field copy, for tests that vary one field of a shared image.
    #[cfg(test)]
    pub fn clone_for_test(&self) -> WorkingImage {
        WorkingImage {
            width: self.width,
            height: self.height,
            linear: self.linear.clone(),
            cam_mul: self.cam_mul,
            pre_mul: self.pre_mul,
            rgb_cam: self.rgb_cam,
            wbct: self.wbct.clone(),
            decoder: self.decoder,
            camera_ev: self.camera_ev,
            #[cfg(feature = "raw")]
            lens: self.lens.clone(),
        }
    }

    pub fn bytes(&self) -> usize {
        self.linear.as_raw().len() * std::mem::size_of::<f32>()
    }
}

/// The pixels a render starts from.
pub enum RenderSource<'a> {
    /// The camera's preview JPEG bytes (decoded through the one-slot cache).
    PreviewJpeg(&'a [u8]),
    /// An already-decoded image — export's full-res source, tests. Never cached.
    Decoded(DynamicImage),
    /// The resident working image, named by its token.
    Working { token: SourceToken, image: Arc<WorkingImage> },
}

impl RenderSource<'_> {
    pub fn is_working(&self) -> bool {
        matches!(self, RenderSource::Working { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_token_parses_and_prints_roundtrip() {
        for t in [SourceToken::Preview, SourceToken::Working { photo_id: 5, generation: 3 }] {
            assert_eq!(SourceToken::parse(&t.to_query()), Some(t.clone()));
        }
        assert_eq!(SourceToken::parse(""), Some(SourceToken::Preview));
        assert_eq!(SourceToken::parse("w:5:3").unwrap().to_query(), "w:5:3");
    }

    #[test]
    fn source_token_rejects_garbage() {
        for bad in ["x", "w:", "w:5", "w:a:3", "w:5:3:1", "p:1"] {
            assert!(SourceToken::parse(bad).is_none(), "{bad:?} must not parse");
        }
    }
}
