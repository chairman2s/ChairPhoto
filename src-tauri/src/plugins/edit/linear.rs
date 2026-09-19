//! Engine 2's scene-linear stages (docs/plans/raw-foundation): exposure and white balance
//! as multiplications of light, then one display transform that fits the result onto the
//! screen. Everything after that — zones, region sliders, contrast, saturation, and the
//! whole finish (B&W, LUT, split, fade, vignette, grain) — runs on the display-encoded
//! image exactly as engine 1 does, so presets and looks mean the same thing on both.
//!
//! Pure: no I/O, no globals. Every function is exercised by the tests below.

use image::{GrayImage, Rgb32FImage, RgbImage};
use rayon::prelude::*;

/// Engine 2's white balance on the record — a tagged meaning, so a saved version renders
/// the same forever (decision 1). `relative` is warmer/cooler than as-shot, a *look*;
/// `kelvin` is a stated scene light, parsed now and rendered in the Kelvin slice.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WbSpec {
    Relative { temp: f32, tint: f32 },
    Kelvin { kelvin: f32, tint: f32 },
}

impl WbSpec {
    /// From the record's `tone.wb`: `mode: "kelvin"` selects the Kelvin meaning; anything
    /// else (including every existing record) is relative.
    pub fn from_record(mode: Option<&str>, temp: f32, tint: f32, kelvin: Option<f32>) -> Self {
        match (mode, kelvin) {
            (Some("kelvin"), Some(k)) => WbSpec::Kelvin { kelvin: k, tint },
            _ => WbSpec::Relative { temp, tint },
        }
    }
}

/// The per-channel gains a white-balance spec means for this image. Relative gains are the
/// same gentle multipliers engine 1 uses, so "+0.3 warm" looks the same on both engines.
/// Kelvin needs the camera's own multipliers and matrix; until the Kelvin slice renders it,
/// it is refused with a clear error rather than silently treated as relative.
pub fn wb_multipliers(spec: &WbSpec, _cam_mul: &[f32; 4], _rgb_cam: &[[f32; 3]; 3]) -> Result<[f32; 3], String> {
    match *spec {
        WbSpec::Relative { temp, tint } => Ok([1.0 + 0.3 * temp, 1.0 + 0.15 * tint, 1.0 - 0.3 * temp]),
        WbSpec::Kelvin { .. } => Err("Kelvin white balance is not rendered yet (docs/plans/raw-foundation, slice 9)".into()),
    }
}

/// Exposure and white balance in linear light: every channel is multiplied — +1 EV is
/// exactly twice the light, and values above 1.0 survive until [`to_display`].
pub fn apply_exposure_linear(img: &mut Rgb32FImage, ev: f32, wb: [f32; 3]) {
    let gain = 2f32.powf(ev);
    let g = [gain * wb[0], gain * wb[1], gain * wb[2]];
    if g == [1.0, 1.0, 1.0] {
        return;
    }
    let w = img.width() as usize;
    if w == 0 {
        return;
    }
    img.as_mut().par_chunks_mut(w * 3).for_each(|row| {
        for px in row.chunks_exact_mut(3) {
            px[0] *= g[0];
            px[1] *= g[1];
            px[2] *= g[2];
        }
    });
}

/// The rendering transform slot (decision 5): plain sRGB, or sRGB with a soft shoulder
/// that rolls highlights off instead of clipping them. The default is chosen on the
/// user's own photos in slice 2, not here.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DisplayTransform {
    Srgb,
    Soft { shoulder: f32 },
}

impl DisplayTransform {
    /// From the record's `display` field: absent or `"srgb"` = sRGB; `"soft"` = the shoulder.
    pub fn from_record(name: Option<&str>) -> Self {
        match name {
            Some("soft") => DisplayTransform::Soft { shoulder: 0.8 },
            _ => DisplayTransform::Srgb,
        }
    }
}

/// A fixed lift applied before the transform so an as-shot render lands near the camera
/// JPEG's brightness rather than a stop darker (decision 4). Provisional; measured on the
/// corpus in slice 2 and recorded in 00-status.md.
pub const BASELINE_EV: f32 = 0.5;

/// sRGB opto-electronic transfer: linear → encoded, both in 0..1.
pub fn srgb_oetf(x: f32) -> f32 {
    if x <= 0.003_130_8 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

/// Fit the linear image onto the display: baseline lift, the transform, the sRGB curve,
/// 8-bit. Values above display white clip here (Srgb) or roll off (Soft) — nowhere earlier.
pub fn to_display(img: &Rgb32FImage, t: DisplayTransform, baseline_ev: f32) -> RgbImage {
    let (w, h) = img.dimensions();
    let lift = 2f32.powf(baseline_ev);
    let mut out = RgbImage::new(w, h);
    if w == 0 || h == 0 {
        return out;
    }
    let row_w = w as usize;
    out.as_mut()
        .par_chunks_mut(row_w * 3)
        .zip(img.as_raw().par_chunks(row_w * 3))
        .for_each(|(dst, src)| {
            for (d, s) in dst.iter_mut().zip(src.iter()) {
                let mut v = (s * lift).max(0.0);
                if let DisplayTransform::Soft { shoulder } = t {
                    if v > shoulder {
                        let room = 1.0 - shoulder;
                        v = shoulder + room * (1.0 - (-(v - shoulder) / room).exp());
                    }
                }
                *d = (srgb_oetf(v.min(1.0)) * 255.0).round().clamp(0.0, 255.0) as u8;
            }
        });
    out
}

/// Where the sensor itself clipped: 255 wherever any channel is at or above sensor white.
/// The only white that is gone for good (docs/plans/raw-foundation, mockup 02).
pub fn clip_mask(img: &Rgb32FImage) -> GrayImage {
    let (w, h) = img.dimensions();
    let mut out = GrayImage::new(w, h);
    for (o, px) in out.pixels_mut().zip(img.pixels()) {
        o.0[0] = if px.0.iter().any(|&c| c >= 1.0) { 255 } else { 0 };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp() -> Rgb32FImage {
        Rgb32FImage::from_fn(8, 2, |x, _| {
            let v = x as f32 / 7.0;
            image::Rgb([v * 1.4, v, v * 0.5])
        })
    }

    #[test]
    fn wb_spec_relative_zero_is_as_shot_and_kelvin_is_refused() {
        let cm = [2.0, 1.0, 1.5, 1.0];
        let m = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
        assert_eq!(wb_multipliers(&WbSpec::Relative { temp: 0.0, tint: 0.0 }, &cm, &m).unwrap(), [1.0, 1.0, 1.0]);
        let warm = wb_multipliers(&WbSpec::Relative { temp: 0.5, tint: 0.0 }, &cm, &m).unwrap();
        assert!(warm[0] > 1.0 && warm[2] < 1.0);
        let k = WbSpec::from_record(Some("kelvin"), 0.0, 0.0, Some(5400.0));
        assert_eq!(k, WbSpec::Kelvin { kelvin: 5400.0, tint: 0.0 });
        assert!(wb_multipliers(&k, &cm, &m).is_err(), "never silently relative");
        assert_eq!(WbSpec::from_record(None, 0.3, -0.1, None), WbSpec::Relative { temp: 0.3, tint: -0.1 });
    }

    #[test]
    fn linear_tone_ev_is_a_pure_multiply_and_headroom_survives() {
        let mut img = ramp();
        let before: Vec<f32> = img.as_raw().clone();
        apply_exposure_linear(&mut img, 1.0, [1.0, 1.0, 1.0]);
        for (a, b) in img.as_raw().iter().zip(before.iter()) {
            assert!((a - 2.0 * b).abs() < 1e-6);
        }
        assert!(img.as_raw().iter().any(|&v| v > 1.0), "values above white are kept");
    }

    #[test]
    fn headroom_recovers_above_display_white() {
        // A patch 1.4× above display white: at 0 EV it shows as white; at −1 EV the
        // detail comes back. An 8-bit rendering of the same patch cannot do this — it
        // was clipped to 255 before any slider ran.
        let mut bright = Rgb32FImage::from_fn(2, 1, |x, _| image::Rgb([if x == 0 { 1.4 } else { 1.2 }; 3]));
        let at0 = to_display(&bright, DisplayTransform::Srgb, 0.0);
        assert_eq!(at0.get_pixel(0, 0).0, at0.get_pixel(1, 0).0, "both clip to white at 0 EV");
        assert_eq!(at0.get_pixel(0, 0).0, [255, 255, 255]);
        apply_exposure_linear(&mut bright, -1.0, [1.0; 3]);
        let at_minus1 = to_display(&bright, DisplayTransform::Srgb, 0.0);
        assert_ne!(at_minus1.get_pixel(0, 0).0, at_minus1.get_pixel(1, 0).0, "detail is back");
        assert!(at_minus1.get_pixel(0, 0).0[0] < 255);
        // The 8-bit path: clip first, then halve — both patches stay identical.
        let eight = at0.clone();
        let halved: Vec<u8> = eight.as_raw().iter().map(|&v| (v as f32 * 0.5).round() as u8).collect();
        assert_eq!(halved[0..3], halved[3..6]);
    }

    #[test]
    fn srgb_oetf_hits_the_known_points() {
        assert!((srgb_oetf(0.0)).abs() < 1e-6);
        assert!((srgb_oetf(1.0) - 1.0).abs() < 1e-6);
        assert!((srgb_oetf(0.18) - 0.4613).abs() < 1e-3, "18% grey encodes to ~46%");
        assert!((srgb_oetf(0.002) - 0.02584).abs() < 1e-4, "linear toe");
    }

    #[test]
    fn soft_transform_rolls_off_instead_of_clipping() {
        let img = Rgb32FImage::from_fn(3, 1, |x, _| image::Rgb([[0.5, 1.0, 1.2][x as usize]; 3]));
        let srgb = to_display(&img, DisplayTransform::Srgb, 0.0);
        let soft = to_display(&img, DisplayTransform::Soft { shoulder: 0.8 }, 0.0);
        assert_eq!(srgb.get_pixel(0, 0).0, soft.get_pixel(0, 0).0, "below the shoulder they agree");
        assert_eq!(srgb.get_pixel(1, 0).0, srgb.get_pixel(2, 0).0, "sRGB clips 1.0 and 1.2 alike");
        assert!(soft.get_pixel(1, 0).0[0] < soft.get_pixel(2, 0).0[0], "the shoulder keeps them apart");
        assert!(soft.get_pixel(2, 0).0[0] < 255, "…and 1.2× white is still short of clipping");
    }

    #[test]
    fn clip_mask_marks_only_sensor_white() {
        let img = Rgb32FImage::from_fn(3, 1, |x, _| image::Rgb([[0.999, 1.0, 0.2][x as usize], 0.1, 0.1]));
        let m = clip_mask(&img);
        assert_eq!([m.get_pixel(0, 0).0[0], m.get_pixel(1, 0).0[0], m.get_pixel(2, 0).0[0]], [0, 255, 0]);
    }
}
