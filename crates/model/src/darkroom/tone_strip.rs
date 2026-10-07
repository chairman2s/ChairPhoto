//! The Darkroom's adjustable histogram — the pure part of
//! `src/components/darkroom/ToneStrip.tsx` (docs/plans/darkroom/mockups/04-tone-strip.html):
//! eight EV zones, fill height = pixel mass, and dragging a zone writes an EV offset into
//! the edit record's `zones`.

/// Zones on the strip, blacks → whites.
pub const ZONE_COUNT: usize = 8;
/// Zone offsets clamp to ±2 EV — beyond that a "zone" nudge stops being a nuance.
pub const MAX_ZONE_EV: f64 = 2.0;
/// Vertical drag pixels per EV.
pub const PX_PER_EV: f64 = 60.0;

/// The zones' names, for their tooltips and the label row.
pub const ZONE_LABELS: [&str; ZONE_COUNT] =
    ["blacks", "deep shadows", "shadows", "low mids", "high mids", "lights", "highlights", "whites"];

/// Dark → light chip ramp so the strip reads as a tonal scale (`0xRRGGBB`).
pub const ZONE_FILLS: [u32; ZONE_COUNT] = [0x1a1d24, 0x2a2f3a, 0x4a5265, 0x707a90, 0x9aa3b5, 0xc3cad8, 0xe4e8ef, 0xffffff];

/// One drag step: zone `zone` moved by `delta_ev`, clamped to ±[`MAX_ZONE_EV`]. Pure — the
/// input is never mutated, and a missing or wrong-length array becomes a zeroed strip.
pub fn apply_zone_drag(zones: Option<&[f64]>, zone: usize, delta_ev: f64) -> Vec<f64> {
    let mut out = match zones {
        Some(z) if z.len() == ZONE_COUNT => z.to_vec(),
        _ => vec![0.0; ZONE_COUNT],
    };
    out[zone] = crate::js_compat::clamp(out[zone] + delta_ev, -MAX_ZONE_EV, MAX_ZONE_EV);
    out
}

/// Double-click: the zone back to 0 (a missing or malformed strip becomes a zeroed one).
pub fn reset_zone(zones: Option<&[f64]>, zone: usize) -> Vec<f64> {
    let mut out = apply_zone_drag(zones, zone, 0.0);
    out[zone] = 0.0;
    out
}

/// A drag from `start_y` to `y` (pixels, y down): up is brighter.
pub fn drag_delta_ev(start_y: f64, y: f64) -> f64 {
    (start_y - y) / PX_PER_EV
}

/// Each zone's fill height as a share of the strip (0–1): masses normalized against the
/// largest (any scale), a missing mass as 0.
pub fn fill_heights(masses: &[f32]) -> [f64; ZONE_COUNT] {
    let max = masses.iter().map(|m| *m as f64).fold(1e-6, f64::max);
    std::array::from_fn(|i| masses.get(i).map_or(0.0, |m| *m as f64 / max))
}

/// The delta label over a zone: `+0.5`, `-1.2`; none at 0.
pub fn delta_label(dz: f64) -> Option<String> {
    (dz != 0.0).then(|| format!("{}{}", if dz > 0.0 { "+" } else { "" }, crate::js_compat::to_fixed(dz, 1)))
}

#[cfg(test)]
mod tests {
    // --- src/components/darkroom/__tests__/toneStrip.test.ts (4 cases) ---
    use super::*;

    #[test]
    fn initializes_a_zeroed_strip_when_no_zones_exist_yet() {
        let z = apply_zone_drag(None, 2, 0.5);
        assert_eq!(z.len(), ZONE_COUNT);
        assert!((z[2] - 0.5).abs() < 1e-9);
        assert!(z.iter().enumerate().filter(|(i, _)| *i != 2).all(|(_, v)| *v == 0.0));
    }

    #[test]
    fn clamps_to_max_zone_ev() {
        assert_eq!(apply_zone_drag(None, 0, 99.0)[0], MAX_ZONE_EV);
        assert_eq!(apply_zone_drag(None, 0, -99.0)[0], -MAX_ZONE_EV);
    }

    #[test]
    fn never_mutates_its_input() {
        let input = vec![0.25; ZONE_COUNT];
        let out = apply_zone_drag(Some(&input), 3, 1.0);
        assert!(input.iter().all(|v| *v == 0.25));
        assert!((out[3] - 1.25).abs() < 1e-9);
    }

    #[test]
    fn repairs_a_wrong_length_array_to_a_fresh_strip() {
        let out = apply_zone_drag(Some(&[1.0, 2.0]), 1, 0.5);
        assert_eq!(out.len(), ZONE_COUNT);
        assert!((out[1] - 0.5).abs() < 1e-9);
        assert_eq!(out[0], 0.0);
    }

    // --- the view's arithmetic (ToneStrip.tsx), beyond the TS tests ---

    #[test]
    fn view_arithmetic_reset_drag_fill_and_label() {
        assert_eq!(reset_zone(Some(&[1.0; ZONE_COUNT]), 4)[4], 0.0);
        assert_eq!(reset_zone(Some(&[1.0; ZONE_COUNT]), 4)[3], 1.0);
        assert_eq!(drag_delta_ev(100.0, 40.0), 1.0, "60 px up is +1 EV");
        assert_eq!(fill_heights(&[1.0, 2.0, 4.0]), [0.25, 0.5, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(fill_heights(&[]), [0.0; ZONE_COUNT]);
        assert_eq!(delta_label(0.0), None);
        assert_eq!(delta_label(0.5).as_deref(), Some("+0.5"));
        assert_eq!(delta_label(-1.25).as_deref(), Some("-1.3"));
    }
}
