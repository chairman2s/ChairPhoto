//! The Map module's view logic without GPUI: per-host tile consent, fence drawing, hit
//! tests, the fence palette and wheel/pinch zoom steps. Unit-tested here; the view
//! (`view.rs`) only wires them to input and paint.

use chairphoto_core::plugins::map::LatLng;
use std::collections::BTreeMap;

// --- consent ------------------------------------------------------------------------------

/// The per-machine preference ([`crate::machine_prefs::MachinePrefs`]) holding the per-host
/// answers: a JSON object `{"tile.openstreetmap.org": true, "tiles.example.org": false}`.
/// Decision #118 says "per host, remembered, changeable in Preferences"; what a tile request
/// reveals (this computer's IP address) is about the machine, not the catalog, so another
/// catalog on this computer does not ask again.
pub const MACHINE_TILE_HOSTS: &str = "map.tileHosts";
/// The module setting where the answers used to live, per catalog (`map.tileHosts`, the
/// first GPUI port). Read once per catalog, merged into [`MACHINE_TILE_HOSTS`]
/// ([`HostConsent::merge_legacy`]) and then emptied.
pub const TILE_HOSTS_KEY: &str = "tileHosts";
/// The module setting holding the tile URL template (`map.tileUrl`, React's key).
pub const TILE_URL_KEY: &str = "tileUrl";

/// What the user said about loading tiles from one host (decision #118: ask on first open,
/// per host, remembered).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consent {
    /// Never asked: the map asks before anything is fetched.
    Unknown,
    Allowed,
    Denied,
}

/// Every host's remembered answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostConsent(BTreeMap<String, bool>);

impl HostConsent {
    /// Parse the stored value. Only `true` (allowed) and `false` (denied) entries count;
    /// anything else — an unreadable value, a non-boolean entry — is "never asked" for that
    /// host: the safe direction, since it asks again rather than fetching.
    pub fn parse(stored: Option<&str>) -> Self {
        let map: BTreeMap<String, serde_json::Value> =
            stored.and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default();
        HostConsent(map.into_iter().filter_map(|(h, v)| v.as_bool().map(|a| (h, a))).collect())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Fold a catalog's old per-catalog answers into these (the machine's). A host this
    /// machine has no answer for takes the catalog's; where they disagree, **denied wins**
    /// (two catalogs that disagree, or a catalog that blocked what another allowed, end up
    /// blocked — the privacy-safe direction; the user can allow it again in Preferences).
    /// Returns whether anything changed.
    pub fn merge_legacy(&mut self, legacy: &HostConsent) -> bool {
        let mut changed = false;
        for (host, &allowed) in &legacy.0 {
            match self.0.get(host) {
                None => {
                    self.0.insert(host.clone(), allowed);
                    changed = true;
                }
                Some(true) if !allowed => {
                    self.0.insert(host.clone(), false);
                    changed = true;
                }
                Some(_) => {}
            }
        }
        changed
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(&self.0).unwrap_or_else(|_| "{}".into())
    }

    pub fn get(&self, host: &str) -> Consent {
        match self.0.get(host) {
            Some(true) => Consent::Allowed,
            Some(false) => Consent::Denied,
            None => Consent::Unknown,
        }
    }

    pub fn set(&mut self, host: &str, allowed: bool) {
        self.0.insert(host.to_string(), allowed);
    }

    /// Forget a host's answer: the map asks again.
    pub fn forget(&mut self, host: &str) {
        self.0.remove(host);
    }

    pub fn hosts(&self) -> impl Iterator<Item = (&str, bool)> {
        self.0.iter().map(|(h, a)| (h.as_str(), *a))
    }
}

// --- fences -------------------------------------------------------------------------------

/// The fence palette, cycled by list index (map.tsx `FENCE_COLORS`).
pub const FENCE_COLORS: [u32; 7] = [0xf59e0b, 0x10b981, 0x8b5cf6, 0xec4899, 0x14b8a6, 0xf97316, 0xef4444];

pub fn fence_color(index: usize) -> u32 {
    FENCE_COLORS[index % FENCE_COLORS.len()]
}

/// A click closes the polygon when it lands this close to the first vertex (map.tsx: 16 px).
pub const CLOSE_RADIUS_PX: f64 = 16.0;

/// What a click while drawing did.
#[derive(Debug, Clone, PartialEq)]
pub enum DraftStep {
    /// A vertex was added.
    Added,
    /// The polygon is closed: these vertices go to the fence editor.
    Closed(Vec<LatLng>),
    /// Closed with fewer than three vertices: drawing is cancelled.
    Cancelled,
}

/// A fence being drawn (map.tsx's drawing refs).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Draft {
    pub vertices: Vec<LatLng>,
}

impl Draft {
    /// A single click at `at`, `first_px` being the screen distance from the click to the
    /// first vertex: with three or more vertices a click within [`CLOSE_RADIUS_PX`] of the
    /// first closes the polygon, otherwise it adds a vertex.
    pub fn click(&mut self, at: LatLng, first_px: Option<f64>) -> DraftStep {
        if self.vertices.len() >= 3 && first_px.is_some_and(|d| d < CLOSE_RADIUS_PX) {
            return self.finish();
        }
        self.vertices.push(at);
        DraftStep::Added
    }

    /// Close the polygon (a double-click, or a click on the first vertex). A trailing
    /// duplicate vertex is dropped; fewer than three vertices cancel.
    pub fn finish(&mut self) -> DraftStep {
        let mut v = std::mem::take(&mut self.vertices);
        if v.len() >= 2 && v[v.len() - 1] == v[v.len() - 2] {
            v.pop();
        }
        if v.len() < 3 {
            DraftStep::Cancelled
        } else {
            DraftStep::Closed(v)
        }
    }
}

/// A fence editor's fields, checked (map.tsx `FenceEditorDialog.handleSave`).
pub fn check_fence_fields(name: &str, tag_path: &str) -> Result<(String, String), &'static str> {
    let (name, tag_path) = (name.trim(), tag_path.trim());
    if name.is_empty() {
        return Err("Name is required.");
    }
    if tag_path.is_empty() {
        return Err("Tag path is required (e.g. Places/Oslo/Aker Brygge).");
    }
    Ok((name.to_string(), tag_path.to_string()))
}

// --- hit tests ----------------------------------------------------------------------------

/// The index of the target whose centre is nearest `(x, y)` within its radius; on a tie the
/// later (drawn on top) wins. Targets are `(x, y, radius)` in screen pixels.
pub fn hit_nearest(targets: &[(f64, f64, f64)], x: f64, y: f64) -> Option<usize> {
    let mut best: Option<(usize, f64)> = None;
    for (i, &(tx, ty, r)) in targets.iter().enumerate() {
        let d2 = (tx - x).powi(2) + (ty - y).powi(2);
        if d2 <= r * r && best.is_none_or(|(_, b)| d2 <= b) {
            best = Some((i, d2));
        }
    }
    best.map(|(i, _)| i)
}

/// The radius a marker's hit target and bubble get: a single photo's pin, or a cluster
/// bubble in markercluster's three size classes (< 10, < 100, ≥ 100).
pub fn marker_radius(count: usize) -> f64 {
    match count {
        0 | 1 => 9.0,
        2..=9 => 15.0,
        10..=99 => 18.0,
        _ => 21.0,
    }
}

// --- zoom steps ---------------------------------------------------------------------------

/// Zoom levels per wheel *line* (a mouse notch): one, as Leaflet's wheel zoom snapped to.
pub const ZOOM_PER_LINE: f64 = 1.0;
/// Pixels of touchpad scroll per zoom level (Leaflet's `wheelPxPerZoomLevel` is 60; touchpad
/// deltas are larger, so this needs tuning on the real hardware).
pub const PX_PER_ZOOM: f64 = 120.0;

/// The zoom change for a wheel event: `lines` for a mouse wheel, `pixels` for a touchpad.
/// GPUI's sign: positive `y` scrolls up, which zooms in.
pub fn wheel_zoom(lines: Option<f64>, pixels: Option<f64>) -> f64 {
    match (lines, pixels) {
        (Some(l), _) => l * ZOOM_PER_LINE,
        (None, Some(p)) => p / PX_PER_ZOOM,
        (None, None) => 0.0,
    }
}

/// The zoom change for a pinch step: GPUI's `delta` is the relative scale change (0.1 = 10 %
/// bigger), and zoom is log₂ of scale.
pub fn pinch_zoom(delta: f64) -> f64 {
    (1.0 + delta).max(0.01).log2()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consent_is_per_host_and_unreadable_storage_asks_again() {
        let mut c = HostConsent::parse(None);
        assert_eq!(c.get("tile.openstreetmap.org"), Consent::Unknown);
        c.set("tile.openstreetmap.org", true);
        c.set("tiles.example.org", false);
        let back = HostConsent::parse(Some(&c.to_json()));
        assert_eq!(back, c);
        assert_eq!(back.get("tile.openstreetmap.org"), Consent::Allowed);
        assert_eq!(back.get("tiles.example.org"), Consent::Denied);
        assert_eq!(back.get("a.tile.openstreetmap.org"), Consent::Unknown, "another host asks again");
        assert_eq!(HostConsent::parse(Some("{broken")).get("tile.openstreetmap.org"), Consent::Unknown);
        let mut f = back;
        f.forget("tiles.example.org");
        assert_eq!(f.get("tiles.example.org"), Consent::Unknown);
    }

    /// Migration from the per-catalog store: only boolean entries count; a host the machine
    /// never answered takes the catalog's answer; on disagreement denied wins, whichever
    /// side denied; agreement changes nothing.
    #[test]
    fn legacy_answers_merge_only_booleans_and_denied_wins() {
        let mut machine = HostConsent::parse(Some(r#"{"both.example":true,"kept.example":false}"#));
        let legacy = HostConsent::parse(Some(
            r#"{"new.example":true,"both.example":false,"kept.example":true,"odd.example":"allowed","n.example":1}"#,
        ));
        assert_eq!(legacy.hosts().count(), 3, "non-boolean entries are not answers");
        assert!(machine.merge_legacy(&legacy));
        assert_eq!(machine.get("new.example"), Consent::Allowed);
        assert_eq!(machine.get("both.example"), Consent::Denied, "a catalog's block wins over an allow");
        assert_eq!(machine.get("kept.example"), Consent::Denied, "a catalog's allow never lifts a block");
        assert_eq!(machine.get("odd.example"), Consent::Unknown);
        assert!(!machine.merge_legacy(&legacy), "merging again changes nothing");
    }

    #[test]
    fn drawing_adds_vertices_and_closes_on_the_first_vertex_or_finish() {
        let mut d = Draft::default();
        assert_eq!(d.click((0.0, 0.0), None), DraftStep::Added);
        // Near the first vertex, but only two vertices: still adds.
        assert_eq!(d.click((0.0, 1.0), Some(3.0)), DraftStep::Added);
        assert_eq!(d.click((1.0, 1.0), Some(40.0)), DraftStep::Added);
        assert_eq!(d.click((9.0, 9.0), Some(15.9)), DraftStep::Closed(vec![(0.0, 0.0), (0.0, 1.0), (1.0, 1.0)]));
        assert!(d.vertices.is_empty());
        // A double-click's duplicate trailing vertex is dropped; under three cancels.
        let mut d = Draft { vertices: vec![(0.0, 0.0), (0.0, 1.0), (0.0, 1.0)] };
        assert_eq!(d.finish(), DraftStep::Cancelled);
        let mut d = Draft { vertices: vec![(0.0, 0.0), (0.0, 1.0), (1.0, 1.0), (1.0, 1.0)] };
        assert_eq!(d.finish(), DraftStep::Closed(vec![(0.0, 0.0), (0.0, 1.0), (1.0, 1.0)]));
    }

    #[test]
    fn fence_fields_are_required_and_trimmed() {
        assert_eq!(check_fence_fields(" Brygga ", " Places/Brygga "), Ok(("Brygga".into(), "Places/Brygga".into())));
        assert_eq!(check_fence_fields(" ", "x"), Err("Name is required."));
        assert!(check_fence_fields("x", "").unwrap_err().starts_with("Tag path is required"));
    }

    #[test]
    fn hits_take_the_nearest_target_within_its_radius_topmost_on_a_tie() {
        let targets = [(10.0, 10.0, 9.0), (20.0, 10.0, 9.0), (10.0, 10.0, 9.0)];
        assert_eq!(hit_nearest(&targets, 12.0, 10.0), Some(2), "the later of two equal hits");
        assert_eq!(hit_nearest(&targets, 18.0, 10.0), Some(1));
        assert_eq!(hit_nearest(&targets, 50.0, 50.0), None);
        assert_eq!(hit_nearest(&[(0.0, 0.0, 5.0)], 5.0, 0.0), Some(0), "the edge counts");
    }

    #[test]
    fn zoom_steps_follow_gpuis_signs() {
        assert_eq!(wheel_zoom(Some(1.0), None), 1.0, "wheel up zooms in one level");
        assert_eq!(wheel_zoom(Some(-2.0), None), -2.0);
        assert_eq!(wheel_zoom(None, Some(-60.0)), -0.5);
        assert!((pinch_zoom(1.0) - 1.0).abs() < 1e-12, "doubling the scale is one level");
        assert!(pinch_zoom(-0.5) < 0.0);
        assert_eq!(marker_radius(1), 9.0);
        assert!(marker_radius(150) > marker_radius(50) && marker_radius(50) > marker_radius(5));
        assert_eq!(fence_color(7), fence_color(0));
    }
}
