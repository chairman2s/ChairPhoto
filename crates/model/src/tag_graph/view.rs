//! The canvas's pan/zoom transform (`viewRef` in `tagGraph.tsx`).
//!
//! The draw transform is `translate(x, y) · scale(k) · translate(w/2, h/2)` for a canvas of
//! `w × h`: graph point `g` lands at `(g + (w/2, h/2))·k + (x, y)` in canvas pixels. At the
//! default `{k: 1, x: 0, y: 0}` the ring's centre is the canvas centre.

use super::{LABEL_EXTENT, RING_R};

/// The zoom clamp: 0.05–6, as React's.
pub const K_MIN: f64 = 0.05;
pub const K_MAX: f64 = 6.;
/// One wheel notch zooms by this factor (React: ×1.15 per wheel event).
pub const WHEEL_STEP: f64 = 1.15;
/// The −/＋ buttons zoom by this factor about the centre.
pub const BUTTON_STEP: f64 = 1.2;

/// A canvas size, logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Size {
    pub w: f64,
    pub h: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    pub k: f64,
    pub x: f64,
    pub y: f64,
}

impl Default for View {
    fn default() -> Self {
        View { k: 1., x: 0., y: 0. }
    }
}

fn clamp_k(k: f64) -> f64 {
    k.clamp(K_MIN, K_MAX)
}

impl View {
    /// Canvas → graph coordinates (`toGraph`).
    pub fn to_graph(&self, p: (f64, f64), size: Size) -> (f64, f64) {
        ((p.0 - self.x) / self.k - size.w / 2., (p.1 - self.y) / self.k - size.h / 2.)
    }

    /// Graph → canvas coordinates.
    pub fn to_screen(&self, g: (f64, f64), size: Size) -> (f64, f64) {
        ((g.0 + size.w / 2.) * self.k + self.x, (g.1 + size.h / 2.) * self.k + self.y)
    }

    /// The ring's centre on the canvas.
    pub fn centre(&self, size: Size) -> (f64, f64) {
        self.to_screen((0., 0.), size)
    }

    /// Zoom by `factor` about canvas point `at`, k clamped (`zoomAt` and the wheel handler).
    pub fn zoom_at(&self, factor: f64, at: (f64, f64)) -> View {
        let k = clamp_k(self.k * factor);
        View { k, x: at.0 - (k / self.k) * (at.0 - self.x), y: at.1 - (k / self.k) * (at.1 - self.y) }
    }

    /// Pan by a pointer delta.
    pub fn panned(&self, start: View, delta: (f64, f64)) -> View {
        View { k: self.k, x: start.x + delta.0, y: start.y + delta.1 }
    }

    /// Fit: the ring plus the label band, centred (`fitView` in bundle mode).
    pub fn fit(size: Size) -> View {
        let margin = LABEL_EXTENT + 28.;
        let k = clamp_k((size.w.min(size.h) / 2. - margin) / RING_R);
        View { k, x: (size.w / 2.) * (1. - k), y: (size.h / 2.) * (1. - k) }
    }

    /// The canvas transform under `self` of a point that was at canvas `p` under `then` — how
    /// a raster made under `then` maps onto the canvas now.
    pub fn reproject(&self, then: View, p: (f64, f64)) -> (f64, f64) {
        let s = self.k / then.k;
        ((p.0 - then.x) * s + self.x, (p.1 - then.y) * s + self.y)
    }
}

/// The zoom factor for a wheel delta in lines (positive = wheel up = zoom in). GPUI reports
/// one notch as 3 lines (`SCROLL_LINES`), so one notch is React's one wheel event: ×1.15.
pub fn wheel_factor_lines(lines: f64) -> f64 {
    WHEEL_STEP.powf(lines / 3.)
}

/// The zoom factor for a pixel-precise (touchpad) wheel delta: ×1.15 per 50 px, a choice —
/// React's handler zoomed ×1.15 per event whatever its size, which a touchpad's stream of
/// small events turns into a jump.
pub fn wheel_factor_pixels(px: f64) -> f64 {
    WHEEL_STEP.powf(px / 50.)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: Size = Size { w: 800., h: 600. };

    #[test]
    fn graph_and_screen_round_trip_and_the_default_centres_the_ring() {
        let v = View { k: 2.5, x: -30., y: 12. };
        let g = (123.4, -56.7);
        let back = v.to_graph(v.to_screen(g, SIZE), SIZE);
        assert!((back.0 - g.0).abs() < 1e-9 && (back.1 - g.1).abs() < 1e-9);
        assert_eq!(View::default().centre(SIZE), (400., 300.));
    }

    #[test]
    fn zooming_keeps_the_point_under_the_cursor_and_clamps_k() {
        let v = View { k: 1., x: 10., y: 20. };
        let at = (250., 140.);
        let before = v.to_graph(at, SIZE);
        let z = v.zoom_at(1.15, at);
        let after = z.to_graph(at, SIZE);
        assert!((before.0 - after.0).abs() < 1e-9 && (before.1 - after.1).abs() < 1e-9);
        assert_eq!(v.zoom_at(1000., at).k, K_MAX);
        assert_eq!(v.zoom_at(1e-6, at).k, K_MIN);
    }

    #[test]
    fn fit_puts_the_ring_and_its_label_band_inside_the_shorter_side() {
        let v = View::fit(SIZE);
        assert!((v.k - (300. - 128.) / 400.).abs() < 1e-12);
        assert_eq!(v.centre(SIZE), (400., 300.));
        // Ring plus label band: exactly half the short side minus React's 28 px.
        assert!(((RING_R * v.k + LABEL_EXTENT) - (300. - 28.)).abs() < 1e-9);
    }

    #[test]
    fn a_raster_made_under_one_view_reprojects_onto_the_next() {
        let then = View { k: 1., x: 0., y: 0. };
        let now = then.zoom_at(2., (400., 300.));
        // A graph point's old canvas position, reprojected, is its new canvas position.
        let g = (100., 50.);
        let p = now.reproject(then, then.to_screen(g, SIZE));
        let q = now.to_screen(g, SIZE);
        assert!((p.0 - q.0).abs() < 1e-9 && (p.1 - q.1).abs() < 1e-9);
    }

    #[test]
    fn one_wheel_notch_is_one_react_wheel_event() {
        assert!((wheel_factor_lines(3.) - 1.15).abs() < 1e-12);
        assert!((wheel_factor_lines(-3.) - 1. / 1.15).abs() < 1e-12);
        assert!((wheel_factor_pixels(50.) - 1.15).abs() < 1e-12);
    }
}
