//! Tiled sharpness scoring — the pure pixel-math core of H16 (out-of-focus culling).
//!
//! # Why tiled, not global (the bokeh trap)
//!
//! The textbook focus metric — variance of the Laplacian over the whole image —
//! measures *average* sharpness. A tack-sharp bird on creamy bokeh has a large soft
//! area, so its global score sits *below* a boring everything-in-focus snapshot: the
//! best shallow-DOF keepers get flagged as soft. This app's audience shoots exactly
//! those, so a global score is worse than useless here.
//!
//! Instead we cut the (grayscale) image into an 8×8 grid, compute the Laplacian
//! variance of each tile independently, and take a **top-percentile** tile as the
//! photo's score — "is anything sharp *anywhere*?". A sharp subject on a blurred
//! background lights up its handful of tiles and scores high; a genuinely soft frame
//! (motion blur, focus hunt, whole frame missed) has no sharp tile and scores low.
//!
//! This module is a **pure function**: it takes image bytes / a `DynamicImage` and
//! returns a number. No I/O, no catalog, no crates beyond `image` (already a dep).
//! It is the shared helper H15b (burst representative selection) and H16b (the index
//! job) consume — whichever ships first wires it in. Storage, thresholds, methods
//! (face-/AF-aware), and the async index job all live elsewhere; this is just the math.

use image::{DynamicImage, GrayImage};

/// The tile grid is `GRID × GRID`. 8×8 = 64 tiles: fine enough that a subject
/// occupying a small fraction of the frame still owns several whole tiles, coarse
/// enough that each tile has plenty of pixels for a stable variance even on a small
/// preview. Matches the design doc's "8×8 grid".
pub const GRID: u32 = 8;

/// Which tile (by rank, sharpest-first) becomes the photo's score. We want "roughly
/// the 90th-percentile tile" per the design — sharp enough to reject a fully-soft
/// frame, but not the single sharpest tile (that would reward a lone specular
/// glint / hot pixel / JPEG block and be noisy). With 64 tiles the 90th percentile
/// sits ~6 tiles down from the top; see [`aggregate`].
const TOP_PERCENTILE: f64 = 0.90;

/// Score a decoded image's sharpness. Higher = sharper *somewhere*.
///
/// Grayscale → 8×8 tiles → Laplacian variance per tile → ~90th-percentile tile.
/// Returns `0.0` for a degenerate image too small to tile (no meaningful signal).
/// Never panics.
pub fn score_image(img: &DynamicImage) -> f64 {
    score_gray(&img.to_luma8())
}

/// Score already-grayscale pixels. The workhorse; [`score_image`] is a thin wrapper
/// so callers that already hold a `GrayImage` (or synthesize one, as the tests do)
/// skip a redundant luma conversion.
pub fn score_gray(gray: &GrayImage) -> f64 {
    let variances = tile_variances(gray);
    aggregate(&variances)
}

/// Laplacian variance of every `GRID × GRID` tile, row-major. Tiles that are too
/// small to hold an interior Laplacian pixel contribute `0.0` (they carry no signal
/// rather than being dropped, so the grid length is always `GRID*GRID` when the
/// image tiles at all). An image smaller than the grid yields an empty vec.
fn tile_variances(gray: &GrayImage) -> Vec<f64> {
    let (w, h) = gray.dimensions();
    // Need at least one pixel per tile column/row, plus a 1px border for the
    // 3×3 Laplacian kernel to have interior pixels. If the image can't host that,
    // there is nothing to measure.
    if w < GRID + 2 || h < GRID + 2 {
        return Vec::new();
    }

    let mut out = Vec::with_capacity((GRID * GRID) as usize);
    for ty in 0..GRID {
        // Split the image evenly; the last tile absorbs the remainder so every
        // pixel is covered exactly once regardless of divisibility.
        let y0 = ty * h / GRID;
        let y1 = (ty + 1) * h / GRID;
        for tx in 0..GRID {
            let x0 = tx * w / GRID;
            let x1 = (tx + 1) * w / GRID;
            out.push(laplacian_variance(gray, x0, y0, x1, y1));
        }
    }
    out
}

/// Variance of the discrete Laplacian over the tile `[x0,x1) × [y0,y1)`.
///
/// The Laplacian (4-neighbour: `4·c − up − down − left − right`) is a second-order
/// edge response: near-zero in flat/soft regions, large across crisp edges. Its
/// *variance* over a tile is the classic "focus measure" — a sharp tile has a wide
/// spread of edge responses, a blurred one a narrow one. Only interior pixels of the
/// tile (those with all four neighbours *inside the tile*) are evaluated, so tiles
/// never reach across their own borders and stay independent. Tiles with fewer than
/// two interior pixels return `0.0`.
///
/// `pub(crate)` so the region-aware scorers (H16c: face-box / AF-point) can reuse the
/// exact same focus measure over an arbitrary sub-rectangle. The score of a face box
/// or the AF neighbourhood is *directly comparable* to a tile score because it is the
/// identical metric — only the rectangle differs.
pub(crate) fn laplacian_variance(gray: &GrayImage, x0: u32, y0: u32, x1: u32, y1: u32) -> f64 {
    // Interior = one pixel in from each tile edge (the kernel radius).
    if x1 < x0 + 3 || y1 < y0 + 3 {
        return 0.0;
    }
    let mut n: u64 = 0;
    let mut sum = 0.0_f64;
    let mut sum_sq = 0.0_f64;

    for y in (y0 + 1)..(y1 - 1) {
        for x in (x0 + 1)..(x1 - 1) {
            let c = gray.get_pixel(x, y)[0] as f64;
            let up = gray.get_pixel(x, y - 1)[0] as f64;
            let down = gray.get_pixel(x, y + 1)[0] as f64;
            let left = gray.get_pixel(x - 1, y)[0] as f64;
            let right = gray.get_pixel(x + 1, y)[0] as f64;
            let lap = 4.0 * c - up - down - left - right;
            n += 1;
            sum += lap;
            sum_sq += lap * lap;
        }
    }

    if n < 2 {
        return 0.0;
    }
    let nf = n as f64;
    let mean = sum / nf;
    // Population variance E[x²] − E[x]²; clamp tiny negatives from float error.
    (sum_sq / nf - mean * mean).max(0.0)
}

/// Collapse per-tile variances into one score: the `TOP_PERCENTILE`-th tile,
/// sharpest-first. Concretely: sort ascending, then pick the tile whose rank sits at
/// the 90th percentile from the *bottom* (10th from the top). This is deliberately
/// **not** the max — a single freak-sharp tile (specular highlight, sensor noise, a
/// JPEG block edge) shouldn't define the score. Taking a tile a little below the peak
/// makes the score reflect "a coherent sharp region exists", and — because it's a
/// rank, not an average — it is invariant to how many *soft* tiles surround the
/// subject, which is the whole point of the tiled approach.
///
/// Empty input → `0.0`.
fn aggregate(variances: &[f64]) -> f64 {
    if variances.is_empty() {
        return 0.0;
    }
    let mut sorted = variances.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    // Rank index at the given percentile (nearest-rank on `len-1`). For 64 tiles this
    // lands on index 56 → the 8th-sharpest tile (~top eighth), a stable "is anything
    // sharp here" signal. For small grids it degrades gracefully toward the max.
    let last = sorted.len() - 1;
    let idx = (TOP_PERCENTILE * last as f64).round() as usize;
    sorted[idx.min(last)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{imageops, GrayImage, Luma};

    /// A high-contrast checkerboard: maximally sharp everywhere (every pixel is an
    /// edge). `cell` is the square size in pixels.
    fn checkerboard(w: u32, h: u32, cell: u32) -> GrayImage {
        GrayImage::from_fn(w, h, |x, y| {
            let on = ((x / cell) + (y / cell)) % 2 == 0;
            Luma([if on { 255u8 } else { 0u8 }])
        })
    }

    /// A flat mid-gray field: no edges anywhere → minimally sharp.
    fn flat(w: u32, h: u32) -> GrayImage {
        GrayImage::from_pixel(w, h, Luma([128u8]))
    }

    /// Gaussian-blur a grayscale image (softens every edge). Uses the `image`
    /// crate's blur, already a dependency.
    fn blur(g: &GrayImage, sigma: f32) -> GrayImage {
        imageops::blur(g, sigma)
    }

    /// A sharp checkerboard patch dropped onto an otherwise blurred/flat background —
    /// the shallow-DOF "subject on bokeh" case. `patch` is the side of the centered
    /// sharp square.
    fn sharp_patch_on_soft(w: u32, h: u32, patch: u32) -> GrayImage {
        // Start from a blurred checkerboard so the background carries *some* soft
        // texture (like real bokeh), not a dead flat field.
        let mut base = blur(&checkerboard(w, h, 6), 4.0);
        let px0 = w / 2 - patch / 2;
        let py0 = h / 2 - patch / 2;
        let sharp = checkerboard(patch, patch, 2);
        for y in 0..patch {
            for x in 0..patch {
                base.put_pixel(px0 + x, py0 + y, *sharp.get_pixel(x, y));
            }
        }
        base
    }

    #[test]
    fn sharp_checkerboard_beats_its_blur() {
        let sharp = checkerboard(256, 256, 4);
        let soft = blur(&sharp, 3.0);
        let s_sharp = score_gray(&sharp);
        let s_soft = score_gray(&soft);
        assert!(
            s_sharp > s_soft,
            "sharp checkerboard ({s_sharp}) should score above its blurred copy ({s_soft})"
        );
    }

    #[test]
    fn flat_field_scores_near_zero() {
        // No edges → Laplacian is zero everywhere → variance zero.
        assert_eq!(score_gray(&flat(200, 200)), 0.0);
    }

    #[test]
    fn sharp_patch_on_blur_beats_fully_blurred() {
        // The bokeh trap: a small sharp subject on a soft field must beat a frame
        // that is soft everywhere. A *global* Laplacian variance would get this
        // wrong (the fully-blurred-but-textured frame can have higher average
        // response than a mostly-soft frame); the tiled top-percentile gets it right.
        let subject = sharp_patch_on_soft(256, 256, 48);
        let all_soft = blur(&checkerboard(256, 256, 6), 4.0);
        let s_subject = score_gray(&subject);
        let s_soft = score_gray(&all_soft);
        assert!(
            s_subject > s_soft,
            "sharp subject on bokeh ({s_subject}) must beat fully-blurred ({s_soft})"
        );
    }

    #[test]
    fn tiny_sharp_patch_survives_mostly_soft_frame() {
        // Even a subject occupying a small fraction of the frame — where a global
        // average would be dominated by the soft majority — must still register as
        // sharp because it owns whole tiles.
        let subject = sharp_patch_on_soft(320, 320, 40);
        let all_soft = blur(&checkerboard(320, 320, 6), 5.0);
        assert!(
            score_gray(&subject) > score_gray(&all_soft),
            "a small sharp subject should still outscore a fully-soft frame"
        );
    }

    #[test]
    fn ordering_stable_across_sizes() {
        // The same scene at different resolutions must keep sharp > soft. The score
        // is a rank of tile variances, so absolute values shift with size, but the
        // *ordering* between a sharp frame and its blur must hold at every size.
        for &(w, h) in &[(128u32, 128u32), (256, 256), (512, 384), (800, 600)] {
            let sharp = checkerboard(w, h, 4);
            let soft = blur(&sharp, 3.0);
            let s = score_gray(&sharp);
            let b = score_gray(&soft);
            assert!(
                s > b,
                "sharp>soft must hold at {w}x{h}: sharp={s}, soft={b}"
            );
        }
    }

    #[test]
    fn sharp_patch_ordering_stable_across_sizes() {
        // The bokeh-trap ordering (subject-on-soft > fully-soft) must also be
        // size-stable, not an artefact of one resolution.
        for &(w, h) in &[(160u32, 160u32), (256, 256), (400, 300), (640, 480)] {
            let patch = (w.min(h) / 5).max(24);
            let subject = sharp_patch_on_soft(w, h, patch);
            let all_soft = blur(&checkerboard(w, h, 6), 4.0);
            assert!(
                score_gray(&subject) > score_gray(&all_soft),
                "subject-on-soft > fully-soft must hold at {w}x{h}"
            );
        }
    }

    #[test]
    fn score_image_matches_score_gray() {
        // The DynamicImage entry point must agree with the gray workhorse.
        let g = checkerboard(200, 200, 5);
        let dynimg = DynamicImage::ImageLuma8(g.clone());
        assert_eq!(score_image(&dynimg), score_gray(&g));
    }

    #[test]
    fn degenerate_small_image_is_zero_not_panic() {
        // Smaller than the grid+border: no tiles → 0.0, and crucially no panic.
        assert_eq!(score_gray(&checkerboard(4, 4, 1)), 0.0);
        assert_eq!(score_gray(&flat(1, 1)), 0.0);
    }

    #[test]
    fn increasing_blur_is_monotonic() {
        // More blur → lower (or equal) score. A sanity check that the metric tracks
        // the physical quantity, not just a binary sharp/soft split.
        let sharp = checkerboard(256, 256, 4);
        let mut prev = score_gray(&sharp);
        for &sigma in &[1.0f32, 2.0, 4.0, 8.0] {
            let s = score_gray(&blur(&sharp, sigma));
            assert!(
                s <= prev + 1e-6,
                "score should not rise with more blur (sigma {sigma}): {s} vs prev {prev}"
            );
            prev = s;
        }
    }
}
