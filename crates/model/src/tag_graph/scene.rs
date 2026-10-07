//! What the canvas draws, computed off the UI thread: the ring layout plus every drawn edge's
//! bundled curve, grouped into the base layer's (colour, alpha) buckets (`bundle` and the
//! edge cache of `drawBundle` in `tagGraph.tsx`).
//!
//! A [`SceneInput`] is a snapshot of what the layout depends on (the shown kinds' nodes with
//! their branch-relative paths, the drawn links, the isolation set, the colours); building it
//! into a [`Scene`] is pure and can take tens of milliseconds at library scale, so the view
//! runs it on a background thread and drops a scene a newer input superseded (its
//! `generation`).

use super::bundle::{build_bundle_layout, bundle_path, BundleLayout, BundleNode, PathSink};
use super::graph::NodeId;
use super::{Rgb, BUNDLE_BETA, CAMERA_COLOR, RING_R};
use std::collections::{HashMap, HashSet};

/// One drawn link, as the scene needs it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DrawLink {
    pub source: NodeId,
    pub target: NodeId,
    pub weight: i64,
    /// A camera ↔ tag link: amber whatever its ends.
    pub camera: bool,
}

/// Everything a scene is built from.
#[derive(Debug, Clone, Default)]
pub struct SceneInput {
    pub generation: u64,
    /// The ring nodes: the shown kinds, tags with branch-relative paths.
    pub nodes: Vec<BundleNode<NodeId>>,
    /// Each ring node's colour (`nodeColor`).
    pub colors: HashMap<NodeId, Rgb>,
    /// Co-occurrence links at or above the strength threshold, and camera links (hierarchy is
    /// drawn as ring adjacency, never as an edge).
    pub links: Vec<DrawLink>,
    /// The isolated neighbourhood: only these nodes and the edges between them are drawn.
    pub isolate: Option<HashSet<NodeId>>,
}

/// One path segment of a bundled curve, graph units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Seg {
    Move(f64, f64),
    Line(f64, f64),
    Cubic(f64, f64, f64, f64, f64, f64),
}

/// A drawn edge.
#[derive(Debug, Clone, PartialEq)]
pub struct SceneEdge {
    pub source: NodeId,
    pub target: NodeId,
    /// The colour of each end — a lit edge takes the far end's.
    pub source_color: Rgb,
    pub target_color: Rgb,
    pub path: Vec<Seg>,
    /// `(min x, min y, max x, max y)` of the path's points and control points.
    pub bbox: (f64, f64, f64, f64),
}

impl SceneEdge {
    /// The colour of this edge lit from `focus`: its far end's.
    pub fn lit_color(&self, focus: impl Fn(NodeId) -> bool) -> Rgb {
        if focus(self.source) {
            self.target_color
        } else {
            self.source_color
        }
    }
}

/// The base layer's edges of one colour and alpha.
#[derive(Debug, Clone, PartialEq)]
pub struct EdgeBucket {
    pub color: Rgb,
    /// `0.05 + 0.35·log1p(w)/log1p(max w)`, rounded to steps of 1/50.
    pub alpha: f64,
    /// Indices into [`Scene::edges`].
    pub edges: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct Scene {
    pub generation: u64,
    pub layout: BundleLayout<NodeId>,
    pub edges: Vec<SceneEdge>,
    pub buckets: Vec<EdgeBucket>,
    /// Edge indices per end, for the lit edges of a focus.
    incident: HashMap<NodeId, Vec<u32>>,
    /// The nodes drawn: on the ring and inside the isolation set, if any.
    shown: HashSet<NodeId>,
}

struct SegSink<'a>(&'a mut Vec<Seg>);

impl PathSink for SegSink<'_> {
    fn move_to(&mut self, x: f64, y: f64) {
        self.0.push(Seg::Move(x, y));
    }
    fn line_to(&mut self, x: f64, y: f64) {
        self.0.push(Seg::Line(x, y));
    }
    fn bezier_curve_to(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, x: f64, y: f64) {
        self.0.push(Seg::Cubic(x1, y1, x2, y2, x, y));
    }
}

fn bbox(path: &[Seg]) -> (f64, f64, f64, f64) {
    let mut b = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
    let mut add = |x: f64, y: f64| {
        b = (b.0.min(x), b.1.min(y), b.2.max(x), b.3.max(y));
    };
    for s in path {
        match *s {
            Seg::Move(x, y) | Seg::Line(x, y) => add(x, y),
            Seg::Cubic(x1, y1, x2, y2, x, y) => {
                add(x1, y1);
                add(x2, y2);
                add(x, y);
            }
        }
    }
    b
}

impl Scene {
    /// Lay the ring out and route every drawn edge. Pure; blocking for large inputs.
    pub fn build(input: &SceneInput) -> Scene {
        let layout = build_bundle_layout(&input.nodes, RING_R);
        let count: HashMap<NodeId, i64> = input.nodes.iter().map(|n| (n.id, n.count)).collect();
        let shown: HashSet<NodeId> = layout
            .order
            .iter()
            .copied()
            .filter(|id| input.isolate.as_ref().is_none_or(|s| s.contains(id)))
            .collect();
        let color = |id: NodeId| input.colors.get(&id).copied().unwrap_or(CAMERA_COLOR);

        // Alpha follows log weight so the strong pairs read and the long tail stays a haze;
        // the maximum is over every drawn link, as React's.
        let max_w = input.links.iter().map(|l| l.weight).fold(1, i64::max);
        let ln_max = (max_w as f64).ln_1p();
        let mut edges = Vec::new();
        let mut buckets: Vec<EdgeBucket> = Vec::new();
        let mut bucket_of: HashMap<(Rgb, u64), usize> = HashMap::new();
        let mut incident: HashMap<NodeId, Vec<u32>> = HashMap::new();
        for l in &input.links {
            if !shown.contains(&l.source) || !shown.contains(&l.target) {
                continue;
            }
            let Some(pts) = layout.path(&l.source, &l.target) else { continue };
            let mut path = Vec::with_capacity(pts.len() + 2);
            bundle_path(&mut SegSink(&mut path), &pts, BUNDLE_BETA);
            // Colour follows the heavier end, so a bundle out of one family is one hue.
            let heavy = if count[&l.source] >= count[&l.target] { l.source } else { l.target };
            let c = if l.camera { CAMERA_COLOR } else { color(heavy) };
            let alpha = crate::js_compat::round((0.05 + 0.35 * ((l.weight as f64).ln_1p() / ln_max)) * 50.) / 50.;
            let i = edges.len() as u32;
            let b = *bucket_of.entry((c, alpha.to_bits())).or_insert_with(|| {
                buckets.push(EdgeBucket { color: c, alpha, edges: Vec::new() });
                buckets.len() - 1
            });
            buckets[b].edges.push(i);
            incident.entry(l.source).or_default().push(i);
            if l.target != l.source {
                incident.entry(l.target).or_default().push(i);
            }
            let bbox = bbox(&path);
            edges.push(SceneEdge {
                source: l.source,
                target: l.target,
                source_color: color(l.source),
                target_color: color(l.target),
                path,
                bbox,
            });
        }
        Scene { generation: input.generation, layout, edges, buckets, incident, shown }
    }

    /// Whether a ring node is drawn (`nodeShown`: on the ring, inside the isolation set).
    pub fn is_shown(&self, id: NodeId) -> bool {
        self.shown.contains(&id)
    }

    /// The edges lit by `focus`: every drawn edge with an end in it, each once, in edge order.
    pub fn lit_edges(&self, focus: &HashSet<NodeId>) -> Vec<u32> {
        let mut out: Vec<u32> = focus.iter().filter_map(|id| self.incident.get(id)).flatten().copied().collect();
        out.sort_unstable();
        out.dedup();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::bundle::BundleKind;
    use super::*;

    fn tag(id: i64, path: &str, count: i64) -> BundleNode<NodeId> {
        BundleNode { id: NodeId::Tag(id), kind: BundleKind::Tag, full_path: path.into(), count }
    }

    fn input() -> SceneInput {
        let nodes = vec![tag(1, "A/x", 10), tag(2, "A/y", 3), tag(3, "B/z", 7)];
        let colors = [(NodeId::Tag(1), 0x111111), (NodeId::Tag(2), 0x222222), (NodeId::Tag(3), 0x333333)].into();
        let links = vec![
            DrawLink { source: NodeId::Tag(1), target: NodeId::Tag(2), weight: 1, camera: false },
            DrawLink { source: NodeId::Tag(2), target: NodeId::Tag(3), weight: 20, camera: false },
            // An end that is not on the ring (a hidden kind): not drawn.
            DrawLink { source: NodeId::Camera(0), target: NodeId::Tag(3), weight: 5, camera: true },
        ];
        SceneInput { generation: 7, nodes, colors, links, isolate: None }
    }

    #[test]
    fn edges_take_the_heavier_ends_colour_and_a_log_weight_alpha() {
        let scene = Scene::build(&input());
        assert_eq!(scene.generation, 7);
        assert_eq!(scene.edges.len(), 2, "the camera link has no ring slot to start from");
        let alpha = |w: f64| crate::js_compat::round((0.05 + 0.35 * (w.ln_1p() / 20f64.ln_1p())) * 50.) / 50.;
        assert_eq!(
            scene.buckets.iter().map(|b| (b.color, b.alpha, b.edges.clone())).collect::<Vec<_>>(),
            [(0x111111, alpha(1.), vec![0]), (0x333333, 0.4, vec![1])]
        );
        let e = &scene.edges[1];
        assert!(matches!(e.path[0], Seg::Move(..)));
        assert!(e.bbox.0 <= e.bbox.2 && e.bbox.1 <= e.bbox.3);
    }

    #[test]
    fn isolation_hides_nodes_outside_the_set_and_their_edges() {
        let mut i = input();
        i.isolate = Some([NodeId::Tag(2), NodeId::Tag(3)].into());
        let scene = Scene::build(&i);
        assert!(!scene.is_shown(NodeId::Tag(1)));
        assert_eq!(scene.layout.order.len(), 3, "isolation does not move the ring");
        assert_eq!(scene.edges.len(), 1);
        assert_eq!((scene.edges[0].source, scene.edges[0].target), (NodeId::Tag(2), NodeId::Tag(3)));
    }

    #[test]
    fn lit_edges_are_every_edge_touching_the_focus_coloured_by_the_far_end() {
        let scene = Scene::build(&input());
        let focus: HashSet<NodeId> = [NodeId::Tag(2)].into();
        assert_eq!(scene.lit_edges(&focus), [0, 1]);
        assert_eq!(scene.edges[0].lit_color(|id| focus.contains(&id)), 0x111111);
        assert_eq!(scene.edges[1].lit_color(|id| focus.contains(&id)), 0x333333);
        let both: HashSet<NodeId> = [NodeId::Tag(2), NodeId::Tag(3)].into();
        assert_eq!(scene.lit_edges(&both), [0, 1], "an edge inside the focus is lit once");
    }
}
