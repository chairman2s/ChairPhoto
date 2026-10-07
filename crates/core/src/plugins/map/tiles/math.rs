//! Web Mercator maths for a slippy map with fractional zoom.
//!
//! Positions live in **unit space**: the whole world is the square `[0, 1] × [0, 1]`, `u`
//! growing east from the antimeridian and `v` growing south from the top of the map
//! (latitude ±[`MAX_LATITUDE`]). At zoom `z` the world is [`world_size`]`(z)` =
//! `256 · 2^z` pixels across, so a screen position is `(unit − centre) · world_size + half
//! the viewport`. Unit space does not change with zoom, which is what lets clustering, fence
//! hit-tests and the tile grid share one projection.
//!
//! Tiles are drawn at the **integer zoom nearest** the view's (Leaflet's tile zoom), scaled
//! by `2^(zoom − tile_zoom)` ∈ [0.71, 1.41]. Only tiles that intersect the viewport are
//! listed — no margin, no other zoom — as the OSM tile policy asks ("do not fetch tiles a
//! user is not currently viewing"). The world repeats horizontally; tile keys wrap `x`.

use super::super::LatLng;

/// A tile's edge in pixels at its own zoom.
pub const TILE_SIZE: f64 = 256.0;
/// Web Mercator's latitude limit: the top and bottom edges of the square world.
pub const MAX_LATITUDE: f64 = 85.051_128_779_806_59;
/// The view's zoom range (Leaflet's defaults with OSM's `maxZoom: 19`).
pub const MIN_ZOOM: f64 = 0.0;
pub const MAX_ZOOM: f64 = 19.0;

/// The world's width (and height) in pixels at `zoom`.
pub fn world_size(zoom: f64) -> f64 {
    TILE_SIZE * zoom.exp2()
}

/// `(lat, lng)` → unit space. Latitudes past ±[`MAX_LATITUDE`] clamp to the edge; longitudes
/// are not wrapped (180° maps to `u = 1`).
pub fn project((lat, lng): LatLng) -> (f64, f64) {
    let lat = lat.clamp(-MAX_LATITUDE, MAX_LATITUDE).to_radians();
    let u = (lng + 180.0) / 360.0;
    let v = (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0;
    (u, v)
}

/// Unit space → `(lat, lng)`. The inverse of [`project`] for `u` in `[0, 1]`.
pub fn unproject(u: f64, v: f64) -> LatLng {
    let n = std::f64::consts::PI * (1.0 - 2.0 * v);
    let lat = n.sinh().atan().to_degrees();
    let lng = u * 360.0 - 180.0;
    (lat, lng)
}

/// Wrap `u` into `[0, 1)`: the world repeats east–west.
pub fn wrap_unit(u: f64) -> f64 {
    u.rem_euclid(1.0)
}

/// One tile of the pyramid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileKey {
    pub z: u8,
    pub x: u32,
    pub y: u32,
}

impl TileKey {
    /// The tile one zoom out that contains this one, if any.
    pub fn parent(self) -> Option<TileKey> {
        (self.z > 0).then(|| TileKey { z: self.z - 1, x: self.x / 2, y: self.y / 2 })
    }
}

/// A screen rectangle in viewport pixels, origin top-left.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// A tile to draw and where: `key` is the (x-wrapped) tile to fetch, `rect` the screen
/// rectangle of this copy of it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TilePlacement {
    pub key: TileKey,
    pub rect: ScreenRect,
}

/// What the map shows: a centre in unit space, a fractional zoom and the viewport size in
/// pixels. Every method keeps the invariants: zoom within [`MIN_ZOOM`, `max_zoom`], `u`
/// wrapped into `[0, 1)`, and `v` clamped so the world's top and bottom edges never come
/// inside the viewport when the world is taller than it (centred when it is not).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    u: f64,
    v: f64,
    zoom: f64,
    width: f64,
    height: f64,
    max_zoom: f64,
}

impl Viewport {
    pub fn new(center: LatLng, zoom: f64, width: f64, height: f64) -> Self {
        let (u, v) = project(center);
        let mut vp = Viewport { u, v, zoom, width: width.max(0.0), height: height.max(0.0), max_zoom: MAX_ZOOM };
        vp.normalise();
        vp
    }

    /// Cap the zoom (a tile source's own maximum).
    pub fn with_max_zoom(mut self, max_zoom: f64) -> Self {
        self.max_zoom = max_zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        self.normalise();
        self
    }

    pub fn zoom(&self) -> f64 {
        self.zoom
    }

    pub fn max_zoom(&self) -> f64 {
        self.max_zoom
    }

    pub fn size(&self) -> (f64, f64) {
        (self.width, self.height)
    }

    pub fn center_unit(&self) -> (f64, f64) {
        (self.u, self.v)
    }

    pub fn center(&self) -> LatLng {
        unproject(self.u, self.v)
    }

    /// The world's size in pixels at this zoom.
    pub fn world_size(&self) -> f64 {
        world_size(self.zoom)
    }

    /// The integer zoom tiles are fetched at: the nearest, within `[0, max_zoom]`.
    pub fn tile_zoom(&self) -> u8 {
        self.zoom.round().clamp(MIN_ZOOM, self.max_zoom.floor()) as u8
    }

    fn normalise(&mut self) {
        if !self.zoom.is_finite() {
            self.zoom = MIN_ZOOM;
        }
        self.zoom = self.zoom.clamp(MIN_ZOOM, self.max_zoom);
        if !self.u.is_finite() || !self.v.is_finite() {
            (self.u, self.v) = (0.5, 0.5);
        }
        self.u = wrap_unit(self.u);
        let ws = self.world_size();
        let half = self.height / 2.0 / ws;
        self.v = if half >= 0.5 { 0.5 } else { self.v.clamp(half, 1.0 - half) };
    }

    pub fn set_size(&mut self, width: f64, height: f64) {
        self.width = width.max(0.0);
        self.height = height.max(0.0);
        self.normalise();
    }

    pub fn set_center(&mut self, center: LatLng) {
        (self.u, self.v) = project(center);
        self.normalise();
    }

    pub fn set_zoom(&mut self, zoom: f64) {
        self.zoom = zoom;
        self.normalise();
    }

    /// Unit space → screen pixels, for the copy of the world nearest the centre.
    pub fn unit_to_screen(&self, u: f64, v: f64) -> (f64, f64) {
        let ws = self.world_size();
        let mut du = u - self.u;
        du -= du.round(); // nearest copy: |du| ≤ 0.5
        (du * ws + self.width / 2.0, (v - self.v) * ws + self.height / 2.0)
    }

    pub fn to_screen(&self, ll: LatLng) -> (f64, f64) {
        let (u, v) = project(ll);
        self.unit_to_screen(u, v)
    }

    /// Screen pixels → unit space, `u` wrapped.
    pub fn screen_to_unit(&self, x: f64, y: f64) -> (f64, f64) {
        let ws = self.world_size();
        (wrap_unit(self.u + (x - self.width / 2.0) / ws), self.v + (y - self.height / 2.0) / ws)
    }

    pub fn screen_to_latlng(&self, x: f64, y: f64) -> LatLng {
        let (u, v) = self.screen_to_unit(x, y);
        unproject(u, v.clamp(0.0, 1.0))
    }

    /// Every screen position at which unit point `(u, v)` appears: one per visible copy of
    /// the world (several when zoomed far out on a wide viewport).
    pub fn screen_copies(&self, u: f64, v: f64) -> Vec<(f64, f64)> {
        let ws = self.world_size();
        let (x0, y) = self.unit_to_screen(u, v);
        let mut out = vec![(x0, y)];
        let mut k = 1.0;
        loop {
            let mut any = false;
            for x in [x0 - k * ws, x0 + k * ws] {
                if x >= -ws && x <= self.width + ws {
                    any = true;
                    if x >= -TILE_SIZE && x <= self.width + TILE_SIZE {
                        out.push((x, y));
                    }
                }
            }
            if !any {
                return out;
            }
            k += 1.0;
        }
    }

    /// Drag the map by `(dx, dy)` screen pixels: what was under the pointer stays under it.
    pub fn pan_by(&mut self, dx: f64, dy: f64) {
        let ws = self.world_size();
        self.u -= dx / ws;
        self.v -= dy / ws;
        self.normalise();
    }

    /// Change the zoom to `zoom`, keeping the point under screen position `(x, y)` fixed
    /// (wheel and pinch zoom about the cursor). The latitude clamp may move it vertically
    /// when zooming out near the poles.
    pub fn zoom_around(&mut self, x: f64, y: f64, zoom: f64) {
        let ws = self.world_size();
        let (pu, pv) = (self.u + (x - self.width / 2.0) / ws, self.v + (y - self.height / 2.0) / ws);
        self.zoom = zoom;
        self.normalise();
        let ws = self.world_size();
        self.u = pu - (x - self.width / 2.0) / ws;
        self.v = pv - (y - self.height / 2.0) / ws;
        self.normalise();
    }

    /// The tiles intersecting the viewport at [`tile_zoom`](Self::tile_zoom), nearest the
    /// centre first (the order they should be fetched in). A copy of the world repeated east
    /// or west yields the same key at another rectangle.
    pub fn visible_tiles(&self) -> Vec<TilePlacement> {
        if self.width <= 0.0 || self.height <= 0.0 {
            return Vec::new();
        }
        let tz = self.tile_zoom();
        let n = 1u64 << tz;
        let ws = self.world_size();
        let tile_px = ws / n as f64;
        // Unit coordinates of the viewport's edges (u unwrapped).
        let left = self.u - self.width / 2.0 / ws;
        let right = self.u + self.width / 2.0 / ws;
        let top = self.v - self.height / 2.0 / ws;
        let bottom = self.v + self.height / 2.0 / ws;
        let nf = n as f64;
        let i0 = (left * nf).floor() as i64;
        let i1 = ((right * nf).ceil() as i64 - 1).max(i0);
        let j0 = ((top * nf).floor() as i64).max(0);
        let j1 = ((bottom * nf).ceil() as i64 - 1).min(n as i64 - 1);
        let mut out = Vec::new();
        for j in j0..=j1 {
            for i in i0..=i1 {
                let x = (i as f64 / nf - self.u) * ws + self.width / 2.0;
                let y = (j as f64 / nf - self.v) * ws + self.height / 2.0;
                let key = TileKey { z: tz, x: (i.rem_euclid(n as i64)) as u32, y: j as u32 };
                out.push(TilePlacement { key, rect: ScreenRect { x, y, w: tile_px, h: tile_px } });
            }
        }
        let (cx, cy) = (self.width / 2.0, self.height / 2.0);
        let dist = |p: &TilePlacement| {
            let dx = p.rect.x + p.rect.w / 2.0 - cx;
            let dy = p.rect.y + p.rect.h / 2.0 - cy;
            dx * dx + dy * dy
        };
        out.sort_by(|a, b| dist(a).total_cmp(&dist(b)));
        out
    }
}

/// The view that shows every point (Leaflet's `fitBounds(bounds.pad(pad), {maxZoom})`):
/// the bounding box of `points` grown by `pad` of its size on each side, centred, at the
/// largest **integer** zoom (Leaflet's `zoomSnap: 1`) at which it fits in `width × height`,
/// capped at `max_zoom`. `None` for no points. Points spanning the antimeridian are fitted
/// the long way round, as Leaflet does.
pub fn fit_bounds(points: &[LatLng], width: f64, height: f64, pad: f64, max_zoom: f64) -> Option<(LatLng, f64)> {
    let first = points.first()?;
    let (mut s, mut n, mut w, mut e) = (first.0, first.0, first.1, first.1);
    for &(lat, lng) in points {
        s = s.min(lat);
        n = n.max(lat);
        w = w.min(lng);
        e = e.max(lng);
    }
    let (dlat, dlng) = ((n - s) * pad, (e - w) * pad);
    let (s, n, w, e) =
        ((s - dlat).max(-MAX_LATITUDE), (n + dlat).min(MAX_LATITUDE), (w - dlng).max(-180.0), (e + dlng).min(180.0));
    let (u0, v0) = project((n, w));
    let (u1, v1) = project((s, e));
    let (du, dv) = ((u1 - u0).abs(), (v1 - v0).abs());
    let fits = |extent: f64, px: f64| if extent <= 0.0 { f64::INFINITY } else { (px / (extent * TILE_SIZE)).log2() };
    let zoom = fits(du, width).min(fits(dv, height)).floor().clamp(MIN_ZOOM, max_zoom);
    let center = unproject((u0 + u1) / 2.0, (v0 + v1) / 2.0);
    Some((center, zoom))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() <= eps
    }

    #[test]
    fn project_maps_known_points() {
        assert_eq!(project((0.0, 0.0)), (0.5, 0.5));
        let (u, v) = project((MAX_LATITUDE, -180.0));
        assert!(close(u, 0.0, 1e-12) && close(v, 0.0, 1e-9), "{u} {v}");
        let (u, v) = project((-MAX_LATITUDE, 180.0));
        assert!(close(u, 1.0, 1e-12) && close(v, 1.0, 1e-9), "{u} {v}");
        // Past the limit clamps to the edge.
        assert_eq!(project((89.9, 0.0)).1, project((MAX_LATITUDE, 0.0)).1);
    }

    #[test]
    fn project_and_unproject_round_trip() {
        for &(lat, lng) in &[(59.91, 10.75), (-33.87, 151.21), (0.0, -179.99), (85.0, 179.99), (-85.0, 0.0)] {
            let (u, v) = project((lat, lng));
            let (lat2, lng2) = unproject(u, v);
            assert!(close(lat, lat2, 1e-9) && close(lng, lng2, 1e-9), "({lat},{lng}) → ({lat2},{lng2})");
        }
    }

    /// The tile under a known place matches the OSM wiki's slippy-map formula.
    #[test]
    fn the_tile_under_a_point_matches_the_osm_formula() {
        let vp = Viewport::new((59.9139, 10.7522), 12.0, 256.0, 256.0); // Oslo
        let centre = vp.visible_tiles().into_iter().find(|t| {
            t.rect.x <= 128.0 && t.rect.x + t.rect.w > 128.0 && t.rect.y <= 128.0 && t.rect.y + t.rect.h > 128.0
        });
        let lat = 59.9139f64.to_radians();
        let n = 4096.0_f64;
        let x = ((10.7522 + 180.0) / 360.0 * n).floor() as u32;
        let y = ((1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * n).floor() as u32;
        assert_eq!(centre.unwrap().key, TileKey { z: 12, x, y });
    }

    #[test]
    fn zoom_zero_in_a_tile_sized_viewport_is_the_one_world_tile() {
        let vp = Viewport::new((0.0, 0.0), 0.0, 256.0, 256.0);
        let tiles = vp.visible_tiles();
        assert_eq!(tiles, vec![TilePlacement {
            key: TileKey { z: 0, x: 0, y: 0 },
            rect: ScreenRect { x: 0.0, y: 0.0, w: 256.0, h: 256.0 }
        }]);
    }

    /// Only the tiles that intersect the viewport, never a margin: a viewport exactly two
    /// tiles wide and one high, aligned to the grid, needs exactly two.
    #[test]
    fn visible_tiles_are_exactly_those_intersecting_the_viewport() {
        let mut vp = Viewport::new((0.0, 0.0), 3.0, 512.0, 256.0);
        // Align the centre to a tile corner/edge: u = 0.5 is a column boundary at z3, v
        // half a tile below a row boundary.
        vp.u = 0.5;
        vp.v = 0.5 + 0.5 / 8.0;
        let keys: Vec<TileKey> = vp.visible_tiles().iter().map(|t| t.key).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(sorted, vec![TileKey { z: 3, x: 3, y: 4 }, TileKey { z: 3, x: 4, y: 4 }]);
        for t in vp.visible_tiles() {
            let r = t.rect;
            assert!(r.x < 512.0 && r.x + r.w > 0.0 && r.y < 256.0 && r.y + r.h > 0.0, "{t:?} off screen");
        }
    }

    #[test]
    fn visible_tiles_wrap_x_across_the_antimeridian_and_start_at_the_centre() {
        let vp = Viewport::new((0.0, 179.9), 2.0, 600.0, 300.0);
        let tiles = vp.visible_tiles();
        assert!(tiles.iter().all(|t| t.key.x < 4 && t.key.y < 4), "{tiles:?}");
        let xs: std::collections::BTreeSet<u32> = tiles.iter().map(|t| t.key.x).collect();
        assert!(xs.contains(&3) && xs.contains(&0), "both sides of the antimeridian: {xs:?}");
        // Nearest the centre first.
        let d = |t: &TilePlacement| (t.rect.x + t.rect.w / 2.0 - 300.0).powi(2) + (t.rect.y + t.rect.h / 2.0 - 150.0).powi(2);
        assert!(tiles.windows(2).all(|w| d(&w[0]) <= d(&w[1])));
    }

    #[test]
    fn fractional_zoom_scales_tiles_of_the_nearest_integer_zoom() {
        let vp = Viewport::new((10.0, 10.0), 4.4, 800.0, 600.0);
        assert_eq!(vp.tile_zoom(), 4);
        let t = vp.visible_tiles()[0];
        assert!(close(t.rect.w, 256.0 * 0.4f64.exp2(), 1e-9));
        let vp = Viewport::new((10.0, 10.0), 4.6, 800.0, 600.0);
        assert_eq!(vp.tile_zoom(), 5);
        assert!(close(vp.visible_tiles()[0].rect.w, 256.0 * (-0.4f64).exp2(), 1e-9));
    }

    #[test]
    fn screen_and_latlng_round_trip_through_the_viewport() {
        let vp = Viewport::new((48.85, 2.35), 9.3, 1000.0, 700.0);
        let (x, y) = vp.to_screen((48.85, 2.35));
        assert!(close(x, 500.0, 1e-6) && close(y, 350.0, 1e-6));
        let ll = vp.screen_to_latlng(123.0, 456.0);
        let (x, y) = vp.to_screen(ll);
        assert!(close(x, 123.0, 1e-6) && close(y, 456.0, 1e-6), "{x} {y}");
    }

    #[test]
    fn pan_keeps_the_grabbed_point_under_the_pointer() {
        let mut vp = Viewport::new((40.0, -3.0), 7.0, 800.0, 600.0);
        let grabbed = vp.screen_to_latlng(300.0, 200.0);
        vp.pan_by(57.0, -31.0);
        let (x, y) = vp.to_screen(grabbed);
        assert!(close(x, 357.0, 1e-6) && close(y, 169.0, 1e-6), "{x} {y}");
    }

    #[test]
    fn zoom_around_keeps_the_point_under_the_cursor() {
        let mut vp = Viewport::new((40.0, -3.0), 7.0, 800.0, 600.0);
        let under = vp.screen_to_latlng(620.0, 140.0);
        vp.zoom_around(620.0, 140.0, 9.25);
        assert_eq!(vp.zoom(), 9.25);
        let (x, y) = vp.to_screen(under);
        assert!(close(x, 620.0, 1e-6) && close(y, 140.0, 1e-6), "{x} {y}");
    }

    #[test]
    fn zoom_is_clamped_and_the_poles_never_come_into_view() {
        let mut vp = Viewport::new((0.0, 0.0), 30.0, 800.0, 600.0);
        assert_eq!(vp.zoom(), MAX_ZOOM);
        vp.set_zoom(-3.0);
        assert_eq!(vp.zoom(), MIN_ZOOM);
        // At zoom 0 the 256 px world is shorter than the viewport: centred.
        assert_eq!(vp.center_unit().1, 0.5);
        let mut vp = Viewport::new((0.0, 0.0), 5.0, 800.0, 600.0);
        vp.pan_by(0.0, 1.0e6); // drag far down: the top edge stops at the viewport's top
        let (_, top) = vp.unit_to_screen(0.5, 0.0);
        assert!(close(top, 0.0, 1e-6), "{top}");
        let vp = Viewport::new((0.0, 0.0), 18.5, 800.0, 600.0).with_max_zoom(17.0);
        assert_eq!((vp.zoom(), vp.tile_zoom()), (17.0, 17));
    }

    #[test]
    fn pan_wraps_east_west() {
        let mut vp = Viewport::new((0.0, 170.0), 3.0, 800.0, 600.0);
        vp.pan_by(-world_size(3.0) * 30.0 / 360.0, 0.0); // 30° east
        assert!(close(vp.center().1, -160.0, 1e-6), "{:?}", vp.center());
    }

    #[test]
    fn fit_bounds_matches_leaflets_integer_fit() {
        // Two points 1° of longitude apart on the equator: 1.1° padded. At zoom z the span
        // is 256·2^z·1.1/360 px; the widest integer z under 800 px is 9 (≈ 400 px at 9).
        let (c, z) = fit_bounds(&[(0.0, 10.0), (0.0, 11.0)], 800.0, 600.0, 0.05, 12.0).unwrap();
        assert_eq!(z, 9.0);
        assert!(close(c.1, 10.5, 1e-9) && close(c.0, 0.0, 1e-9), "{c:?}");
        // One point (or all at one place): the cap.
        let (c, z) = fit_bounds(&[(59.0, 10.0)], 800.0, 600.0, 0.05, 12.0).unwrap();
        assert!(z == 12.0 && close(c.0, 59.0, 1e-9) && close(c.1, 10.0, 1e-9), "{c:?} {z}");
        assert_eq!(fit_bounds(&[], 800.0, 600.0, 0.05, 12.0), None);
        // The whole world fits only at zoom 0 (or not at all in a tiny viewport: still 0).
        let (_, z) = fit_bounds(&[(-80.0, -170.0), (80.0, 170.0)], 300.0, 200.0, 0.05, 12.0).unwrap();
        assert_eq!(z, 0.0);
    }

    #[test]
    fn copies_repeat_a_point_across_visible_worlds() {
        // Zoom 0 in a 1000 px viewport: the 256 px world shows about four times.
        let vp = Viewport::new((0.0, 0.0), 0.0, 1000.0, 300.0);
        let (u, v) = project((0.0, 0.0));
        let copies = vp.screen_copies(u, v);
        assert!(copies.len() >= 4, "{copies:?}");
        // Zoomed in, one.
        let vp = Viewport::new((0.0, 0.0), 6.0, 1000.0, 300.0);
        assert_eq!(vp.screen_copies(u, v).len(), 1);
    }
}
