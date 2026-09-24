//! "What you see is what you export" (docs/plans/raw-foundation/01-product.md, the success
//! metric). An engine-2 export is the working image through the same pipeline at full
//! size, so at 100 % it equals the view by construction (locked by
//! `export_at_full_size_is_the_view_at_full_size`). At Fit the view downsamples the
//! linear image before the look while the export is scaled after it, so there the two
//! may differ by resampling alone; every engine-2 export is checked for that in-app — the
//! view's render at [`PARITY_EDGE`] against the export scaled to the same size — and
//! tallied, so a count of differing exports can be read back.

use super::{render_proxy, RenderOpts, RenderSource, SourceToken, WorkingImage};
use image::DynamicImage;
use std::sync::{Arc, Mutex};

/// The long edge both sides are compared at.
pub const PARITY_EDGE: u32 = 512;

/// Largest mean |Δ| (levels of 255) that resampling order alone explains; above it an
/// export counts as differing from the view. Measured on the corpus
/// (`develop::tests::fixture_export_matches_the_view_at_fit`, 2026-09-24): the seven ARWs
/// 0.07–1.32 without grain and 1.79–2.67 with it (grain is drawn per output pixel, so it
/// never averages alike); the detailed DNG 3.60–5.28 — how much the order matters grows
/// with fine detail, and averaging the export in linear light instead only moved it
/// (DNG 1.89–3.17, ARWs worse). So the bar is set above every resampling-only case. What it
/// still catches is the failure the metric exists for — an export from another source or
/// pipeline: the old tone-matched path, a lost camera match or transform, all 8–60 levels.
/// Exactness itself is locked at 100 % by `export_at_full_size_is_the_view_at_full_size`.
pub const PARITY_TOLERANCE: f32 = 6.0;

/// Exports checked and exports that differed, since the last [`take`].
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParityTally {
    pub checked: u64,
    pub differing: u64,
}

static TALLY: Mutex<ParityTally> = Mutex::new(ParityTally { checked: 0, differing: 0 });

/// Mean |Δ| between the view's render of `edit_json` at [`PARITY_EDGE`] and `export`
/// scaled to that size.
pub fn fit_difference(
    token: &SourceToken,
    image: &Arc<WorkingImage>,
    edit_json: &str,
    export: &DynamicImage,
) -> Result<f32, String> {
    let view = render_proxy(
        RenderSource::Working { token: token.clone(), image: image.clone() },
        edit_json,
        PARITY_EDGE,
        RenderOpts::default(),
    )?
    .to_rgb8();
    let scaled = image::imageops::thumbnail(&export.to_rgb8(), view.width(), view.height());
    if scaled.dimensions() != view.dimensions() {
        return Err(format!("export {:?} does not scale to the view {:?}", scaled.dimensions(), view.dimensions()));
    }
    let sum: u64 = view.as_raw().iter().zip(scaled.as_raw()).map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as u64).sum();
    Ok(sum as f32 / view.as_raw().len() as f32)
}

/// Tally one checked export.
pub fn record(difference: f32) {
    let mut t = TALLY.lock().unwrap_or_else(|e| e.into_inner());
    t.checked += 1;
    if difference > PARITY_TOLERANCE {
        t.differing += 1;
        eprintln!("export: differs from the view at Fit by {difference:.2} levels (tolerance {PARITY_TOLERANCE})");
    }
}

/// The tally since the last take, reset to zero.
pub fn take() -> ParityTally {
    std::mem::take(&mut *TALLY.lock().unwrap_or_else(|e| e.into_inner()))
}

impl ParityTally {
    /// This tally added to `other` (a persisted total).
    pub fn plus(self, other: ParityTally) -> ParityTally {
        ParityTally { checked: self.checked + other.checked, differing: self.differing + other.differing }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb32FImage;

    fn working() -> Arc<WorkingImage> {
        let linear = Rgb32FImage::from_fn(96, 64, |x, y| {
            let v = 0.02 + (x as f32 / 95.0) * 0.5 * (1.0 + ((x / 3 + y / 3) % 2) as f32 * 0.4);
            image::Rgb([v, v * 0.9, v * 0.7])
        });
        Arc::new(WorkingImage {
            width: 96,
            height: 64,
            linear,
            cam_mul: [1.0; 4],
            rgb_cam: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            decoder: "test",
            camera_ev: None,
        })
    }

    #[test]
    fn the_same_render_passes_and_another_pipeline_is_caught() {
        let image = working();
        let token = SourceToken::Working { photo_id: 1, generation: 42 };
        let json = r#"{"engine":2,"display":"camera.2"}"#;
        let full = |j: &str| {
            super::super::render_image_opts(RenderSource::Working { token: token.clone(), image: image.clone() }, j, 0, RenderOpts::default()).unwrap()
        };
        let same = fit_difference(&token, &image, json, &full(json)).unwrap();
        assert!(same <= PARITY_TOLERANCE, "the export itself: {same}");
        // An export that lost its camera match (a stop) is the kind of mismatch to catch.
        let other = fit_difference(&token, &image, json, &full(r#"{"engine":2,"display":"camera.2","cameraEv":1.0}"#)).unwrap();
        assert!(other > PARITY_TOLERANCE, "a different pipeline: {other}");
    }

    #[test]
    fn the_tally_counts_and_resets() {
        let _ = take();
        record(0.5);
        record(PARITY_TOLERANCE + 1.0);
        assert_eq!(take(), ParityTally { checked: 2, differing: 1 });
        assert_eq!(take(), ParityTally::default());
        assert_eq!(ParityTally { checked: 2, differing: 1 }.plus(ParityTally { checked: 5, differing: 0 }), ParityTally { checked: 7, differing: 1 });
    }
}
