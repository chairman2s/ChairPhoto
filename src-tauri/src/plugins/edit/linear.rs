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

/// The rendering transform slot (decision 5): plain sRGB, sRGB with a soft shoulder that
/// rolls highlights off instead of clipping them, or the camera-style curve
/// ([`CAMERA_CURVE`]). New engine-2 edits get `camera` (user decision 2026-09-24: the
/// default should be closer to the camera's picture style); a record without the field
/// stays plain sRGB, so a saved version renders the same forever.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DisplayTransform {
    Srgb,
    Soft { shoulder: f32 },
    Camera,
}

impl DisplayTransform {
    /// From the record's `display` field: absent or `"srgb"` = sRGB; `"soft"` = the
    /// shoulder; `"camera"` = the camera-style curve.
    pub fn from_record(name: Option<&str>) -> Self {
        match name {
            Some("soft") => DisplayTransform::Soft { shoulder: 0.8 },
            Some("camera") => DisplayTransform::Camera,
            _ => DisplayTransform::Srgb,
        }
    }
}

/// The camera-style tone curve: (linear value after the [`BASELINE_EV`] lift, encoded
/// display value), applied to each channel. Fitted by `develop::camera_fit` on 2026-09-24
/// against the embedded camera JPEGs of five Sony ARWs (A7 IV "Standard", A7R VI "Vivid",
/// DRO Auto): per-bin medians of every channel of the centre 80 %, smoothed over five
/// bins and made monotone. Mean |Δ| to the camera JPEG fell from 10.1 (plain sRGB) to 3.6
/// levels of 255, and the per-channel curve alone brought the colour up to the camera's
/// (median chroma 58 → 75 against the camera's 75 on the Vivid frame), so there is no
/// separate saturation step. Left out of the fit, with the reason: two frames at extended
/// low ISO (50 and 80, below the bodies' base 100), which the camera exposes brighter and
/// pulls down about a stop in its own processing — a per-photo exposure, not the style —
/// and one DNG whose camera preview sits ~0.3 stop darker for a reason not yet found.
/// Compared with sRGB it holds shadows lower (0.016 vs 0.033 at the first knot), lifts the
/// midtones a little, and keeps about a stop and a half above display white in a shoulder
/// instead of clipping it. Beyond the last measured knot it reaches white at 3.0.
pub const CAMERA_CURVE: &[(f32, f32)] = &[
    (0.002533, 0.0157),
    (0.003012, 0.0167),
    (0.003582, 0.0180),
    (0.004260, 0.0220),
    (0.005066, 0.0259),
    (0.006024, 0.0298),
    (0.007164, 0.0353),
    (0.008520, 0.0424),
    (0.010132, 0.0502),
    (0.012049, 0.0596),
    (0.014328, 0.0714),
    (0.017039, 0.0855),
    (0.020263, 0.1012),
    (0.024097, 0.1192),
    (0.028656, 0.1396),
    (0.034078, 0.1624),
    (0.040526, 0.1875),
    (0.048194, 0.2165),
    (0.057313, 0.2478),
    (0.068157, 0.2816),
    (0.081052, 0.3192),
    (0.096388, 0.3592),
    (0.114626, 0.4000),
    (0.136313, 0.4424),
    (0.162105, 0.4855),
    (0.192776, 0.5271),
    (0.229251, 0.5694),
    (0.272627, 0.6141),
    (0.324210, 0.6612),
    (0.385553, 0.7075),
    (0.458502, 0.7529),
    (0.545254, 0.7953),
    (0.648420, 0.8408),
    (0.771105, 0.8776),
    (0.917004, 0.9129),
    (1.090508, 0.9459),
    (1.296840, 0.9725),
    (1.834008, 0.9814),
    (2.181015, 0.9922),
    (3.0, 1.0),
];

/// The camera curve at `x`: monotone cubic (Fritsch–Carlson) through [`CAMERA_CURVE`] in
/// log2(x), so the curve is smooth between knots and never overshoots; a straight line
/// to zero below the first knot; white from the last.
pub fn camera_curve(x: f32) -> f32 {
    let k = CAMERA_CURVE;
    if x <= 0.0 {
        return 0.0;
    }
    if x <= k[0].0 {
        return k[0].1 * x / k[0].0;
    }
    if x >= k[k.len() - 1].0 {
        return 1.0;
    }
    let lx: Vec<f32> = k.iter().map(|p| p.0.log2()).collect();
    let n = k.len();
    let d: Vec<f32> = (0..n - 1).map(|i| (k[i + 1].1 - k[i].1) / (lx[i + 1] - lx[i])).collect();
    let tangent = |i: usize| -> f32 {
        if i == 0 {
            return d[0];
        }
        if i == n - 1 {
            return d[n - 2];
        }
        if d[i - 1] * d[i] <= 0.0 {
            return 0.0;
        }
        // Harmonic mean keeps the interpolant monotone where the data is.
        2.0 / (1.0 / d[i - 1] + 1.0 / d[i])
    };
    let t = x.log2();
    let i = lx.windows(2).position(|w| t <= w[1]).unwrap_or(n - 2);
    let h = lx[i + 1] - lx[i];
    let s = (t - lx[i]) / h;
    let (m0, m1) = (tangent(i) * h, tangent(i + 1) * h);
    let (s2, s3) = (s * s, s * s * s);
    (2.0 * s3 - 3.0 * s2 + 1.0) * k[i].1 + (s3 - 2.0 * s2 + s) * m0 + (-2.0 * s3 + 3.0 * s2) * k[i + 1].1 + (s3 - s2) * m1
}

/// [`camera_curve`] tabulated for the per-pixel path: `CAMERA_LUT_SIZE` samples over
/// 0..=3.0, linear between them. Built once.
const CAMERA_LUT_SIZE: usize = 8192;
fn camera_lut() -> &'static [f32] {
    static LUT: std::sync::OnceLock<Vec<f32>> = std::sync::OnceLock::new();
    LUT.get_or_init(|| {
        (0..CAMERA_LUT_SIZE)
            .map(|i| camera_curve(i as f32 * 3.0 / (CAMERA_LUT_SIZE - 1) as f32))
            .collect()
    })
}

fn camera_lookup(lut: &[f32], v: f32) -> f32 {
    let p = (v * (CAMERA_LUT_SIZE - 1) as f32 / 3.0).clamp(0.0, (CAMERA_LUT_SIZE - 1) as f32);
    let i = (p as usize).min(CAMERA_LUT_SIZE - 2);
    let f = p - i as f32;
    lut[i] + (lut[i + 1] - lut[i]) * f
}

/// A fixed lift applied before the transform so an as-shot render lands near the camera
/// JPEG's brightness rather than a stop darker (decision 4). Measured on the user's Sony
/// A7R VI `_DSC8291.ARW` (slice 2, 2026-09-19): the camera preview's mean sRGB value is
/// 105.6; a plain linear decode lands there at +1.4 EV (66.8 at 0, 93.8 at +1.0, 109.9 at
/// +1.5). That gap is also the headroom that returns when exposure is pulled down. One
/// photo, one body — a per-camera value is the better answer later (decision 4).
pub const BASELINE_EV: f32 = 1.4;

/// sRGB opto-electronic transfer: linear → encoded, both in 0..1.
pub fn srgb_oetf(x: f32) -> f32 {
    if x <= 0.003_130_8 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

/// Fit the linear image onto the display: baseline lift, the transform, the sRGB curve,
/// 8-bit. Values above display white clip here (Srgb) or roll off (Soft, Camera) —
/// nowhere earlier. `Camera` replaces the sRGB curve with [`CAMERA_CURVE`].
pub fn to_display(img: &Rgb32FImage, t: DisplayTransform, baseline_ev: f32) -> RgbImage {
    let (w, h) = img.dimensions();
    let lift = 2f32.powf(baseline_ev);
    let mut out = RgbImage::new(w, h);
    if w == 0 || h == 0 {
        return out;
    }
    let row_w = w as usize;
    let lut = (t == DisplayTransform::Camera).then(camera_lut);
    out.as_mut()
        .par_chunks_mut(row_w * 3)
        .zip(img.as_raw().par_chunks(row_w * 3))
        .for_each(|(dst, src)| {
            if let Some(lut) = lut {
                // The camera curve is fitted to encoded output: no sRGB curve after it.
                for (d, s) in dst.iter_mut().zip(src.iter()) {
                    *d = (camera_lookup(lut, (s * lift).max(0.0)) * 255.0).round().clamp(0.0, 255.0) as u8;
                }
                return;
            }
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

/// Downscale a linear image so its long edge is `max_edge`, by exact area averaging
/// (every source pixel contributes its covered fraction to exactly one or two output
/// pixels per axis). Written here because the `image` crate's `thumbnail`/`resize` are
/// wrong on f32 buffers — on a real decode they turned a mean of 0.08 into 0.58 and
/// produced values above the source's maximum — so the linear path never calls them.
/// Never upscales; `max_edge = 0` returns the input.
pub fn downscale_linear(img: &Rgb32FImage, max_edge: u32) -> Rgb32FImage {
    let (w, h) = img.dimensions();
    if max_edge == 0 || w.max(h) <= max_edge || w == 0 || h == 0 {
        return img.clone();
    }
    let scale = max_edge as f64 / w.max(h) as f64;
    let ow = ((w as f64 * scale).round() as u32).max(1);
    let oh = ((h as f64 * scale).round() as u32).max(1);
    // Per-axis coverage: for output index i, the source span [i*w/ow, (i+1)*w/ow).
    let spans = |n_out: u32, n_in: u32| -> Vec<Vec<(u32, f32)>> {
        let ratio = n_in as f64 / n_out as f64;
        (0..n_out)
            .map(|i| {
                let a = i as f64 * ratio;
                let b = (i as f64 + 1.0) * ratio;
                let mut v = Vec::new();
                let mut x = a.floor() as u32;
                while (x as f64) < b && x < n_in {
                    let lo = a.max(x as f64);
                    let hi = b.min(x as f64 + 1.0);
                    if hi > lo {
                        v.push((x, (hi - lo) as f32));
                    }
                    x += 1;
                }
                v
            })
            .collect()
    };
    let xs = spans(ow, w);
    let ys = spans(oh, h);
    let src = img.as_raw();
    let sw = w as usize;
    let mut out = vec![0f32; ow as usize * oh as usize * 3];
    out.par_chunks_mut(ow as usize * 3)
        .enumerate()
        .for_each(|(oy, row)| {
            let yspan = &ys[oy];
            let inv_area: f32 = 1.0 / (yspan.iter().map(|(_, wy)| wy).sum::<f32>() * 1.0);
            for (ox, px) in row.chunks_exact_mut(3).enumerate() {
                let xspan = &xs[ox];
                let xw: f32 = xspan.iter().map(|(_, wx)| wx).sum();
                let mut acc = [0f32; 3];
                for &(sy, wy) in yspan {
                    let base = sy as usize * sw * 3;
                    for &(sx, wx) in xspan {
                        let i = base + sx as usize * 3;
                        let wgt = wx * wy;
                        acc[0] += src[i] * wgt;
                        acc[1] += src[i + 1] * wgt;
                        acc[2] += src[i + 2] * wgt;
                    }
                }
                let norm = inv_area / xw;
                px[0] = acc[0] * norm;
                px[1] = acc[1] * norm;
                px[2] = acc[2] * norm;
            }
        });
    Rgb32FImage::from_raw(ow, oh, out).expect("sized to its buffer")
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
    fn display_transform_names() {
        assert_eq!(DisplayTransform::from_record(None), DisplayTransform::Srgb);
        assert_eq!(DisplayTransform::from_record(Some("srgb")), DisplayTransform::Srgb);
        assert_eq!(DisplayTransform::from_record(Some("camera")), DisplayTransform::Camera);
        assert_eq!(DisplayTransform::from_record(Some("unknown")), DisplayTransform::Srgb);
    }

    #[test]
    fn camera_curve_passes_its_knots_and_is_monotone() {
        for &(x, y) in CAMERA_CURVE {
            assert!((camera_curve(x) - y).abs() < 1e-4 || x >= 3.0, "knot {x}");
        }
        assert_eq!(camera_curve(0.0), 0.0);
        assert_eq!(camera_curve(3.0), 1.0);
        assert_eq!(camera_curve(10.0), 1.0);
        let mut prev = -1.0f32;
        for i in 0..=30_000 {
            let y = camera_curve(i as f32 * 1e-4);
            assert!(y >= prev - 1e-6, "monotone at {}", i as f32 * 1e-4);
            assert!((0.0..=1.0).contains(&y));
            prev = y;
        }
    }

    #[test]
    fn camera_transform_is_the_camera_s_curve_with_a_shoulder() {
        let at = |v: f32| {
            let img = Rgb32FImage::from_pixel(1, 1, image::Rgb([v; 3]));
            to_display(&img, DisplayTransform::Camera, 0.0).get_pixel(0, 0).0[0]
        };
        let srgb = |v: f32| {
            let img = Rgb32FImage::from_pixel(1, 1, image::Rgb([v; 3]));
            to_display(&img, DisplayTransform::Srgb, 0.0).get_pixel(0, 0).0[0]
        };
        assert!(at(0.005) < srgb(0.005), "deeper shadows");
        assert!(at(0.16) > srgb(0.16), "brighter midtones");
        assert_eq!(srgb(1.3), 255);
        assert!(at(1.3) < 255 && at(1.3) > at(1.0), "detail kept above display white");
        assert_eq!(at(3.5), 255);
        // Per channel: a coloured pixel keeps its hue order and gains chroma in the mids.
        let img = Rgb32FImage::from_pixel(1, 1, image::Rgb([0.2, 0.1, 0.05]));
        let (c, s) = (to_display(&img, DisplayTransform::Camera, 0.0), to_display(&img, DisplayTransform::Srgb, 0.0));
        let (c, s) = (c.get_pixel(0, 0).0, s.get_pixel(0, 0).0);
        assert!(c[0] > c[1] && c[1] > c[2]);
        assert!(c[0] as i32 - c[2] as i32 > s[0] as i32 - s[2] as i32);
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
    fn downscale_linear_preserves_mean_and_never_exceeds_the_source() {
        // A constant image stays constant; a ramp keeps its mean; nothing exceeds the max.
        let flat = Rgb32FImage::from_fn(100, 60, |_, _| image::Rgb([0.08, 0.3, 1.0]));
        let small = downscale_linear(&flat, 33);
        assert_eq!(small.dimensions(), (33, 20));
        for px in small.pixels() {
            for (a, b) in px.0.iter().zip([0.08f32, 0.3, 1.0]) {
                assert!((a - b).abs() < 1e-5, "{a} vs {b}");
            }
        }
        let ramp = Rgb32FImage::from_fn(1000, 10, |x, _| image::Rgb([x as f32 / 999.0; 3]));
        let mean = |v: &Rgb32FImage| v.as_raw().iter().sum::<f32>() / v.as_raw().len() as f32;
        let small = downscale_linear(&ramp, 97);
        assert!((mean(&small) - mean(&ramp)).abs() < 0.01, "{} vs {}", mean(&small), mean(&ramp));
        assert!(small.as_raw().iter().all(|&v| v <= 1.0 + 1e-6));
        // No-ops.
        assert_eq!(downscale_linear(&ramp, 0).dimensions(), (1000, 10));
        assert_eq!(downscale_linear(&ramp, 5000).dimensions(), (1000, 10));
    }

    #[test]
    fn clip_mask_marks_only_sensor_white() {
        let img = Rgb32FImage::from_fn(3, 1, |x, _| image::Rgb([[0.999, 1.0, 0.2][x as usize], 0.1, 0.1]));
        let m = clip_mask(&img);
        assert_eq!([m.get_pixel(0, 0).0[0], m.get_pixel(1, 0).0[0], m.get_pixel(2, 0).0[0]], [0, 255, 0]);
    }
}
