//! Map module CPU measurements (#119), headless and offline: no window, no network, no
//! catalog. Synthetic GPS points (clustered like a real library: a few dense places plus a
//! sparse spread) through the work a frame or a zoom change costs.
//!
//! ```sh
//! cargo run --release -p chairphoto-app --example map_bench [points]
//! ```
//!
//! - clustering one zoom level (runs off the UI thread on an integer-zoom change);
//! - the visible tile grid for a 2560×1440 viewport at fractional zooms (every frame);
//! - projecting every marker cluster to the screen, all world copies (every frame).
//!
//! Frame times in the running app (render + paint CPU per frame) come from
//! `CHAIRPHOTO_MAP_TIMING=1`; GPU time and pacing on the EIZO need the display.

use chairphoto_core::plugins::map::cluster::{cluster, project_points, CLUSTER_RADIUS_PX};
use chairphoto_core::plugins::map::tiles::math::Viewport;
use chairphoto_core::plugins::map::PhotoPoint;
use std::time::{Duration, Instant};

fn summary(label: &str, mut v: Vec<Duration>) {
    v.sort();
    let p = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize].as_secs_f64() * 1e3;
    println!("{label:<44} n={:<4} p50 {:>8.3} ms  p95 {:>8.3} ms  max {:>8.3} ms", v.len(), p(0.5), p(0.95), p(1.0));
}

/// A tiny deterministic generator (no extra dependency).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(50_000);
    let mut rng = Lcg(119);
    let places = [(59.91, 10.75), (48.85, 2.35), (40.71, -74.0), (-33.87, 151.21), (35.68, 139.69)];
    let points: Vec<PhotoPoint> = (0..n)
        .map(|i| {
            let (lat, lng) = if i % 5 == 4 {
                (rng.next() * 140.0 - 70.0, rng.next() * 360.0 - 180.0)
            } else {
                let (plat, plng) = places[i % places.len()];
                (plat + (rng.next() - 0.5) * 0.2, plng + (rng.next() - 0.5) * 0.2)
            };
            PhotoPoint { id: i as i64, lat, lng }
        })
        .collect();
    println!("{n} synthetic points");

    let t = Instant::now();
    let projected = project_points(&points);
    println!("{:<44} {:>8.3} ms", "project_points (once per catalog read)", t.elapsed().as_secs_f64() * 1e3);

    for z in [2u8, 6, 10, 14, 18] {
        let samples = (0..10)
            .map(|_| {
                let t = Instant::now();
                std::hint::black_box(cluster(&projected, z, CLUSTER_RADIUS_PX));
                t.elapsed()
            })
            .collect();
        let count = cluster(&projected, z, CLUSTER_RADIUS_PX).len();
        summary(&format!("cluster z{z} ({count} markers)"), samples);
    }

    let mut grid = Vec::new();
    let mut markers = Vec::new();
    for i in 0..200 {
        let zoom = 2.0 + (i as f64) * 0.08;
        let vp = Viewport::new((59.91, 10.75), zoom, 2560.0, 1440.0);
        let t = Instant::now();
        std::hint::black_box(vp.visible_tiles());
        grid.push(t.elapsed());
        let clusters = cluster(&projected, zoom.round() as u8, CLUSTER_RADIUS_PX);
        let t = Instant::now();
        let mut on = 0usize;
        for c in &clusters {
            on += vp.screen_copies(c.u, c.v).len();
        }
        std::hint::black_box(on);
        markers.push(t.elapsed());
    }
    summary("visible_tiles 2560×1440 (per frame)", grid);
    summary("marker screen positions (per frame)", markers);
}
