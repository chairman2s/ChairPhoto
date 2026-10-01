//! The Tag graph canvas (`drawBundle` in `tagGraph.tsx`): the base edge raster, then live on
//! the canvas — lit edges, community arcs, dots, labels, group names and the tooltip.
//!
//! Everything but the base raster is drawn every frame from a [`Frame`] the view snapshots
//! in `render`. Placed labels and the canvas's bounds go back to the view through the
//! shared [`PaintCache`] for hit testing. Lit edges are stroked as one GPUI path per edge
//! (a path past ~65k vertices fails to tessellate, `path_builder.rs`), cached until the
//! focus, the scene or the view changes.

use super::raster::Region;
use crate::shell::style::Colors;
use chairphoto_model::tag_graph::bundle::CAMERA_GROUP;
use chairphoto_model::tag_graph::graph::{NodeId, Scoped};
use chairphoto_model::tag_graph::labels::{place_labels, slot_point, PlacedLabel, LABEL_FONT, LABEL_H};
use chairphoto_model::tag_graph::scene::{Scene, Seg};
use chairphoto_model::tag_graph::view::{Size, View};
use chairphoto_model::tag_graph::{truncate, Rgb, LABEL_EXTENT};
use gpui_kit::{
    fill, point, px, quad, App, BorderStyle, Bounds, FontWeight, Hsla, Path, PathBuilder, Pixels, Point,
    RenderImage, SharedString, TextAlign, TextRun, Window,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// `0xRRGGBB` at `alpha`.
pub fn rgb(c: Rgb, alpha: f32) -> Hsla {
    let mut h: Hsla = gpui_kit::rgb(c).into();
    h.a = alpha;
    h
}

/// The base raster on screen: the image, where it was made, and under which view.
#[derive(Clone)]
pub struct RasterShown {
    pub image: Arc<RenderImage>,
    pub region: Region,
    pub view: View,
    pub scene_generation: u64,
}

/// What one frame of the canvas shows; snapshotted by the view in `render`.
pub struct Frame {
    pub scene: Arc<Scene>,
    pub scoped: Arc<Scoped>,
    pub view: View,
    pub hover: Option<NodeId>,
    pub selected: Option<NodeId>,
    pub emph: Option<NodeId>,
    pub emph_neighbors: Vec<NodeId>,
    pub focus: Option<HashSet<NodeId>>,
    pub dimmed: HashSet<NodeId>,
    /// `(name, colour, dimmed)` per ring group.
    pub groups: Vec<(String, Rgb, bool)>,
    pub raster: Option<RasterShown>,
    pub colors: Colors,
}

/// Lit edges built for one focus/scene/view/origin.
type LitKey = (u64, Vec<NodeId>, [u64; 3], [i32; 2]);

/// State the canvas shares with its view across frames.
#[derive(Default)]
pub struct PaintCache {
    /// The canvas's bounds at the last prepaint, window coordinates.
    pub bounds: Option<Bounds<Pixels>>,
    pub scale: f32,
    /// The labels of the last frame (label-zone hit testing).
    pub labels: Vec<PlacedLabel>,
    widths: HashMap<NodeId, f64>,
    widths_scene: u64,
    lit: Option<(LitKey, Vec<(Path<Pixels>, Hsla)>)>,
    prepaint_took: Duration,
    /// Prepaint + paint time of recent frames, and when each was painted (the bench reads
    /// both).
    pub timings: Vec<Duration>,
    pub painted_at: Vec<Instant>,
}

impl PaintCache {
    pub fn origin(&self) -> Point<Pixels> {
        self.bounds.map(|b| b.origin).unwrap_or_default()
    }

    pub fn canvas_size(&self) -> Option<Size> {
        self.bounds.map(|b| Size { w: f64::from(b.size.width), h: f64::from(b.size.height) })
    }

    fn record(&mut self, d: Duration) {
        if self.timings.len() >= 4096 {
            self.timings.drain(..2048);
            self.painted_at.drain(..2048);
        }
        self.timings.push(d);
        self.painted_at.push(Instant::now());
    }
}

fn label_font(window: &Window, weight: FontWeight) -> gpui_kit::Font {
    let mut font = window.text_style().font();
    font.weight = weight;
    font
}

fn shape(text: &str, weight: FontWeight, color: Hsla, window: &Window) -> gpui_kit::ShapedLine {
    let run = TextRun {
        len: text.len(),
        font: label_font(window, weight),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window.text_system().shape_line(SharedString::from(text.to_string()), px(LABEL_FONT as f32), &[run], None)
}

/// Prepaint: lay out the labels (measuring their text), record the bounds.
pub fn prepaint(bounds: Bounds<Pixels>, frame: &Frame, cache: &mut PaintCache, window: &mut Window) {
    let started = Instant::now();
    cache.bounds = Some(bounds);
    cache.scale = window.scale_factor();
    let size = Size { w: f64::from(bounds.size.width), h: f64::from(bounds.size.height) };
    if cache.widths_scene != frame.scene.generation {
        cache.widths.clear();
        cache.widths_scene = frame.scene.generation;
    }
    let scoped = &frame.scoped;
    let widths = &mut cache.widths;
    cache.labels = place_labels(&frame.scene, frame.view, size, frame.emph, &frame.emph_neighbors, |id| {
        *widths.entry(id).or_insert_with(|| {
            let text = scoped.node(id).map(|n| truncate(&n.label)).unwrap_or_default();
            f64::from(shape(&text, FontWeight::MEDIUM, Hsla::default(), window).width())
        })
    });
    cache.prepaint_took = started.elapsed();
}

fn lit_paths(frame: &Frame, size: Size, origin: Point<Pixels>, focus: &HashSet<NodeId>) -> Vec<(Path<Pixels>, Hsla)> {
    let v = frame.view;
    let at = |x: f64, y: f64| {
        let (sx, sy) = v.to_screen((x, y), size);
        origin + point(px(sx as f32), px(sy as f32))
    };
    let mut out = Vec::new();
    for e in frame.scene.lit_edges(focus) {
        let edge = &frame.scene.edges[e as usize];
        let mut pb = PathBuilder::stroke(px(1.5));
        for seg in &edge.path {
            match *seg {
                Seg::Move(x, y) => pb.move_to(at(x, y)),
                Seg::Line(x, y) => pb.line_to(at(x, y)),
                Seg::Cubic(x1, y1, x2, y2, x, y) => pb.cubic_bezier_to(at(x, y), at(x1, y1), at(x2, y2)),
            }
        }
        if let Ok(path) = pb.build() {
            out.push((path, rgb(edge.lit_color(|id| focus.contains(&id)), 0.85)));
        }
    }
    out
}

/// Paint the canvas.
pub fn paint(bounds: Bounds<Pixels>, frame: &Frame, cache: &mut PaintCache, window: &mut Window, cx: &mut App) {
    let started = Instant::now();
    let origin = bounds.origin;
    let size = Size { w: f64::from(bounds.size.width), h: f64::from(bounds.size.height) };
    let v = frame.view;
    let c = frame.colors;
    let at = |p: (f64, f64)| origin + point(px(p.0 as f32), px(p.1 as f32));
    let layout = &frame.scene.layout;
    let (cx_, cy_) = v.centre(size);
    let rs = layout.r * v.k;

    window.with_content_mask(Some(gpui_kit::ContentMask { bounds }), |window| {
        // ── Base edges: the raster, reprojected from the view it was made under. ──
        if let Some(r) = &frame.raster {
            let s = v.k / r.view.k;
            let o = v.reproject(r.view, (r.region.x, r.region.y));
            let image_bounds = Bounds::new(at(o), gpui_kit::size(px((r.region.w * s) as f32), px((r.region.h * s) as f32)));
            let _ = window.paint_image(bounds, image_bounds, (0.).into(), r.image.clone(), 0, false);
            if frame.focus.is_some() {
                // Focus dims the base edges to ×0.2: the canvas colour at 0.8 over them.
                window.paint_quad(fill(bounds, c.canvas.opacity(0.8)));
            }
        }

        // ── Lit edges, coloured by their far end. ──
        if let Some(focus) = &frame.focus {
            let mut ids: Vec<NodeId> = focus.iter().copied().collect();
            ids.sort();
            let key: LitKey = (
                frame.scene.generation,
                ids,
                [v.k.to_bits(), v.x.to_bits(), v.y.to_bits()],
                [f32::from(origin.x).round() as i32, f32::from(origin.y).round() as i32],
            );
            if cache.lit.as_ref().is_none_or(|(k, _)| *k != key) {
                cache.lit = Some((key, lit_paths(frame, size, origin, focus)));
            }
            for (path, color) in &cache.lit.as_ref().unwrap().1 {
                window.paint_path(path.clone(), *color);
            }
        }

        // ── Community arcs, 3 px at R + 6. ──
        for (g, (_, color, dim)) in layout.groups.iter().zip(&frame.groups) {
            let r = rs + 6.;
            let steps = (((g.a1 - g.a0) * r / 4.).ceil() as usize).clamp(2, 720);
            let mut pb = PathBuilder::stroke(px(3.));
            for i in 0..=steps {
                let a = g.a0 + (g.a1 - g.a0) * i as f64 / steps as f64;
                let p = at((cx_ + r * a.cos(), cy_ + r * a.sin()));
                if i == 0 {
                    pb.move_to(p)
                } else {
                    pb.line_to(p)
                }
            }
            if let Ok(path) = pb.build() {
                window.paint_path(path, rgb(*color, if *dim { 0.25 } else { 0.9 }));
            }
        }

        // ── Dots. ──
        let mut hovered = None;
        for id in &layout.order {
            if !frame.scene.is_shown(*id) {
                continue;
            }
            let Some(n) = frame.scoped.node(*id) else { continue };
            let p = layout.placement[id];
            let r = 1.5 + 4.5 * (n.count as f64 / layout.max_count as f64).sqrt();
            let (sx, sy) = (cx_ + rs * p.angle.cos(), cy_ + rs * p.angle.sin());
            if sx < -r || sy < -r || sx > size.w + r || sy > size.h + r {
                continue;
            }
            if Some(*id) == frame.hover {
                hovered = Some((n, sx, sy, r));
            }
            let alpha = if frame.dimmed.contains(id) { 0.18 } else { 1. };
            let color = rgb(frame.scoped.node_color(*id), alpha);
            let ring = Some(*id) == frame.selected || Some(*id) == frame.hover;
            let b = Bounds::new(at((sx - r, sy - r)), gpui_kit::size(px((2. * r) as f32), px((2. * r) as f32)));
            window.paint_quad(quad(
                b,
                px(r as f32),
                color,
                px(if ring { 1.5 } else { 0. }),
                gpui_kit::white(),
                BorderStyle::Solid,
            ));
        }

        // ── Labels: horizontal, on a canvas-coloured plate (the React halo). ──
        for l in &cache.labels {
            let Some(n) = frame.scoped.node(l.id) else { continue };
            let dim = frame.dimmed.contains(&l.id) && !l.emph;
            let color = if l.emph { rgb(frame.scoped.node_color(l.id), 1.) } else { c.txt.opacity(if dim { 0.3 } else { 1. }) };
            let weight = if l.emph { FontWeight::BOLD } else { FontWeight::MEDIUM };
            let line = shape(&truncate(&n.label), weight, color, window);
            let w = f64::from(line.width());
            let x = if l.right_aligned { l.x + l.w - w } else { l.x };
            window.paint_quad(
                fill(Bounds::new(at((x - 2., l.y)), gpui_kit::size(px((w + 4.) as f32), px(l.h as f32))), c.canvas.opacity(0.75))
                    .corner_radii(px(3.)),
            );
            let _ = line.paint(at((x, l.y)), px(LABEL_H as f32), TextAlign::Left, None, window, cx);
        }

        // ── Group names, horizontal at the arc midpoints, where the arc can carry them. ──
        for (g, (name, color, dim)) in layout.groups.iter().zip(&frame.groups) {
            let text = if name == CAMERA_GROUP { "Cameras" } else { name.as_str() };
            let line = shape(text, FontWeight::BOLD, rgb(*color, if *dim { 0.3 } else { 0.95 }), window);
            let w = f64::from(line.width());
            if (g.a1 - g.a0) * rs < w + 10. {
                continue;
            }
            let mid = (g.a0 + g.a1) / 2.;
            let (gx, gy) = slot_point(v, size, mid, LABEL_EXTENT + 10.);
            let (x, y) = (gx - w / 2., gy - LABEL_H / 2.);
            window.paint_quad(
                fill(Bounds::new(at((x - 3., y)), gpui_kit::size(px((w + 6.) as f32), px(LABEL_H as f32))), c.canvas.opacity(0.75))
                    .corner_radii(px(3.)),
            );
            let _ = line.paint(at((x, y)), px(LABEL_H as f32), TextAlign::Left, None, window, cx);
        }

        // ── Tooltip beside the hovered node. ──
        if let Some((n, sx, sy, r)) = hovered {
            let text = format!("{} · {} photo(s)", n.full_path, n.count);
            let line = window.text_system().shape_line(
                SharedString::from(text.clone()),
                px(12.),
                &[TextRun {
                    len: text.len(),
                    font: label_font(window, FontWeight::MEDIUM),
                    color: c.txt,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }],
                None,
            );
            let w = f64::from(line.width());
            let tx = (sx + 10.).max(4.).min((size.w - w - 12.).max(4.));
            let ty = (sy - r - 10.).max(16.).min(size.h - 8.);
            let b = Bounds::new(at((tx - 6., ty - 13.)), gpui_kit::size(px((w + 12.) as f32), px(19.)));
            window.paint_quad(quad(b, px(0.), c.canvas.opacity(0.92), px(1.), c.border, BorderStyle::Solid));
            let _ = line.paint(at((tx, ty - 13.)), px(19.), TextAlign::Left, None, window, cx);
        }
    });
    let took = cache.prepaint_took + started.elapsed();
    cache.record(took);
}
