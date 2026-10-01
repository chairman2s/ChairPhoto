//! The Collage dialog's canvas: layout templates and pointer-driven placement editing. Port of
//! `src/modules/plugins/collageTemplates.ts` (one to one) and of the pure parts of
//! `CollageDialog.tsx` (the canvas size, gesture maths, swap, z-order, wheel zoom and the
//! cover-fill geometry that mirrors core `collage::resize_cover_offset`). See
//! docs/collage.md.
//!
//! Semantic choices against the TypeScript:
//! - Numbers are `f64`, as JavaScript's were; the app converts to core's `f32` placements only
//!   when it calls the backend.
//! - `clamp(v, lo, hi)` keeps the TS definition `min(max(v, lo), max(lo, hi))`: an inverted
//!   range clamps to `lo` instead of panicking like `f64::clamp`.
//! - Pointer positions are window pixels and the canvas rect is its on-screen bounds, as
//!   `clientX`/`getBoundingClientRect()` were.

/// A normalized cell rect (0–1 of the canvas).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cell {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// A layout template: a generator of one cell per photo, in order (`CollageTemplate`).
#[derive(Debug, Clone, Copy)]
pub struct Template {
    pub id: &'static str,
    pub label: &'static str,
    /// The auto-tag suffix (`Collage/<kind>`) — `TEMPLATE_KIND` in the dialog.
    pub kind: &'static str,
    gen: fn(usize) -> Option<Vec<Cell>>,
}

impl Template {
    /// Cells for `n` photos (`len == n`), or `None` if the template doesn't fit this count.
    pub fn cells(&self, n: usize) -> Option<Vec<Cell>> {
        (self.gen)(n)
    }
}

/// Even rows × cols grid (cols ≈ √n), row-major, filling all `n`; the last (short) row
/// stretches its cells to fill the width.
fn grid_cells(n: usize) -> Vec<Cell> {
    // JS Math.round rounds .5 up; √n is never exactly k.5 for an integer n ≥ 1, so `round`
    // agrees.
    let cols = ((n as f64).sqrt().round() as usize).max(1);
    let rows = n.div_ceil(cols);
    (0..n)
        .map(|i| {
            let r = i / cols;
            let c = i % cols;
            let in_row = if r == rows - 1 && n % cols != 0 { n % cols } else { cols };
            Cell { x: c as f64 / in_row as f64, y: r as f64 / rows as f64, w: 1.0 / in_row as f64, h: 1.0 / rows as f64 }
        })
        .collect()
}

fn feature(n: usize, first: Cell, rest: impl Fn(f64, f64) -> Cell) -> Option<Vec<Cell>> {
    if n < 2 {
        return None;
    }
    let m = (n - 1) as f64;
    let mut cells = vec![first];
    cells.extend((0..n - 1).map(|i| rest(i as f64, m)));
    Some(cells)
}

/// The templates, in menu order. In every "feature" template `cells[0]` is the large feature
/// slot — so swapping a photo into slot 0 (drag-to-swap in locked mode) makes it the feature.
pub const TEMPLATES: [Template; 7] = [
    Template { id: "grid", label: "Grid", kind: "Grid", gen: |n| (n >= 1).then(|| grid_cells(n)) },
    Template {
        id: "columns",
        label: "Columns",
        kind: "Columns",
        gen: |n| (n >= 1).then(|| (0..n).map(|i| Cell { x: i as f64 / n as f64, y: 0.0, w: 1.0 / n as f64, h: 1.0 }).collect()),
    },
    Template {
        id: "rows",
        label: "Rows",
        kind: "Rows",
        gen: |n| (n >= 1).then(|| (0..n).map(|i| Cell { x: 0.0, y: i as f64 / n as f64, w: 1.0, h: 1.0 / n as f64 }).collect()),
    },
    Template {
        id: "feature-left",
        label: "Feature + column (left)",
        kind: "Feature-left",
        gen: |n| feature(n, Cell { x: 0.0, y: 0.0, w: 0.62, h: 1.0 }, |i, m| Cell { x: 0.62, y: i / m, w: 0.38, h: 1.0 / m }),
    },
    Template {
        id: "feature-right",
        label: "Feature + column (right)",
        kind: "Feature-right",
        gen: |n| feature(n, Cell { x: 0.38, y: 0.0, w: 0.62, h: 1.0 }, |i, m| Cell { x: 0.0, y: i / m, w: 0.38, h: 1.0 / m }),
    },
    Template {
        id: "feature-top",
        label: "Feature + strip (top)",
        kind: "Feature-top",
        gen: |n| feature(n, Cell { x: 0.0, y: 0.0, w: 1.0, h: 0.62 }, |i, m| Cell { x: i / m, y: 0.62, w: 1.0 / m, h: 0.38 }),
    },
    Template {
        id: "feature-bottom",
        label: "Feature + strip (bottom)",
        kind: "Feature-bottom",
        gen: |n| feature(n, Cell { x: 0.0, y: 0.38, w: 1.0, h: 0.62 }, |i, m| Cell { x: i / m, y: 0.0, w: 1.0 / m, h: 0.38 }),
    },
];

/// The template with this id.
pub fn template(id: &str) -> Option<&'static Template> {
    TEMPLATES.iter().find(|t| t.id == id)
}

/// Canvas (and export) aspects. No "Free" — the canvas needs a bounded shape.
pub const ASPECTS: [(&str, u32, u32); 7] =
    [("1:1", 1, 1), ("4:5", 4, 5), ("5:4", 5, 4), ("3:2", 3, 2), ("2:3", 2, 3), ("16:9", 16, 9), ("9:16", 9, 16)];
/// Output widths offered.
pub const WIDTH_PRESETS: [u32; 3] = [1080, 2048, 4096];
pub const CANVAS_MAX_W: f64 = 520.0;
pub const CANVAS_MAX_H: f64 = 460.0;
/// The smallest tile, as a fraction of the canvas.
pub const MIN_TILE: f64 = 0.06;
/// What a fresh layout is tagged when no template made it.
pub const FREEFORM: &str = "Freeform";
/// What Auto-arrange's justified mosaic is tagged.
pub const MOSAIC: &str = "Mosaic";

/// `w / h` of an aspect id; an unknown id is the first aspect (1:1), as the dialog's
/// `ASPECTS.find(…) ?? ASPECTS[0]`.
pub fn aspect_ratio(id: &str) -> f64 {
    let (_, w, h) = ASPECTS.iter().find(|a| a.0 == id).copied().unwrap_or(ASPECTS[0]);
    w as f64 / h as f64
}

/// The on-screen canvas size for `ratio`: 520 wide, or 460 tall when that would exceed it.
pub fn display_size(ratio: f64) -> (f64, f64) {
    let (mut w, mut h) = (CANVAS_MAX_W, CANVAS_MAX_W / ratio);
    if h > CANVAS_MAX_H {
        h = CANVAS_MAX_H;
        w = CANVAS_MAX_H * ratio;
    }
    (w, h)
}

/// The output height for `width` at `ratio` (`Math.round(width / ratio)`).
pub fn output_height(width: u32, ratio: f64) -> u32 {
    (width as f64 / ratio).round() as u32
}

/// `min(max(v, lo), max(lo, hi))` — the dialog's clamp.
pub fn clamp(v: f64, lo: f64, hi: f64) -> f64 {
    v.max(lo).min(lo.max(hi))
}

/// One tile: normalized top-left and size (0–1 of the canvas), stacking order (higher on
/// top), the cover-crop's focal offset (0.5 = centred) and zoom (≥ 1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub photo_id: i64,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub z: i64,
    pub ox: f64,
    pub oy: f64,
    pub zoom: f64,
}

/// Where the photo sits inside its tile box (`CanvasTile`), as `(left, top, width, height)`
/// relative to the box: cover-filled at `zoom`, panned by `ox`/`oy`. Mirrors core
/// `resize_cover_offset`, so the canvas matches the export.
pub fn cover_rect(box_w: f64, box_h: f64, photo_aspect: f64, zoom: f64, ox: f64, oy: f64) -> (f64, f64, f64, f64) {
    let (w, h) = cover_size(box_w, box_h, photo_aspect, zoom);
    (-(w - box_w) * ox, -(h - box_h) * oy, w, h)
}

fn cover_size(box_w: f64, box_h: f64, photo_aspect: f64, zoom: f64) -> (f64, f64) {
    let ba = box_w / box_h.max(1.0);
    if photo_aspect > ba {
        let h = box_h * zoom;
        (h * photo_aspect, h)
    } else {
        let w = box_w * zoom;
        (w, w / photo_aspect)
    }
}

/// What a pointer-down on a tile starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GestureMode {
    /// Drag the tile (unlocked).
    Move,
    /// Corner-drag to resize (unlocked, selected tile's handle).
    Resize,
    /// Shift-drag: reposition the photo inside its frame.
    Pan,
    /// Locked layout: drop onto another tile to swap.
    Swap,
}

/// The gesture a pointer-down on a tile body starts: Shift pans, a locked layout swaps,
/// otherwise move.
pub fn body_gesture(shift: bool, locked: bool) -> GestureMode {
    if shift {
        GestureMode::Pan
    } else if locked {
        GestureMode::Swap
    } else {
        GestureMode::Move
    }
}

/// The on-screen canvas bounds, in the same pixels as pointer positions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CanvasRect {
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Copy)]
struct Gesture {
    id: i64,
    mode: GestureMode,
    px: f64,
    py: f64,
    start: Placement,
    photo_aspect: f64,
}

/// The canvas editor's state: the placements and what the pointer is doing to them.
#[derive(Debug, Clone, Default)]
pub struct CollageCanvas {
    pub placements: Vec<Placement>,
    pub selected: Option<i64>,
    /// Locked (template) mode: slots are fixed and dragging swaps photos between them.
    pub locked: bool,
    /// The slot a swap would drop onto (highlighted).
    pub swap_target: Option<i64>,
    /// The layout used, for the auto-tag `Collage/<kind>`.
    pub layout_kind: String,
    gesture: Option<Gesture>,
}

impl CollageCanvas {
    pub fn new() -> Self {
        CollageCanvas { layout_kind: FREEFORM.into(), ..Default::default() }
    }

    /// Auto-arrange's result: a free starting point you can tweak (unlocked, `Mosaic`).
    pub fn set_arranged(&mut self, placements: Vec<Placement>) {
        self.placements = placements;
        self.locked = false;
        self.layout_kind = MOSAIC.into();
    }

    /// Lay `photo_ids` into template `id`'s cells: fixed slots (locked), framing reset.
    /// `Err` says why it doesn't fit, as the dialog's error line.
    pub fn apply_template(&mut self, id: &str, photo_ids: &[i64]) -> Result<(), String> {
        let Some(t) = template(id) else { return Ok(()) };
        let Some(cells) = t.cells(photo_ids.len()) else {
            return Err(format!("Template \"{}\" doesn't fit {} photos.", t.label, photo_ids.len()));
        };
        self.placements = cells
            .iter()
            .zip(photo_ids)
            .enumerate()
            .map(|(i, (c, &photo_id))| Placement {
                photo_id,
                x: c.x,
                y: c.y,
                w: c.w,
                h: c.h,
                z: i as i64,
                ox: 0.5,
                oy: 0.5,
                zoom: 1.0,
            })
            .collect();
        self.selected = None;
        self.locked = true;
        self.layout_kind = t.kind.into();
        Ok(())
    }

    fn update(&mut self, id: i64, f: impl FnOnce(&mut Placement)) {
        if let Some(p) = self.placements.iter_mut().find(|p| p.photo_id == id) {
            f(p);
        }
    }

    pub fn placement(&self, id: i64) -> Option<&Placement> {
        self.placements.iter().find(|p| p.photo_id == id)
    }

    /// The placements in paint order (low `z` first).
    pub fn sorted(&self) -> Vec<Placement> {
        let mut v = self.placements.clone();
        v.sort_by_key(|p| p.z);
        v
    }

    /// A pointer-down on the empty canvas deselects.
    pub fn deselect(&mut self) {
        self.selected = None;
    }

    /// Whether a gesture is in progress.
    pub fn dragging(&self) -> bool {
        self.gesture.is_some()
    }

    /// Pointer-down on tile `id` at window position `(px, py)`: select it and start `mode`.
    /// `photo_aspect` is the photo's displayed aspect (1 until its thumbnail has loaded).
    pub fn begin(&mut self, id: i64, mode: GestureMode, px: f64, py: f64, photo_aspect: f64) {
        let Some(&start) = self.placement(id) else { return };
        self.selected = Some(id);
        self.gesture = Some(Gesture { id, mode, px, py, start, photo_aspect });
    }

    /// Pointer move to `(px, py)` over a canvas at `rect`.
    pub fn pointer_move(&mut self, px: f64, py: f64, rect: CanvasRect) {
        let Some(g) = self.gesture else { return };
        let dx = (px - g.px) / rect.width;
        let dy = (py - g.py) / rect.height;
        match g.mode {
            GestureMode::Swap => {
                // Highlight the topmost other slot under the pointer; the tile stays put.
                let nx = (px - rect.left) / rect.width;
                let ny = (py - rect.top) / rect.height;
                let mut target = None;
                let mut best_z = i64::MIN;
                for p in &self.placements {
                    if p.photo_id == g.id {
                        continue;
                    }
                    if nx >= p.x && nx <= p.x + p.w && ny >= p.y && ny <= p.y + p.h && p.z > best_z {
                        best_z = p.z;
                        target = Some(p.photo_id);
                    }
                }
                self.swap_target = target;
            }
            GestureMode::Move => {
                let x = clamp(g.start.x + dx, 0.0, 1.0 - g.start.w);
                let y = clamp(g.start.y + dy, 0.0, 1.0 - g.start.h);
                self.update(g.id, |p| {
                    p.x = x;
                    p.y = y;
                });
            }
            GestureMode::Pan => {
                // Pan each axis that has slack once cover-filled at the start zoom — both can
                // overflow once zoomed in.
                let box_w = g.start.w * rect.width;
                let box_h = g.start.h * rect.height;
                let (img_w, img_h) = cover_size(box_w, box_h, g.photo_aspect, g.start.zoom);
                let (overflow_x, overflow_y) = (img_w - box_w, img_h - box_h);
                let (mut ox, mut oy) = (g.start.ox, g.start.oy);
                if overflow_x > 0.5 {
                    ox = clamp(g.start.ox - (px - g.px) / overflow_x, 0.0, 1.0);
                }
                if overflow_y > 0.5 {
                    oy = clamp(g.start.oy - (py - g.py) / overflow_y, 0.0, 1.0);
                }
                self.update(g.id, |p| {
                    p.ox = ox;
                    p.oy = oy;
                });
            }
            GestureMode::Resize => {
                // Free resize from the corner — any cell shape (the photo cover-fills it); it
                // may overlap or bleed off the canvas (clipped).
                let w = clamp(g.start.w + dx, MIN_TILE, 1.0);
                let h = clamp(g.start.h + dy, MIN_TILE, 1.0);
                self.update(g.id, |p| {
                    p.w = w;
                    p.h = h;
                });
            }
        }
    }

    /// Pointer up: a move or resize makes the layout `Freeform`; a swap exchanges the two
    /// slots' photos (slots stay fixed) and resets both framings.
    pub fn pointer_up(&mut self) {
        let Some(g) = self.gesture.take() else {
            self.swap_target = None;
            return;
        };
        match g.mode {
            GestureMode::Move | GestureMode::Resize => self.layout_kind = FREEFORM.into(),
            GestureMode::Swap => {
                if let Some(target) = self.swap_target.filter(|&t| t != g.id) {
                    for p in &mut self.placements {
                        let swapped = if p.photo_id == g.id {
                            Some(target)
                        } else if p.photo_id == target {
                            Some(g.id)
                        } else {
                            None
                        };
                        if let Some(id) = swapped {
                            p.photo_id = id;
                            p.ox = 0.5;
                            p.oy = 0.5;
                            p.zoom = 1.0;
                        }
                    }
                }
            }
            GestureMode::Pan => {}
        }
        self.swap_target = None;
    }

    /// Wheel over tile `id`: select it and zoom its crop by `exp(-delta_y · 0.0015)`, within
    /// 1–6×.
    pub fn wheel(&mut self, id: i64, delta_y: f64) {
        self.selected = Some(id);
        let factor = (-delta_y * 0.0015).exp();
        self.update(id, |p| p.zoom = clamp(p.zoom * factor, 1.0, 6.0));
    }

    /// Front: the selected tile above every other.
    pub fn bring_to_front(&mut self) {
        let Some(id) = self.selected else { return };
        let max_z = self.placements.iter().fold(0, |m, p| m.max(p.z));
        self.update(id, |p| p.z = max_z + 1);
    }

    /// Back: the selected tile below every other.
    pub fn send_to_back(&mut self) {
        let Some(id) = self.selected else { return };
        let min_z = self.placements.iter().fold(0, |m, p| m.min(p.z));
        self.update(id, |p| p.z = min_z - 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn area(cells: &[Cell]) -> f64 {
        cells.iter().map(|c| c.w * c.h).sum()
    }

    fn inside(c: &Cell) -> bool {
        c.x >= -1e-9 && c.y >= -1e-9 && c.x + c.w <= 1.0 + 1e-9 && c.y + c.h <= 1.0 + 1e-9
    }

    #[test]
    fn every_template_tiles_the_whole_canvas_with_one_cell_per_photo() {
        for t in &TEMPLATES {
            for n in 2..=9 {
                let cells = t.cells(n).unwrap();
                assert_eq!(cells.len(), n, "{} n={n}", t.id);
                assert!(close(area(&cells), 1.0), "{} n={n}: area {}", t.id, area(&cells));
                assert!(cells.iter().all(inside), "{} n={n}", t.id);
            }
        }
    }

    #[test]
    fn feature_templates_need_two_photos_and_put_the_feature_first() {
        for id in ["feature-left", "feature-right", "feature-top", "feature-bottom"] {
            let t = template(id).unwrap();
            assert!(t.cells(1).is_none() && t.cells(0).is_none(), "{id}");
            let cells = t.cells(4).unwrap();
            assert!(close(cells[0].w * cells[0].h, 0.62), "{id}: the feature is 62 %");
            assert!(cells[1..].iter().all(|c| close(c.w * c.h, 0.38 / 3.0)), "{id}");
        }
        assert_eq!(template("feature-left").unwrap().cells(2).unwrap()[0], Cell { x: 0.0, y: 0.0, w: 0.62, h: 1.0 });
        assert_eq!(template("feature-right").unwrap().cells(2).unwrap()[0], Cell { x: 0.38, y: 0.0, w: 0.62, h: 1.0 });
        assert_eq!(template("feature-top").unwrap().cells(3).unwrap()[2], Cell { x: 0.5, y: 0.62, w: 0.5, h: 0.38 });
        assert_eq!(template("feature-bottom").unwrap().cells(3).unwrap()[1], Cell { x: 0.0, y: 0.0, w: 0.5, h: 0.38 });
    }

    #[test]
    fn single_photo_templates_and_empty_counts() {
        for id in ["grid", "columns", "rows"] {
            assert_eq!(template(id).unwrap().cells(1).unwrap(), [Cell { x: 0.0, y: 0.0, w: 1.0, h: 1.0 }], "{id}");
            assert!(template(id).unwrap().cells(0).is_none(), "{id}");
        }
    }

    #[test]
    fn grid_stretches_its_short_last_row() {
        // n = 5: cols = round(√5) = 2, rows = 3; the last row has one cell, full width.
        let cells = template("grid").unwrap().cells(5).unwrap();
        assert_eq!(cells[0], Cell { x: 0.0, y: 0.0, w: 0.5, h: 1.0 / 3.0 });
        assert_eq!(cells[3], Cell { x: 0.5, y: 1.0 / 3.0, w: 0.5, h: 1.0 / 3.0 });
        assert_eq!(cells[4], Cell { x: 0.0, y: 2.0 / 3.0, w: 1.0, h: 1.0 / 3.0 });
        // n = 7: cols = 3, rows = 3, last row one cell.
        let cells = template("grid").unwrap().cells(7).unwrap();
        assert!(close(cells[6].w, 1.0) && close(cells[5].w, 1.0 / 3.0));
        // n = 3: cols = 2, rows 2; the second row's one cell spans.
        let cells = template("grid").unwrap().cells(3).unwrap();
        assert_eq!(cells[2], Cell { x: 0.0, y: 0.5, w: 1.0, h: 0.5 });
    }

    #[test]
    fn template_kinds_and_menu_order_match_the_dialog() {
        let ids: Vec<_> = TEMPLATES.iter().map(|t| (t.id, t.kind)).collect();
        assert_eq!(
            ids,
            [
                ("grid", "Grid"),
                ("columns", "Columns"),
                ("rows", "Rows"),
                ("feature-left", "Feature-left"),
                ("feature-right", "Feature-right"),
                ("feature-top", "Feature-top"),
                ("feature-bottom", "Feature-bottom"),
            ]
        );
    }

    #[test]
    fn canvas_sizes_fit_520_by_460() {
        assert_eq!(display_size(1.0), (460.0, 460.0));
        assert_eq!(display_size(16.0 / 9.0), (520.0, 292.5));
        assert_eq!(display_size(9.0 / 16.0), (460.0 * 9.0 / 16.0, 460.0));
        assert_eq!(output_height(2048, aspect_ratio("4:5")), 2560);
        assert_eq!(output_height(1080, aspect_ratio("16:9")), 608);
        assert_eq!(aspect_ratio("nonsense"), 1.0);
    }

    fn rect() -> CanvasRect {
        CanvasRect { left: 100.0, top: 50.0, width: 400.0, height: 200.0 }
    }

    fn templated(locked_id: &str) -> CollageCanvas {
        let mut c = CollageCanvas::new();
        c.apply_template(locked_id, &[1, 2, 3]).unwrap();
        c
    }

    #[test]
    fn a_template_locks_and_tags_the_layout_and_reports_a_misfit() {
        let mut c = templated("columns");
        assert!(c.locked);
        assert_eq!(c.layout_kind, "Columns");
        assert_eq!(c.placements.iter().map(|p| (p.photo_id, p.z)).collect::<Vec<_>>(), [(1, 0), (2, 1), (3, 2)]);
        let err = c.apply_template("feature-left", &[1]).unwrap_err();
        assert_eq!(err, "Template \"Feature + column (left)\" doesn't fit 1 photos.");
        assert_eq!(c.placements.len(), 3, "a misfit changes nothing");
        c.set_arranged(vec![]);
        assert!(!c.locked);
        assert_eq!(c.layout_kind, "Mosaic");
    }

    #[test]
    fn the_gesture_follows_shift_and_lock() {
        assert_eq!(body_gesture(true, true), GestureMode::Pan);
        assert_eq!(body_gesture(true, false), GestureMode::Pan);
        assert_eq!(body_gesture(false, true), GestureMode::Swap);
        assert_eq!(body_gesture(false, false), GestureMode::Move);
    }

    #[test]
    fn a_move_stays_on_the_canvas_and_makes_the_layout_freeform() {
        let mut c = templated("columns"); // tile 1: x 0, w 1/3
        c.locked = false;
        c.begin(1, GestureMode::Move, 150.0, 100.0, 1.0);
        assert_eq!(c.selected, Some(1));
        c.pointer_move(150.0 + 100.0, 100.0 + 20.0, rect()); // +0.25, +0.1
        let p = *c.placement(1).unwrap();
        assert!(close(p.x, 0.25) && close(p.y, 0.0), "y clamps at 1 - h = 0: {p:?}");
        c.pointer_move(150.0 + 4000.0, 100.0, rect());
        assert!(close(c.placement(1).unwrap().x, 2.0 / 3.0), "x clamps at 1 - w");
        c.pointer_move(150.0 - 4000.0, 100.0, rect());
        assert!(close(c.placement(1).unwrap().x, 0.0));
        c.pointer_up();
        assert_eq!(c.layout_kind, "Freeform");
        assert!(!c.dragging());
    }

    #[test]
    fn a_resize_keeps_the_minimum_tile_and_the_canvas_bound() {
        let mut c = templated("grid");
        c.begin(2, GestureMode::Resize, 0.0, 0.0, 1.0);
        c.pointer_move(-10_000.0, -10_000.0, rect());
        let p = *c.placement(2).unwrap();
        assert!(close(p.w, MIN_TILE) && close(p.h, MIN_TILE));
        c.pointer_move(10_000.0, 10_000.0, rect());
        let p = *c.placement(2).unwrap();
        assert!(close(p.w, 1.0) && close(p.h, 1.0));
        c.pointer_up();
        assert_eq!(c.layout_kind, "Freeform");
    }

    #[test]
    fn a_pan_moves_only_an_axis_with_slack_and_keeps_the_layout_kind() {
        let mut c = templated("columns"); // a 1/3 × 1 cell = 133.3 × 200 px: a portrait box
        // A landscape photo (2:1) cover-fills by height: 400 px wide, 266.7 px overflow in x.
        c.begin(1, GestureMode::Pan, 0.0, 0.0, 2.0);
        c.pointer_move(-133.33333333333334, -50.0, rect());
        let p = *c.placement(1).unwrap();
        assert!(close(p.ox, 1.0), "x: 0.5 + 133.3/266.7 = 1.0: {p:?}");
        assert!(close(p.oy, 0.5), "no y slack at zoom 1");
        c.pointer_up();
        assert_eq!(c.layout_kind, "Columns", "a pan is not a layout edit");
        // Zoomed in, both axes have slack.
        c.wheel(1, -1000.0);
        let zoom = c.placement(1).unwrap().zoom;
        assert!(close(zoom, 1.5f64.exp().min(6.0)), "{zoom}");
        c.begin(1, GestureMode::Pan, 0.0, 0.0, 2.0);
        c.pointer_move(0.0, 10.0, rect());
        assert!(c.placement(1).unwrap().oy < 0.5);
    }

    #[test]
    fn the_wheel_zoom_stays_within_one_to_six() {
        let mut c = templated("rows");
        c.wheel(3, 10_000.0);
        assert_eq!(c.placement(3).unwrap().zoom, 1.0);
        c.wheel(3, -10_000.0);
        assert_eq!(c.placement(3).unwrap().zoom, 6.0);
        assert_eq!(c.selected, Some(3));
    }

    #[test]
    fn a_locked_drag_swaps_the_photos_not_the_slots() {
        let mut c = CollageCanvas::new();
        c.apply_template("feature-left", &[1, 2, 3]).unwrap();
        c.wheel(3, -500.0);
        // Drag photo 3 (bottom-right cell) onto the feature cell (left 62 %).
        c.begin(3, GestureMode::Swap, 450.0, 200.0, 1.0);
        c.pointer_move(150.0, 100.0, rect()); // normalized (0.125, 0.25): the feature cell
        assert_eq!(c.swap_target, Some(1));
        c.pointer_up();
        let feature = c.placements[0];
        assert_eq!((feature.photo_id, feature.w), (3, 0.62), "photo 3 is now the feature");
        assert_eq!(c.placements[2].photo_id, 1);
        assert!(c.placements.iter().all(|p| p.zoom == 1.0 && p.ox == 0.5), "framing reset");
        assert_eq!(c.swap_target, None);
        assert_eq!(c.layout_kind, "Feature-left", "a swap keeps the template");
    }

    #[test]
    fn a_swap_dropped_on_nothing_or_itself_changes_nothing() {
        let mut c = templated("columns");
        let before = c.placements.clone();
        c.begin(1, GestureMode::Swap, 120.0, 60.0, 1.0);
        c.pointer_move(120.0, 60.0, rect()); // over itself only
        assert_eq!(c.swap_target, None);
        c.pointer_move(10.0, 10.0, rect()); // off the canvas
        c.pointer_up();
        assert_eq!(c.placements, before);
    }

    #[test]
    fn a_swap_targets_the_topmost_overlapping_tile() {
        let mut c = templated("columns");
        c.locked = false;
        // Pull tile 3 over tile 2, on top.
        c.update(3, |p| p.x = 1.0 / 3.0);
        c.selected = Some(3);
        c.bring_to_front();
        c.begin(1, GestureMode::Swap, 0.0, 0.0, 1.0);
        c.pointer_move(100.0 + 0.5 * 400.0, 100.0, rect());
        assert_eq!(c.swap_target, Some(3));
    }

    #[test]
    fn front_and_back_restack_the_selected_tile() {
        let mut c = templated("rows");
        c.bring_to_front();
        assert_eq!(c.placements.iter().map(|p| p.z).collect::<Vec<_>>(), [0, 1, 2], "nothing selected");
        c.selected = Some(1);
        c.bring_to_front();
        assert_eq!(c.placement(1).unwrap().z, 3);
        assert_eq!(c.sorted().last().unwrap().photo_id, 1);
        c.selected = Some(3);
        c.send_to_back();
        assert_eq!(c.placement(3).unwrap().z, -1);
        assert_eq!(c.sorted()[0].photo_id, 3);
    }

    #[test]
    fn the_cover_rect_mirrors_the_export_crop() {
        // Same aspect at zoom 1: the whole photo, unpanned.
        assert_eq!(cover_rect(200.0, 100.0, 2.0, 1.0, 0.5, 0.5), (0.0, 0.0, 200.0, 100.0));
        // A portrait photo in a landscape box fills the width and overflows in y.
        let (l, t, w, h) = cover_rect(200.0, 100.0, 0.5, 1.0, 0.5, 0.0);
        assert_eq!((l, t, w, h), (0.0, 0.0, 200.0, 400.0));
        let (_, t, _, _) = cover_rect(200.0, 100.0, 0.5, 1.0, 0.5, 1.0);
        assert_eq!(t, -300.0, "oy 1 shows the bottom");
        // Zoom 2 doubles both sides around the focal point.
        assert_eq!(cover_rect(200.0, 100.0, 2.0, 2.0, 0.5, 0.5), (-100.0, -50.0, 400.0, 200.0));
    }

    #[test]
    fn the_clamp_is_the_dialogs() {
        assert_eq!(clamp(5.0, 0.0, 1.0), 1.0);
        assert_eq!(clamp(-1.0, 0.0, 1.0), 0.0);
        assert_eq!(clamp(0.5, 0.2, 0.1), 0.2, "an inverted range clamps to lo");
    }
}
