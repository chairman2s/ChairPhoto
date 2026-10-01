//! The graph the Tag Graph draws: `library_graph`'s answer ([`LibraryGraph`]) shaped into
//! nodes and links ([`Graph`], the `load` effect of `tagGraph.tsx`), and that graph scoped
//! to a branch with everything derived from it ([`Scoped`]: communities and their colours,
//! neighbours, degrees, the hub threshold, children counts).

use super::bundle::{parent_path, relative_to_branch};
use super::{leaf, top_level, Rgb, CAMERA_COLOR, PALETTE};
use std::collections::{HashMap, HashSet};

/// A ring node's identity: `t<id>` / `c<idx>` in the TypeScript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NodeId {
    /// A tag, by tag id.
    Tag(i64),
    /// A camera model, by its index in [`LibraryGraph::cameras`].
    Camera(i64),
}

impl NodeId {
    pub fn is_camera(self) -> bool {
        matches!(self, NodeId::Camera(_))
    }

    pub fn is_tag(self) -> bool {
        matches!(self, NodeId::Tag(_))
    }
}

/// A node (`GNode` minus the force-layout fields, which went with the Photo ↔ tag mode).
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub id: NodeId,
    /// Leaf label for display: the last path segment of a tag, the model of a camera.
    pub label: String,
    /// The full tag path, or the camera model.
    pub full_path: String,
    pub count: i64,
    /// Top-level path segment (tags; re-rooted inside a branch), or [`CAMERA_COMMUNITY`].
    pub community: String,
}

/// The community every camera belongs to.
pub const CAMERA_COMMUNITY: &str = "__camera";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    /// Two tags on the same photos; weight = shared photos.
    Cooc,
    /// Parent → child in the tag tree; weight 1. Drawn as ring adjacency, never as an edge.
    Hierarchy,
    /// A camera and a tag on the same photos; weight = shared photos.
    Camera,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Link {
    pub source: NodeId,
    pub target: NodeId,
    pub weight: i64,
    pub kind: LinkKind,
}

/// `library_graph`'s answer (`LibraryGraphData`), as the Tauri command shaped it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LibraryGraph {
    /// `(tag id, full path, photos)`: tags with at least one visible photo.
    pub tags: Vec<(i64, String, i64)>,
    /// `(index, model, photos)`.
    pub cameras: Vec<(i64, String, i64)>,
    /// `(tag a, tag b, shared photos)`.
    pub cooc_edges: Vec<(i64, i64, i64)>,
    /// `(parent tag, child tag)` over the whole tag tree.
    pub hierarchy_edges: Vec<(i64, i64)>,
    /// `(camera index, tag, shared photos)`.
    pub camera_edges: Vec<(i64, i64, i64)>,
}

impl LibraryGraph {
    /// `Catalog::library_graph`'s tuples, with cameras numbered by their position and camera
    /// edges remapped from the model name to that number — what `commands/graph.rs`
    /// `library_graph` did before handing the JSON to the TypeScript.
    pub fn from_catalog(
        tags: Vec<(i64, String, i64)>,
        cameras: Vec<(String, i64)>,
        cooc_edges: Vec<(i64, i64, i64)>,
        hierarchy_edges: Vec<(i64, i64)>,
        camera_edges: Vec<(String, i64, i64)>,
    ) -> Self {
        let index: HashMap<&str, i64> = cameras.iter().enumerate().map(|(i, (m, _))| (m.as_str(), i as i64)).collect();
        let camera_edges = camera_edges
            .into_iter()
            .filter_map(|(model, tag, w)| index.get(model.as_str()).map(|&i| (i, tag, w)))
            .collect();
        let cameras = cameras.into_iter().enumerate().map(|(i, (m, c))| (i as i64, m, c)).collect();
        LibraryGraph { tags, cameras, cooc_edges, hierarchy_edges, camera_edges }
    }
}

/// The whole library's graph (`loaded` in `tagGraph.tsx`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Graph {
    /// Tags, then cameras, in the order `library_graph` returned them.
    pub nodes: Vec<Node>,
    /// Co-occurrence, hierarchy and camera links whose both ends are nodes.
    pub links: Vec<Link>,
    /// Every tag path → id, including parents with no direct photos (which are not nodes).
    pub tag_id_by_path: HashMap<String, i64>,
}

impl Graph {
    /// Shape `library_graph`'s answer (the community branch of `load`).
    pub fn build(g: &LibraryGraph) -> Graph {
        let mut nodes: Vec<Node> = g
            .tags
            .iter()
            .map(|(id, path, count)| Node {
                id: NodeId::Tag(*id),
                label: leaf(path).to_string(),
                full_path: path.clone(),
                count: *count,
                community: top_level(path).to_string(),
            })
            .collect();
        nodes.extend(g.cameras.iter().map(|(id, model, count)| Node {
            id: NodeId::Camera(*id),
            label: model.clone(),
            full_path: model.clone(),
            count: *count,
            community: CAMERA_COMMUNITY.to_string(),
        }));
        // Only edges whose BOTH endpoints are nodes: hierarchy edges span the whole tag tree,
        // and a parent like "Animals" may have no direct photos.
        let ids: HashSet<NodeId> = nodes.iter().map(|n| n.id).collect();
        let links: Vec<Link> = g
            .cooc_edges
            .iter()
            .map(|&(a, b, w)| Link { source: NodeId::Tag(a), target: NodeId::Tag(b), weight: w, kind: LinkKind::Cooc })
            .chain(g.hierarchy_edges.iter().map(|&(p, c)| Link {
                source: NodeId::Tag(p),
                target: NodeId::Tag(c),
                weight: 1,
                kind: LinkKind::Hierarchy,
            }))
            .chain(g.camera_edges.iter().map(|&(ci, t, w)| Link {
                source: NodeId::Camera(ci),
                target: NodeId::Tag(t),
                weight: w,
                kind: LinkKind::Camera,
            }))
            .filter(|l| ids.contains(&l.source) && ids.contains(&l.target))
            .collect();
        // Parents without direct photos are not nodes, but hierarchy edges name their ids and
        // a child's path names theirs: walk the edges until no parent is left unnamed (one
        // pass per missing level).
        let mut path_by_id: HashMap<i64, String> = g.tags.iter().map(|(id, p, _)| (*id, p.clone())).collect();
        let mut grew = true;
        while grew {
            grew = false;
            for &(p, c) in &g.hierarchy_edges {
                if path_by_id.contains_key(&p) {
                    continue;
                }
                let parent = path_by_id.get(&c).and_then(|cp| parent_path(cp)).map(str::to_string);
                if let Some(pp) = parent {
                    path_by_id.insert(p, pp);
                    grew = true;
                }
            }
        }
        let tag_id_by_path = path_by_id.into_iter().map(|(id, path)| (path, id)).collect();
        Graph { nodes, links, tag_id_by_path }
    }
}

/// The graph on screen: the whole library, or one tag's branch — the tag and its descendants
/// plus every camera, with `community` re-rooted to the branch's direct children so the arcs,
/// colours and the Communities list are the branch's families (`graph` in `tagGraph.tsx`),
/// and what the view derives from it.
#[derive(Debug, Clone, Default)]
pub struct Scoped {
    pub nodes: Vec<Node>,
    pub links: Vec<Link>,
    index: HashMap<NodeId, usize>,
    /// Communities (tag families) by total photo count, descending; ties by first appearance.
    pub communities: Vec<(String, i64)>,
    color: HashMap<String, Rgb>,
    /// Tag ids per community, in node order — the edge focus set of an active community.
    members: HashMap<String, Vec<NodeId>>,
    /// Neighbours over every link (hierarchy included), in first-link order.
    neighbors: HashMap<NodeId, Vec<NodeId>>,
    /// Top-decile degree — the "hub" chip. `None` = no links at all (∞ in the TypeScript).
    pub hub_threshold: Option<usize>,
    children: HashMap<NodeId, usize>,
}

impl Scoped {
    pub fn new(graph: &Graph, branch: Option<&str>) -> Scoped {
        let (nodes, links) = match branch {
            None => (graph.nodes.clone(), graph.links.clone()),
            Some(branch) => {
                let mut keep = HashSet::new();
                let nodes: Vec<Node> = graph
                    .nodes
                    .iter()
                    .filter_map(|n| {
                        if n.id.is_camera() {
                            keep.insert(n.id);
                            return Some(n.clone());
                        }
                        let rel = relative_to_branch(&n.full_path, branch)?;
                        let community = if rel.is_empty() { n.label.clone() } else { top_level(rel).to_string() };
                        keep.insert(n.id);
                        Some(Node { community, ..n.clone() })
                    })
                    .collect();
                let links =
                    graph.links.iter().filter(|l| keep.contains(&l.source) && keep.contains(&l.target)).copied().collect();
                (nodes, links)
            }
        };
        let index = nodes.iter().enumerate().map(|(i, n)| (n.id, i)).collect();

        let mut communities: Vec<(String, i64)> = Vec::new();
        let mut members: HashMap<String, Vec<NodeId>> = HashMap::new();
        for n in nodes.iter().filter(|n| n.id.is_tag()) {
            match communities.iter_mut().find(|(name, _)| *name == n.community) {
                Some((_, total)) => *total += n.count,
                None => communities.push((n.community.clone(), n.count)),
            }
            members.entry(n.community.clone()).or_default().push(n.id);
        }
        communities.sort_by(|a, b| b.1.cmp(&a.1));
        // The palette cycles down the size-ordered list, which is also the ring order, so
        // neighbouring arcs differ.
        let color = communities.iter().enumerate().map(|(i, (name, _))| (name.clone(), PALETTE[i % PALETTE.len()])).collect();

        let mut neighbors: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        let mut seen: HashSet<(NodeId, NodeId)> = HashSet::new();
        let mut add = |a: NodeId, b: NodeId| {
            if seen.insert((a, b)) {
                neighbors.entry(a).or_default().push(b);
            }
        };
        for l in &links {
            add(l.source, l.target);
            add(l.target, l.source);
        }
        let mut degrees: Vec<usize> = neighbors.values().map(Vec::len).collect();
        degrees.sort_unstable();
        let hub_threshold = degrees.get((degrees.len() as f64 * 0.9).floor() as usize).copied();

        let mut children: HashMap<NodeId, usize> = HashMap::new();
        for l in links.iter().filter(|l| l.kind == LinkKind::Hierarchy) {
            *children.entry(l.source).or_default() += 1;
        }

        Scoped { nodes, links, index, communities, color, members, neighbors, hub_threshold, children }
    }

    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.index.get(&id).map(|&i| &self.nodes[i])
    }

    /// Amber for cameras, the community colour for tags (`nodeColor`).
    pub fn node_color(&self, id: NodeId) -> Rgb {
        match self.node(id) {
            Some(n) if n.id.is_tag() => self.community_color(&n.community),
            _ => CAMERA_COLOR,
        }
    }

    pub fn community_color(&self, name: &str) -> Rgb {
        self.color.get(name).copied().unwrap_or(PALETTE[0])
    }

    pub fn members(&self, community: &str) -> &[NodeId] {
        self.members.get(community).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn neighbors(&self, id: NodeId) -> &[NodeId] {
        self.neighbors.get(&id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Links over the full set — the Links stat.
    pub fn degree(&self, id: NodeId) -> usize {
        self.neighbors(id).len()
    }

    pub fn is_hub(&self, id: NodeId) -> bool {
        self.hub_threshold.is_some_and(|t| self.degree(id) >= t)
    }

    /// Hierarchy children — the Children stat.
    pub fn children(&self, id: NodeId) -> usize {
        self.children.get(&id).copied().unwrap_or(0)
    }

    /// Top neighbours by edge weight (the CONNECTED chips): a pair linked by more than one
    /// kind keeps its strongest link; at most 12.
    pub fn connected(&self, id: NodeId) -> Vec<(NodeId, i64)> {
        let mut best: Vec<(NodeId, i64)> = Vec::new();
        for l in &self.links {
            let other = if l.source == id {
                l.target
            } else if l.target == id {
                l.source
            } else {
                continue;
            };
            if self.node(other).is_none() {
                continue;
            }
            match best.iter_mut().find(|(n, _)| *n == other) {
                Some(e) if l.weight > e.1 => e.1 = l.weight,
                Some(_) => {}
                None => best.push((other, l.weight)),
            }
        }
        best.sort_by(|a, b| b.1.cmp(&a.1));
        best.truncate(12);
        best
    }

    /// How many tags and cameras (the NODE TYPES counts).
    pub fn kind_counts(&self) -> (usize, usize) {
        let tags = self.nodes.iter().filter(|n| n.id.is_tag()).count();
        (tags, self.nodes.len() - tags)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Animals (no photos of its own) › Bird (5) › Seagull (3), Animals › Dog (4), Places (2);
    /// one camera on Bird and Dog.
    pub(crate) fn sample() -> LibraryGraph {
        LibraryGraph::from_catalog(
            vec![
                (2, "Animals/Bird".into(), 5),
                (3, "Animals/Bird/Seagull".into(), 3),
                (4, "Animals/Dog".into(), 4),
                (5, "Places".into(), 2),
            ],
            vec![("X100".into(), 6)],
            vec![(2, 3, 3), (2, 4, 1), (3, 5, 2)],
            vec![(1, 2), (2, 3), (1, 4)],
            vec![("X100".into(), 2, 4), ("X100".into(), 4, 2), ("Gone".into(), 4, 9)],
        )
    }

    #[test]
    fn camera_edges_are_remapped_to_camera_indices_and_unknown_models_dropped() {
        let g = sample();
        assert_eq!(g.cameras, [(0, "X100".to_string(), 6)]);
        assert_eq!(g.camera_edges, [(0, 2, 4), (0, 4, 2)]);
    }

    #[test]
    fn links_keep_only_edges_between_nodes_and_parents_get_their_paths() {
        let graph = Graph::build(&sample());
        assert_eq!(graph.nodes.len(), 5);
        assert_eq!(graph.nodes[0].label, "Bird");
        assert_eq!(graph.nodes[0].community, "Animals");
        assert_eq!(graph.nodes[4].community, CAMERA_COMMUNITY);
        // (1, 2) and (1, 4) name Animals, which has no photos: dropped.
        let hierarchy: Vec<_> = graph.links.iter().filter(|l| l.kind == LinkKind::Hierarchy).collect();
        assert_eq!(hierarchy.len(), 1);
        assert_eq!(graph.tag_id_by_path.get("Animals"), Some(&1), "a parent is named through its child");
        assert_eq!(graph.tag_id_by_path.get("Animals/Bird/Seagull"), Some(&3));
    }

    #[test]
    fn a_branch_keeps_its_subtree_and_the_cameras_and_reroots_communities() {
        let graph = Graph::build(&sample());
        let s = Scoped::new(&graph, Some("Animals/Bird"));
        let ids: Vec<NodeId> = s.nodes.iter().map(|n| n.id).collect();
        assert_eq!(ids, [NodeId::Tag(2), NodeId::Tag(3), NodeId::Camera(0)]);
        // The root takes its own label as community; descendants their first relative segment.
        assert_eq!(s.node(NodeId::Tag(2)).unwrap().community, "Bird");
        assert_eq!(s.node(NodeId::Tag(3)).unwrap().community, "Seagull");
        assert!(s.links.iter().all(|l| ids.contains(&l.source) && ids.contains(&l.target)));
        // The whole library is untouched by scoping.
        assert_eq!(Scoped::new(&graph, None).node(NodeId::Tag(3)).unwrap().community, "Animals");
    }

    #[test]
    fn communities_order_by_total_and_cycle_the_palette() {
        let s = Scoped::new(&Graph::build(&sample()), None);
        assert_eq!(s.communities, [("Animals".to_string(), 12), ("Places".to_string(), 2)]);
        assert_eq!(s.node_color(NodeId::Tag(5)), PALETTE[1]);
        assert_eq!(s.node_color(NodeId::Camera(0)), CAMERA_COLOR);
        assert_eq!(s.members("Animals"), [NodeId::Tag(2), NodeId::Tag(3), NodeId::Tag(4)]);
    }

    #[test]
    fn connected_keeps_the_strongest_link_per_neighbour_by_weight() {
        let s = Scoped::new(&Graph::build(&sample()), None);
        // Bird: Seagull by cooc 3 and hierarchy 1 (strongest 3), Dog 1, camera 4.
        assert_eq!(
            s.connected(NodeId::Tag(2)),
            [(NodeId::Camera(0), 4), (NodeId::Tag(3), 3), (NodeId::Tag(4), 1)]
        );
        assert_eq!(s.degree(NodeId::Tag(2)), 3);
        assert_eq!(s.children(NodeId::Tag(2)), 1);
    }

    #[test]
    fn the_hub_threshold_is_the_top_decile_degree() {
        let s = Scoped::new(&Graph::build(&sample()), None);
        // Degrees: Bird 3, Seagull 2, Dog 2, Places 1, X100 2 → sorted [1,2,2,2,3]; index 4.
        assert_eq!(s.hub_threshold, Some(3));
        assert!(s.is_hub(NodeId::Tag(2)));
        assert!(!s.is_hub(NodeId::Tag(4)));
        assert_eq!(Scoped::new(&Graph::default(), None).hub_threshold, None);
    }
}
