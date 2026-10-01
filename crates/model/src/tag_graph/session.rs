//! [`GraphSession`]: the Tag Graph's state and rules (`GraphView` in `tagGraph.tsx`, minus
//! the painting) — what is loaded, the branch, the selection, hover, node types, the active
//! community, the link threshold, isolation and the pan/zoom — as generation-tagged
//! request/answer pairs the view runs off the UI thread:
//!
//! - **load**: [`begin_load`](GraphSession::begin_load) → `library_graph` on a worker →
//!   [`apply_load`](GraphSession::apply_load). A catalog switch ([`reset`](GraphSession::reset))
//!   bumps the generation, so a load started against the old catalog is dropped.
//! - **scene**: whenever an input of the layout changes, [`take_scene_request`]
//!   (GraphSession::take_scene_request) hands out a [`SceneInput`]; the worker builds the
//!   [`Scene`]; [`apply_scene`](GraphSession::apply_scene) keeps it only if no newer request
//!   went out since.

use super::bundle::{parent_path, relative_to_branch, BundleKind, BundleNode};
use super::graph::{Graph, LinkKind, Node, NodeId, Scoped};
use super::scene::{DrawLink, Scene, SceneInput};
use super::view::{Size, View, BUTTON_STEP};
use super::{leaf, top_level, Rgb, CAMERA_COLOR, PALETTE};
use std::collections::HashSet;
use std::sync::Arc;

/// Which node types are drawn. Both start off: a 1,400-node ring is a worse default than a
/// blank canvas with a hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Visible {
    pub tags: bool,
    pub cameras: bool,
}

impl Visible {
    pub fn shows(&self, id: NodeId) -> bool {
        match id {
            NodeId::Tag(_) => self.tags,
            NodeId::Camera(_) => self.cameras,
        }
    }
}

/// The link-strength slider: co-occurrence links below this many shared photos are dropped.
pub const THRESHOLD_MAX: i64 = 20;

/// The community card's subject (`community` in `tagGraph.tsx`).
#[derive(Debug, Clone, PartialEq)]
pub struct CommunityCard {
    pub name: String,
    /// The family's tag path: a top-level family, or a sub-family inside the branch.
    pub path: String,
    /// The family tag's id, known even when it has no photos of its own (and so no slot);
    /// `None` when the vocabulary has no such tag.
    pub tag_id: Option<i64>,
    pub color: Rgb,
    pub photos: i64,
    /// Members on the ring, by count.
    pub members: Vec<NodeId>,
    /// "Focus on this branch" applies: not already focused, and more than one tag.
    pub can_focus: bool,
}

/// What the pop-out loupe mirrors (`LoupeCard`); the pop-out loupe window (#110) shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct LoupeCard {
    pub title: String,
    pub subtitle: String,
    pub color: Rgb,
    pub chips: Vec<String>,
    pub stats: Vec<(String, i64)>,
    /// `(label, detail, colour)`.
    pub related: Vec<(String, String, Rgb)>,
    pub photos: Option<LoupePhotos>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoupePhotos {
    Tag(i64),
    Camera(String),
}

/// The status strip's counts (`shownStats`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ShownStats {
    pub nodes: usize,
    pub links: usize,
    pub communities: usize,
}

#[derive(Debug, Default)]
pub struct GraphSession {
    graph: Option<Arc<Graph>>,
    scoped: Option<Arc<Scoped>>,
    error: Option<String>,
    load_generation: u64,
    branch: Option<String>,
    selected: Option<NodeId>,
    hover: Option<NodeId>,
    visible: Visible,
    active_community: Option<String>,
    link_threshold: i64,
    isolate: Option<NodeId>,
    view: View,
    size: Option<Size>,
    /// The ring has been fitted to the canvas once for this graph (React's `fittedRef`).
    fitted: bool,
    scene: Option<Arc<Scene>>,
    scene_generation: u64,
    scene_dirty: bool,
}

impl GraphSession {
    pub fn new() -> Self {
        Self::default()
    }

    // --- load --------------------------------------------------------------------------

    /// A load is starting: its generation, for [`apply_load`](Self::apply_load).
    pub fn begin_load(&mut self) -> u64 {
        self.load_generation += 1;
        self.load_generation
    }

    /// A load's answer. `false` (and nothing changes) when a newer load or a reset
    /// superseded it. Reloading keeps the branch, selection and view; what no longer exists
    /// is dropped.
    pub fn apply_load(&mut self, generation: u64, result: Result<Graph, String>) -> bool {
        if generation != self.load_generation {
            return false;
        }
        match result {
            Ok(graph) => {
                let first = self.graph.is_none();
                self.graph = Some(Arc::new(graph));
                self.error = None;
                if first {
                    self.fitted = false;
                }
                self.rescope();
                let s = self.scoped.clone().expect("rescoped");
                for id in [&mut self.selected, &mut self.hover, &mut self.isolate] {
                    if id.is_some_and(|i| s.node(i).is_none()) {
                        *id = None;
                    }
                }
                if self.active_community.as_ref().is_some_and(|c| s.members(c).is_empty()) {
                    self.active_community = None;
                }
            }
            Err(e) => self.error = Some(e),
        }
        self.scene_dirty = true;
        true
    }

    /// The catalog switched (or the view is going away): forget everything that named the old
    /// catalog, and make every load and scene in flight stale.
    pub fn reset(&mut self) {
        let (load, scene) = (self.load_generation + 1, self.scene_generation + 1);
        let size = self.size;
        *self = GraphSession { load_generation: load, scene_generation: scene, size, ..Default::default() };
    }

    pub fn graph(&self) -> Option<&Arc<Graph>> {
        self.graph.as_ref()
    }

    pub fn scoped(&self) -> Option<&Arc<Scoped>> {
        self.scoped.as_ref()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    fn rescope(&mut self) {
        self.scoped = self.graph.as_ref().map(|g| Arc::new(Scoped::new(g, self.branch.as_deref())));
    }

    // --- scene -------------------------------------------------------------------------

    /// The scene's inputs changed since the last request: a snapshot to build, tagged with a
    /// new generation. `None` when nothing changed or nothing is loaded.
    pub fn take_scene_request(&mut self) -> Option<SceneInput> {
        if !self.scene_dirty {
            return None;
        }
        let s = self.scoped.clone()?;
        self.scene_dirty = false;
        self.scene_generation += 1;
        let shown: Vec<&Node> = s.nodes.iter().filter(|n| self.visible.shows(n.id)).collect();
        let nodes = shown
            .iter()
            .map(|n| BundleNode {
                id: n.id,
                kind: if n.id.is_camera() { BundleKind::Camera } else { BundleKind::Tag },
                // Inside a branch, the ring is laid out from paths relative to the branch root,
                // so its direct children become the arcs; the root keeps a slot under its name.
                full_path: match (&self.branch, n.id.is_tag()) {
                    (Some(branch), true) => match relative_to_branch(&n.full_path, branch) {
                        None => n.full_path.clone(),
                        Some("") => n.label.clone(),
                        Some(rel) => rel.to_string(),
                    },
                    _ => n.full_path.clone(),
                },
                count: n.count,
            })
            .collect();
        let colors = shown.iter().map(|n| (n.id, s.node_color(n.id))).collect();
        let links = self
            .draw_links(&s)
            .map(|l| DrawLink { source: l.source, target: l.target, weight: l.weight, camera: l.kind == LinkKind::Camera })
            .collect();
        Some(SceneInput { generation: self.scene_generation, nodes, colors, links, isolate: self.isolate_set() })
    }

    /// The links painted: co-occurrence at or above the threshold, and camera links.
    fn draw_links<'a>(&self, s: &'a Scoped) -> impl Iterator<Item = &'a super::graph::Link> + 'a {
        let threshold = self.link_threshold;
        s.links.iter().filter(move |l| match l.kind {
            LinkKind::Cooc => l.weight >= threshold,
            LinkKind::Hierarchy => false,
            LinkKind::Camera => true,
        })
    }

    /// A built scene. `false` (dropped) when a newer request went out since its input.
    pub fn apply_scene(&mut self, scene: Scene) -> bool {
        if scene.generation != self.scene_generation {
            return false;
        }
        let has_nodes = !scene.layout.order.is_empty();
        self.scene = Some(Arc::new(scene));
        // Fit the ring the first time it has something on it for this graph.
        if has_nodes && !self.fitted {
            if let Some(size) = self.size {
                self.view = View::fit(size);
                self.fitted = true;
            }
        }
        true
    }

    pub fn scene(&self) -> Option<&Arc<Scene>> {
        self.scene.as_ref()
    }

    pub fn scene_generation(&self) -> u64 {
        self.scene_generation
    }

    // --- the canvas ----------------------------------------------------------------------

    pub fn view(&self) -> View {
        self.view
    }

    pub fn set_view(&mut self, view: View) {
        self.view = view;
    }

    pub fn size(&self) -> Option<Size> {
        self.size
    }

    /// The canvas was laid out at `size`. Fits a ring that is waiting for a size.
    pub fn set_size(&mut self, size: Size) -> bool {
        if self.size == Some(size) {
            return false;
        }
        self.size = Some(size);
        if !self.fitted && self.scene.as_ref().is_some_and(|s| !s.layout.order.is_empty()) {
            self.view = View::fit(size);
            self.fitted = true;
        }
        true
    }

    /// −/＋: zoom about the centre.
    pub fn zoom_step(&mut self, zoom_in: bool) {
        let Some(size) = self.size else { return };
        let factor = if zoom_in { BUTTON_STEP } else { 1. / BUTTON_STEP };
        self.view = self.view.zoom_at(factor, (size.w / 2., size.h / 2.));
    }

    /// Fit: the ring and its label band, centred.
    pub fn fit(&mut self) {
        if let Some(size) = self.size {
            self.view = View::fit(size);
        }
    }

    /// Re-center: back to `{k: 1, x: 0, y: 0}`.
    pub fn recenter(&mut self) {
        self.view = View::default();
    }

    // --- hover, selection, focus ---------------------------------------------------------

    pub fn hover(&self) -> Option<NodeId> {
        self.hover
    }

    /// Returns whether it changed.
    pub fn set_hover(&mut self, id: Option<NodeId>) -> bool {
        let changed = self.hover != id;
        self.hover = id;
        changed
    }

    pub fn selected(&self) -> Option<NodeId> {
        self.selected
    }

    pub fn select(&mut self, id: Option<NodeId>) {
        self.selected = id;
    }

    /// The selected node, if it is in the scoped graph.
    pub fn selected_node(&self) -> Option<&Node> {
        self.scoped.as_ref()?.node(self.selected?)
    }

    /// The node(s) whose edges light up: hover beats selection beats the active community.
    pub fn focus(&self) -> Option<HashSet<NodeId>> {
        if let Some(h) = self.hover {
            return Some([h].into());
        }
        if let Some(s) = self.selected {
            return Some([s].into());
        }
        let c = self.active_community.as_ref()?;
        Some(self.scoped.as_ref()?.members(c).iter().copied().collect())
    }

    /// The hovered, else the selected node: its label is forced, its neighbours' tried.
    pub fn emphasised(&self) -> Option<NodeId> {
        self.hover.or(self.selected)
    }

    /// Whether a ring dot is dimmed: something else is hovered and it is no neighbour, or a
    /// community is active and this tag is outside it.
    pub fn dimmed(&self, id: NodeId) -> bool {
        let Some(s) = &self.scoped else { return false };
        if let Some(h) = self.hover {
            if h != id && !s.neighbors(h).contains(&id) {
                return true;
            }
        }
        match (&self.active_community, s.node(id)) {
            (Some(c), Some(n)) => n.id.is_tag() && &n.community != c,
            _ => false,
        }
    }

    /// Whether a group arc is dimmed by the active community.
    pub fn group_dimmed(&self, group: &str) -> bool {
        self.active_community.as_deref().is_some_and(|c| group != super::bundle::CAMERA_GROUP && group != c)
    }

    pub fn group_color(&self, group: &str) -> Rgb {
        if group == super::bundle::CAMERA_GROUP {
            return CAMERA_COLOR;
        }
        self.scoped.as_ref().map(|s| s.community_color(group)).unwrap_or(PALETTE[0])
    }

    /// Escape: deselect; with nothing selected, climb one branch level.
    pub fn escape(&mut self) {
        if self.selected.is_some() {
            self.selected = None;
        } else if let Some(branch) = self.branch.clone() {
            self.focus_branch(parent_path(&branch).map(str::to_string));
        }
    }

    // --- left panel ------------------------------------------------------------------------

    pub fn visible(&self) -> Visible {
        self.visible
    }

    pub fn toggle_tags(&mut self) {
        self.visible.tags = !self.visible.tags;
        self.scene_dirty = true;
    }

    pub fn toggle_cameras(&mut self) {
        self.visible.cameras = !self.visible.cameras;
        self.scene_dirty = true;
    }

    pub fn link_threshold(&self) -> i64 {
        self.link_threshold
    }

    pub fn set_link_threshold(&mut self, t: i64) {
        let t = t.clamp(0, THRESHOLD_MAX);
        if t != self.link_threshold {
            self.link_threshold = t;
            self.scene_dirty = true;
        }
    }

    pub fn active_community(&self) -> Option<&str> {
        self.active_community.as_deref()
    }

    /// A row of the Communities list: toggles that community.
    pub fn toggle_community(&mut self, name: &str) {
        if self.active_community.as_deref() == Some(name) {
            self.active_community = None;
        } else {
            self.active_community = Some(name.to_string());
        }
    }

    pub fn clear_community(&mut self) {
        self.active_community = None;
    }

    pub fn branch(&self) -> Option<&str> {
        self.branch.as_deref()
    }

    /// Scope the ring to a tag's branch, or `None` for the whole library. Community focus and
    /// isolation lift (they were about the previous ring); the selection stays. The new ring
    /// has the same radius, so fitting now is exact.
    pub fn focus_branch(&mut self, path: Option<String>) {
        self.branch = path;
        self.active_community = None;
        self.isolate = None;
        self.rescope();
        self.scene_dirty = true;
        self.fit();
        self.fitted = self.size.is_some();
    }

    /// Breadcrumbs: `(segment, path)` from the top of the branch down.
    pub fn crumbs(&self) -> Vec<(String, String)> {
        let Some(branch) = &self.branch else { return Vec::new() };
        let segs: Vec<&str> = branch.split('/').collect();
        (0..segs.len()).map(|i| (segs[i].to_string(), segs[..=i].join("/"))).collect()
    }

    // --- isolation --------------------------------------------------------------------------

    pub fn isolated(&self) -> Option<NodeId> {
        self.isolate
    }

    /// "Isolate neighbours" / "Show all" for the selected node.
    pub fn toggle_isolate(&mut self, id: NodeId) {
        self.isolate = if self.isolate == Some(id) { None } else { Some(id) };
        self.scene_dirty = true;
    }

    /// The selected node and its neighbours, or `None` = everything.
    fn isolate_set(&self) -> Option<HashSet<NodeId>> {
        let id = self.isolate?;
        let mut set: HashSet<NodeId> = [id].into();
        if let Some(s) = &self.scoped {
            set.extend(s.neighbors(id).iter().copied());
        }
        Some(set)
    }

    // --- inspector ------------------------------------------------------------------------

    /// "Focus on this branch" for the selected node: a tag with children, not already focused.
    pub fn can_focus_selected(&self) -> bool {
        let (Some(n), Some(s)) = (self.selected_node(), &self.scoped) else { return false };
        n.id.is_tag() && s.children(n.id) > 0 && self.branch.as_deref() != Some(n.full_path.as_str())
    }

    /// The active community as the inspector's subject when no node is selected.
    pub fn community_card(&self) -> Option<CommunityCard> {
        let name = self.active_community.as_ref()?;
        let s = self.scoped.as_ref()?;
        let mut members: Vec<&Node> = s.members(name).iter().filter_map(|&id| s.node(id)).collect();
        members.sort_by(|a, b| b.count.cmp(&a.count));
        let first = members.first()?;
        let path = match &self.branch {
            None => top_level(&first.full_path).to_string(),
            Some(branch) => match relative_to_branch(&first.full_path, branch)? {
                "" => branch.clone(),
                rel => format!("{branch}/{}", top_level(rel)),
            },
        };
        let tag_id = self.graph.as_ref().and_then(|g| g.tag_id_by_path.get(&path).copied());
        Some(CommunityCard {
            name: name.clone(),
            can_focus: self.branch.as_deref() != Some(path.as_str()) && members.len() > 1,
            path,
            tag_id,
            color: s.community_color(name),
            photos: s.communities.iter().find(|(c, _)| c == name).map(|(_, n)| *n).unwrap_or(0),
            members: members.iter().map(|n| n.id).collect(),
        })
    }

    /// What the pop-out loupe mirrors: the selected node, else the community card.
    pub fn loupe_card(&self) -> Option<LoupeCard> {
        let s = self.scoped.as_ref()?;
        if let Some(n) = self.selected_node() {
            let is_tag = n.id.is_tag();
            let links = s.degree(n.id) as i64;
            let mut stats = vec![("Photos".to_string(), n.count)];
            if is_tag {
                stats.push(("Children".into(), s.children(n.id) as i64));
            }
            stats.push(("Links".into(), links));
            let chips = if is_tag {
                vec![
                    format!("Tag{}", if s.is_hub(n.id) { " · hub" } else { "" }),
                    format!("Community: {}", n.community),
                ]
            } else {
                vec!["Camera".into()]
            };
            return Some(LoupeCard {
                title: n.label.clone(),
                subtitle: if is_tag { n.full_path.clone() } else { "Camera".into() },
                color: s.node_color(n.id),
                chips,
                stats,
                related: s
                    .connected(n.id)
                    .into_iter()
                    .filter_map(|(id, w)| s.node(id).map(|m| (m.label.clone(), w.to_string(), s.node_color(id))))
                    .collect(),
                photos: Some(match n.id {
                    NodeId::Tag(id) => LoupePhotos::Tag(id),
                    NodeId::Camera(_) => LoupePhotos::Camera(n.full_path.clone()),
                }),
            });
        }
        let c = self.community_card()?;
        Some(LoupeCard {
            title: c.name.clone(),
            subtitle: c.path.clone(),
            color: c.color,
            chips: vec![if self.branch.is_some() { "Branch" } else { "Community" }.into()],
            stats: vec![("Photos".into(), c.photos), ("Tags".into(), c.members.len() as i64)],
            related: c
                .members
                .iter()
                .take(12)
                .filter_map(|&id| s.node(id).map(|m| (m.label.clone(), m.count.to_string(), s.node_color(id))))
                .collect(),
            photos: c.tag_id.map(LoupePhotos::Tag),
        })
    }

    // --- status ---------------------------------------------------------------------------

    /// Nodes drawn, links drawn among them, and communities (`shownStats`).
    pub fn shown_stats(&self) -> ShownStats {
        let Some(s) = &self.scoped else { return ShownStats::default() };
        let isolate = self.isolate_set();
        let shown: HashSet<NodeId> = s
            .nodes
            .iter()
            .map(|n| n.id)
            .filter(|&id| self.visible.shows(id) && isolate.as_ref().is_none_or(|set| set.contains(&id)))
            .collect();
        let links = self.draw_links(s).filter(|l| shown.contains(&l.source) && shown.contains(&l.target)).count();
        ShownStats { nodes: shown.len(), links, communities: s.communities.len() }
    }

    /// The status strip.
    pub fn status(&self) -> String {
        if self.scoped.is_none() {
            return "Loading…".into();
        }
        let st = self.shown_stats();
        let prefix = self.branch.as_ref().map(|b| format!("{b} · ")).unwrap_or_default();
        let what = if self.branch.is_some() { "branches" } else { "communities" };
        format!("{prefix}{} nodes · {} links · {} {what}", st.nodes, st.links, st.communities)
    }

    /// The Communities list's heading.
    pub fn communities_heading(&self) -> String {
        match &self.branch {
            Some(b) => format!("Under {}", leaf(b)),
            None => "Communities".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::graph::LibraryGraph;
    use super::*;

    fn sample() -> Graph {
        Graph::build(&LibraryGraph::from_catalog(
            vec![
                (2, "Animals/Bird".into(), 5),
                (3, "Animals/Bird/Seagull".into(), 3),
                (4, "Animals/Dog".into(), 4),
                (5, "Places".into(), 2),
            ],
            vec![("X100".into(), 6)],
            vec![(2, 3, 3), (2, 4, 1), (3, 5, 2)],
            vec![(1, 2), (2, 3), (1, 4)],
            vec![("X100".into(), 2, 4), ("X100".into(), 4, 2)],
        ))
    }

    const SIZE: Size = Size { w: 900., h: 700. };

    fn loaded() -> GraphSession {
        let mut s = GraphSession::new();
        s.set_size(SIZE);
        let g = s.begin_load();
        assert!(s.apply_load(g, Ok(sample())));
        s
    }

    fn build(s: &mut GraphSession) -> bool {
        match s.take_scene_request() {
            Some(input) => s.apply_scene(Scene::build(&input)),
            None => false,
        }
    }

    #[test]
    fn a_load_superseded_by_a_newer_one_or_a_reset_is_dropped() {
        let mut s = GraphSession::new();
        let old = s.begin_load();
        let new = s.begin_load();
        assert!(!s.apply_load(old, Ok(sample())), "older load");
        assert!(s.graph().is_none());
        s.reset();
        assert!(!s.apply_load(new, Ok(sample())), "started before the catalog switch");
        assert!(s.graph().is_none());
        let g = s.begin_load();
        assert!(s.apply_load(g, Ok(sample())));
        assert_eq!(s.status(), "0 nodes · 0 links · 2 communities");
    }

    #[test]
    fn a_scene_superseded_by_a_newer_request_is_dropped_and_the_first_one_fits() {
        let mut s = loaded();
        s.toggle_tags();
        let first = s.take_scene_request().unwrap();
        s.toggle_cameras();
        let second = s.take_scene_request().unwrap();
        assert!(second.generation > first.generation);
        assert!(!s.apply_scene(Scene::build(&first)), "superseded");
        assert!(s.scene().is_none());
        assert!(s.apply_scene(Scene::build(&second)));
        assert_eq!(s.scene().unwrap().layout.order.len(), 5);
        assert_eq!(s.view(), View::fit(SIZE), "the first ring with nodes is fitted");
        assert!(s.take_scene_request().is_none(), "nothing changed since");
    }

    #[test]
    fn the_ring_starts_empty_and_types_toggle_on() {
        let mut s = loaded();
        assert!(build(&mut s));
        assert!(s.scene().unwrap().layout.order.is_empty(), "both node types start off");
        assert_eq!(s.view(), View::default(), "an empty ring is not fitted");
        s.toggle_tags();
        assert!(build(&mut s));
        assert_eq!(s.scene().unwrap().layout.order.len(), 4);
        assert_eq!(s.status(), "4 nodes · 3 links · 2 communities");
    }

    #[test]
    fn the_threshold_drops_weak_cooccurrence_but_never_camera_links() {
        let mut s = loaded();
        s.toggle_tags();
        s.toggle_cameras();
        build(&mut s);
        assert_eq!(s.shown_stats().links, 5);
        s.set_link_threshold(3);
        assert!(build(&mut s), "the threshold is a scene input");
        // Cooc 3 stays; 1 and 2 go; both camera links (4, 2) stay.
        assert_eq!(s.shown_stats().links, 3);
        assert_eq!(s.scene().unwrap().edges.len(), 3);
        s.set_link_threshold(99);
        assert_eq!(s.link_threshold(), THRESHOLD_MAX);
    }

    #[test]
    fn focus_is_hover_then_selection_then_community() {
        let mut s = loaded();
        assert_eq!(s.focus(), None);
        s.toggle_community("Animals");
        assert_eq!(s.focus().unwrap().len(), 3);
        s.select(Some(NodeId::Tag(5)));
        assert_eq!(s.focus(), Some([NodeId::Tag(5)].into()));
        s.set_hover(Some(NodeId::Tag(2)));
        assert_eq!(s.focus(), Some([NodeId::Tag(2)].into()));
        assert_eq!(s.emphasised(), Some(NodeId::Tag(2)));
        // Hovering Bird dims what is not its neighbour; the active community dims Places too.
        assert!(!s.dimmed(NodeId::Tag(3)));
        assert!(s.dimmed(NodeId::Tag(5)));
        s.set_hover(None);
        assert!(s.dimmed(NodeId::Tag(5)), "outside the active community");
        assert!(!s.dimmed(NodeId::Camera(0)), "cameras are never community-dimmed");
        assert!(s.group_dimmed("Places") && !s.group_dimmed("Animals") && !s.group_dimmed("__camera"));
    }

    #[test]
    fn escape_deselects_then_climbs_one_branch_level() {
        let mut s = loaded();
        s.focus_branch(Some("Animals/Bird".into()));
        s.select(Some(NodeId::Tag(3)));
        s.escape();
        assert_eq!(s.selected(), None);
        assert_eq!(s.branch(), Some("Animals/Bird"));
        s.escape();
        assert_eq!(s.branch(), Some("Animals"));
        s.escape();
        assert_eq!(s.branch(), None);
        s.escape();
        assert_eq!(s.branch(), None);
    }

    #[test]
    fn focusing_a_branch_lifts_community_and_isolation_relays_the_ring_and_fits() {
        let mut s = loaded();
        s.toggle_tags();
        build(&mut s);
        s.toggle_community("Animals");
        s.toggle_isolate(NodeId::Tag(2));
        s.set_view(View { k: 3., x: 1., y: 2. });
        s.focus_branch(Some("Animals/Bird".into()));
        assert_eq!((s.active_community(), s.isolated()), (None, None));
        assert_eq!(s.view(), View::fit(SIZE));
        let input = s.take_scene_request().unwrap();
        // The branch root keeps a slot under its own label; descendants are relative.
        let paths: Vec<&str> = input.nodes.iter().map(|n| n.full_path.as_str()).collect();
        assert_eq!(paths, ["Bird", "Seagull"]);
        assert_eq!(s.crumbs(), [("Animals".into(), "Animals".into()), ("Bird".into(), "Animals/Bird".into())]);
        assert_eq!(s.communities_heading(), "Under Bird");
        assert!(s.status().starts_with("Animals/Bird · 2 nodes"));
    }

    #[test]
    fn the_community_card_names_a_family_tag_without_photos_of_its_own() {
        let mut s = loaded();
        s.toggle_community("Animals");
        let c = s.community_card().unwrap();
        assert_eq!(c.path, "Animals");
        assert_eq!(c.tag_id, Some(1), "Animals has no photos, but the vocabulary knows its id");
        assert_eq!(c.members, [NodeId::Tag(2), NodeId::Tag(4), NodeId::Tag(3)], "by count");
        assert_eq!(c.photos, 12);
        assert!(c.can_focus);
        s.focus_branch(Some("Animals".into()));
        s.toggle_community("Bird");
        let c = s.community_card().unwrap();
        assert_eq!(c.path, "Animals/Bird");
        assert_eq!(c.tag_id, Some(2));
    }

    #[test]
    fn the_loupe_card_mirrors_the_selection_or_the_community() {
        let mut s = loaded();
        assert_eq!(s.loupe_card(), None);
        s.select(Some(NodeId::Tag(2)));
        let card = s.loupe_card().unwrap();
        assert_eq!(card.title, "Bird");
        assert_eq!(card.chips, ["Tag · hub", "Community: Animals"]);
        assert_eq!(
            card.stats,
            [("Photos".to_string(), 5), ("Children".to_string(), 1), ("Links".to_string(), 3)]
        );
        assert_eq!(card.photos, Some(LoupePhotos::Tag(2)));
        assert_eq!(card.related[0].0, "X100");
        s.select(Some(NodeId::Camera(0)));
        assert_eq!(s.loupe_card().unwrap().photos, Some(LoupePhotos::Camera("X100".into())));
        s.select(None);
        s.toggle_community("Places");
        let card = s.loupe_card().unwrap();
        assert_eq!((card.title.as_str(), card.chips[0].as_str()), ("Places", "Community"));
        assert_eq!(card.photos, Some(LoupePhotos::Tag(5)));
    }

    #[test]
    fn a_reload_keeps_the_view_and_drops_what_vanished() {
        let mut s = loaded();
        s.toggle_tags();
        build(&mut s);
        s.select(Some(NodeId::Tag(5)));
        s.set_view(View { k: 2., x: 5., y: 5. });
        let mut g = sample();
        g.nodes.retain(|n| n.id != NodeId::Tag(5));
        let gen = s.begin_load();
        s.apply_load(gen, Ok(g));
        assert_eq!(s.selected(), None, "the selected tag is gone");
        assert_eq!(s.view(), View { k: 2., x: 5., y: 5. }, "no refit on a reload");
        assert!(build(&mut s));
    }

    #[test]
    fn isolating_a_node_shows_only_its_neighbourhood() {
        let mut s = loaded();
        s.toggle_tags();
        s.toggle_isolate(NodeId::Tag(5));
        build(&mut s);
        let scene = s.scene().unwrap();
        assert!(scene.is_shown(NodeId::Tag(3)) && scene.is_shown(NodeId::Tag(5)));
        assert!(!scene.is_shown(NodeId::Tag(2)));
        assert_eq!(s.shown_stats(), ShownStats { nodes: 2, links: 1, communities: 2 });
        s.toggle_isolate(NodeId::Tag(5));
        assert_eq!(s.isolated(), None, "Show all");
    }
}
