//! The base edge layer: every drawn edge of a [`Scene`] stroked with tiny-skia into a BGRA
//! `RenderImage`, off the UI thread (docs/plans/gpui/tag-graph.md § Recommendation 2).
//!
//! Stroking ~8,600 bundled curves as GPUI paths is not viable (the plan measured a million
//! triangles uploaded every frame), so the base layer is a raster made at the current zoom
//! and shown with `paint_image`. While a wheel or pan gesture runs the view reprojects the
//! last raster (soft while zooming — owner decision in #120) and asks for a crisp one once the
//! gesture has been quiet for [`SETTLE`].
//!
//! The raster covers the ring's box on the canvas, clipped to the canvas plus a quarter of
//! its size on every side (so a short pan does not uncover blank canvas), in device pixels,
//! at most [`MAX_EDGE`] on a side. It is cut into horizontal strips stroked on scoped threads;
//! each strip strokes only the edges whose box reaches it. Strokes are 1 screen pixel wide
//! with round caps, in each bucket's colour and alpha, buckets in scene order — React's
//! `ctx.stroke(b.path)` loop.

use chairphoto_model::tag_graph::scene::{Scene, Seg};
use chairphoto_model::tag_graph::view::{Size, View};
use chairphoto_model::tag_graph::RING_R;
use gpui_kit::RenderImage;
use std::sync::Arc;
use std::time::Duration;
use tiny_skia::{Color, LineCap, Paint, PathBuilder, Pixmap, Stroke, Transform};

/// Quiet time after the last wheel/pan/resize before the crisp raster is made.
pub const SETTLE: Duration = Duration::from_millis(150);
/// The longest raster side, device pixels.
pub const MAX_EDGE: f64 = 4096.;
/// How far past the canvas the raster reaches, as a fraction of the canvas size.
const MARGIN: f64 = 0.25;
/// Rows each strip strokes past its own, above and below.
const OVERLAP: u32 = 8;

/// A rectangle on the canvas, logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Region {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// What one raster is made of.
#[derive(Clone)]
pub struct RasterJob {
    pub scene: Arc<Scene>,
    pub view: View,
    pub size: Size,
    pub region: Region,
    /// Device pixels per logical pixel.
    pub scale: f64,
    pub strips: usize,
}

/// A finished raster: BGRA pixels with straight alpha, as `RenderImage` wants them.
pub struct RasterPixels {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// The region to raster for `view` on a canvas of `size`: the ring's box, clipped to the
/// canvas plus the margin. `None` when it is empty (the ring is off the canvas).
pub fn region_for(view: View, size: Size) -> Option<Region> {
    let (cx, cy) = view.centre(size);
    let r = RING_R * view.k + 4.;
    let (mx, my) = (size.w * MARGIN, size.h * MARGIN);
    let x0 = (cx - r).max(-mx);
    let y0 = (cy - r).max(-my);
    let x1 = (cx + r).min(size.w + mx);
    let y1 = (cy + r).min(size.h + my);
    (x1 > x0 && y1 > y0).then(|| Region { x: x0.floor(), y: y0.floor(), w: (x1 - x0).ceil(), h: (y1 - y0).ceil() })
}

/// The device scale for `region`: the window's, reduced so no side passes [`MAX_EDGE`].
pub fn scale_for(region: Region, window_scale: f64) -> f64 {
    window_scale.min(MAX_EDGE / region.w.max(1.)).min(MAX_EDGE / region.h.max(1.))
}

/// How many strips to cut a raster into: one per core, at most 8, at least 32 rows each.
pub fn strips_for(height: u32) -> usize {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(8);
    cores.min((height / 32).max(1) as usize).max(1)
}

/// Stroke the scene's edges. Blocking and CPU-heavy: background threads only.
pub fn rasterize(job: &RasterJob) -> Option<RasterPixels> {
    let width = (job.region.w * job.scale).ceil() as u32;
    let height = (job.region.h * job.scale).ceil() as u32;
    if width == 0 || height == 0 {
        return None;
    }
    // Graph → device: canvas point minus the region's origin, times the scale.
    let (k, s) = (job.view.k, job.scale);
    let ox = ((job.size.w / 2.) * k + job.view.x - job.region.x) * s;
    let oy = ((job.size.h / 2.) * k + job.view.y - job.region.y) * s;
    let ks = k * s;
    let dev = |x: f64, y: f64| ((x * ks + ox) as f32, (y * ks + oy) as f32);

    let strips = job.strips.clamp(1, height as usize);
    let rows = height.div_ceil(strips as u32);
    let mut out = vec![0u8; width as usize * height as usize * 4];
    let chunks: Vec<&mut [u8]> = out.chunks_mut(rows as usize * width as usize * 4).collect();
    std::thread::scope(|scope| {
        for (i, chunk) in chunks.into_iter().enumerate() {
            let scene = &job.scene;
            scope.spawn(move || {
                let y0 = i as u32 * rows;
                let h = (chunk.len() / (width as usize * 4)) as u32;
                stroke_strip(scene, &dev, width, y0, h, s as f32, chunk);
            });
        }
    });
    Some(RasterPixels { width, height, bgra: out })
}

/// Stroke one strip (`y0..y0+h` device rows) into `out` as straight-alpha BGRA.
fn stroke_strip(
    scene: &Scene,
    dev: &(impl Fn(f64, f64) -> (f32, f32) + Sync),
    width: u32,
    y0: u32,
    h: u32,
    scale: f32,
    out: &mut [u8],
) {
    // Each strip is stroked with OVERLAP extra rows above and below, and only its own rows are
    // kept: tiny-skia's anti-aliasing of the first rows of a pixmap differs from the same rows
    // mid-pixmap, which showed as seams.
    let top_pad = y0.min(OVERLAP);
    let Some(mut pixmap) = Pixmap::new(width, h + top_pad + OVERLAP) else { return };
    let y0 = y0 - top_pad;
    // Miter joins reach up to 2 widths past the path (limit 4); round caps half a width.
    let reach = 2. * scale + 2.;
    let (top, bottom) = (y0 as f32 - reach, (y0 + h + top_pad + OVERLAP) as f32 + reach);
    let stroke = Stroke { width: scale, line_cap: LineCap::Round, ..Default::default() };
    let shift = Transform::from_translate(0., -(y0 as f32));
    for bucket in &scene.buckets {
        let mut pb = PathBuilder::new();
        for &e in &bucket.edges {
            let edge = &scene.edges[e as usize];
            let (bx0, by0) = dev(edge.bbox.0, edge.bbox.1);
            let (bx1, by1) = dev(edge.bbox.2, edge.bbox.3);
            if by1 < top || by0 > bottom || bx1 < -reach || bx0 > width as f32 + reach {
                continue;
            }
            for seg in &edge.path {
                match *seg {
                    Seg::Move(x, y) => {
                        let (x, y) = dev(x, y);
                        pb.move_to(x, y)
                    }
                    Seg::Line(x, y) => {
                        let (x, y) = dev(x, y);
                        pb.line_to(x, y)
                    }
                    Seg::Cubic(x1, y1, x2, y2, x, y) => {
                        let (x1, y1) = dev(x1, y1);
                        let (x2, y2) = dev(x2, y2);
                        let (x, y) = dev(x, y);
                        pb.cubic_to(x1, y1, x2, y2, x, y)
                    }
                }
            }
        }
        let Some(path) = pb.finish() else { continue };
        let c = bucket.color;
        let mut paint = Paint::default();
        paint.anti_alias = true;
        paint.set_color(
            Color::from_rgba((c >> 16 & 0xff) as f32 / 255., (c >> 8 & 0xff) as f32 / 255., (c & 0xff) as f32 / 255., bucket.alpha as f32)
                .unwrap_or(Color::TRANSPARENT),
        );
        pixmap.stroke_path(&path, &paint, &stroke, shift, None);
    }
    let own = &pixmap.pixels()[(top_pad * width) as usize..];
    for (px, dst) in own.iter().zip(out.chunks_exact_mut(4)) {
        let c = px.demultiply();
        dst.copy_from_slice(&[c.blue(), c.green(), c.red(), c.alpha()]);
    }
}

/// The pixels as a GPUI image.
pub fn to_render_image(pixels: RasterPixels) -> Arc<RenderImage> {
    let buf = image::RgbaImage::from_raw(pixels.width, pixels.height, pixels.bgra).expect("w*h*4 bytes");
    Arc::new(RenderImage::new(vec![image::Frame::new(buf)]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chairphoto_model::tag_graph::graph::Graph;
    use chairphoto_model::tag_graph::session::GraphSession;
    use chairphoto_model::tag_graph::synthetic::{library, Scale};

    const SIZE: Size = Size { w: 600., h: 500. };

    fn scene(scale: Scale) -> Arc<Scene> {
        let mut s = GraphSession::new();
        let g = s.begin_load();
        s.apply_load(g, Ok(Graph::build(&library(scale, 5))));
        s.toggle_tags();
        Arc::new(Scene::build(&s.take_scene_request().unwrap()))
    }

    fn job(scene: Arc<Scene>, strips: usize) -> RasterJob {
        let view = View::fit(SIZE);
        let region = region_for(view, SIZE).unwrap();
        RasterJob { scene, view, size: SIZE, region, scale: 1., strips }
    }

    #[test]
    fn the_region_is_the_rings_box_clipped_to_the_canvas_and_margin() {
        let fit = View::fit(SIZE);
        let r = region_for(fit, SIZE).unwrap();
        let rs = RING_R * fit.k + 4.;
        assert!((r.w - (2. * rs).ceil()).abs() <= 1. && r.x >= 0. && r.x + r.w <= SIZE.w + 1.);
        let near = fit.zoom_at(6., (300., 250.));
        let r = region_for(near, SIZE).unwrap();
        assert_eq!((r.x, r.y, r.w, r.h), (-150., -125., 900., 750.), "canvas plus a quarter each side");
        assert_eq!(region_for(View { k: 1., x: 5000., y: 0. }, SIZE), None, "the ring is off the canvas");
        assert_eq!(scale_for(Region { x: 0., y: 0., w: 8192., h: 100. }, 2.), 0.5);
    }

    #[test]
    fn edges_are_stroked_and_the_rest_stays_transparent() {
        let scene = scene(Scale { tags: 200, edges: 600, cameras: 0 });
        let px = rasterize(&job(scene, 1)).unwrap();
        let alpha: Vec<u8> = px.bgra.chunks_exact(4).map(|p| p[3]).collect();
        let lit = alpha.iter().filter(|&&a| a > 0).count();
        assert!(lit > alpha.len() / 50, "edges cover some of the ring: {lit} of {}", alpha.len());
        // The corners of the ring's box are outside the ring: nothing there.
        assert_eq!(alpha[0], 0);
        assert_eq!(*alpha.last().unwrap(), 0);
    }

    #[test]
    fn strips_join_without_seams() {
        let scene = scene(Scale { tags: 200, edges: 600, cameras: 0 });
        let one = rasterize(&job(scene.clone(), 1)).unwrap();
        let many = rasterize(&job(scene, 7)).unwrap();
        assert_eq!((one.width, one.height), (many.width, many.height));
        // Same picture up to anti-aliasing rounding (without the overlap rows, the first rows
        // of every strip differed by up to 194).
        let alpha = |p: &RasterPixels| p.bgra.chunks_exact(4).map(|c| c[3]).collect::<Vec<u8>>();
        let (a, b) = (alpha(&one), alpha(&many));
        let worst = a.iter().zip(&b).map(|(x, y)| x.abs_diff(*y)).max().unwrap();
        let lit = a.iter().filter(|&&x| x > 0).count();
        let off = a.iter().zip(&b).filter(|(x, y)| x.abs_diff(**y) > 2).count();
        eprintln!("strips: worst alpha difference {worst}, {off} of {lit} lit pixels off by more than 2");
        assert!(worst <= 8, "a strip changed a pixel's coverage by {worst}");
        assert!(off * 1000 < lit, "{off} of {lit} lit pixels differ");
    }

    #[test]
    fn colour_comes_out_as_straight_alpha_bgra() {
        use chairphoto_model::tag_graph::scene::{EdgeBucket, SceneEdge};
        let mut scene = (*scene(Scale { tags: 4, edges: 0, cameras: 0 })).clone();
        // One fat horizontal line through the middle, pure red at half alpha.
        scene.edges = vec![SceneEdge {
            source: scene.layout.order[0],
            target: scene.layout.order[1],
            source_color: 0,
            target_color: 0,
            path: vec![Seg::Move(-300., 0.), Seg::Line(300., 0.)],
            bbox: (-300., 0., 300., 0.),
        }];
        scene.buckets = vec![EdgeBucket { color: 0xFF0000, alpha: 0.5, edges: vec![0] }];
        let j = RasterJob { scale: 3., ..job(Arc::new(scene), 2) };
        let px = rasterize(&j).unwrap();
        let mid = ((px.height / 2) * px.width + px.width / 2) as usize * 4;
        let p = &px.bgra[mid..mid + 4];
        assert_eq!((p[0], p[1], p[2]), (0, 0, 255), "BGRA, demultiplied: {p:?}");
        assert!((p[3] as i32 - 128).abs() <= 2, "half alpha: {p:?}");
    }
}
