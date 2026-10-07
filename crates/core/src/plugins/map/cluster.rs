//! Marker clustering for the map view: what `leaflet.markercluster` did with
//! `maxClusterRadius: 60` (map.tsx), as a grid in screen-pixel space.
//!
//! At integer zoom `z`, points are bucketed into square cells [`CLUSTER_RADIUS_PX`] pixels
//! wide. A plain grid splits two photos a pixel apart when a cell edge runs between them, so
//! one merge pass follows: in cell order, each cluster absorbs the clusters of its eight
//! neighbouring cells whose centroids lie within the radius of its own. A cluster's position
//! is the mean of its members (in projected space), and its members keep the input order —
//! the order the filmstrip shows them in.
//!
//! O(n) per zoom level; the map recomputes it off the UI thread when the integer zoom
//! changes. If the grid look is unacceptable, `supercluster` (MIT) is the documented
//! fallback behind the same function (docs/plans/gpui/map.md).

use super::tiles::math::{project, unproject, world_size, wrap_unit};
use super::{LatLng, PhotoPoint};
use std::collections::{BTreeMap, HashMap};

/// markercluster's `maxClusterRadius` (map.tsx).
pub const CLUSTER_RADIUS_PX: f64 = 60.0;

/// A photo's position in unit space (`tiles::math`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProjectedPoint {
    pub id: i64,
    pub u: f64,
    pub v: f64,
}

/// Project every point once; clustering at any zoom reuses the result.
pub fn project_points(points: &[PhotoPoint]) -> Vec<ProjectedPoint> {
    points
        .iter()
        .filter(|p| p.lat.is_finite() && p.lng.is_finite())
        .map(|p| {
            let (u, v) = project((p.lat, p.lng));
            ProjectedPoint { id: p.id, u: wrap_unit(u), v }
        })
        .collect()
}

/// One marker on the map: a single photo, or a group.
#[derive(Debug, Clone, PartialEq)]
pub struct Cluster {
    pub u: f64,
    pub v: f64,
    pub ids: Vec<i64>,
}

impl Cluster {
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn center(&self) -> LatLng {
        unproject(self.u, self.v)
    }
}

struct Acc {
    su: f64,
    sv: f64,
    members: Vec<usize>,
    absorbed: bool,
}

impl Acc {
    fn centre(&self) -> (f64, f64) {
        let n = self.members.len() as f64;
        (self.su / n, self.sv / n)
    }
}

/// Cluster `points` for display at integer zoom `zoom` with markers merging within
/// `radius_px` screen pixels.
pub fn cluster(points: &[ProjectedPoint], zoom: u8, radius_px: f64) -> Vec<Cluster> {
    let ws = world_size(zoom as f64);
    let cell = radius_px / ws;
    let mut cells: BTreeMap<(i64, i64), Acc> = BTreeMap::new();
    for (i, p) in points.iter().enumerate() {
        let key = ((p.u / cell).floor() as i64, (p.v / cell).floor() as i64);
        let acc = cells.entry(key).or_insert(Acc { su: 0.0, sv: 0.0, members: Vec::new(), absorbed: false });
        acc.su += p.u;
        acc.sv += p.v;
        acc.members.push(i);
    }
    let keys: Vec<(i64, i64)> = cells.keys().copied().collect();
    let index: HashMap<(i64, i64), usize> = keys.iter().enumerate().map(|(i, k)| (*k, i)).collect();
    let mut accs: Vec<Acc> = cells.into_values().collect();
    let r2 = (radius_px / ws).powi(2);
    for i in 0..accs.len() {
        if accs[i].absorbed {
            continue;
        }
        let (cx, cy) = keys[i];
        for dy in -1..=1 {
            for dx in -1..=1 {
                let Some(&j) = index.get(&(cx + dx, cy + dy)) else { continue };
                if j == i || accs[j].absorbed {
                    continue;
                }
                let (a, b) = (accs[i].centre(), accs[j].centre());
                if (a.0 - b.0).powi(2) + (a.1 - b.1).powi(2) < r2 {
                    let other = std::mem::replace(
                        &mut accs[j],
                        Acc { su: 0.0, sv: 0.0, members: Vec::new(), absorbed: true },
                    );
                    let me = &mut accs[i];
                    me.su += other.su;
                    me.sv += other.sv;
                    me.members.extend(other.members);
                }
            }
        }
    }
    accs.into_iter()
        .filter(|a| !a.absorbed && !a.members.is_empty())
        .map(|mut a| {
            a.members.sort_unstable();
            let (u, v) = a.centre();
            Cluster { u, v, ids: a.members.iter().map(|&m| points[m].id).collect() }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(id: i64, lat: f64, lng: f64) -> PhotoPoint {
        PhotoPoint { id, lat, lng }
    }

    /// Unit-space point at screen-pixel offset `(x, y)` from the world's origin at `zoom`.
    fn at(id: i64, x: f64, y: f64, zoom: u8) -> ProjectedPoint {
        let ws = world_size(zoom as f64);
        ProjectedPoint { id, u: x / ws, v: y / ws }
    }

    #[test]
    fn near_points_group_and_far_points_stay_apart() {
        let pts = project_points(&[pt(1, 59.91, 10.75), pt(2, 59.92, 10.76), pt(3, -33.87, 151.21)]);
        let z2 = cluster(&pts, 2, CLUSTER_RADIUS_PX);
        assert_eq!(z2.len(), 2);
        assert!(z2.iter().any(|c| c.ids == [1, 2]) && z2.iter().any(|c| c.ids == [3]), "{z2:?}");
        // Zoomed in to street level the two Oslo photos (about 1.3 km apart) separate.
        assert_eq!(cluster(&pts, 16, CLUSTER_RADIUS_PX).len(), 3);
    }

    #[test]
    fn a_cluster_sits_at_its_members_mean() {
        let z = 10;
        let pts = [at(1, 1000.0, 1000.0, z), at(2, 1010.0, 1020.0, z)];
        let c = &cluster(&pts, z, CLUSTER_RADIUS_PX)[0];
        let ws = world_size(z as f64);
        assert!((c.u * ws - 1005.0).abs() < 1e-6 && (c.v * ws - 1010.0).abs() < 1e-6, "{c:?}");
    }

    /// Two photos a few pixels apart across a cell edge are one marker, not two overlapping.
    #[test]
    fn a_cell_edge_does_not_split_neighbours() {
        let z = 8;
        let edge = CLUSTER_RADIUS_PX * 10.0;
        let pts = [at(1, edge - 2.0, 300.0, z), at(2, edge + 2.0, 300.0, z)];
        let clusters = cluster(&pts, z, CLUSTER_RADIUS_PX);
        assert_eq!(clusters.len(), 1, "{clusters:?}");
        assert_eq!(clusters[0].ids, [1, 2]);
        // Neighbouring cells whose groups are further apart than the radius stay apart.
        let pts = [at(1, edge - 55.0, 300.0, z), at(2, edge + 55.0, 300.0, z)];
        assert_eq!(cluster(&pts, z, CLUSTER_RADIUS_PX).len(), 2);
    }

    #[test]
    fn every_point_lands_in_exactly_one_cluster_in_input_order() {
        let pts: Vec<ProjectedPoint> =
            (0..500).map(|i| at(i, (i * 37 % 4000) as f64, (i * 91 % 3000) as f64, 6)).collect();
        for z in [0u8, 3, 6, 12] {
            let clusters = cluster(&pts, z, CLUSTER_RADIUS_PX);
            let mut all: Vec<i64> = clusters.iter().flat_map(|c| c.ids.clone()).collect();
            assert!(clusters.iter().all(|c| c.ids.windows(2).all(|w| w[0] < w[1])), "input order within a cluster");
            all.sort_unstable();
            assert_eq!(all, (0..500).collect::<Vec<_>>(), "zoom {z}");
        }
        assert!(cluster(&[], 5, CLUSTER_RADIUS_PX).is_empty());
    }

    #[test]
    fn non_finite_coordinates_are_left_out() {
        let pts = project_points(&[pt(1, f64::NAN, 1.0), pt(2, 1.0, 2.0)]);
        assert_eq!(pts.len(), 1);
        assert_eq!(pts[0].id, 2);
    }
}
