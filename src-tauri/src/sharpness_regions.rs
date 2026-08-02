//! Region-aware sharpness refinement (H16c): score *where the shot matters*, not the
//! whole frame.
//!
//! The tiled baseline ([`crate::sharpness`]) answers "is anything sharp anywhere?". That
//! kills the bokeh false-positive but has a blind spot: a photo whose *background* is
//! sharp while the intended subject is soft still scores high. When we know where the
//! subject is, we should measure *there*. Two cheap sources of "where":
//!
//! 1. **Face boxes** (H13, `faces` feature). A soft face on a sharp background is exactly
//!    the frame to flag — for people photos this *is* the feature. We score the Laplacian
//!    variance *inside each detected face box* and take the sharpest box. `method='face'`.
//! 2. **AF point** (Sony `FocusLocation` makernote). The camera recorded where it focused;
//!    score a neighbourhood around that point — "did the AF land?" — subject-awareness with
//!    no ML. `method='afpoint'`.
//!
//! Both reuse [`crate::sharpness::laplacian_variance`], so a face/AF score is on the *same
//! scale* as a tile score (identical focus measure, different rectangle). Callers pick the
//! method by priority: face → AF → tile (see [`Method`] / the indexer). This module is
//! **pure pixel math + a tiny string parser**: no I/O, no catalog, no `faces`-feature
//! dependency (it takes already-extracted boxes / an already-parsed point), so it compiles
//! and is tested with and without `--features faces`.

use image::GrayImage;

use crate::sharpness::laplacian_variance;

/// How a photo's sharpness was measured. Serialized to `photos.sharpness_method`.
///
/// Kept here (next to the region scorers that produce `Face`/`AfPoint`) rather than in the
/// indexer so the priority chain has one obvious home. `Tile` is the baseline fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// Scored inside detected face boxes (H13, `faces` feature).
    Face,
    /// Scored at the camera's autofocus point (Sony `FocusLocation`).
    AfPoint,
    /// Tiled top-percentile baseline — nothing better was available.
    Tile,
}

impl Method {
    /// The string persisted to `photos.sharpness_method`.
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Face => "face",
            Method::AfPoint => "afpoint",
            Method::Tile => "tile",
        }
    }
}

/// A normalized bounding box on the **oriented** image: `x,y` top-left, `w,h` extent, all
/// in `[0,1]`. This is exactly the `faces__faces.bbox` `[x,y,w,h]` layout, so the caller
/// hands face rows straight through. Foreign coordinate systems are the caller's problem.
#[derive(Debug, Clone, Copy)]
pub struct NormBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Score sharpness inside the given face boxes. Returns the **sharpest** box's Laplacian
/// variance — a single soft face among sharp ones shouldn't drag the score down; we want
/// "is the subject we care about sharp?", and any sharp face answers yes. Burst-relative
/// flagging (H16e) catches the "this frame's face is softer than its siblings" case.
///
/// Boxes are clamped to the image and converted to pixel rectangles. A box too small to
/// hold an interior Laplacian pixel contributes `0.0`. Returns `None` when there are no
/// usable boxes (empty input, or every box degenerate/off-image) so the caller can fall
/// through to the next method rather than storing a meaningless `0.0`.
pub fn score_faces(gray: &GrayImage, boxes: &[NormBox]) -> Option<f64> {
    let (w, h) = gray.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    let mut best: Option<f64> = None;
    for b in boxes {
        let Some((x0, y0, x1, y1)) = norm_rect_to_px(b.x, b.y, b.w, b.h, w, h) else {
            continue;
        };
        let v = laplacian_variance(gray, x0, y0, x1, y1);
        best = Some(best.map_or(v, |cur: f64| cur.max(v)));
    }
    best
}

/// Score sharpness in a neighbourhood around a normalized AF point `(nx, ny)` on the
/// **oriented** image. The camera gives a single focus *point*; a point has no area, so we
/// measure a box of side `frac` of the shorter image edge, centered on the point (clamped
/// to stay on-image). `frac` ~0.15 is a small central patch — big enough for a stable
/// variance, small enough to isolate the subject from a sharp background.
///
/// Returns `None` when the point is out of `[0,1]` (bad/absent data) or the resulting box
/// is degenerate, so the caller falls through to tiles.
pub fn score_af_point(gray: &GrayImage, nx: f32, ny: f32, frac: f32) -> Option<f64> {
    let (w, h) = gray.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    if !(0.0..=1.0).contains(&nx) || !(0.0..=1.0).contains(&ny) || !frac.is_finite() || frac <= 0.0 {
        return None;
    }
    let half = (frac.min(1.0) as f64) * (w.min(h) as f64) / 2.0;
    let cx = nx as f64 * w as f64;
    let cy = ny as f64 * h as f64;
    let x0 = (cx - half).floor().max(0.0) as u32;
    let y0 = (cy - half).floor().max(0.0) as u32;
    let x1 = ((cx + half).ceil() as i64).clamp(0, w as i64) as u32;
    let y1 = ((cy + half).ceil() as i64).clamp(0, h as i64) as u32;
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let v = laplacian_variance(gray, x0, y0, x1, y1);
    Some(v)
}

/// The default AF-neighbourhood side as a fraction of the shorter image edge. See
/// [`score_af_point`].
pub const AF_PATCH_FRAC: f32 = 0.15;

/// Convert a normalized `[x,y,w,h]` box to a pixel rectangle `[x0,x1) × [y0,y1)` clamped to
/// the image. Returns `None` if the box is empty/degenerate after clamping (nothing to
/// score). Non-finite or negative extents are rejected.
fn norm_rect_to_px(
    x: f32,
    y: f32,
    bw: f32,
    bh: f32,
    img_w: u32,
    img_h: u32,
) -> Option<(u32, u32, u32, u32)> {
    if ![x, y, bw, bh].iter().all(|v| v.is_finite()) || bw <= 0.0 || bh <= 0.0 {
        return None;
    }
    let fx0 = (x.max(0.0) as f64) * img_w as f64;
    let fy0 = (y.max(0.0) as f64) * img_h as f64;
    let fx1 = ((x + bw) as f64) * img_w as f64;
    let fy1 = ((y + bh) as f64) * img_h as f64;
    let x0 = fx0.floor().max(0.0) as u32;
    let y0 = fy0.floor().max(0.0) as u32;
    let x1 = (fx1.ceil() as i64).clamp(0, img_w as i64) as u32;
    let y1 = (fy1.ceil() as i64).clamp(0, img_h as i64) as u32;
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some((x0, y0, x1, y1))
}

/// Parse a Sony `FocusLocation` makernote value into a normalized AF point on the
/// **oriented** image, applying the EXIF `orientation` code so the point lands correctly on
/// the oriented preview the scorer measures.
///
/// # Format (verified on the owner's bodies, spike 2026-07-07)
///
/// exiftool reports `FocusLocation` as four integers: `"imgW imgH afX afY"` — the AF point
/// `(afX, afY)` in **unrotated sensor** pixel coordinates, with the sensor's own width/height.
/// On the ILCE-7RM6 (and a second body at 7008×4672) *every* ARW carried it (542/542 in the
/// owner's library); a centered AF point reads as exactly `imgW/2 imgH/2`.
///
/// The catch: `imgW imgH` is always the **unrotated** sensor frame even for a portrait shot
/// (`Orientation = Rotate 90 CW`), while our cached preview is fully oriented. So we
/// normalize in the sensor frame, then rotate the normalized point by the EXIF orientation
/// to match the oriented preview.
///
/// `orientation` is the standard EXIF Orientation tag value (1–8); unknown/zero is treated
/// as 1 (no transform). Returns `None` on any malformed input (wrong field count, zero
/// dimensions, unparseable numbers, out-of-range point).
pub fn parse_focus_location(value: &str, orientation: u16) -> Option<(f32, f32)> {
    let nums: Vec<f64> = value
        .split_whitespace()
        .map(|t| t.parse::<f64>())
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    if nums.len() != 4 {
        return None;
    }
    let (img_w, img_h, af_x, af_y) = (nums[0], nums[1], nums[2], nums[3]);
    if img_w <= 0.0 || img_h <= 0.0 {
        return None;
    }
    // Normalized point in the unrotated sensor frame.
    let sx = (af_x / img_w) as f32;
    let sy = (af_y / img_h) as f32;
    if !(0.0..=1.0).contains(&sx) || !(0.0..=1.0).contains(&sy) {
        return None;
    }
    Some(orient_point(sx, sy, orientation))
}

/// Map a normalized point in the unrotated frame to the oriented frame for a given EXIF
/// orientation. Only the rotations/flips that change coordinates are handled; the common
/// cases here are 1 (normal) and 6 (Rotate 90 CW). Covers all eight EXIF orientations for
/// completeness.
fn orient_point(x: f32, y: f32, orientation: u16) -> (f32, f32) {
    match orientation {
        // 1 = normal, 0/unknown → no transform.
        2 => (1.0 - x, y),             // mirror horizontal
        3 => (1.0 - x, 1.0 - y),       // rotate 180
        4 => (x, 1.0 - y),             // mirror vertical
        5 => (y, x),                   // mirror horizontal + rotate 270 CW
        6 => (1.0 - y, x),             // rotate 90 CW
        7 => (1.0 - y, 1.0 - x),       // mirror horizontal + rotate 90 CW
        8 => (y, 1.0 - x),             // rotate 270 CW
        _ => (x, y),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{imageops, GrayImage, Luma};

    fn checkerboard(w: u32, h: u32, cell: u32) -> GrayImage {
        GrayImage::from_fn(w, h, |x, y| {
            let on = ((x / cell) + (y / cell)) % 2 == 0;
            Luma([if on { 255u8 } else { 0u8 }])
        })
    }

    fn blur(g: &GrayImage, sigma: f32) -> GrayImage {
        imageops::blur(g, sigma)
    }

    /// A frame whose *background* is sharp but a chosen box region is soft — the case a
    /// region-aware scorer must catch and the tiled baseline misses. Paints a sharp
    /// checkerboard everywhere, then blurs a rectangle at (bx0,by0)-(bx1,by1).
    fn sharp_bg_soft_region(
        w: u32,
        h: u32,
        bx0: u32,
        by0: u32,
        bx1: u32,
        by1: u32,
    ) -> GrayImage {
        let mut base = checkerboard(w, h, 4);
        // Blur just the sub-rectangle by blurring a crop and pasting it back.
        let cw = bx1 - bx0;
        let ch = by1 - by0;
        let crop = image::imageops::crop_imm(&base, bx0, by0, cw, ch).to_image();
        let soft = blur(&crop, 4.0);
        for y in 0..ch {
            for x in 0..cw {
                base.put_pixel(bx0 + x, by0 + y, *soft.get_pixel(x, y));
            }
        }
        base
    }

    // ── Face-box scoring ────────────────────────────────────────────────────────

    #[test]
    fn face_box_scores_the_box_not_the_frame() {
        // A sharp frame with a soft face region: the face-box score must reflect the SOFT
        // face, far below the score of a sharp box elsewhere. This is the whole point —
        // "sharp background, soft subject" is invisible to the tiled baseline.
        let img = sharp_bg_soft_region(256, 256, 96, 96, 160, 160);
        let soft_face = NormBox { x: 96.0 / 256.0, y: 96.0 / 256.0, w: 64.0 / 256.0, h: 64.0 / 256.0 };
        let sharp_face = NormBox { x: 0.0, y: 0.0, w: 64.0 / 256.0, h: 64.0 / 256.0 };
        let s_soft = score_faces(&img, &[soft_face]).unwrap();
        let s_sharp = score_faces(&img, &[sharp_face]).unwrap();
        assert!(
            s_sharp > s_soft,
            "a sharp face box ({s_sharp}) must outscore a soft one ({s_soft})"
        );
    }

    #[test]
    fn face_box_takes_the_sharpest_of_several() {
        // Multiple faces → the sharpest wins (one soft face shouldn't sink a sharp one).
        let img = sharp_bg_soft_region(256, 256, 0, 0, 96, 96);
        let soft_face = NormBox { x: 0.0, y: 0.0, w: 96.0 / 256.0, h: 96.0 / 256.0 };
        let sharp_face = NormBox { x: 150.0 / 256.0, y: 150.0 / 256.0, w: 80.0 / 256.0, h: 80.0 / 256.0 };
        let both = score_faces(&img, &[soft_face, sharp_face]).unwrap();
        let only_sharp = score_faces(&img, &[sharp_face]).unwrap();
        assert!(
            (both - only_sharp).abs() < 1e-9,
            "score of [soft, sharp] ({both}) must equal score of [sharp] ({only_sharp})"
        );
    }

    #[test]
    fn face_box_empty_or_degenerate_is_none() {
        let img = checkerboard(100, 100, 4);
        assert!(score_faces(&img, &[]).is_none(), "no boxes → None");
        // Zero-size box.
        let zero = NormBox { x: 0.5, y: 0.5, w: 0.0, h: 0.0 };
        assert!(score_faces(&img, &[zero]).is_none(), "degenerate box → None");
        // Fully off-image box.
        let off = NormBox { x: 2.0, y: 2.0, w: 0.5, h: 0.5 };
        assert!(score_faces(&img, &[off]).is_none(), "off-image box → None");
    }

    #[test]
    fn face_box_clamps_partially_offscreen_box() {
        // A box that spills past the right/bottom edge must still score (clamped), not panic.
        let img = checkerboard(120, 120, 4);
        let spill = NormBox { x: 0.8, y: 0.8, w: 0.5, h: 0.5 };
        let s = score_faces(&img, &[spill]);
        assert!(s.is_some(), "partially off-image box should clamp and score");
    }

    // ── AF-point scoring ────────────────────────────────────────────────────────

    #[test]
    fn af_point_scores_neighbourhood_soft_below_sharp() {
        // AF point on a soft region scores below the same point size on a sharp region.
        let img = sharp_bg_soft_region(256, 256, 96, 96, 160, 160);
        // Center point (0.5,0.5) is in the soft rectangle; a corner point is sharp.
        let s_soft = score_af_point(&img, 0.5, 0.5, AF_PATCH_FRAC).unwrap();
        let s_sharp = score_af_point(&img, 0.1, 0.1, AF_PATCH_FRAC).unwrap();
        assert!(
            s_sharp > s_soft,
            "AF point on sharp area ({s_sharp}) must beat AF point on soft area ({s_soft})"
        );
    }

    #[test]
    fn af_point_out_of_range_is_none() {
        let img = checkerboard(100, 100, 4);
        assert!(score_af_point(&img, -0.1, 0.5, AF_PATCH_FRAC).is_none());
        assert!(score_af_point(&img, 0.5, 1.5, AF_PATCH_FRAC).is_none());
        assert!(score_af_point(&img, 0.5, 0.5, 0.0).is_none(), "zero frac → None");
    }

    #[test]
    fn af_point_at_corner_clamps() {
        // A point in the extreme corner still yields a valid on-image box.
        let img = checkerboard(100, 100, 4);
        assert!(score_af_point(&img, 0.0, 0.0, AF_PATCH_FRAC).is_some());
        assert!(score_af_point(&img, 1.0, 1.0, AF_PATCH_FRAC).is_some());
    }

    // ── FocusLocation parsing ─────────────────────────────────────────────────────

    #[test]
    fn parse_focus_location_centered() {
        // Centered AF point on a landscape frame → (0.5, 0.5), orientation normal.
        let (nx, ny) = parse_focus_location("9984 6656 4992 3328", 1).unwrap();
        assert!((nx - 0.5).abs() < 1e-4, "nx={nx}");
        assert!((ny - 0.5).abs() < 1e-4, "ny={ny}");
    }

    #[test]
    fn parse_focus_location_off_center() {
        // A point in the right third of a landscape frame.
        let (nx, ny) = parse_focus_location("7008 4672 5256 1168", 1).unwrap();
        assert!((nx - 0.75).abs() < 1e-3, "nx={nx}");
        assert!((ny - 0.25).abs() < 1e-3, "ny={ny}");
    }

    #[test]
    fn parse_focus_location_rotate_90_cw() {
        // Orientation 6 (Rotate 90 CW): the sensor-frame point must be rotated to the
        // oriented preview. Sensor point (0.75, 0.25) → oriented (1-0.25, 0.75) = (0.75, 0.75).
        let (nx, ny) = parse_focus_location("7008 4672 5256 1168", 6).unwrap();
        assert!((nx - 0.75).abs() < 1e-3, "nx={nx}");
        assert!((ny - 0.75).abs() < 1e-3, "ny={ny}");
    }

    #[test]
    fn parse_focus_location_rejects_malformed() {
        assert!(parse_focus_location("", 1).is_none());
        assert!(parse_focus_location("9984 6656 4992", 1).is_none(), "3 fields");
        assert!(parse_focus_location("9984 6656 4992 3328 0", 1).is_none(), "5 fields");
        assert!(parse_focus_location("0 6656 0 0", 1).is_none(), "zero width");
        assert!(parse_focus_location("a b c d", 1).is_none(), "non-numeric");
        // AF point outside the sensor frame → rejected.
        assert!(parse_focus_location("100 100 200 50", 1).is_none(), "x out of range");
    }

    #[test]
    fn orient_point_round_trips_normal() {
        assert_eq!(orient_point(0.3, 0.7, 1), (0.3, 0.7));
        assert_eq!(orient_point(0.3, 0.7, 0), (0.3, 0.7));
    }

    #[test]
    fn method_strings() {
        assert_eq!(Method::Face.as_str(), "face");
        assert_eq!(Method::AfPoint.as_str(), "afpoint");
        assert_eq!(Method::Tile.as_str(), "tile");
    }
}
