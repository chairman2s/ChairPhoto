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

/// A 3×3 colour matrix acting on linear RGB (rows = output channels).
pub type Mat3 = [[f32; 3]; 3];

/// The identity matrix.
pub const IDENTITY: Mat3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

/// The white-balance matrix a spec means for this image. Relative is the same gentle
/// per-channel gains engine 1 uses, so "+0.3 warm" looks the same on both engines. Kelvin
/// renders a stated scene light (docs/plans/raw-foundation, slice 9 — see
/// [`kelvin_matrix`]); it needs the camera's daylight multipliers, and a camera without
/// them is refused with a clear error rather than silently treated as relative.
pub fn wb_matrix(
    spec: &WbSpec,
    cam_mul: &[f32; 4],
    pre_mul: &[f32; 4],
    rgb_cam: &Mat3,
    wbct: &[[f32; 4]],
) -> Result<Mat3, String> {
    match *spec {
        WbSpec::Relative { temp, tint } => {
            let g = [1.0 + 0.3 * temp, 1.0 + 0.15 * tint, 1.0 - 0.3 * temp];
            Ok([[g[0], 0.0, 0.0], [0.0, g[1], 0.0], [0.0, 0.0, g[2]]])
        }
        WbSpec::Kelvin { kelvin, tint } => {
            let cam = WbCamera::new(cam_mul, pre_mul, rgb_cam, wbct)
                .ok_or("Kelvin white balance needs the camera's white-balance table or daylight white, which this file does not give")?;
            Ok(cam.kelvin_matrix(kelvin, tint))
        }
    }
}

/// Exposure and white balance in linear light: +1 EV is exactly twice the light, the
/// white-balance matrix re-weights the channels, and values above 1.0 survive until
/// [`to_display`].
pub fn apply_exposure_linear(img: &mut Rgb32FImage, ev: f32, wb: Mat3) {
    let gain = 2f32.powf(ev);
    let m: Mat3 = wb.map(|row| row.map(|v| v * gain));
    if m == IDENTITY {
        return;
    }
    let w = img.width() as usize;
    if w == 0 {
        return;
    }
    let diagonal = m[0][1] == 0.0 && m[0][2] == 0.0 && m[1][0] == 0.0 && m[1][2] == 0.0 && m[2][0] == 0.0 && m[2][1] == 0.0;
    img.as_mut().par_chunks_mut(w * 3).for_each(|row| {
        for px in row.chunks_exact_mut(3) {
            if diagonal {
                px[0] *= m[0][0];
                px[1] *= m[1][1];
                px[2] *= m[2][2];
            } else {
                let (r, g, b) = (px[0], px[1], px[2]);
                px[0] = m[0][0] * r + m[0][1] * g + m[0][2] * b;
                px[1] = m[1][0] * r + m[1][1] * g + m[1][2] * b;
                px[2] = m[2][0] * r + m[2][1] * g + m[2][2] * b;
            }
        }
    });
}

// ── Kelvin (docs/plans/raw-foundation, slice 9) ─────────────────────────────
//
// The working image is the camera's RGB balanced to the as-shot light and taken to sRGB
// by `rgb_cam`, which LibRaw normalizes so that daylight white (the `pre_mul`
// multipliers) is (1,1,1). In that daylight-normalized camera space a neutral lit by an
// illuminant with sRGB-linear white s sits at u = rgb_cam⁻¹·s, and the camera's as-shot
// balance is a = cam_mul / pre_mul — it made the as-shot neutral u_as ∝ 1/a white. To
// render a stated light T instead, balance with b ∝ 1/u_T and change the image by
// rgb_cam · diag(b / a) · rgb_cam⁻¹ (green held, so exposure stays). The as-shot light is
// the T whose red/blue balance equals the camera's; its tint the green left over — so
// rendering the as-shot Kelvin and tint is exactly the identity.
//
// Where the file carries the camera's own white-balance table (LibRaw's `WBCT_Coeffs`:
// the raw multipliers the camera uses at stated temperatures), Kelvin is calibrated to
// that instead: b is the table's multipliers at T (interpolated in mireds), the change is
// rgb_cam · diag(b / cam_mul) · rgb_cam⁻¹, and no matrix-derived light enters. Measured
// 2026-09-24: on the A7 IV the model alone maps the camera's labelled 2500/3200/4500/
// 6000/8500 K presets to 2639/3332/4526/5829/8036 K, but on the A7 R VI to
// 2213/2670/3452/4260/5535 K — the decoder's colour data for that newer body is off, and
// the camera's own table is not.

/// The Kelvin range rendered and offered (Kim et al.'s locus covers 1667–25000 K).
pub const KELVIN_MIN: f32 = 2000.0;
pub const KELVIN_MAX: f32 = 12000.0;

/// Tint units: +100 is one stop less green (magenta), −100 one stop more.
pub const TINT_UNITS_PER_STOP: f64 = 100.0;

/// CIE 1931 xy of a Planckian radiator at `kelvin` (Kim et al. 2002 cubic splines,
/// 1667–25000 K; clamped there).
pub fn kelvin_xy(kelvin: f32) -> (f64, f64) {
    let t = (kelvin as f64).clamp(1667.0, 25000.0);
    let (t2, t3) = (t * t, t * t * t);
    let x = if t <= 4000.0 {
        -0.266_123_9e9 / t3 - 0.234_358_9e6 / t2 + 0.877_695_6e3 / t + 0.179_910
    } else {
        -3.025_846_9e9 / t3 + 2.107_037_9e6 / t2 + 0.222_634_7e3 / t + 0.240_390
    };
    let (x2, x3) = (x * x, x * x * x);
    let y = if t <= 2222.0 {
        -1.106_381_4 * x3 - 1.348_110_20 * x2 + 2.185_558_32 * x - 0.202_196_83
    } else if t <= 4000.0 {
        -0.954_947_6 * x3 - 1.374_185_93 * x2 + 2.091_370_15 * x - 0.167_488_67
    } else {
        3.081_758_0 * x3 - 5.873_386_70 * x2 + 3.751_129_97 * x - 0.370_014_83
    };
    (x, y)
}

/// The linear-sRGB white (Y = 1) of a Planckian light at `kelvin`.
fn srgb_white(kelvin: f32) -> [f64; 3] {
    let (x, y) = kelvin_xy(kelvin);
    let xyz = [x / y, 1.0, (1.0 - x - y) / y];
    const M: [[f64; 3]; 3] = [[3.2406, -1.5372, -0.4986], [-0.9689, 1.8758, 0.0415], [0.0557, -0.2040, 1.0570]];
    [0, 1, 2].map(|r| M[r][0] * xyz[0] + M[r][1] * xyz[1] + M[r][2] * xyz[2])
}

fn inverse3(m: &[[f64; 3]; 3]) -> Option<[[f64; 3]; 3]> {
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1]) - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    if det.abs() < 1e-9 || !det.is_finite() {
        return None;
    }
    let c = |r0: usize, c0: usize, r1: usize, c1: usize| m[r0][c0] * m[r1][c1] - m[r0][c1] * m[r1][c0];
    Some([
        [c(1, 1, 2, 2) / det, -c(0, 1, 2, 2) / det, c(0, 1, 1, 2) / det],
        [-c(1, 0, 2, 2) / det, c(0, 0, 2, 2) / det, -c(0, 0, 1, 2) / det],
        [c(1, 0, 2, 1) / det, -c(0, 0, 2, 1) / det, c(0, 0, 1, 1) / det],
    ])
}

fn mul3(m: &[[f64; 3]; 3], v: [f64; 3]) -> [f64; 3] {
    [0, 1, 2].map(|r| m[r][0] * v[0] + m[r][1] * v[1] + m[r][2] * v[2])
}

/// What Kelvin white balance needs of one camera: its matrix both ways, its as-shot
/// balance, and — when the file has it — the camera's own white-balance table.
pub struct WbCamera {
    rgb_cam: [[f64; 3]; 3],
    cam_from_srgb: [[f64; 3]; 3],
    /// The model's view: cam_mul / pre_mul, green = 1 (unused with a table).
    as_shot: [f64; 3],
    /// The camera's raw as-shot multipliers, green = 1.
    cam: [f64; 3],
    /// (mireds, raw multipliers with green = 1), ascending in mireds; ≥ 2 rows or empty.
    table: Vec<(f64, [f64; 3])>,
}

impl WbCamera {
    /// `None` when the file gives neither a usable white-balance table nor daylight
    /// multipliers, or no invertible matrix.
    pub fn new(cam_mul: &[f32; 4], pre_mul: &[f32; 4], rgb_cam: &Mat3, wbct: &[[f32; 4]]) -> Option<WbCamera> {
        let ok = |v: f32| v.is_finite() && v > 0.0;
        if !(0..3).all(|c| ok(cam_mul[c])) {
            return None;
        }
        let cam = [0, 1, 2].map(|c| cam_mul[c] as f64 / cam_mul[1] as f64);
        let mut table: Vec<(f64, [f64; 3])> = wbct
            .iter()
            .filter(|r| ok(r[0]) && ok(r[1]) && ok(r[2]) && ok(r[3]))
            .map(|r| (1e6 / r[0] as f64, [r[1] as f64 / r[2] as f64, 1.0, r[3] as f64 / r[2] as f64]))
            .collect();
        table.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        table.dedup_by(|a, b| (a.0 - b.0).abs() < 1e-9);
        if table.len() < 2 {
            table.clear();
        }
        let have_daylight = (0..3).all(|c| ok(pre_mul[c]));
        if table.is_empty() && !have_daylight {
            return None;
        }
        let as_shot = if have_daylight {
            let a = [0, 1, 2].map(|c| cam_mul[c] as f64 / pre_mul[c] as f64);
            a.map(|v| v / a[1])
        } else {
            [1.0; 3]
        };
        let rgb_cam = rgb_cam.map(|r| r.map(|v| v as f64));
        let cam_from_srgb = inverse3(&rgb_cam)?;
        Some(WbCamera { rgb_cam, cam_from_srgb, as_shot, cam, table })
    }

    /// Whether Kelvin here is calibrated to the camera's own table.
    pub fn calibrated(&self) -> bool {
        !self.table.is_empty()
    }

    /// The camera's raw multipliers (green = 1) for a light at `kelvin`: log ratios linear
    /// in mireds between the table's rows, extended along the end segments beyond them.
    fn table_mul(&self, kelvin: f64) -> [f64; 3] {
        let m = 1e6 / kelvin;
        let t = &self.table;
        let i = t.windows(2).position(|w| m <= w[1].0).unwrap_or(t.len() - 2);
        let (a, b) = (&t[i], &t[i + 1]);
        let s = (m - a.0) / (b.0 - a.0);
        let lerp = |x: f64, y: f64| (x.ln() + s * (y.ln() - x.ln())).exp();
        [lerp(a.1[0], b.1[0]), 1.0, lerp(a.1[2], b.1[2])]
    }

    /// Where a neutral lit at `kelvin` sits in the daylight-normalized camera space.
    fn white(&self, kelvin: f32) -> [f64; 3] {
        mul3(&self.cam_from_srgb, srgb_white(kelvin))
    }

    /// The as-shot light: the Kelvin whose red/blue balance is the camera's (bisection in
    /// mireds, where the locus is closest to even), and the tint that is the green left
    /// over. `None` when the camera's balance lies outside the locus's range.
    pub fn as_shot(&self) -> Option<(f32, f32)> {
        if self.calibrated() {
            return self.as_shot_from_table();
        }
        let a = self.as_shot;
        // ln(u_T.B/u_T.R) rises with T; the as-shot T makes it equal ln(a.R/a.B).
        let target = (a[0] / a[2]).ln();
        let f = |k: f64| {
            let u = self.white(k as f32);
            // Very warm lights leave the sRGB gamut (blue goes negative below ~1800 K); a
            // floor keeps the ratio defined and monotone there.
            (u[2].max(1e-9) / u[0].max(1e-9)).ln() - target
        };
        let (mut lo, mut hi) = (1e6 / 25000.0, 1e6 / 2000.0); // mireds: hi = warm end
        let (flo, fhi) = (f(1e6 / lo), f(1e6 / hi));
        if !(flo.is_finite() && fhi.is_finite()) || flo.signum() == fhi.signum() {
            return None;
        }
        for _ in 0..60 {
            let mid = 0.5 * (lo + hi);
            if f(1e6 / mid).signum() == flo.signum() {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let kelvin = 1e6 / (0.5 * (lo + hi));
        let u = self.white(kelvin as f32);
        let g = a[1] * u[1] / (a[0] * u[0] * a[2] * u[2]).sqrt();
        let tint = -TINT_UNITS_PER_STOP * g.log2();
        Some((kelvin as f32, tint as f32))
    }

    /// The as-shot light from the camera's table: the Kelvin whose red/blue multipliers
    /// match the camera's as-shot ones, and the tint that is the green left over.
    fn as_shot_from_table(&self) -> Option<(f32, f32)> {
        let c = self.cam;
        let target = (c[0] / c[2]).ln();
        // Red/blue gain rises with the light's temperature (bluer light, more red gain).
        let f = |k: f64| {
            let m = self.table_mul(k);
            (m[0] / m[2]).ln() - target
        };
        let (mut lo, mut hi) = (1e6 / 25000.0, 1e6 / 1667.0);
        let (flo, fhi) = (f(1e6 / lo), f(1e6 / hi));
        if !(flo.is_finite() && fhi.is_finite()) || flo.signum() == fhi.signum() {
            return None;
        }
        for _ in 0..60 {
            let mid = 0.5 * (lo + hi);
            if f(1e6 / mid).signum() == flo.signum() {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let kelvin = 1e6 / (0.5 * (lo + hi));
        let m = self.table_mul(kelvin);
        let g = (c[1] / (c[0] * c[2]).sqrt()) / (m[1] / (m[0] * m[2]).sqrt());
        Some((kelvin as f32, (-TINT_UNITS_PER_STOP * g.log2()) as f32))
    }

    /// The matrix that renders the image as lit by `kelvin` with `tint`, green held.
    pub fn kelvin_matrix(&self, kelvin: f32, tint: f32) -> Mat3 {
        let green = 2f64.powf(-tint as f64 / TINT_UNITS_PER_STOP);
        let kelvin = kelvin.clamp(1667.0, 25000.0);
        // b / a: the balance for the stated light over the as-shot one, in the space each
        // lives in — raw multipliers with the camera's table, daylight-normalized without.
        let r = if self.calibrated() {
            let m = self.table_mul(kelvin as f64);
            let b = [m[0], green * m[1], m[2]];
            [0, 1, 2].map(|c| b[c] / self.cam[c])
        } else {
            let u = self.white(kelvin);
            let b = [1.0 / u[0], green / u[1], 1.0 / u[2]];
            [0, 1, 2].map(|c| b[c] / self.as_shot[c])
        };
        let r = r.map(|v| v / r[1]);
        let mut out = [[0f32; 3]; 3];
        for (i, row) in out.iter_mut().enumerate() {
            for (j, v) in row.iter_mut().enumerate() {
                *v = (0..3).map(|k| self.rgb_cam[i][k] * r[k] * self.cam_from_srgb[k][j]).sum::<f64>() as f32;
            }
        }
        out
    }
}

/// The as-shot Kelvin and tint of an image, when its camera gives what Kelvin needs.
pub fn as_shot_kelvin(cam_mul: &[f32; 4], pre_mul: &[f32; 4], rgb_cam: &Mat3, wbct: &[[f32; 4]]) -> Option<(f32, f32)> {
    WbCamera::new(cam_mul, pre_mul, rgb_cam, wbct)?.as_shot()
}

/// The rendering transform slot (decision 5): plain sRGB, sRGB with a soft shoulder that
/// rolls highlights off instead of clipping them, the camera-style curve
/// ([`CAMERA_CURVE`]), or that curve after the camera colour matrix ([`CAMERA_MATRIX`]).
/// New engine-2 edits get `camera.2` (user decision 2026-09-24: the default should be
/// closer to the camera's picture style). Every name keeps its meaning once shipped — a
/// record without the field stays plain sRGB and a `camera` record never gains the matrix
/// — so a saved version renders the same forever.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DisplayTransform {
    Srgb,
    Soft { shoulder: f32 },
    /// `"camera"`: the curve alone.
    Camera,
    /// `"camera.2"`: [`CAMERA_MATRIX`] in linear light, then the curve.
    CameraColour,
}

impl DisplayTransform {
    /// From the record's `display` field: absent or `"srgb"` = sRGB; `"soft"` = the
    /// shoulder; `"camera"` = the camera-style curve; `"camera.2"` = matrix and curve.
    pub fn from_record(name: Option<&str>) -> Self {
        match name {
            Some("soft") => DisplayTransform::Soft { shoulder: 0.8 },
            Some("camera") => DisplayTransform::Camera,
            Some("camera.2") => DisplayTransform::CameraColour,
            _ => DisplayTransform::Srgb,
        }
    }
}

/// The camera colour matrix of `camera.2`, applied to linear RGB before the curve. Rows
/// sum to 1, so neutrals stay neutral. Fitted by `develop::camera_fit` on 2026-09-24 over
/// all eight corpus frames, each first brought to its own camera match, by coordinate
/// descent on the off-diagonals minimizing mean |Δ| to the camera JPEG. It turns the
/// decode's cyan-leaning blues toward the camera's violet and deepens blue: on the A7R VI
/// "Vivid" blue frame the error fell 4.7 → 2.1 levels; the others moved by −0.4..+0.5
/// (the DNG, 6.1 → 6.6, most); mean over eight 3.7 → 3.35. One blue scene carries most of
/// that evidence, and the Standard (A7 IV) and Vivid frames alone fit different matrices;
/// this is the compromise over both.
pub const CAMERA_MATRIX: [[f32; 3]; 3] = [
    [0.936, 0.055, 0.009],
    [-0.035, 1.073, -0.038],
    [0.047, -0.319, 1.272],
];

/// `px` through [`CAMERA_MATRIX`].
#[inline]
pub fn camera_matrix(px: [f32; 3]) -> [f32; 3] {
    let m = &CAMERA_MATRIX;
    [
        m[0][0] * px[0] + m[0][1] * px[1] + m[0][2] * px[2],
        m[1][0] * px[0] + m[1][1] * px[1] + m[1][2] * px[2],
        m[2][0] * px[0] + m[2][1] * px[1] + m[2][2] * px[2],
    ]
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
/// 8-bit. Values above display white clip here (Srgb) or roll off (Soft, the camera
/// transforms) — nowhere earlier. The camera transforms replace the sRGB curve with
/// [`CAMERA_CURVE`]; `CameraColour` applies [`CAMERA_MATRIX`] first.
pub fn to_display(img: &Rgb32FImage, t: DisplayTransform, baseline_ev: f32) -> RgbImage {
    let (w, h) = img.dimensions();
    let lift = 2f32.powf(baseline_ev);
    let mut out = RgbImage::new(w, h);
    if w == 0 || h == 0 {
        return out;
    }
    let row_w = w as usize;
    let lut = matches!(t, DisplayTransform::Camera | DisplayTransform::CameraColour).then(camera_lut);
    out.as_mut()
        .par_chunks_mut(row_w * 3)
        .zip(img.as_raw().par_chunks(row_w * 3))
        .for_each(|(dst, src)| {
            if let (Some(lut), DisplayTransform::CameraColour) = (lut, t) {
                for (d, s) in dst.chunks_exact_mut(3).zip(src.chunks_exact(3)) {
                    let v = camera_matrix([s[0] * lift, s[1] * lift, s[2] * lift]);
                    for c in 0..3 {
                        d[c] = (camera_lookup(lut, v[c].max(0.0)) * 255.0).round().clamp(0.0, 255.0) as u8;
                    }
                }
                return;
            }
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

/// The exposure offset, in EV, at which `lin` (a small linear image, as-shot) rendered
/// through the `camera.2` transform best matches the camera's own JPEG of the same frame in
/// brightness: mean |Δ| of luma over the centre 80 % (every other pixel), searched over
/// −3..+2 EV coarse to fine, to 0.01. It carries what a global curve cannot — the pull an
/// extended low ISO gets in the camera, a body's metering bias, a global part of DRO.
/// `None` when the two frames do not have the same shape (a crop in the camera JPEG, a
/// failed turn).
pub fn camera_match_ev(lin: &Rgb32FImage, camera: &RgbImage) -> Option<f32> {
    let (w, h) = lin.dimensions();
    let (cw, ch) = camera.dimensions();
    if w < 10 || h < 10 || cw == 0 || ch == 0 {
        return None;
    }
    let (a, b) = (w as f32 / h as f32, cw as f32 / ch as f32);
    if (a / b - 1.0).abs() > 0.03 {
        return None;
    }
    let (x0, y0, x1, y1) = (w / 10, h / 10, w - w / 10, h - h / 10);
    let mut px: Vec<([f32; 3], f32)> = Vec::with_capacity(((x1 - x0) * (y1 - y0)) as usize);
    // Every other pixel each way: a quarter of the work, and the match is a mean anyway.
    // The camera's luma for a pixel is the mean over the block of the JPEG it covers — a
    // box average by hand, because the `image` crate's generic resize runs unoptimized in
    // a debug build and cost most of a second here.
    // The JPEG's rows (or columns) under output row `i` of `n`: never empty, never past `cn`.
    let block = |i: u32, n: u32, cn: u32| {
        let (i, n, cn) = (i as u64, n as u64, cn as u64);
        let a = (i * cn / n).min(cn - 1);
        let b = ((i + 1) * cn).div_ceil(n).clamp(a + 1, cn);
        (a as u32, b as u32)
    };
    for y in (y0..y1).step_by(2) {
        let (cy0, cy1) = block(y, h, ch);
        for x in (x0..x1).step_by(2) {
            let (cx0, cx1) = block(x, w, cw);
            let (mut sum, mut n) = (0f32, 0f32);
            for cy in cy0..cy1 {
                for cx in cx0..cx1 {
                    let c = camera.get_pixel(cx, cy).0;
                    sum += 0.2126 * c[0] as f32 + 0.7152 * c[1] as f32 + 0.0722 * c[2] as f32;
                    n += 1.0;
                }
            }
            px.push((lin.get_pixel(x, y).0, sum / n / 255.0));
        }
    }
    // Measured through the transform new records get (`camera.2`); the matrix keeps
    // neutrals, so a `camera` record's match would differ only by its colour.
    let lut = camera_lut();
    let err = |ev: f32| -> f32 {
        let lift = 2f32.powf(BASELINE_EV + ev);
        let mut e = 0.0;
        for (s, l) in &px {
            let m = camera_matrix([s[0] * lift, s[1] * lift, s[2] * lift]);
            let v = [camera_lookup(lut, m[0].max(0.0)), camera_lookup(lut, m[1].max(0.0)), camera_lookup(lut, m[2].max(0.0))];
            e += (0.2126 * v[0] + 0.7152 * v[1] + 0.0722 * v[2] - l).abs();
        }
        e / px.len() as f32
    };
    let search = |from: f32, to: f32, step: f32| -> f32 {
        let n = ((to - from) / step).round() as i32;
        (0..=n)
            .map(|i| from + i as f32 * step)
            .min_by(|a, b| err(*a).partial_cmp(&err(*b)).unwrap())
            .unwrap()
    };
    // Coarse to fine (21 + 10 + 10 evaluations instead of 121): the error is a smooth
    // valley in EV, so each pass only has to bracket the previous one's minimum.
    let coarse = search(-3.0, 2.0, 0.25);
    let mid = search(coarse - 0.25, coarse + 0.25, 0.05);
    let fine = search(mid - 0.05, mid + 0.05, 0.01);
    Some((fine * 100.0).round() / 100.0)
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
    fn wb_spec_relative_zero_is_as_shot_and_kelvin_needs_a_daylight_white() {
        let cm = [2.0, 1.0, 1.5, 1.0];
        let m = IDENTITY;
        let none = [0.0; 4];
        assert_eq!(wb_matrix(&WbSpec::Relative { temp: 0.0, tint: 0.0 }, &cm, &none, &m, &[]).unwrap(), IDENTITY);
        let warm = wb_matrix(&WbSpec::Relative { temp: 0.5, tint: 0.0 }, &cm, &none, &m, &[]).unwrap();
        assert!(warm[0][0] > 1.0 && warm[2][2] < 1.0);
        let k = WbSpec::from_record(Some("kelvin"), 0.0, 0.0, Some(5400.0));
        assert_eq!(k, WbSpec::Kelvin { kelvin: 5400.0, tint: 0.0 });
        assert!(wb_matrix(&k, &cm, &none, &m, &[]).is_err(), "no daylight white: refused, never silently relative");
        assert_eq!(WbSpec::from_record(None, 0.3, -0.1, None), WbSpec::Relative { temp: 0.3, tint: -0.1 });
    }

    /// A camera like the corpus's (a Sony-shaped matrix and daylight multipliers) that
    /// shot under a light of `kelvin`/`tint`: its cam_mul is what balances that light.
    fn camera_under(kelvin: f32, tint: f32) -> ([f32; 4], [f32; 4], Mat3) {
        let rgb_cam: Mat3 = [[1.7, -0.6, -0.1], [-0.2, 1.5, -0.3], [0.0, -0.4, 1.4]];
        let pre_mul = [2.4, 1.0, 1.3, 1.0];
        let probe = WbCamera::new(&[1.0; 4], &[1.0; 4], &rgb_cam, &[]).unwrap();
        let u = probe.white(kelvin);
        let green = 2f64.powf(-tint as f64 / TINT_UNITS_PER_STOP);
        let a = [1.0 / u[0], green / u[1], 1.0 / u[2]];
        let cam_mul = [0, 1, 2, 1].map(|c| (a[c] / a[1]) as f32 * pre_mul[c]);
        (cam_mul, pre_mul, rgb_cam)
    }

    #[test]
    fn kelvin_locus_hits_known_points() {
        // Kim et al.: 6500 K ≈ (0.3135, 0.3237); 2856 K (illuminant A) ≈ (0.4476, 0.4074).
        let (x, y) = kelvin_xy(6500.0);
        assert!((x - 0.3135).abs() < 0.002 && (y - 0.3237).abs() < 0.002, "{x} {y}");
        let (x, y) = kelvin_xy(2856.0);
        assert!((x - 0.4476).abs() < 0.002 && (y - 0.4074).abs() < 0.002, "{x} {y}");
    }

    #[test]
    fn the_as_shot_light_is_recovered_and_rendering_it_is_the_identity() {
        for (k, t) in [(3200.0, 0.0), (5600.0, 8.0), (7500.0, -12.0)] {
            let (cm, pm, rc) = camera_under(k, t);
            let (ak, at) = as_shot_kelvin(&cm, &pm, &rc, &[]).unwrap();
            assert!((ak - k).abs() / k < 0.002, "kelvin {ak} for {k}");
            assert!((at - t).abs() < 0.2, "tint {at} for {t}");
            let m = wb_matrix(&WbSpec::Kelvin { kelvin: ak, tint: at }, &cm, &pm, &rc, &[]).unwrap();
            for (i, row) in m.iter().enumerate() {
                for (j, v) in row.iter().enumerate() {
                    let want = if i == j { 1.0 } else { 0.0 };
                    assert!((v - want).abs() < 1e-3, "as-shot is the identity: {m:?}");
                }
            }
        }
    }

    /// The A7 R VI's own table (WB_RGBLevels<N>K from `_DSC8191.ARW`).
    const A7RVI_TABLE: [[f32; 4]; 5] = [
        [2500.0, 1207.0, 1024.0, 3946.0],
        [3200.0, 1533.0, 1024.0, 2883.0],
        [4500.0, 1971.0, 1024.0, 2105.0],
        [6000.0, 2339.0, 1024.0, 1750.0],
        [8500.0, 2752.0, 1024.0, 1476.0],
    ];

    #[test]
    fn with_the_cameras_table_its_own_presets_read_as_their_temperatures() {
        let rc: Mat3 = [[1.7, -0.6, -0.1], [-0.2, 1.5, -0.3], [0.0, -0.4, 1.4]];
        let pm = [0.0; 4]; // no daylight white needed on this path
        for row in A7RVI_TABLE {
            let cm = [row[1], row[2], row[3], row[2]];
            let (k, t) = as_shot_kelvin(&cm, &pm, &rc, &A7RVI_TABLE).unwrap();
            assert!((k - row[0]).abs() / row[0] < 0.001, "{k} for {}", row[0]);
            assert!(t.abs() < 0.05, "on the table: no tint ({t})");
            let m = wb_matrix(&WbSpec::Kelvin { kelvin: k, tint: t }, &cm, &pm, &rc, &A7RVI_TABLE).unwrap();
            assert!((0..3).all(|i| (0..3).all(|j| (m[i][j] - if i == j { 1.0 } else { 0.0 }).abs() < 1e-4)), "{m:?}");
        }
        // The Daylight preset (2226/1024/1912) lies between the 4500 and 6000 K rows.
        let (k, _) = as_shot_kelvin(&[2226.0, 1024.0, 1912.0, 1024.0], &pm, &rc, &A7RVI_TABLE).unwrap();
        assert!((4500.0..6000.0).contains(&k), "daylight read as {k}");
    }

    #[test]
    fn a_higher_kelvin_warms_and_a_positive_tint_turns_magenta() {
        let (cm, pm, rc) = camera_under(5000.0, 0.0);
        let grey = |m: Mat3| {
            let mut img = Rgb32FImage::from_pixel(1, 1, image::Rgb([0.2; 3]));
            apply_exposure_linear(&mut img, 0.0, m);
            img.get_pixel(0, 0).0
        };
        let warm = grey(wb_matrix(&WbSpec::Kelvin { kelvin: 6500.0, tint: 0.0 }, &cm, &pm, &rc, &[]).unwrap());
        let cool = grey(wb_matrix(&WbSpec::Kelvin { kelvin: 3500.0, tint: 0.0 }, &cm, &pm, &rc, &[]).unwrap());
        assert!(warm[0] > warm[2], "telling it the light was bluer warms the picture: {warm:?}");
        assert!(cool[2] > cool[0], "and redder light cools it: {cool:?}");
        let magenta = grey(wb_matrix(&WbSpec::Kelvin { kelvin: 5000.0, tint: 30.0 }, &cm, &pm, &rc, &[]).unwrap());
        assert!(magenta[1] < magenta[0] && magenta[1] < magenta[2], "{magenta:?}");
    }

    #[test]
    fn linear_tone_ev_is_a_pure_multiply_and_headroom_survives() {
        let mut img = ramp();
        let before: Vec<f32> = img.as_raw().clone();
        apply_exposure_linear(&mut img, 1.0, IDENTITY);
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
        assert_eq!(DisplayTransform::from_record(Some("camera.2")), DisplayTransform::CameraColour);
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
    fn camera_matrix_keeps_neutrals_and_moves_blue_toward_violet() {
        for row in CAMERA_MATRIX {
            assert!((row.iter().sum::<f32>() - 1.0).abs() < 1e-6);
        }
        let grey = Rgb32FImage::from_pixel(1, 1, image::Rgb([0.1; 3]));
        let (a, b) = (to_display(&grey, DisplayTransform::CameraColour, 0.0), to_display(&grey, DisplayTransform::Camera, 0.0));
        assert_eq!(a.get_pixel(0, 0), b.get_pixel(0, 0), "a neutral is untouched by the matrix");
        let sky = Rgb32FImage::from_pixel(1, 1, image::Rgb([0.02, 0.06, 0.12]));
        let (c2, c1) = (to_display(&sky, DisplayTransform::CameraColour, 0.0), to_display(&sky, DisplayTransform::Camera, 0.0));
        let (c2, c1) = (c2.get_pixel(0, 0).0, c1.get_pixel(0, 0).0);
        // Violet leans: red up relative to green, blue deeper.
        assert!((c2[0] as i32 - c2[1] as i32) > (c1[0] as i32 - c1[1] as i32), "{c2:?} vs {c1:?}");
        assert!(c2[2] >= c1[2]);
    }

    #[test]
    fn camera_match_ev_recovers_a_known_offset_and_refuses_another_shape() {
        // A scene with shadows, midtones and a highlight; the "camera" rendered it a
        // stop and a quarter darker than the transform's as-shot render.
        let lin = Rgb32FImage::from_fn(96, 64, |x, y| {
            let v = 0.002 * 1.07f32.powi(x as i32) * (1.0 + y as f32 / 64.0);
            image::Rgb([v * 1.1, v, v * 0.8])
        });
        let camera = to_display(&lin, DisplayTransform::CameraColour, BASELINE_EV - 1.25);
        let ev = camera_match_ev(&lin, &camera).unwrap();
        assert!((ev + 1.25).abs() <= 0.02, "matched {ev}");
        // At the camera's own size too (the preview is larger than the 256 px copy).
        let big = image::imageops::resize(&camera, 192, 128, image::imageops::FilterType::Triangle);
        assert!((camera_match_ev(&lin, &big).unwrap() + 1.25).abs() <= 0.05);
        // A turned or cropped JPEG is not this frame.
        let turned = image::imageops::rotate90(&camera);
        assert_eq!(camera_match_ev(&lin, &turned), None);
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
        apply_exposure_linear(&mut bright, -1.0, IDENTITY);
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
