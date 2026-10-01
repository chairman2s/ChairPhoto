//! Ring labels and hit testing, in canvas pixels.
//!
//! **Labels are horizontal** (owner decision in #120: gpui-pre 0.3.7 cannot rotate text).
//! React rotated each label along its radius and kept them apart by an angular gap; here a
//! label sits 12 px outside its slot along the radius, left-aligned on the right half of the
//! ring and right-aligned on the left half, and two labels may not overlap as rectangles. The
//! greedy order is React's: by count, at most 500, then the hovered/selected label forced and
//! its neighbours where they fit. Only labels that reach into the canvas compete, so zooming
//! in makes room for more of them.
//!
//! **Hit testing** keeps React's ring rule (nearest slot by angle, within the slot or 5 px),
//! and the label zone tests the labels just placed as rectangles instead of by angle.

use super::graph::NodeId;
use super::scene::Scene;
use super::view::{Size, View};
use std::collections::{HashMap, HashSet};

/// Label font size, px.
pub const LABEL_FONT: f64 = 11.;
/// A label's box height, px.
pub const LABEL_H: f64 = 14.;
/// The gap between a slot and its label, px along the radius.
pub const LABEL_GAP: f64 = 12.;
/// At most this many labels from the by-count pass.
pub const MAX_LABELS: usize = 500;
/// Horizontal breathing room between two labels, px.
const PAD: f64 = 3.;

/// A placed label: its box on the canvas.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlacedLabel {
    pub id: NodeId,
    /// The box: left, top, width, height.
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// On the ring's left half: the text ends at the box's right edge, next to the slot.
    pub right_aligned: bool,
    /// The hovered or selected node's label.
    pub emph: bool,
}

impl PlacedLabel {
    pub fn contains(&self, p: (f64, f64)) -> bool {
        p.0 >= self.x && p.0 <= self.x + self.w && p.1 >= self.y && p.1 <= self.y + self.h
    }

    fn overlaps(&self, x: f64, y: f64, w: f64, h: f64) -> bool {
        x < self.x + self.w + PAD && self.x < x + w + PAD && y < self.y + self.h && self.y < y + h
    }
}

/// The canvas position of a ring slot at `angle`, `extra` px outside the ring.
pub fn slot_point(view: View, size: Size, angle: f64, extra: f64) -> (f64, f64) {
    let (cx, cy) = view.centre(size);
    let r = super::RING_R * view.k + extra;
    (cx + r * angle.cos(), cy + r * angle.sin())
}

/// Place the labels (`tryLabel` in `drawBundle`). `width` measures a node's label text in px.
pub fn place_labels(
    scene: &Scene,
    view: View,
    size: Size,
    emph: Option<NodeId>,
    emph_neighbors: &[NodeId],
    mut width: impl FnMut(NodeId) -> f64,
) -> Vec<PlacedLabel> {
    let layout = &scene.layout;
    let mut placed: Vec<PlacedLabel> = Vec::new();
    let mut labelled: HashSet<NodeId> = HashSet::new();
    // Placed boxes by row band, so a candidate checks only its neighbours.
    let mut rows: HashMap<i64, Vec<usize>> = HashMap::new();
    let row = |y: f64| (y / LABEL_H).floor() as i64;
    let mut try_label = |id: NodeId, force: bool, placed: &mut Vec<PlacedLabel>, emph_flag: bool| {
        if labelled.contains(&id) || !scene.is_shown(id) {
            return;
        }
        let Some(p) = layout.placement.get(&id) else { return };
        let (ax, ay) = slot_point(view, size, p.angle, LABEL_GAP);
        let right_aligned = p.angle.cos() < 0.;
        let w = width(id);
        let (x, y, h) = (if right_aligned { ax - w } else { ax }, ay - LABEL_H / 2., LABEL_H);
        if !force {
            if x + w < 0. || x > size.w || y + h < 0. || y > size.h {
                return; // off the canvas: it would only take room
            }
            let r = row(y);
            for band in r - 1..=r + 1 {
                if rows.get(&band).is_some_and(|ix| ix.iter().any(|&i| placed[i].overlaps(x, y, w, h))) {
                    return;
                }
            }
        }
        rows.entry(row(y)).or_default().push(placed.len());
        placed.push(PlacedLabel { id, x, y, w, h, right_aligned, emph: emph_flag });
        labelled.insert(id);
    };
    for &id in &layout.label_order {
        if placed.len() >= MAX_LABELS {
            break;
        }
        try_label(id, false, &mut placed, false);
    }
    if let Some(e) = emph {
        if let Some(i) = placed.iter().position(|l| l.id == e) {
            placed[i].emph = true;
        } else {
            try_label(e, true, &mut placed, true);
        }
        for &nb in emph_neighbors {
            try_label(nb, false, &mut placed, false);
        }
    }
    placed
}

/// `angleDiff`: an angle difference normalised into (-π, π].
pub fn angle_diff(a: f64, b: f64) -> f64 {
    use std::f64::consts::PI;
    let d = (a - b) % (PI * 2.);
    if d > PI {
        d - PI * 2.
    } else if d <= -PI {
        d + PI * 2.
    } else {
        d
    }
}

/// The node under canvas point `p` (`hitTest` in bundle mode): within 8 px of the ring, the
/// nearest shown slot by angle (within half a slot or 5 px); further out, a label's box.
pub fn hit_test(scene: &Scene, view: View, size: Size, p: (f64, f64), labels: &[PlacedLabel]) -> Option<NodeId> {
    let layout = &scene.layout;
    let g = view.to_graph(p, size);
    let r = g.0.hypot(g.1);
    let ring_tol = 8. / view.k;
    if r < layout.r - ring_tol {
        return None;
    }
    if r > layout.r + ring_tol {
        return labels.iter().rev().find(|l| l.contains(p)).map(|l| l.id);
    }
    let a = g.1.atan2(g.0);
    let tol = (layout.slot / 2.).max(5. / (layout.r * view.k));
    let mut best = None;
    let mut best_d = f64::INFINITY;
    for id in &layout.order {
        if !scene.is_shown(*id) {
            continue;
        }
        let d = angle_diff(a, layout.placement[id].angle).abs();
        if d < best_d {
            best_d = d;
            best = Some(*id);
        }
    }
    if best_d <= tol {
        best
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::super::bundle::{BundleKind, BundleNode};
    use super::super::scene::SceneInput;
    use super::*;

    const SIZE: Size = Size { w: 1000., h: 1000. };

    fn ring(n: i64) -> Scene {
        let nodes = (0..n)
            .map(|i| BundleNode {
                id: NodeId::Tag(i),
                kind: BundleKind::Tag,
                full_path: format!("G{}/t{i}", i % 4),
                count: 100 - i,
            })
            .collect();
        Scene::build(&SceneInput { nodes, ..Default::default() })
    }

    fn no_overlaps(labels: &[PlacedLabel]) {
        for (i, a) in labels.iter().enumerate() {
            for b in &labels[i + 1..] {
                assert!(!a.overlaps(b.x, b.y, b.w, b.h) || a.emph || b.emph, "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn labels_never_overlap_and_take_the_biggest_counts_first() {
        let scene = ring(200);
        let view = View::fit(SIZE);
        let labels = place_labels(&scene, view, SIZE, None, &[], |_| 60.);
        assert!(!labels.is_empty() && labels.len() < 200, "{} labels", labels.len());
        no_overlaps(&labels);
        assert_eq!(labels[0].id, scene.layout.label_order[0], "the biggest tag is labelled first");
    }

    #[test]
    fn labels_sit_outside_the_ring_on_their_own_side() {
        let scene = ring(40);
        let view = View::fit(SIZE);
        let (cx, _) = view.centre(SIZE);
        for l in place_labels(&scene, view, SIZE, None, &[], |_| 50.) {
            let angle = scene.layout.placement[&l.id].angle;
            if angle.cos() < 0. {
                assert!(l.right_aligned && l.x + l.w <= cx, "{l:?} on the left half");
            } else {
                assert!(!l.right_aligned && l.x >= cx, "{l:?} on the right half");
            }
        }
    }

    #[test]
    fn the_emphasised_label_is_forced_and_its_neighbours_fill_in() {
        let scene = ring(200);
        let view = View::fit(SIZE);
        // The smallest tag never makes the by-count pass on a crowded ring …
        let last = *scene.layout.label_order.last().unwrap();
        let plain = place_labels(&scene, view, SIZE, None, &[], |_| 120.);
        assert!(!plain.iter().any(|l| l.id == last));
        // … but is forced in when hovered.
        let lit = place_labels(&scene, view, SIZE, Some(last), &[], |_| 120.);
        assert!(lit.iter().any(|l| l.id == last && l.emph));
    }

    #[test]
    fn more_labels_fit_when_zoomed_in() {
        let scene = ring(400);
        let fit = View::fit(SIZE);
        // Zoom ×4 about 3 o'clock, where horizontal labels stack.
        let near = fit.zoom_at(4., slot_point(fit, SIZE, 0., 0.));
        let on_canvas = |p: (f64, f64)| p.0 >= 0. && p.0 <= SIZE.w && p.1 >= 0. && p.1 <= SIZE.h;
        // The labels the fitted view gave the slots that are on the canvas once zoomed in …
        let far = place_labels(&scene, fit, SIZE, None, &[], |_| 60.)
            .iter()
            .filter(|l| on_canvas(slot_point(near, SIZE, scene.layout.placement[&l.id].angle, 0.)))
            .count();
        // … against the zoomed view's.
        let close = place_labels(&scene, near, SIZE, None, &[], |_| 60.);
        no_overlaps(&close);
        assert!(close.len() > 2 * far, "{} labels zoomed in vs {far} for the same slots", close.len());
        assert!(close.iter().all(|l| l.x + l.w >= 0. && l.x <= SIZE.w && l.y + l.h >= 0. && l.y <= SIZE.h));
    }

    #[test]
    fn hits_a_slot_by_angle_on_the_ring_and_a_label_by_its_box() {
        let scene = ring(12);
        let view = View::fit(SIZE);
        let id = scene.layout.order[3];
        let a = scene.layout.placement[&id].angle;
        assert_eq!(hit_test(&scene, view, SIZE, slot_point(view, SIZE, a, 0.), &[]), Some(id));
        assert_eq!(hit_test(&scene, view, SIZE, slot_point(view, SIZE, a, -20.), &[]), None, "inside the ring");
        let labels = place_labels(&scene, view, SIZE, None, &[], |_| 40.);
        let l = labels.iter().find(|l| l.id == id).unwrap();
        let inside = (l.x + l.w / 2., l.y + l.h / 2.);
        assert_eq!(hit_test(&scene, view, SIZE, inside, &labels), Some(id));
        assert_eq!(hit_test(&scene, view, SIZE, inside, &[]), None, "an unlabelled slot is not hit out there");
    }

    #[test]
    fn angle_diff_wraps_into_minus_pi_to_pi() {
        use std::f64::consts::PI;
        assert!((angle_diff(0.1, 2. * PI - 0.1) - 0.2).abs() < 1e-12);
        assert!((angle_diff(PI, -PI)).abs() < 1e-12);
        assert!((angle_diff(-3., 3.) - (2. * PI - 6.)).abs() < 1e-12);
    }
}
