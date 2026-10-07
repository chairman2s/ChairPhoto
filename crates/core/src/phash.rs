//! Perceptual hash core (H15a) — a 64-bit **dHash** (difference hash) and a
//! Hamming-distance helper. Pure pixel math: no I/O, no catalog, no crates beyond
//! the `image` types the rest of the app already uses.
//!
//! ## Why dHash (and not aHash / pHash-DCT)
//!
//! dHash is the difference hash: reduce the image to a tiny 9×8 grayscale grid, then
//! set one bit per adjacent-pixel comparison along each row (9 columns → 8 comparisons
//! × 8 rows = 64 bits). It encodes *gradients* rather than absolute brightness, so it is
//! naturally robust to global exposure/brightness shifts — exactly the burst case we
//! care about (H15: same subject, half a stop brighter). It is also cheaper and needs no
//! DCT. The 32×32-DCT pHash was evaluated as an alternative; on the H15 target case
//! (small exposure/crop shifts within a burst) dHash separates same-scene from
//! different-scene cleanly (see the unit tests), so we ship the simpler hash. The public
//! surface is a plain `u64` + [`hamming_distance`], so swapping the internal algorithm
//! later would not change callers.
//!
//! ## Reusable core infra (not the `ai` plugin)
//!
//! The hash lives in core because it has several consumers beyond AI burst grouping:
//! near-duplicate detection and auto-stacks (design doc, `ai-tagging.md` §Burst grouping).
//! It is computed once from the cached preview/thumbnail decode and stored on
//! `photos.phash`; consumers read the column and compare with [`hamming_distance`].

use image::{imageops::FilterType, DynamicImage, GenericImageView};

/// Width of the reduced grid. One extra column vs. [`DHASH_H`] so each row yields
/// [`DHASH_H`] adjacent-pixel comparisons.
const DHASH_W: u32 = 9;
/// Height of the reduced grid. Rows × (columns − 1) = 8 × 8 = 64 comparison bits.
const DHASH_H: u32 = 8;

/// Compute the 64-bit dHash of an already-decoded image.
///
/// Steps: convert to grayscale → resize to a 9×8 grid (a fast box/triangle downscale;
/// the input resolution is irrelevant since we crush it to 72 pixels either way) → for
/// each of the 8 rows, compare the 8 adjacent horizontal pairs; bit = 1 when the left
/// pixel is brighter than the right. Bits are packed MSB-first in raster order, giving a
/// stable `u64` that is identical for identical images and differs by a small Hamming
/// distance for visually similar ones.
pub fn dhash(img: &DynamicImage) -> u64 {
    // Grayscale + tiny resize. `Triangle` gives a smooth average that is stable to minor
    // crops/scale changes (a nearest-neighbour resize would be jittery). The luma
    // conversion happens first so we resize a single channel.
    let small = DynamicImage::ImageLuma8(img.to_luma8()).resize_exact(
        DHASH_W,
        DHASH_H,
        FilterType::Triangle,
    );
    dhash_from_grid(&small)
}

/// Core bit-packing over an already-reduced 9×8 (or larger) grayscale image. Split out so
/// tests can feed a hand-built grid directly. Reads luma from the first channel.
fn dhash_from_grid(small: &DynamicImage) -> u64 {
    let mut hash: u64 = 0;
    let mut bit = 0u32;
    for y in 0..DHASH_H {
        for x in 0..DHASH_H {
            // Compare pixel (x, y) with its right neighbour (x+1, y).
            let left = small.get_pixel(x, y).0[0];
            let right = small.get_pixel(x + 1, y).0[0];
            if left > right {
                hash |= 1u64 << (63 - bit);
            }
            bit += 1;
        }
    }
    hash
}

/// Hamming distance between two 64-bit perceptual hashes: the number of differing bits
/// (0 = identical, 64 = fully inverted). This is the "how similar" metric — smaller means
/// more visually alike. Callers threshold it (H15b picks the burst-split threshold).
#[inline]
pub fn hamming_distance(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, Rgb, RgbImage};

    /// A deterministic test image whose **large-scale** structure depends on `seed`, so it
    /// survives the crush to a 9×8 grid (a fine, high-frequency pattern would average out to
    /// a flat field and collide). Each seed picks distinct low-frequency spatial phases for a
    /// pair of sinusoids, giving genuinely different reduced grids for different seeds while a
    /// smooth image the dHash can still track under exposure/crop shifts.
    fn scene(w: u32, h: u32, seed: u32) -> DynamicImage {
        let mut img = RgbImage::new(w, h);
        // Low spatial frequencies (a few cycles across the frame) chosen from the seed.
        let fx = 1.0 + (seed % 3) as f32; // 1..=3 cycles horizontally
        let fy = 1.0 + ((seed / 3) % 3) as f32; // 1..=3 cycles vertically
        let phase = seed as f32 * 0.9;
        for y in 0..h {
            for x in 0..w {
                let nx = x as f32 / w as f32;
                let ny = y as f32 / h as f32;
                let s = (nx * fx * std::f32::consts::TAU + phase).sin()
                    + (ny * fy * std::f32::consts::TAU - phase).sin();
                // Map [-2, 2] → [0, 255].
                let v = ((s + 2.0) * 63.75).clamp(0.0, 255.0) as u8;
                img.put_pixel(x, y, Rgb([v, v, v]));
            }
        }
        DynamicImage::ImageRgb8(img)
    }

    /// Apply a uniform brightness offset (simulate an exposure shift), clamped.
    fn brighten(img: &DynamicImage, delta: i32) -> DynamicImage {
        let mut out = img.to_rgb8();
        for p in out.pixels_mut() {
            for c in 0..3 {
                p.0[c] = (p.0[c] as i32 + delta).clamp(0, 255) as u8;
            }
        }
        DynamicImage::ImageRgb8(out)
    }

    /// Crop a few percent off each edge and rescale back (simulate a small reframe).
    fn small_crop(img: &DynamicImage) -> DynamicImage {
        let (w, h) = img.dimensions();
        let dx = w / 20; // 5% inset per side
        let dy = h / 20;
        img.crop_imm(dx, dy, w - 2 * dx, h - 2 * dy)
            .resize_exact(w, h, FilterType::Triangle)
    }

    #[test]
    fn identical_image_distance_zero() {
        let a = scene(400, 300, 1);
        let h1 = dhash(&a);
        let h2 = dhash(&a.clone());
        assert_eq!(h1, h2, "same image must hash identically");
        assert_eq!(hamming_distance(h1, h2), 0);
    }

    #[test]
    fn exposure_shift_is_small_distance() {
        let a = scene(400, 300, 1);
        // dHash encodes gradients, so a uniform brightness change should barely move it.
        let brighter = brighten(&a, 25);
        let d = hamming_distance(dhash(&a), dhash(&brighter));
        assert!(d <= 8, "exposure shift should be a small distance, got {d}");
    }

    #[test]
    fn small_crop_is_small_distance() {
        let a = scene(400, 300, 1);
        let cropped = small_crop(&a);
        let d = hamming_distance(dhash(&a), dhash(&cropped));
        assert!(d <= 12, "small crop/reframe should be a small distance, got {d}");
    }

    #[test]
    fn unrelated_images_large_distance() {
        // Two structurally different scenes must be far apart in Hamming space.
        let a = scene(400, 300, 1);
        let b = scene(400, 300, 9);
        let d = hamming_distance(dhash(&a), dhash(&b));
        assert!(d >= 18, "unrelated images should be a large distance, got {d}");
    }

    #[test]
    fn resolution_invariant() {
        // The same scene decoded at two resolutions (e.g. 256px thumbnail vs 2048px
        // preview) must produce nearly the same hash, since the analyzer hook may fire on
        // either. A couple of bits of jitter from resampling is acceptable.
        let big = scene(2048, 1536, 3);
        let small = big.resize_exact(256, 192, FilterType::Triangle);
        let d = hamming_distance(dhash(&big), dhash(&small));
        assert!(d <= 6, "same scene at different resolutions should barely differ, got {d}");
    }

    #[test]
    fn hamming_basic() {
        assert_eq!(hamming_distance(0, 0), 0);
        assert_eq!(hamming_distance(0, u64::MAX), 64);
        assert_eq!(hamming_distance(0b1011, 0b0001), 2);
    }

    #[test]
    fn dhash_grid_packs_first_bit_msb() {
        // Build a 9×8 grid whose first row is a strict left→right descent (each left pixel
        // brighter than its right neighbour) → every bit of the first row is 1; the rest 0.
        let mut g = image::GrayImage::new(DHASH_W, DHASH_H);
        for x in 0..DHASH_W {
            let v = (255 - x * 20) as u8; // strictly decreasing across the row
            g.put_pixel(x, 0, image::Luma([v]));
        }
        let h = dhash_from_grid(&DynamicImage::ImageLuma8(g));
        // First 8 bits (MSB-first) set, remaining 56 clear.
        assert_eq!(h, 0xFF00_0000_0000_0000);
    }
}
