//! A generated `library_graph` answer at a chosen scale, for tests and the Tag Graph bench
//! (`crates/app/examples/tag_graph_bench.rs`).
//!
//! The default is the scale the radial view was designed for — 1,400 tags and about 8,600
//! co-occurrence edges, the only recorded measurement of the real library (commit `1a2f3a0`,
//! docs/plans/gpui/tag-graph.md) — in the shape the plan's benchmark used: 12 top-level
//! families × 6 sub-families × leaves, 60 % of the edges inside one family, weights skewed
//! towards 1. Deterministic for a seed (a xorshift generator; no `rand` dependency).

use super::graph::LibraryGraph;

/// The size of a generated library.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scale {
    /// Leaf tags (with photos). Family and sub-family tags come on top.
    pub tags: usize,
    pub edges: usize,
    pub cameras: usize,
}

impl Default for Scale {
    fn default() -> Self {
        Scale { tags: 1400, edges: 8600, cameras: 6 }
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    /// Skewed towards small values: 1 + ⌊max·u³⌋.
    fn skewed(&mut self, max: i64) -> i64 {
        let u = (self.next() % 1_000_000) as f64 / 1_000_000.;
        1 + (max as f64 * u * u * u) as i64
    }
}

const FAMILIES: usize = 12;
const SUBFAMILIES: usize = 6;

/// A library of `scale`'s size.
pub fn library(scale: Scale, seed: u64) -> LibraryGraph {
    let mut rng = Rng(seed.max(1));
    let mut tags = Vec::new();
    let mut hierarchy = Vec::new();
    let mut next_id = 1i64;
    let mut leaves_by_family: Vec<Vec<i64>> = vec![Vec::new(); FAMILIES];
    let mut all_leaves = Vec::new();
    let per_sub = scale.tags.div_ceil(FAMILIES * SUBFAMILIES).max(1);
    let mut made = 0;
    'families: for f in 0..FAMILIES {
        let family = next_id;
        next_id += 1;
        let family_path = format!("Family {f}");
        // Every other family tag has photos of its own (a self slot); the rest are pure
        // parents, named only through hierarchy edges.
        if f % 2 == 0 {
            tags.push((family, family_path.clone(), rng.skewed(400)));
        }
        for s in 0..SUBFAMILIES {
            let sub = next_id;
            next_id += 1;
            let sub_path = format!("{family_path}/Sub {s}");
            tags.push((sub, sub_path.clone(), rng.skewed(300)));
            hierarchy.push((family, sub));
            for l in 0..per_sub {
                if made == scale.tags {
                    break 'families;
                }
                let id = next_id;
                next_id += 1;
                tags.push((id, format!("{sub_path}/Tag {f}.{s}.{l}"), rng.skewed(500)));
                hierarchy.push((sub, id));
                leaves_by_family[f].push(id);
                all_leaves.push(id);
                made += 1;
            }
        }
    }
    let mut cooc = std::collections::HashSet::new();
    let mut cooc_edges = Vec::new();
    let mut attempts = 0;
    while cooc_edges.len() < scale.edges && attempts < scale.edges * 20 && all_leaves.len() > 1 {
        attempts += 1;
        let (a, b) = if rng.below(10) < 6 {
            let fam = &leaves_by_family[rng.below(FAMILIES)];
            if fam.len() < 2 {
                continue;
            }
            (fam[rng.below(fam.len())], fam[rng.below(fam.len())])
        } else {
            (all_leaves[rng.below(all_leaves.len())], all_leaves[rng.below(all_leaves.len())])
        };
        let (a, b) = (a.min(b), a.max(b));
        if a != b && cooc.insert((a, b)) {
            cooc_edges.push((a, b, rng.skewed(60)));
        }
    }
    let cameras: Vec<(String, i64)> = (0..scale.cameras).map(|i| (format!("Camera {i}"), rng.skewed(5000))).collect();
    let mut camera_edges = Vec::new();
    for (model, _) in &cameras {
        for _ in 0..(all_leaves.len() / 4) {
            camera_edges.push((model.clone(), all_leaves[rng.below(all_leaves.len())], rng.skewed(80)));
        }
    }
    camera_edges.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
    camera_edges.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
    LibraryGraph::from_catalog(tags, cameras, cooc_edges, hierarchy, camera_edges)
}

#[cfg(test)]
mod tests {
    use super::super::graph::Graph;
    use super::super::scene::Scene;
    use super::super::session::GraphSession;
    use super::*;

    #[test]
    fn the_default_scale_is_the_designed_one_and_deterministic() {
        let g = library(Scale::default(), 7);
        let leaves = g.tags.iter().filter(|t| t.1.matches('/').count() == 2).count();
        assert_eq!(leaves, 1400);
        assert_eq!(g.cooc_edges.len(), 8600);
        assert_eq!(g, library(Scale::default(), 7));
    }

    /// The whole pipeline at library scale: every tag on the ring, every edge routed.
    #[test]
    fn a_library_scale_scene_routes_every_edge() {
        let mut s = GraphSession::new();
        let gen = s.begin_load();
        s.apply_load(gen, Ok(Graph::build(&library(Scale::default(), 3))));
        s.toggle_tags();
        s.toggle_cameras();
        let input = s.take_scene_request().unwrap();
        let ring = input.nodes.len();
        let scene = Scene::build(&input);
        assert_eq!(scene.layout.order.len(), ring);
        assert_eq!(scene.edges.len(), input.links.len(), "both ends of every drawn link are on the ring");
        assert!(scene.buckets.iter().map(|b| b.edges.len()).sum::<usize>() == scene.edges.len());
    }
}
