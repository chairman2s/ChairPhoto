//! The film-look stage of the render engine: the per-pixel pipeline that turns the
//! decoded (straightened/cropped/scaled) image into the version's final look — tone,
//! B&W channel mixing, split toning, fade, vignette — in a fixed order so a preview
//! render matches the export bit-for-bit. Every parameter is a no-op at its serde
//! default, so old edit records render unchanged.

use image::RgbImage;
use serde::Deserialize;

use super::cube::CubeLut;
use super::EditRecord;

/// Black & white conversion via a channel mixer. Classic contrast filters are just
/// weight recipes (red filter ≈ `{0.9, 0.15, -0.05}`); weights are normalized by their
/// sum (falling back to Rec.601 when the sum is ~0) so any recipe keeps overall
/// brightness in the same ballpark.
#[derive(Deserialize)]
#[serde(default)]
pub(super) struct Bw {
    pub enabled: bool,
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

impl Default for Bw {
    fn default() -> Self {
        // Rec.601 luma weights — a neutral B&W conversion.
        Bw { enabled: true, r: 0.299, g: 0.587, b: 0.114 }
    }
}

/// Split toning: tint shadows and highlights independently (sepia/selenium = the same
/// hue on both ends). Hues in degrees (0–360), saturations 0–1, balance -1..1 shifts
/// the shadow/highlight crossover point.
#[derive(Deserialize, Default)]
#[serde(default)]
pub(super) struct Split {
    pub shadow_hue: f32,
    pub shadow_sat: f32,
    pub highlight_hue: f32,
    pub highlight_sat: f32,
    pub balance: f32,
}

/// Film grain, applied last (grain sits "on the print"). Deterministic — the pattern
/// is a pure function of (position, seed), never time-derived — and evaluated in
/// normalized image space so a small preview and a full-size export show the same
/// grain, just resampled. `size` scales the grain frequency (0.5 fine … 3 coarse).
#[derive(Deserialize)]
#[serde(default)]
pub(super) struct Grain {
    pub amount: f32, // 0..1
    pub size: f32,   // 0.5..3, 1 = default
    pub seed: u32,
}

impl Default for Grain {
    fn default() -> Self {
        Grain { amount: 0.0, size: 1.0, seed: 0 }
    }
}

/// Apply the whole look — tone, then B&W mix, LUT, split toning, fade, vignette and
/// grain — to the already-framed image. The order is fixed: the B&W mixer must see
/// the *developed* colour image (a red filter acts on corrected channels), creative
/// LUTs are designed for a developed image, toning tints the final (possibly mono)
/// image, fade lifts last so the matte black isn't re-crushed, the vignette shades
/// the post-crop frame, and grain sits on top like grain on a print.
pub(super) fn apply_look(img: &mut RgbImage, edit: &EditRecord, lut: Option<&CubeLut>) {
    let t = &edit.tone;
    let bw = edit.bw.as_ref().filter(|b| b.enabled);
    let split = edit
        .split
        .as_ref()
        .filter(|s| s.shadow_sat != 0.0 || s.highlight_sat != 0.0);
    let grain = edit.grain.as_ref().filter(|g| g.amount > 0.0);
    let fade = edit.fade.clamp(0.0, 1.0);
    let vignette = edit.vignette.clamp(-1.0, 1.0);

    let tone_active = t.ev != 0.0
        || t.contrast != 0.0
        || t.highlights != 0.0
        || t.shadows != 0.0
        || t.whites != 0.0
        || t.blacks != 0.0
        || t.vibrance != 0.0
        || t.saturation != 0.0
        || t.wb.temp != 0.0
        || t.wb.tint != 0.0;
    if !tone_active
        && bw.is_none()
        && split.is_none()
        && grain.is_none()
        && lut.is_none()
        && fade == 0.0
        && vignette == 0.0
    {
        return;
    }

    let lut_amount = edit
        .lut
        .as_ref()
        .map(|l| l.amount.clamp(0.0, 1.0))
        .unwrap_or(1.0);

    let exposure = 2f32.powf(t.ev); // EV stops → linear gain
    // White balance as channel gains (gentle): warm raises R / lowers B, tint shifts G.
    let r_gain = 1.0 + 0.3 * t.wb.temp;
    let b_gain = 1.0 - 0.3 * t.wb.temp;
    let g_gain = 1.0 + 0.15 * t.wb.tint;
    let contrast = 1.0 + t.contrast; // pivot at mid-grey

    // B&W mixer weights, normalized so a recipe's overall brightness stays sane even
    // with negative components (e.g. a red filter subtracting blue).
    let bw_weights = bw.map(|b| {
        let sum = b.r + b.g + b.b;
        if sum.abs() < 1e-3 {
            [0.299, 0.587, 0.114]
        } else {
            [b.r / sum, b.g / sum, b.b / sum]
        }
    });

    // Split-toning tint directions, precomputed from the hues.
    let split_dirs = split.map(|s| (hue_rgb(s.shadow_hue), hue_rgb(s.highlight_hue)));

    let fade_k = fade * 0.15; // lifted black point: x = k + x*(1-k)

    let (w, h) = img.dimensions();
    // Normalize centre distance so the corner is ~1 regardless of aspect ratio.
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    let corner = (cx * cx + cy * cy).sqrt().max(1.0);
    // Grain frequency: a virtual grid of `freq` cells across the long edge, in
    // normalized space — the same pattern at any output resolution.
    let long_edge = w.max(h) as f32;
    let grain_freq = grain.map(|g| 1200.0 / g.size.clamp(0.5, 3.0));

    for (x, y, px) in img.enumerate_pixels_mut() {
        let mut c = [
            px[0] as f32 / 255.0,
            px[1] as f32 / 255.0,
            px[2] as f32 / 255.0,
        ];
        // Exposure + white balance.
        c[0] *= exposure * r_gain;
        c[1] *= exposure * g_gain;
        c[2] *= exposure * b_gain;
        // Tone-region adjustments use luminance as a soft mask.
        for v in c.iter_mut() {
            let mut x = v.clamp(0.0, 4.0);
            // Shadows: lift darks (weight high where x is low).
            if t.shadows != 0.0 {
                let w = (1.0 - x).clamp(0.0, 1.0);
                x += t.shadows * 0.5 * w * w;
            }
            // Highlights: tame/boost brights (weight high where x is high).
            if t.highlights != 0.0 {
                let w = x.clamp(0.0, 1.0);
                x += t.highlights * 0.5 * w * w;
            }
            // Whites: scale bright endpoint — positive pushes brights toward clipping,
            // negative pulls the top end down. Weight is a smoothstep toward the bright
            // end (mirrors how highlights is weighted).
            if t.whites != 0.0 {
                let w = x.clamp(0.0, 1.0);
                x += t.whites * 0.25 * smoothstep(w);
            }
            // Blacks: scale dark endpoint — negative crushes blacks, positive lifts them.
            // Weight is high where x is low (complement of whites weight).
            if t.blacks != 0.0 {
                let w = (1.0 - x).clamp(0.0, 1.0);
                x += t.blacks * 0.25 * smoothstep(w);
            }
            // Contrast around mid-grey.
            if t.contrast != 0.0 {
                x = (x - 0.5) * contrast + 0.5;
            }
            *v = x;
        }
        // Saturation: luma-preserving channel scale. Rec.601 luma coefficients.
        // saturation = -1 → greyscale; 0 → no-op; +1 → doubled chroma.
        if t.saturation != 0.0 || t.vibrance != 0.0 {
            let luma = luma601(&c);
            if t.saturation != 0.0 {
                let scale = 1.0 + t.saturation;
                for v in c.iter_mut() {
                    *v = luma + (*v - luma) * scale;
                }
            }
            // Vibrance: like saturation but weighted toward low-saturation pixels so
            // already-vivid colours are protected. s = (max-min)/max(max, ε).
            if t.vibrance != 0.0 {
                let cmax = c[0].max(c[1]).max(c[2]);
                let cmin = c[0].min(c[1]).min(c[2]);
                let s = (cmax - cmin) / cmax.max(1e-6);
                let factor = 1.0 + t.vibrance * (1.0 - s);
                // Re-read luma after saturation may have changed c.
                let luma2 = luma601(&c);
                for v in c.iter_mut() {
                    *v = luma2 + (*v - luma2) * factor;
                }
            }
        }
        // B&W channel mixer: collapse to a weighted luma (a colour filter's contrast
        // lives in the weights). Clamped low so negative weights can't go below black.
        if let Some(wts) = &bw_weights {
            let l = (wts[0] * c[0] + wts[1] * c[1] + wts[2] * c[2]).max(0.0);
            c = [l, l, l];
        }
        // 3D LUT: sample the (clamped) developed colour, blend by amount.
        if let Some(l) = lut {
            let inp = [
                c[0].clamp(0.0, 1.0),
                c[1].clamp(0.0, 1.0),
                c[2].clamp(0.0, 1.0),
            ];
            let out = l.sample(inp);
            for i in 0..3 {
                c[i] += (out[i] - c[i]) * lut_amount;
            }
        }
        // Split toning: push shadows/highlights toward their tint colours, weighted by
        // a smoothstep over luma (shifted by balance).
        if let Some(s) = split {
            let (sh_dir, hi_dir) = split_dirs.as_ref().unwrap();
            let l = (luma601(&c) + s.balance * 0.25).clamp(0.0, 1.0);
            let w_hi = smoothstep(l);
            let w_sh = 1.0 - w_hi;
            for (i, v) in c.iter_mut().enumerate() {
                *v += w_sh * s.shadow_sat * 0.25 * (sh_dir[i] - 0.5)
                    + w_hi * s.highlight_sat * 0.25 * (hi_dir[i] - 0.5);
            }
        }
        // Fade: lift the black point for the matte film look.
        if fade_k > 0.0 {
            for v in c.iter_mut() {
                *v = fade_k + *v * (1.0 - fade_k);
            }
        }
        // Vignette: radial shading toward the corners (negative darkens).
        if vignette != 0.0 {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            let d = (dx * dx + dy * dy).sqrt() / corner;
            let wv = smoothstep(((d - 0.25) / 0.75).clamp(0.0, 1.0));
            let gain = 1.0 + vignette * 0.7 * wv;
            for v in c.iter_mut() {
                *v *= gain;
            }
        }
        // Grain: luminance-weighted value noise (lives in the midtones, like film).
        if let Some(g) = grain {
            let freq = grain_freq.unwrap();
            let u = (x as f32 + 0.5) / long_edge * freq;
            let v = (y as f32 + 0.5) / long_edge * freq;
            let n = value_noise(u, v, g.seed);
            let l = luma601(&c).clamp(0.0, 1.0);
            let amp = g.amount.clamp(0.0, 1.0) * 0.12 * 4.0 * l * (1.0 - l);
            for ch in c.iter_mut() {
                *ch += n * amp;
            }
        }
        px[0] = (c[0] * 255.0).round().clamp(0.0, 255.0) as u8;
        px[1] = (c[1] * 255.0).round().clamp(0.0, 255.0) as u8;
        px[2] = (c[2] * 255.0).round().clamp(0.0, 255.0) as u8;
    }
}

/// Value noise in [-1, 1]: bilinear interpolation of a hash at the four surrounding
/// integer grid corners. Smooth enough to look like grain clumps rather than
/// per-pixel static once the grid is coarser than the pixel grid.
fn value_noise(u: f32, v: f32, seed: u32) -> f32 {
    let (iu, iv) = (u.floor(), v.floor());
    let (fu, fv) = (u - iu, v - iv);
    let (x0, y0) = (iu as i64, iv as i64);
    let corner = |dx: i64, dy: i64| hash2(x0 + dx, y0 + dy, seed);
    let top = corner(0, 0) * (1.0 - fu) + corner(1, 0) * fu;
    let bot = corner(0, 1) * (1.0 - fu) + corner(1, 1) * fu;
    top * (1.0 - fv) + bot * fv
}

/// Integer hash of a grid corner → f32 in [-1, 1]. Deterministic across runs and
/// platforms (pure integer mixing, xorshift-multiply style).
fn hash2(x: i64, y: i64, seed: u32) -> f32 {
    let mut h = (x as u32)
        .wrapping_mul(0x9E37_79B1)
        .wrapping_add((y as u32).wrapping_mul(0x85EB_CA77))
        .wrapping_add(seed.wrapping_mul(0xC2B2_AE3D));
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    h = h.wrapping_mul(0x2971_2D39);
    h ^= h >> 15;
    (h as f32 / u32::MAX as f32) * 2.0 - 1.0
}

fn luma601(c: &[f32; 3]) -> f32 {
    0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2]
}

fn smoothstep(x: f32) -> f32 {
    x * x * (3.0 - 2.0 * x)
}

/// A hue (degrees) as a fully-saturated RGB colour — the tint direction used by split
/// toning (sepia ≈ 35°, selenium ≈ 275°).
fn hue_rgb(h_deg: f32) -> [f32; 3] {
    let h = (h_deg.rem_euclid(360.0)) / 60.0;
    let x = 1.0 - ((h % 2.0) - 1.0).abs();
    match h as u32 {
        0 => [1.0, x, 0.0],
        1 => [x, 1.0, 0.0],
        2 => [0.0, 1.0, x],
        3 => [0.0, x, 1.0],
        4 => [x, 0.0, 1.0],
        _ => [1.0, 0.0, x],
    }
}
