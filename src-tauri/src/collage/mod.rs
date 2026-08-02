//! The collage **engine** (behind the `collage` Cargo feature): a justified-mosaic layout
//! solver plus a compositor that renders decoded images onto a single canvas. Pure and
//! catalog-free — it works on aspect ratios + already-decoded images, so the whole module
//! is unit-testable without real files. The caller (the `make_collage` command) resolves
//! each photo to its cached preview, decodes it, and hands the pixels here. See
//! docs/collage.md.
//!
//! Layout is the Flickr/Google-Photos "justified gallery": tiles keep their aspect ratio,
//! greedily fill full-width rows, and each row is scaled so its tiles add up to the canvas
//! width exactly. With a fixed output aspect ratio we binary-search the seed row height
//! until the stacked rows total the target canvas height (monotonic), and any residual is
//! left as background mat.

use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, Rgba, RgbaImage};

/// How each tile fills its rect. A justified mosaic is aspect-preserving by nature, so
/// `Contain` is the natural mode; `Cover` lightly crops each row's tiles toward a common
/// aspect for a more uniform look.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fit {
    /// Show each photo whole — the tile's rect matches its own aspect (default).
    Contain,
    /// Nudge each row's aspects toward the row median before sizing, then crop to fill.
    Cover,
}

/// All knobs for one collage render. Output file format/encoding is the command's concern
/// (CO2) — the engine returns raw pixels.
pub struct CollageOptions {
    /// Output canvas width in pixels.
    pub width: u32,
    /// Fixed output aspect ratio as `(w, h)` (e.g. `(1, 1)`, `(4, 5)`); `None` = Free,
    /// where the height grows with the number of rows.
    pub aspect: Option<(u32, u32)>,
    /// Target row height (px). The plain seed in Free mode; the binary-search seed when a
    /// fixed `aspect` is set.
    pub row_height: u32,
    /// Gap between tiles and around the outer edge (px).
    pub gap: u32,
    /// Background / mat color (RGBA). Fills gaps, rounded-corner cut-outs, and any residual.
    pub background: [u8; 4],
    /// `Contain` (whole photos) or `Cover` (uniform, lightly cropped tiles).
    pub fit: Fit,
    /// Per-tile border stroke width (px); 0 disables.
    pub border_width: u32,
    /// Per-tile rounded-corner radius (px); 0 = square corners.
    pub corner_radius: u32,
}

/// A tile's placement on the canvas (top-left + size, all in pixels).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Lay out `aspect_ratios` (each `w/h`) into a justified mosaic and return the per-tile
/// rects (input order) plus the canvas height. With `opts.aspect` set the height is fixed
/// at `round(width * h / w)` and the seed row height is binary-searched to fit; otherwise
/// the height is whatever the stacked rows yield (Free).
pub fn layout(aspect_ratios: &[f32], opts: &CollageOptions) -> (Vec<Rect>, u32) {
    if aspect_ratios.is_empty() {
        return (Vec::new(), opts.gap.max(1));
    }

    match opts.aspect {
        // Free: a single pass at the target row height; the height grows with the rows.
        None => layout_rows(aspect_ratios, opts, opts.row_height.max(1) as f32),
        // Fixed aspect: the canvas is exactly width × width*h/w. Total height is monotonic
        // in the seed row height (taller seed → fewer/taller rows → taller canvas), so we
        // binary-search the seed until the stacked height matches the target. The residual
        // (e.g. a short last row) is left as background mat.
        Some((rw, rh)) => {
            let target = ((opts.width as f64 * rh as f64 / rw.max(1) as f64).round() as u32).max(1);
            let seed = solve_seed_for_height(aspect_ratios, opts, target);
            let (rects, _) = layout_rows(aspect_ratios, opts, seed);
            (rects, target)
        }
    }
}

/// Binary-search the seed row height so the stacked rows total `target` canvas height.
/// Monotone increasing in the seed, so a plain bisection converges in a few iterations.
fn solve_seed_for_height(aspect_ratios: &[f32], opts: &CollageOptions, target: u32) -> f32 {
    // Bracket: a 1px seed is the floor; the upper bound is the full target (a single tall
    // row can't exceed it by much). Grow the upper bound until it overshoots, just in case.
    let mut lo = 1.0_f32;
    let mut hi = target as f32;
    let height_at = |seed: f32| layout_rows(aspect_ratios, opts, seed).1;
    while height_at(hi) < target && hi < (target * 8).max(8) as f32 {
        hi *= 2.0;
    }
    // 40 bisections is far more than enough to land within a pixel. We return `lo` — the
    // largest seed whose stacked rows stay at-or-under the target — so the packed content
    // fits inside the fixed canvas and the small remainder (a short last row's slack) is
    // left as background mat, rather than overshooting and clipping the bottom row.
    for _ in 0..40 {
        let mid = 0.5 * (lo + hi);
        if height_at(mid) <= target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo
}

/// Greedy justified-rows packer at a given (fractional) target row height. Each row is
/// filled until adding another tile would overflow `width`, then scaled so its tiles span
/// the full width exactly. Returns the rects (input order) and the total canvas height.
fn layout_rows(aspect_ratios: &[f32], opts: &CollageOptions, target_h: f32) -> (Vec<Rect>, u32) {
    let width = opts.width.max(1);
    let gap = opts.gap;
    // Usable width once the outer margins and inter-tile gaps are removed is computed per
    // row (it depends on the tile count), so here we only need the content width.
    let content_w = width.saturating_sub(2 * gap).max(1) as f32;
    let target_h = target_h.max(1.0);

    let mut rects = vec![Rect { x: 0, y: 0, w: 0, h: 0 }; aspect_ratios.len()];
    let mut y = gap as f32;
    let mut row: Vec<usize> = Vec::new();

    let n = aspect_ratios.len();
    let mut i = 0;
    while i < n {
        row.push(i);
        // Aspects used for sizing this row (Cover nudges them toward the row median).
        let aspects = row_aspects(&row, aspect_ratios, opts.fit);
        let sum_a: f32 = aspects.iter().sum();
        // Width the row would occupy at the target height (tiles + inter-tile gaps).
        let gaps = gap as f32 * (row.len().saturating_sub(1)) as f32;
        let row_w = sum_a * target_h + gaps;

        let last = i + 1 == n;
        if row_w >= content_w || last {
            // Finalize: a full/overflowing row is scaled to span the width exactly; a short
            // last row keeps ~target height (left-justified) so it isn't blown up.
            let is_short_last = last && row_w < content_w;
            let h = if is_short_last {
                target_h
            } else {
                let avail = content_w - gaps;
                (avail / sum_a).max(1.0)
            };
            place_row(&row, &aspects, h, gap, y, width, &mut rects);
            y += h + gap as f32;
            row.clear();
        }
        i += 1;
    }

    let height = (y.round() as u32).max(gap.max(1));
    (rects, height)
}

/// The aspect ratios to size a row by. `Contain` uses each tile's own aspect; `Cover`
/// blends each toward the row's median aspect (50%), so tiles end up more uniform — the
/// crop to that target aspect happens later in [`compose`].
fn row_aspects(row: &[usize], aspect_ratios: &[f32], fit: Fit) -> Vec<f32> {
    let own: Vec<f32> = row.iter().map(|&idx| aspect_ratios[idx].max(0.01)).collect();
    match fit {
        Fit::Contain => own,
        Fit::Cover => {
            let median = median(&own);
            own.iter().map(|a| 0.5 * a + 0.5 * median).collect()
        }
    }
}

/// Median of a slice (copying + sorting; rows are tiny so this is cheap).
fn median(values: &[f32]) -> f32 {
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = v.len() / 2;
    if v.len() % 2 == 0 {
        0.5 * (v[mid - 1] + v[mid])
    } else {
        v[mid]
    }
}

/// Write the rects for one row at height `h`, packing tiles left→right from the outer
/// margin with `gap` between them.
fn place_row(
    row: &[usize],
    aspects: &[f32],
    h: f32,
    gap: u32,
    y: f32,
    _width: u32,
    rects: &mut [Rect],
) {
    let mut x = gap as f32;
    let row_h = h.round().max(1.0) as u32;
    for (k, &idx) in row.iter().enumerate() {
        let w = (aspects[k] * h).round().max(1.0) as u32;
        rects[idx] = Rect {
            x: x.round() as u32,
            y: y.round() as u32,
            w,
            h: row_h,
        };
        x += w as f32 + gap as f32;
    }
}

/// Render `images` (input order) onto a `width × height` canvas filled with the background
/// color, each tile resized (Lanczos3) into its laid-out rect with optional rounded-corner
/// alpha and a border stroke. Tiles past the rect count are ignored; missing tiles leave
/// the background showing.
pub fn compose(images: Vec<DynamicImage>, opts: &CollageOptions) -> RgbaImage {
    let aspects: Vec<f32> = images
        .iter()
        .map(|img| {
            let (w, h) = img.dimensions();
            (w as f32 / h.max(1) as f32).max(0.01)
        })
        .collect();
    let (rects, height) = layout(&aspects, opts);

    let mut canvas = RgbaImage::from_pixel(opts.width.max(1), height, Rgba(opts.background));

    for (img, rect) in images.into_iter().zip(rects.into_iter()) {
        if rect.w == 0 || rect.h == 0 {
            continue;
        }
        let tile = render_tile(img, rect.w, rect.h, opts);
        overlay_clamped(&mut canvas, &tile, rect.x, rect.y);
    }
    canvas
}

/// A normalized freeform placement: `x`/`y` top-left and `w`/`h` size as fractions of the
/// canvas (0..1); `z` is stacking order (drawn low→high, so a higher `z` sits on top).
/// `ox`/`oy` are the focal offset (0..1) for the cover-crop — which part of the photo shows
/// when the cell's aspect differs from the photo's (0.5,0.5 = centered, matching CSS
/// `object-position`). Pan the photo inside its frame by changing these.
#[derive(Debug, Clone, Copy)]
pub struct Placement {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub z: i64,
    pub ox: f32,
    pub oy: f32,
    /// Zoom factor for the cover-crop (1.0 = fill the cell; >1 zooms in, cropping tighter).
    pub zoom: f32,
}

/// Composite a **freeform** collage: place each image at its normalized rect on a
/// `canvas_w × canvas_h` canvas in `z` order, allowing overlap (and bleed off any edge).
/// Tiles keep the photo's own aspect (the placement already carries the box the UI sized to
/// that aspect), so this resizes to the box directly; border + rounded corners from `opts`
/// still apply. The mat is `opts.background`.
pub fn compose_freeform(
    items: Vec<(DynamicImage, Placement)>,
    canvas_w: u32,
    canvas_h: u32,
    opts: &CollageOptions,
) -> RgbaImage {
    let cw = canvas_w.max(1);
    let ch = canvas_h.max(1);
    let mut canvas = RgbaImage::from_pixel(cw, ch, Rgba(opts.background));

    let mut items = items;
    items.sort_by_key(|(_, p)| p.z);

    for (img, p) in items {
        let w = (p.w * cw as f32).round().clamp(1.0, cw as f32) as u32;
        let h = (p.h * ch as f32).round().clamp(1.0, ch as f32) as u32;
        let x = (p.x * cw as f32).round() as i64;
        let y = (p.y * ch as f32).round() as i64;

        // Cover-fill the box at the tile's zoom + focal offset: zoom 1 with a box matching
        // the photo's aspect is a plain resize (no crop); a different aspect or zoom>1 crops
        // to fill, panned by ox/oy.
        let mut tile = resize_cover_offset(&img, w, h, p.zoom, p.ox, p.oy).to_rgba8();
        if opts.corner_radius > 0 {
            round_corners(&mut tile, opts.corner_radius);
        }
        if opts.border_width > 0 {
            draw_border(&mut tile, opts.border_width, opts.background);
        }
        overlay_signed(&mut canvas, &tile, x, y);
    }
    canvas
}

/// Resize one source image into a `w × h` RGBA tile (Cover crops to fill, Contain fits the
/// whole image — which here equals the rect's own aspect, so it's a plain resize), then
/// apply rounded-corner alpha and a border stroke.
fn render_tile(img: DynamicImage, w: u32, h: u32, opts: &CollageOptions) -> RgbaImage {
    let resized = match opts.fit {
        Fit::Contain => img.resize_exact(w, h, FilterType::Lanczos3),
        Fit::Cover => resize_cover(&img, w, h),
    };
    let mut tile = resized.to_rgba8();
    if opts.corner_radius > 0 {
        round_corners(&mut tile, opts.corner_radius);
    }
    if opts.border_width > 0 {
        draw_border(&mut tile, opts.border_width, opts.background);
    }
    tile
}

/// Resize-and-crop to exactly `w × h`, scaling the source to cover the rect and
/// center-cropping the overflow.
fn resize_cover(img: &DynamicImage, w: u32, h: u32) -> DynamicImage {
    resize_cover_offset(img, w, h, 1.0, 0.5, 0.5)
}

/// Like [`resize_cover`] but with a `zoom` factor (≥1, scales the cover fill so a higher
/// zoom crops tighter) and the crop window positioned by a focal offset `ox`/`oy` (0..1;
/// 0.5 = centered, matching CSS `object-position`). Lets a tile be zoomed + panned in frame.
fn resize_cover_offset(img: &DynamicImage, w: u32, h: u32, zoom: f32, ox: f32, oy: f32) -> DynamicImage {
    let (sw, sh) = img.dimensions();
    let scale = (w as f32 / sw as f32).max(h as f32 / sh as f32) * zoom.max(1.0);
    let rw = (sw as f32 * scale).ceil().max(w as f32) as u32;
    let rh = (sh as f32 * scale).ceil().max(h as f32) as u32;
    let resized = img.resize_exact(rw, rh, FilterType::Lanczos3);
    let cx = (rw.saturating_sub(w) as f32 * ox.clamp(0.0, 1.0)).round() as u32;
    let cy = (rh.saturating_sub(h) as f32 * oy.clamp(0.0, 1.0)).round() as u32;
    resized.crop_imm(cx, cy, w, h)
}

/// Zero the alpha of pixels outside a rounded rectangle of `radius`, so the background
/// shows through the corners. A 1px feather on the corner arc softens the edge.
fn round_corners(tile: &mut RgbaImage, radius: u32) {
    let (w, h) = tile.dimensions();
    let r = radius.min(w / 2).min(h / 2) as f32;
    if r <= 0.0 {
        return;
    }
    for y in 0..h {
        for x in 0..w {
            // Distance into the nearest corner; only the corner squares can be outside.
            let dx = if (x as f32) < r {
                r - (x as f32 + 0.5)
            } else if (x as f32) >= w as f32 - r {
                (x as f32 + 0.5) - (w as f32 - r)
            } else {
                0.0
            };
            let dy = if (y as f32) < r {
                r - (y as f32 + 0.5)
            } else if (y as f32) >= h as f32 - r {
                (y as f32 + 0.5) - (h as f32 - r)
            } else {
                0.0
            };
            if dx <= 0.0 || dy <= 0.0 {
                continue; // straight edge / interior — fully opaque
            }
            let dist = (dx * dx + dy * dy).sqrt();
            // Coverage: 1 inside the arc, 0 outside, linear over a 1px feather band.
            let coverage = (r - dist + 0.5).clamp(0.0, 1.0);
            let px = tile.get_pixel_mut(x, y);
            px[3] = (px[3] as f32 * coverage).round() as u8;
        }
    }
}

/// Stroke a `bw`-px border around the tile in the border color (the background's RGB, fully
/// opaque). Drawn after rounded corners so the stroke follows the (rounded) tile edge.
fn draw_border(tile: &mut RgbaImage, bw: u32, color: [u8; 4]) {
    let (w, h) = tile.dimensions();
    let bw = bw.min(w / 2).min(h / 2);
    if bw == 0 {
        return;
    }
    let stroke = Rgba([color[0], color[1], color[2], 255]);
    for y in 0..h {
        for x in 0..w {
            let on_border = x < bw || y < bw || x >= w - bw || y >= h - bw;
            if !on_border {
                continue;
            }
            // Respect a rounded corner's edge — skip any non-opaque pixel so the border
            // doesn't paint into the mat or erase the feathered (anti-aliased) corner ring.
            if tile.get_pixel(x, y)[3] < 255 {
                continue;
            }
            tile.put_pixel(x, y, stroke);
        }
    }
}

/// Overlay `tile` onto `canvas` at `(ox, oy)` with source-over alpha, clipping anything
/// past the canvas bounds. (Direct loop rather than `imageops::overlay` so we never panic
/// on a tile that rounds a pixel past the edge.)
fn overlay_clamped(canvas: &mut RgbaImage, tile: &RgbaImage, ox: u32, oy: u32) {
    let (cw, ch) = canvas.dimensions();
    let (tw, th) = tile.dimensions();
    for ty in 0..th {
        let cy = oy + ty;
        if cy >= ch {
            break;
        }
        for tx in 0..tw {
            let cx = ox + tx;
            if cx >= cw {
                break;
            }
            let src = tile.get_pixel(tx, ty).0;
            let a = src[3] as f32 / 255.0;
            if a <= 0.0 {
                continue;
            }
            if a >= 1.0 {
                canvas.put_pixel(cx, cy, Rgba(src));
                continue;
            }
            let dst = canvas.get_pixel(cx, cy).0;
            let mut out = [0u8; 4];
            for c in 0..3 {
                out[c] = (src[c] as f32 * a + dst[c] as f32 * (1.0 - a)).round() as u8;
            }
            out[3] = 255;
            canvas.put_pixel(cx, cy, Rgba(out));
        }
    }
}

/// Like [`overlay_clamped`] but with **signed** offsets, so a freeform tile may bleed past
/// the top/left edges too. Pixels outside the canvas on any side are clipped.
fn overlay_signed(canvas: &mut RgbaImage, tile: &RgbaImage, ox: i64, oy: i64) {
    let (cw, ch) = canvas.dimensions();
    let (tw, th) = tile.dimensions();
    for ty in 0..th {
        let cy = oy + ty as i64;
        if cy < 0 {
            continue;
        }
        if cy >= ch as i64 {
            break;
        }
        for tx in 0..tw {
            let cx = ox + tx as i64;
            if cx < 0 || cx >= cw as i64 {
                continue;
            }
            let src = tile.get_pixel(tx, ty).0;
            let a = src[3] as f32 / 255.0;
            if a <= 0.0 {
                continue;
            }
            let (cx, cy) = (cx as u32, cy as u32);
            if a >= 1.0 {
                canvas.put_pixel(cx, cy, Rgba(src));
                continue;
            }
            let dst = canvas.get_pixel(cx, cy).0;
            let mut out = [0u8; 4];
            for c in 0..3 {
                out[c] = (src[c] as f32 * a + dst[c] as f32 * (1.0 - a)).round() as u8;
            }
            out[3] = 255;
            canvas.put_pixel(cx, cy, Rgba(out));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    fn opts(width: u32, aspect: Option<(u32, u32)>) -> CollageOptions {
        CollageOptions {
            width,
            aspect,
            row_height: 200,
            gap: 0,
            background: [0, 0, 0, 255],
            fit: Fit::Contain,
            border_width: 0,
            corner_radius: 0,
        }
    }

    /// A solid-color image of the given pixel size (for compose tests; no files).
    fn solid(w: u32, h: u32, c: [u8; 3]) -> DynamicImage {
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(w, h, Rgb(c)))
    }

    #[test]
    fn freeform_overlap_respects_z_order() {
        // Two overlapping tiles; the higher-z one wins in the overlap, lower-z shows
        // elsewhere, and the bare canvas shows the background mat.
        let red = solid(50, 50, [255, 0, 0]);
        let blue = solid(50, 50, [0, 0, 255]);
        let o = opts(100, Some((1, 1))); // black mat, no border/corners
        let items = vec![
            (red, Placement { x: 0.1, y: 0.1, w: 0.5, h: 0.5, z: 0, ox: 0.5, oy: 0.5, zoom: 1.0 }),
            (blue, Placement { x: 0.3, y: 0.3, w: 0.5, h: 0.5, z: 1, ox: 0.5, oy: 0.5, zoom: 1.0 }),
        ];
        let canvas = compose_freeform(items, 100, 100, &o);
        assert_eq!(canvas.dimensions(), (100, 100));
        assert_eq!(canvas.get_pixel(45, 45).0, [0, 0, 255, 255], "overlap = higher z (blue)");
        assert_eq!(canvas.get_pixel(15, 15).0, [255, 0, 0, 255], "red-only region");
        assert_eq!(canvas.get_pixel(95, 5).0, [0, 0, 0, 255], "background mat");
    }

    /// Right edge (x + w) of the last tile in each row, keyed by row top (y).
    fn row_right_edges(rects: &[Rect]) -> Vec<u32> {
        use std::collections::BTreeMap;
        let mut by_y: BTreeMap<u32, u32> = BTreeMap::new();
        for r in rects {
            let e = by_y.entry(r.y).or_insert(0);
            *e = (*e).max(r.x + r.w);
        }
        by_y.into_values().collect()
    }

    #[test]
    fn rows_fill_the_width() {
        // A mix of landscape (2.0) and portrait (0.5) aspects, enough to force several
        // full rows. Every full row's right edge must reach the canvas width (±1px).
        let aspects: Vec<f32> = (0..12)
            .map(|i| if i % 2 == 0 { 2.0 } else { 0.5 })
            .collect();
        let o = opts(1000, None);
        let (rects, height) = layout(&aspects, &o);
        assert!(height > 0);
        let edges = row_right_edges(&rects);
        // All but possibly the (left-justified) last row span the full width.
        for e in &edges[..edges.len().saturating_sub(1)] {
            assert!((*e as i64 - 1000).abs() <= 1, "row right edge {e} != width 1000");
        }
    }

    #[test]
    fn fixed_aspect_fills_target_canvas() {
        // A 1:1 target on a 1000px wide canvas must produce a ~1000×1000 layout.
        let aspects: Vec<f32> = (0..15)
            .map(|i| if i % 3 == 0 { 1.5 } else { 0.7 })
            .collect();
        let o = opts(1000, Some((1, 1)));
        let (rects, height) = layout(&aspects, &o);
        assert_eq!(height, 1000, "1:1 canvas height should equal width");
        // The packed rows fill the target height down to a small background-mat residual
        // (at most a short last row's worth of slack) and never overshoot/clip the bottom.
        let bottom = rects.iter().map(|r| r.y + r.h).max().unwrap_or(0);
        assert!(bottom <= height, "rows must not overflow {height}px, bottom={bottom}");
        assert!(
            height - bottom <= 60,
            "residual mat should be small: {height}px target, bottom={bottom}"
        );
    }

    #[test]
    fn fixed_aspect_43_fills_target_canvas() {
        // 4:3 → height = width * 3 / 4.
        let aspects: Vec<f32> = (0..10).map(|_| 1.3).collect();
        let o = opts(1200, Some((4, 3)));
        let (_, height) = layout(&aspects, &o);
        assert_eq!(height, 900, "4:3 of 1200 wide = 900 tall");
    }

    #[test]
    fn cover_normalization_changes_tile_widths() {
        // One wide + one tall tile in a single row. Under Contain the wide tile is much
        // wider than the tall one; Cover pulls both toward the median, narrowing the gap.
        let aspects = vec![3.0_f32, 0.4_f32];
        let mut o = opts(2000, None);
        o.fit = Fit::Contain;
        let (contain, _) = layout(&aspects, &o);
        o.fit = Fit::Cover;
        let (cover, _) = layout(&aspects, &o);
        let contain_ratio = contain[0].w as f32 / contain[1].w as f32;
        let cover_ratio = cover[0].w as f32 / cover[1].w as f32;
        assert!(
            cover_ratio < contain_ratio,
            "cover should even out widths: contain={contain_ratio} cover={cover_ratio}"
        );
        // And the widths must actually differ between the two modes.
        assert_ne!(contain[0].w, cover[0].w, "cover must resize the wide tile");
    }

    #[test]
    fn compose_free_returns_full_width_and_positive_height() {
        let images = vec![
            solid(400, 200, [255, 0, 0]),
            solid(200, 400, [0, 255, 0]),
            solid(300, 300, [0, 0, 255]),
        ];
        let o = opts(800, None);
        let canvas = compose(images, &o);
        assert_eq!(canvas.width(), 800, "canvas width matches opts.width");
        assert!(canvas.height() > 0, "free height grows with rows");
    }

    #[test]
    fn compose_fixed_aspect_returns_exact_dimensions() {
        let images: Vec<DynamicImage> = (0..8)
            .map(|i| if i % 2 == 0 { solid(400, 300, [10, 20, 30]) } else { solid(300, 400, [40, 50, 60]) })
            .collect();
        let o = opts(1000, Some((4, 5))); // height = 1000 * 5 / 4 = 1250
        let canvas = compose(images, &o);
        assert_eq!((canvas.width(), canvas.height()), (1000, 1250));
    }

    #[test]
    fn compose_with_border_gap_and_rounded_corners_renders() {
        // Exercise the full tile path — Cover resize, rounded-corner alpha, border stroke,
        // and inter-tile gaps — on a fixed-aspect canvas. The corner cut-outs must let the
        // (opaque) background mat show through, and the canvas must be the exact target size.
        let images: Vec<DynamicImage> = (0..6)
            .map(|i| if i % 2 == 0 { solid(600, 400, [200, 30, 30]) } else { solid(400, 600, [30, 30, 200]) })
            .collect();
        let o = CollageOptions {
            width: 800,
            aspect: Some((1, 1)), // → 800×800
            row_height: 220,
            gap: 12,
            background: [0, 0, 0, 255],
            fit: Fit::Cover,
            border_width: 4,
            corner_radius: 16,
        };
        let canvas = compose(images, &o);
        assert_eq!((canvas.width(), canvas.height()), (800, 800));
        // The gap margins are pure background (top-left corner is inside the outer gap).
        assert_eq!(canvas.get_pixel(0, 0).0, [0, 0, 0, 255], "outer gap is background");
        // Somewhere on the canvas a tile actually painted (not an all-background canvas).
        let painted = canvas.pixels().any(|p| p.0 != [0, 0, 0, 255]);
        assert!(painted, "at least one tile must have rendered");
    }

    #[test]
    fn empty_input_yields_a_minimal_canvas() {
        let o = opts(500, None);
        let (rects, height) = layout(&[], &o);
        assert!(rects.is_empty());
        assert!(height >= 1, "never a zero-height canvas");
        let canvas = compose(Vec::new(), &o);
        assert_eq!(canvas.width(), 500);
    }
}
