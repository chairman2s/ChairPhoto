//! Radial hierarchical edge bundling (Holten 2006) for the Tag Graph — a one-to-one port of
//! `src/modules/plugins/tagGraphBundle.ts`.
//!
//! Every tag with photos gets a slot on a ring, ordered by a depth-first walk of the tag
//! hierarchy so children sit next to their parent and each top-level community is a
//! contiguous arc. Internal hierarchy nodes are placed at inner radii by height (d3's
//! cluster layout), and an edge between two ring nodes is routed through their lowest
//! common ancestor, then smoothed with a straightened B-spline — so edges between the
//! same two families share a path and read as one bundle.
//!
//! Pure layout, no rendering: the view paints the result.
//!
//! **Port notes.** The TypeScript keyed nodes by string ids; here the id type is generic
//! (`I`), so the module uses its own node ids and the tests keep the TS strings. The tree is
//! an arena (`Vec<TreeNode>` with parent indices) instead of object references; the
//! `path` closure is [`BundleLayout::path`]. Geometry is `f64`, as JavaScript numbers are,
//! so the coordinates are the TypeScript's bit for bit. Sorts are stable in both languages,
//! and the comparators are the same, so every tie breaks by first appearance as before.

use std::collections::HashMap;
use std::f64::consts::PI;
use std::hash::Hash;

/// What a ring node is (`BundleNode.kind`). Only cameras are laid out differently: they
/// hang under [`CAMERA_GROUP`] instead of being split on `/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BundleKind {
    Tag,
    Camera,
}

/// A node to place on the ring.
#[derive(Debug, Clone, PartialEq)]
pub struct BundleNode<I> {
    pub id: I,
    pub kind: BundleKind,
    /// `"Animals/Bird/Seagull"` for tags; the model name for cameras.
    pub full_path: String,
    pub count: i64,
}

/// Where a ring node sits.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RingPlacement {
    /// Radians, canvas convention (0 = 3 o'clock, increasing clockwise).
    pub angle: f64,
    pub x: f64,
    pub y: f64,
}

/// One top-level community's arc.
#[derive(Debug, Clone, PartialEq)]
pub struct BundleGroup {
    /// Top-level community name, or [`CAMERA_GROUP`] for the cameras arc.
    pub name: String,
    /// Angular span of the group's slots, radians.
    pub a0: f64,
    pub a1: f64,
    /// Sum of member counts — the ordering key.
    pub total: i64,
}

/// The synthetic top-level group cameras hang under.
pub const CAMERA_GROUP: &str = "__camera";

/// A tag path relative to a branch root: `""` for the root itself, `"Bird/Seagull"` for a
/// descendant, `None` when the path is outside the branch. Segment-exact — `"Animals"` does
/// not contain `"AnimalsX"`.
pub fn relative_to_branch<'a>(path: &'a str, root_path: &str) -> Option<&'a str> {
    if path == root_path {
        return Some("");
    }
    path.strip_prefix(root_path).and_then(|rest| rest.strip_prefix('/'))
}

/// The parent path of a tag path, or `None` at the top level.
pub fn parent_path(path: &str) -> Option<&str> {
    path.rfind('/').map(|i| &path[..i])
}

// Slot gaps: between sibling subtrees and between top-level groups (in slot widths).
const SIBLING_GAP: f64 = 0.35;
const GROUP_GAP: f64 = 2.5;

/// A node of the layout tree (`TreeNode`): an index into [`BundleLayout::tree`].
#[derive(Debug, Clone)]
struct TreeNode {
    name: String,
    parent: Option<usize>,
    children: Vec<usize>,
    /// Graph node this tree node stands for (internal tags too; see the self leaf), as an
    /// index into the input.
    node: Option<usize>,
    /// Set on leaves only: the ring node drawn at this position.
    ring: Option<usize>,
    total: i64,
    height: u32,
    angle: f64,
    x: f64,
    y: f64,
}

fn mk(name: &str, parent: Option<usize>) -> TreeNode {
    TreeNode {
        name: name.to_string(),
        parent,
        children: Vec::new(),
        node: None,
        ring: None,
        total: 0,
        height: 0,
        angle: 0.,
        x: 0.,
        y: 0.,
    }
}

const ROOT: usize = 0;

/// The ring layout (`BundleLayout`).
#[derive(Debug, Clone)]
pub struct BundleLayout<I: Eq + Hash> {
    /// The ring radius, graph units.
    pub r: f64,
    /// Ring node ids in angular order.
    pub order: Vec<I>,
    pub placement: HashMap<I, RingPlacement>,
    /// Groups in ring order.
    pub groups: Vec<BundleGroup>,
    /// Angular width of one ring slot, radians.
    pub slot: f64,
    /// Ring node ids by count, descending — label priority.
    pub label_order: Vec<I>,
    pub max_count: i64,
    tree: Vec<TreeNode>,
    leaf_by_id: HashMap<I, usize>,
}

impl<I: Clone + Eq + Hash> BundleLayout<I> {
    /// Control polyline for an edge between two ring nodes: up from `a` to their lowest
    /// common ancestor and down to `b`. `None` if either id is not on the ring.
    pub fn path(&self, a: &I, b: &I) -> Option<Vec<(f64, f64)>> {
        let la = *self.leaf_by_id.get(a)?;
        let lb = *self.leaf_by_id.get(b)?;
        let mut up = Vec::new();
        let mut t = Some(la);
        while let Some(i) = t {
            up.push(i);
            t = self.tree[i].parent;
        }
        let mut down = Vec::new();
        let mut lca = None;
        let mut t = Some(lb);
        while let Some(i) = t {
            if up.contains(&i) {
                lca = Some(i);
                break;
            }
            down.push(i);
            t = self.tree[i].parent;
        }
        let lca = lca?;
        let mut pts = Vec::with_capacity(up.len() + down.len());
        for &i in &up {
            pts.push((self.tree[i].x, self.tree[i].y));
            if i == lca {
                break;
            }
        }
        for &i in down.iter().rev() {
            pts.push((self.tree[i].x, self.tree[i].y));
        }
        Some(pts)
    }
}

fn segments<I>(n: &BundleNode<I>) -> Vec<&str> {
    match n.kind {
        BundleKind::Camera => vec![CAMERA_GROUP, n.full_path.as_str()],
        BundleKind::Tag => n.full_path.split('/').collect(),
    }
}

/// Lay `nodes` out on a ring of radius `r` (`buildBundleLayout`).
pub fn build_bundle_layout<I: Clone + Eq + Hash>(nodes: &[BundleNode<I>], r: f64) -> BundleLayout<I> {
    let mut tree = vec![mk("", None)];

    // 1. Trie over paths. Order of first appearance is the tie-break for every sort.
    let mut child_by_name: HashMap<(usize, String), usize> = HashMap::new();
    for (ni, n) in nodes.iter().enumerate() {
        let mut t = ROOT;
        for seg in segments(n) {
            let key = (t, seg.to_string());
            let c = match child_by_name.get(&key) {
                Some(&c) => c,
                None => {
                    let c = tree.len();
                    tree.push(mk(seg, Some(t)));
                    tree[t].children.push(c);
                    child_by_name.insert(key, c);
                    c
                }
            };
            t = c;
        }
        tree[t].node = Some(ni);
    }

    // 2. A tag with photos AND children gets a "self" leaf in front of its children, so
    //    it takes a ring slot next to them while the tree node stays internal (the
    //    bundle waypoint for its subtree).
    fn walk(tree: &mut Vec<TreeNode>, t: usize) {
        let children = tree[t].children.clone();
        for c in children {
            walk(tree, c);
        }
        if let Some(node) = tree[t].node {
            if tree[t].children.is_empty() {
                tree[t].ring = Some(node);
            } else {
                let name = tree[t].name.clone();
                let mut own = mk(&name, Some(t));
                own.node = Some(node);
                own.ring = Some(node);
                let s = tree.len();
                tree.push(own);
                tree[t].children.insert(0, s);
            }
        }
    }
    walk(&mut tree, ROOT);

    // 3. Subtree totals and heights, bottom-up; then order children by total (self
    //    leaf first) so each arc reads big-to-small.
    fn measure<I>(tree: &mut Vec<TreeNode>, nodes: &[BundleNode<I>], t: usize) {
        if let Some(ring) = tree[t].ring {
            tree[t].total = nodes[ring].count;
            tree[t].height = 0;
            return;
        }
        let mut total = 0;
        let mut height = 0;
        let children = tree[t].children.clone();
        for &c in &children {
            measure(tree, nodes, c);
            total += tree[c].total;
            height = height.max(tree[c].height + 1);
        }
        tree[t].total = total;
        tree[t].height = height;
        let own = tree[t].node;
        let is_self = |tree: &Vec<TreeNode>, c: usize| tree[c].ring.is_some() && tree[c].node == own;
        let mut sorted = children;
        sorted.sort_by(|&a, &b| {
            if is_self(tree, a) {
                return std::cmp::Ordering::Less;
            }
            if is_self(tree, b) {
                return std::cmp::Ordering::Greater;
            }
            tree[b].total.cmp(&tree[a].total)
        });
        tree[t].children = sorted;
    }
    measure(&mut tree, nodes, ROOT);

    // 4. Ring slots: depth-first leaf order, with gaps where the parent changes.
    let mut leaves = Vec::new();
    collect_leaves(&tree, ROOT, &mut leaves);

    let top_of = |t: usize| {
        let mut n = t;
        while let Some(p) = tree[n].parent {
            if p == ROOT {
                break;
            }
            n = p;
        }
        n
    };

    let mut cursor_for = Vec::with_capacity(leaves.len());
    let mut cursor = 0.0;
    let mut prev: Option<usize> = None;
    for &leaf in &leaves {
        if let Some(p) = prev {
            if top_of(p) != top_of(leaf) {
                cursor += GROUP_GAP;
            } else if tree[p].parent != tree[leaf].parent {
                cursor += SIBLING_GAP;
            }
        }
        cursor_for.push(cursor);
        cursor += 1.0;
        prev = Some(leaf);
    }
    // Close the ring with a group gap so the first and last groups don't touch.
    let total_slots = if leaves.is_empty() { 1.0 } else { cursor + GROUP_GAP };
    let slot = (PI * 2.0) / total_slots;
    let start = -PI / 2.0; // first group begins at 12 o'clock

    let mut placement = HashMap::with_capacity(leaves.len());
    let mut order = Vec::with_capacity(leaves.len());
    for (i, &leaf) in leaves.iter().enumerate() {
        let angle = start + (cursor_for[i] + 0.5) * slot;
        let t = &mut tree[leaf];
        t.angle = angle;
        t.x = r * angle.cos();
        t.y = r * angle.sin();
        let id = nodes[t.ring.expect("a leaf carries a ring node")].id.clone();
        placement.insert(id.clone(), RingPlacement { angle, x: t.x, y: t.y });
        order.push(id);
    }

    // 5. Internal nodes: angle between first and last child, radius by height so every
    //    subtree ends on the ring (d3.cluster). The root sits at the centre.
    let root_h = tree[ROOT].height.max(1) as f64;
    fn place(tree: &mut Vec<TreeNode>, t: usize, r: f64, root_h: f64) {
        if tree[t].ring.is_some() {
            return;
        }
        let children = tree[t].children.clone();
        for &c in &children {
            place(tree, c, r, root_h);
        }
        if let (Some(&first), Some(&last)) = (children.first(), children.last()) {
            tree[t].angle = (tree[first].angle + tree[last].angle) / 2.0;
        }
        let radius = (1.0 - tree[t].height as f64 / root_h) * r;
        tree[t].x = radius * tree[t].angle.cos();
        tree[t].y = radius * tree[t].angle.sin();
    }
    place(&mut tree, ROOT, r, root_h);
    tree[ROOT].x = 0.;
    tree[ROOT].y = 0.;

    // 6. Groups: one arc per top-level child that has ring slots.
    let mut groups = Vec::new();
    for &g in &tree[ROOT].children {
        let mut own = Vec::new();
        collect_leaves(&tree, g, &mut own);
        let (Some(&first), Some(&last)) = (own.first(), own.last()) else { continue };
        groups.push(BundleGroup {
            name: tree[g].name.clone(),
            a0: tree[first].angle - slot / 2.0,
            a1: tree[last].angle + slot / 2.0,
            total: tree[g].total,
        });
    }

    let mut leaf_by_id = HashMap::with_capacity(leaves.len());
    for &leaf in &leaves {
        leaf_by_id.insert(nodes[tree[leaf].ring.unwrap()].id.clone(), leaf);
    }

    let mut max_count = 1;
    for &leaf in &leaves {
        max_count = max_count.max(nodes[tree[leaf].ring.unwrap()].count);
    }
    let count_of = |id: &I| nodes[tree[leaf_by_id[id]].ring.unwrap()].count;
    let mut label_order = order.clone();
    label_order.sort_by(|x, y| count_of(y).cmp(&count_of(x)));

    BundleLayout { r, order, placement, groups, slot, label_order, max_count, tree, leaf_by_id }
}

/// Ring leaves under `t`, depth-first — the ring order.
fn collect_leaves(tree: &[TreeNode], t: usize, out: &mut Vec<usize>) {
    if tree[t].ring.is_some() {
        out.push(t);
    }
    for &c in &tree[t].children {
        collect_leaves(tree, c, out);
    }
}

/// The subset of a path API the bundle spline emits.
pub trait PathSink {
    fn move_to(&mut self, x: f64, y: f64);
    fn line_to(&mut self, x: f64, y: f64);
    fn bezier_curve_to(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, x: f64, y: f64);
}

/// Append the bundled curve through `pts` to `sink` (`bundlePath`). This is d3-shape's
/// curveBundle (straighten each control point toward the chord by `1 - beta`) feeding
/// curveBasis (a uniform cubic B-spline through the straightened points).
pub fn bundle_path(sink: &mut impl PathSink, pts: &[(f64, f64)], beta: f64) {
    let Some(j) = pts.len().checked_sub(1) else { return };
    if j < 1 {
        return;
    }
    let (x0, y0) = pts[0];
    let dx = pts[j].0 - x0;
    let dy = pts[j].1 - y0;

    // curveBasis state machine.
    let (mut bx0, mut by0, mut bx1, mut by1) = (f64::NAN, f64::NAN, f64::NAN, f64::NAN);
    let mut state = 0;
    let bezier = |sink: &mut _, bx0: f64, by0: f64, bx1: f64, by1: f64, x: f64, y: f64| {
        PathSink::bezier_curve_to(
            sink,
            (2. * bx0 + bx1) / 3.,
            (2. * by0 + by1) / 3.,
            (bx0 + 2. * bx1) / 3.,
            (by0 + 2. * by1) / 3.,
            (bx0 + 4. * bx1 + x) / 6.,
            (by0 + 4. * by1 + y) / 6.,
        )
    };
    for (i, p) in pts.iter().enumerate() {
        let t = i as f64 / j as f64;
        let x = beta * p.0 + (1. - beta) * (x0 + t * dx);
        let y = beta * p.1 + (1. - beta) * (y0 + t * dy);
        match state {
            0 => {
                state = 1;
                sink.move_to(x, y);
            }
            1 => state = 2,
            2 => {
                state = 3;
                sink.line_to((5. * bx0 + bx1) / 6., (5. * by0 + by1) / 6.);
                bezier(sink, bx0, by0, bx1, by1, x, y);
            }
            _ => bezier(sink, bx0, by0, bx1, by1, x, y),
        }
        bx0 = bx1;
        bx1 = x;
        by0 = by1;
        by1 = y;
    }
    // lineEnd
    if state == 3 {
        bezier(sink, bx0, by0, bx1, by1, bx1, by1);
    }
    if state >= 2 {
        sink.line_to(bx1, by1);
    }
}

/// Layout invariants for the Tag Graph's radial edge bundling: ring order follows the
/// hierarchy, groups are contiguous arcs, and edge paths route through the tree. One test per
/// `it(...)` of `src/modules/plugins/__tests__/tagGraphBundle.test.ts`, in its order (17).
#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        // `toBeCloseTo(b, 6)`: |a - b| < 5e-7.
        (a - b).abs() < 5e-7
    }

    // --- relativeToBranch / parentPath ---------------------------------------------------

    #[test]
    fn is_empty_for_the_root_itself_and_relative_for_descendants() {
        assert_eq!(relative_to_branch("Animals", "Animals"), Some(""));
        assert_eq!(relative_to_branch("Animals/Bird", "Animals"), Some("Bird"));
        assert_eq!(relative_to_branch("Animals/Bird/Seagull", "Animals"), Some("Bird/Seagull"));
        assert_eq!(relative_to_branch("Animals/Bird/Seagull", "Animals/Bird"), Some("Seagull"));
    }

    #[test]
    fn is_null_outside_the_branch_and_segment_exact() {
        assert_eq!(relative_to_branch("Places/Oslo", "Animals"), None);
        assert_eq!(relative_to_branch("AnimalsX/Bird", "Animals"), None);
        assert_eq!(relative_to_branch("Animal", "Animals"), None);
    }

    #[test]
    fn parent_path_climbs_one_level_and_stops_at_the_top() {
        assert_eq!(parent_path("Animals/Bird/Seagull"), Some("Animals/Bird"));
        assert_eq!(parent_path("Animals/Bird"), Some("Animals"));
        assert_eq!(parent_path("Animals"), None);
    }

    fn tag(full_path: &str, count: i64) -> BundleNode<String> {
        BundleNode { id: format!("t:{full_path}"), kind: BundleKind::Tag, full_path: full_path.into(), count }
    }

    fn camera(model: &str, count: i64) -> BundleNode<String> {
        BundleNode { id: format!("c:{model}"), kind: BundleKind::Camera, full_path: model.into(), count }
    }

    const R: f64 = 100.;

    fn id(s: &str) -> String {
        s.to_string()
    }

    // --- buildBundleLayout — ring order --------------------------------------------------

    #[test]
    fn places_every_node_on_the_ring_children_right_after_their_parent_self_slot_first() {
        let layout = build_bundle_layout(
            &[
                tag("Animals", 5), // has children AND photos → self slot
                tag("Animals/Bird", 20),
                tag("Animals/Bird/Seagull", 15),
                tag("Animals/Bird/Dove", 3),
                tag("Animals/Dog", 30),
            ],
            R,
        );
        // The self slot leads its subtree; siblings order by subtree total, so Bird
        // (20 + 15 + 3 = 38) comes before Dog (30).
        assert_eq!(
            layout.order,
            ["t:Animals", "t:Animals/Bird", "t:Animals/Bird/Seagull", "t:Animals/Bird/Dove", "t:Animals/Dog"]
        );
        assert_eq!(layout.placement.len(), 5);
        for id in &layout.order {
            let p = layout.placement[id];
            assert!(close(p.x.hypot(p.y), R), "{id}");
        }
    }

    #[test]
    fn orders_groups_by_total_and_keeps_angles_increasing_from_12_oclock() {
        let layout =
            build_bundle_layout(&[tag("Small/a", 1), tag("Big/a", 50), tag("Big/b", 40), tag("Mid/a", 10)], R);
        assert_eq!(layout.groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(), ["Big", "Mid", "Small"]);
        let angles: Vec<f64> = layout.order.iter().map(|id| layout.placement[id].angle).collect();
        for i in 1..angles.len() {
            assert!(angles[i] > angles[i - 1]);
        }
        assert!(angles[0] > -PI / 2.);
        assert!(angles[angles.len() - 1] < (3. * PI) / 2.);
    }

    #[test]
    fn leaves_a_wider_gap_between_groups_than_between_sibling_subtrees() {
        let layout = build_bundle_layout(&[tag("A/x/1", 1), tag("A/y/1", 1), tag("B/z/1", 1)], R);
        let a = |s: &str| layout.placement[&id(s)].angle;
        let sibling_gap = a("t:A/y/1") - a("t:A/x/1");
        let group_gap = a("t:B/z/1") - a("t:A/y/1");
        assert!(sibling_gap > layout.slot);
        assert!(group_gap > sibling_gap);
    }

    #[test]
    fn gives_groups_contiguous_non_overlapping_arcs_that_cover_their_members() {
        let layout =
            build_bundle_layout(&[tag("A/1", 3), tag("A/2", 2), tag("B/1", 9), tag("B/2", 1), tag("C", 1)], R);
        for i in 1..layout.groups.len() {
            assert!(layout.groups[i].a0 > layout.groups[i - 1].a1);
        }
        for id in &layout.order {
            let p = layout.placement[id];
            let group_name = id[2..].split('/').next().unwrap();
            let g = layout.groups.iter().find(|x| x.name == group_name).unwrap();
            assert!(p.angle > g.a0);
            assert!(p.angle < g.a1);
        }
    }

    #[test]
    fn puts_cameras_in_their_own_group() {
        let layout = build_bundle_layout(&[tag("A/1", 1), camera("X100", 7)], R);
        assert_eq!(layout.groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(), [CAMERA_GROUP, "A"]);
        assert!(layout.placement.contains_key("c:X100"));
    }

    #[test]
    fn orders_labels_by_count_and_reports_the_maximum() {
        let layout = build_bundle_layout(&[tag("A/1", 3), tag("B/1", 30), tag("A/2", 10)], R);
        assert_eq!(layout.label_order, ["t:B/1", "t:A/2", "t:A/1"]);
        assert_eq!(layout.max_count, 30);
    }

    #[test]
    fn handles_an_empty_input() {
        let layout = build_bundle_layout::<String>(&[], R);
        assert!(layout.order.is_empty());
        assert!(layout.groups.is_empty());
        assert_eq!(layout.path(&id("a"), &id("b")), None);
    }

    // --- buildBundleLayout — edge paths --------------------------------------------------

    fn edge_layout() -> BundleLayout<String> {
        build_bundle_layout(&[tag("A/x/1", 1), tag("A/x/2", 1), tag("A/y/1", 1), tag("B/1", 1)], R)
    }

    #[test]
    fn routes_siblings_through_their_parent_which_sits_inside_the_ring() {
        let layout = edge_layout();
        let pts = layout.path(&id("t:A/x/1"), &id("t:A/x/2")).unwrap();
        assert_eq!(pts.len(), 3);
        let p1 = layout.placement["t:A/x/1"];
        let p2 = layout.placement["t:A/x/2"];
        assert_eq!(pts[0], (p1.x, p1.y));
        assert_eq!(pts[2], (p2.x, p2.y));
        let parent_r = pts[1].0.hypot(pts[1].1);
        assert!(parent_r > 0.);
        assert!(parent_r < R);
    }

    #[test]
    fn routes_cousins_through_the_shared_ancestor_not_the_root() {
        let layout = edge_layout();
        let pts = layout.path(&id("t:A/x/1"), &id("t:A/y/1")).unwrap();
        // x/1 → x → A → y → y/1
        assert_eq!(pts.len(), 5);
        for &(px, py) in &pts[1..pts.len() - 1] {
            assert!(px.hypot(py) > 0.);
        }
    }

    #[test]
    fn routes_across_groups_through_the_centre() {
        let layout = edge_layout();
        let pts = layout.path(&id("t:A/x/1"), &id("t:B/1")).unwrap();
        // x/1 → x → A → root → B → B/1
        assert_eq!(pts.len(), 6);
        assert_eq!(pts[3], (0., 0.));
    }

    #[test]
    fn returns_null_for_ids_that_are_not_on_the_ring() {
        let layout = edge_layout();
        assert_eq!(layout.path(&id("t:A/x/1"), &id("nope")), None);
        assert_eq!(layout.path(&id("t:A"), &id("t:B/1")), None); // "A" has no photos, so no slot
    }

    // --- bundlePath ------------------------------------------------------------------------

    #[derive(Debug, Clone, PartialEq)]
    enum Op {
        M(f64, f64),
        L(f64, f64),
        C([f64; 6]),
    }

    #[derive(Default)]
    struct Record(Vec<Op>);

    impl PathSink for Record {
        fn move_to(&mut self, x: f64, y: f64) {
            self.0.push(Op::M(x, y));
        }
        fn line_to(&mut self, x: f64, y: f64) {
            self.0.push(Op::L(x, y));
        }
        fn bezier_curve_to(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, x: f64, y: f64) {
            self.0.push(Op::C([x1, y1, x2, y2, x, y]));
        }
    }

    #[test]
    fn starts_at_the_first_point_and_ends_at_the_last() {
        let mut ops = Record::default();
        bundle_path(&mut ops, &[(0., 0.), (10., 40.), (50., 50.), (100., 0.)], 0.85);
        assert_eq!(ops.0[0], Op::M(0., 0.));
        let Some(Op::L(x, y)) = ops.0.last().cloned() else { panic!("ends with a lineTo: {:?}", ops.0) };
        assert!(close(x, 100.));
        assert!(close(y, 0.));
        assert!(ops.0.iter().any(|o| matches!(o, Op::C(_))));
    }

    #[test]
    fn with_beta_0_straightens_every_control_point_onto_the_chord() {
        let mut ops = Record::default();
        bundle_path(&mut ops, &[(0., 0.), (10., 90.), (60., -30.), (100., 100.)], 0.);
        // Every emitted coordinate must be collinear with the chord (0,0)→(100,100).
        for op in &ops.0 {
            let a: Vec<f64> = match op {
                Op::M(x, y) | Op::L(x, y) => vec![*x, *y],
                Op::C(c) => c.to_vec(),
            };
            for p in a.chunks(2) {
                assert!(close(p[1], p[0]), "{op:?}");
            }
        }
    }

    #[test]
    fn emits_nothing_for_fewer_than_two_points() {
        let mut ops = Record::default();
        bundle_path(&mut ops, &[(1., 1.)], 0.85);
        assert!(ops.0.is_empty());
    }
}
