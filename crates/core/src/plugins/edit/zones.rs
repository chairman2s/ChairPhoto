//! Tone-strip zones (docs/plans/darkroom): per-EV-zone exposure offsets rendered as a
//! per-luma gain curve, plus the zone-mass histogram that fills the strip.
//!
//! Eight zones split the gamma-luma axis into equal bands, blacks→whites. A zone's
//! offset is an EV nudge for pixels whose luma lives in that band; between zone centres
//! the offset is cosine-interpolated so neighbouring zones feather into each other and
//! the resulting curve has no steps.

use image::RgbImage;

const ZONES: usize = 8;

/// 256-entry per-luma gain LUT from 8 zone EV offsets (blacks→whites).
///
/// All-zero zones produce the exact identity (interpolated EV is exactly 0, and
/// 2⁰ = 1.0), which keeps old records rendering bit-identically — locked by the
/// tests below.
pub(super) fn zone_gain_lut(zones: &[f32; ZONES]) -> [f32; 256] {
    let mut lut = [1.0f32; 256];
    for (i, g) in lut.iter_mut().enumerate() {
        let l = i as f32 / 255.0;
        // Continuous zone coordinate: zone centres sit at (z + 0.5)/8, so p = 0 at the
        // centre of the blacks zone and 7 at the centre of the whites zone; beyond the
        // outermost centres the outer zone's offset holds flat.
        let p = (l * ZONES as f32 - 0.5).clamp(0.0, (ZONES - 1) as f32);
        let i0 = p.floor() as usize;
        let i1 = (i0 + 1).min(ZONES - 1);
        let t = p - i0 as f32;
        // Cosine feathering between the two nearest zone centres.
        let s = (1.0 - (t * std::f32::consts::PI).cos()) * 0.5;
        let ev = zones[i0] * (1.0 - s) + zones[i1] * s;
        *g = 2f32.powf(ev);
    }
    lut
}

/// Share of pixels per zone — 8 equal gamma-luma bands, Rec. 709 luma — subsampled to
/// at most ~1M samples (the same sampling `export::luma_histogram` uses). Sums to ~1.0
/// for a non-empty image; all zeros for an empty one.
pub fn zone_masses(img: &RgbImage) -> [f32; ZONES] {
    let (w, h) = img.dimensions();
    let mut out = [0f32; ZONES];
    if w == 0 || h == 0 {
        return out;
    }
    let step = (((w as u64 * h as u64) as f64 / 1_000_000.0).sqrt().ceil() as u32).max(1);
    let mut counts = [0u64; ZONES];
    let mut total = 0u64;
    let mut y = 0;
    while y < h {
        let mut x = 0;
        while x < w {
            let p = img.get_pixel(x, y).0;
            let luma = (2126 * p[0] as u32 + 7152 * p[1] as u32 + 722 * p[2] as u32) / 10000;
            counts[(luma as usize * ZONES / 256).min(ZONES - 1)] += 1;
            total += 1;
            x += step;
        }
        y += step;
    }
    for (o, c) in out.iter_mut().zip(counts) {
        *o = c as f32 / total as f32;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_jpeg() -> Vec<u8> {
        // A small gradient with all three channels in play.
        let img = image::RgbImage::from_fn(64, 48, |x, y| {
            image::Rgb([(x * 4) as u8, (y * 5) as u8, ((x + y) * 2) as u8])
        });
        crate::plugins::edit::encode_jpeg(&image::DynamicImage::ImageRgb8(img), 95).unwrap()
    }

    #[test]
    fn zones_all_zero_is_identity() {
        assert!(zone_gain_lut(&[0.0; 8]).iter().all(|&g| g == 1.0));
    }

    #[test]
    fn zone_lift_raises_only_its_band() {
        let mut zones = [0.0; 8];
        zones[2] = 1.0; // "shadows"
        let lut = zone_gain_lut(&zones);
        // At the zone's centre (luma ≈ 0.3125 → index 80) the full +1 EV applies.
        assert!((lut[80] - 2.0).abs() < 0.05, "centre gain {}", lut[80]);
        // Feathering reaches only the neighbouring zone centres; beyond them, identity.
        assert_eq!(lut[10], 1.0, "deep blacks untouched");
        assert_eq!(lut[240], 1.0, "whites untouched");
        // Inside the feather toward zone 1 the gain is between 1 and 2.
        assert!(lut[60] > 1.0 && lut[60] < 2.0, "feather gain {}", lut[60]);
    }

    #[test]
    fn zone_masses_sum_to_one() {
        let img = image::RgbImage::from_fn(100, 80, |x, y| {
            image::Rgb([(x * 2) as u8, (y * 3) as u8, 128])
        });
        let sum: f32 = zone_masses(&img).iter().sum();
        assert!((sum - 1.0).abs() < 1e-4, "masses sum {sum}");
    }

    #[test]
    fn zone_masses_of_gradient_spread_evenly() {
        // A neutral 0..255 ramp puts exactly an eighth of the pixels in each band
        // (Rec. 709 of (x, x, x) is x).
        let img = image::RgbImage::from_fn(256, 4, |x, _| image::Rgb([x as u8, x as u8, x as u8]));
        for (i, m) in zone_masses(&img).iter().enumerate() {
            assert!((m - 0.125).abs() < 0.01, "band {i} mass {m}");
        }
    }

    #[test]
    fn zeroed_zones_render_byte_identical_to_no_zones() {
        let jpeg = synthetic_jpeg();
        let without = crate::plugins::edit::render_jpeg(&jpeg, r#"{"tone":{"ev":0.2}}"#, 0).unwrap();
        let with = crate::plugins::edit::render_jpeg(
            &jpeg,
            r#"{"tone":{"ev":0.2},"zones":[0,0,0,0,0,0,0,0]}"#,
            0,
        )
        .unwrap();
        assert_eq!(without, with, "a zeroed zone strip must not change a render");
    }

    #[test]
    fn v1_record_without_zones_parses_and_renders() {
        let jpeg = synthetic_jpeg();
        let v1 = r#"{"crop":{"x":0.1,"y":0.1,"w":0.5,"h":0.5},"tone":{"ev":0.5}}"#;
        assert!(crate::plugins::edit::render_jpeg(&jpeg, v1, 0).is_ok());
    }

    #[test]
    fn decode_proxy_cached_matches_a_plain_decode_and_never_crosses_photos() {
        let a = synthetic_jpeg();
        let b = {
            let img = image::RgbImage::from_fn(32, 32, |x, y| image::Rgb([(x + y) as u8; 3]));
            crate::plugins::edit::encode_jpeg(&image::DynamicImage::ImageRgb8(img), 95).unwrap()
        };
        let direct_a = image::load_from_memory(&a).unwrap().to_rgb8();
        // Twice through the cache (miss, then hit) — both must equal the plain decode.
        for _ in 0..2 {
            let cached = crate::plugins::edit::decode_proxy_cached(&a).unwrap().to_rgb8();
            assert_eq!(cached.as_raw(), direct_a.as_raw());
        }
        // A different JPEG must never be served the cached pixels.
        let direct_b = image::load_from_memory(&b).unwrap().to_rgb8();
        let cached_b = crate::plugins::edit::decode_proxy_cached(&b).unwrap().to_rgb8();
        assert_eq!(cached_b.as_raw(), direct_b.as_raw());
    }
}
