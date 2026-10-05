//! The Darkroom's rails on the view (#112): the version shelf, cover and "+ New version" on
//! the bar; the proof-sheet, duel and "Save as preset" actions; the History panel, Preset
//! browser, Lens and Crop & Rotate sections of the right rail; the crop box (guides, size,
//! corner handles), perspective quad and level line over the frame; and the proof sheet and
//! duel (`crate::loupe`) mounted over the whole view. Ports of `DarkroomView.tsx`'s bar and
//! actions, `HistoryPanel.tsx`, `PresetBrowser.tsx`, `LensRail.tsx`, and `EditStage` /
//! `GeometryRail` / `CropGuides` in `EditControls.tsx`.

use super::*;
use crate::darkroom::session::{DarkroomEvent, OpenPhoto};
use chairphoto_core::catalog::CoverPin;
use crate::loupe::duel::{variant_image, DuelEvent, DuelView};
use crate::loupe::edit_renders::EditRenders;
use crate::loupe::proof_sheet::{ProofEvent, ProofSheet, PROOF_EDGE};
use chairphoto_model::darkroom::controls::preset_is_active;
use chairphoto_model::darkroom::geometry::{self as geo, CROP_CORNERS};
use chairphoto_model::darkroom::history::{step_state, when_label, StepState};
use chairphoto_model::darkroom::lens_rail::lens_hint;
use chairphoto_model::editing::{overlay_lines, CropOverlay, QuadCorner, ASPECTS, OVERLAYS, QUAD_CORNERS, STRAIGHTEN_MAX};
use chairphoto_model::presets::{PresetCategory, PRESET_CATEGORIES};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::Sizable as _;
use gpui_kit::{point, App, Focusable as _, Hsla, MouseUpEvent, PathBuilder, Point};

/// What is mounted over the Darkroom.
pub enum Overlay {
    Proof(Entity<ProofSheet>),
    Duel(Entity<DuelView>),
}

/// The rails' presentation state (the record lives in the [`Darkroom`]).
pub(super) struct RailsState {
    /// "☆ Save as preset"'s inline name field, while it is shown.
    preset_name: Entity<InputState>,
    naming: bool,
    /// The preset browser's rename field and the preset it renames.
    rename: Entity<InputState>,
    renaming: Option<String>,
    presets_open: bool,
    preset_renders: Option<Entity<EditRenders>>,
    straighten: Entity<SliderState>,
    /// "Draw level line" is on: a drag on the frame draws it.
    straighten_mode: bool,
    overlay: Option<Overlay>,
    overlay_subs: Vec<Subscription>,
    /// The frame's rectangle in the stage at the last render (stage-local px).
    frame: Cell<Option<(f64, f64, f64, f64)>>,
}

impl RailsState {
    pub(super) fn new(window: &mut Window, cx: &mut Context<DarkroomView>) -> (Self, Vec<Subscription>) {
        let preset_name = cx.new(|cx| InputState::new(window, cx).placeholder("Preset name"));
        let rename = cx.new(|cx| InputState::new(window, cx).placeholder("Preset name"));
        let straighten = cx.new(|_| {
            SliderState::new().min(-STRAIGHTEN_MAX as f32).max(STRAIGHTEN_MAX as f32).step(0.1).default_value(0.0)
        });
        let subs = vec![
            cx.subscribe_in(&preset_name, window, |this: &mut DarkroomView, _, e: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = e {
                    this.save_preset(window, cx);
                }
            }),
            cx.subscribe_in(&rename, window, |this: &mut DarkroomView, _, e: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = e {
                    this.confirm_rename(window, cx);
                }
            }),
            cx.subscribe_in(&straighten, window, |this: &mut DarkroomView, _, e: &SliderEvent, _, cx| {
                if let SliderEvent::Change(v) = e {
                    let v = snap(v.start() as f64, -STRAIGHTEN_MAX, STRAIGHTEN_MAX, 0.1);
                    this.set_straighten(v, cx);
                }
            }),
        ];
        let state = RailsState {
            preset_name,
            naming: false,
            rename,
            renaming: None,
            presets_open: false,
            preset_renders: None,
            straighten,
            straighten_mode: false,
            overlay: None,
            overlay_subs: Vec::new(),
            frame: Cell::new(None),
        };
        (state, subs)
    }

    /// A text field of the rails has the keys.
    pub(super) fn typing(&self, window: &Window, cx: &App) -> bool {
        (self.naming && self.preset_name.read(cx).focus_handle(cx).is_focused(window))
            || (self.renaming.is_some() && self.rename.read(cx).focus_handle(cx).is_focused(window))
    }

    pub(super) fn sync_straighten(&self, working: &VersionEdit, window: &mut Window, cx: &mut Context<DarkroomView>) {
        let v = geo::straighten_of(working) as f32;
        if (self.straighten.read(cx).value().start() - v).abs() > 1e-6 {
            self.straighten.update(cx, |s, cx| s.set_value(v, window, cx));
        }
    }

    pub(super) fn overlay_open(&self) -> bool {
        self.overlay.is_some()
    }

    pub(super) fn overlay_element(&self) -> Option<AnyElement> {
        match &self.overlay {
            Some(Overlay::Proof(p)) => Some(p.clone().into_any_element()),
            Some(Overlay::Duel(d)) => Some(d.clone().into_any_element()),
            None => None,
        }
    }
}

/// White at `a`.
fn white(a: f32) -> Hsla {
    Hsla { h: 0., s: 0., l: 1., a }
}

fn black(a: f32) -> Hsla {
    Hsla { h: 0., s: 0., l: 0., a }
}

/// A stroked polyline in `bounds`, points as fractions of it.
fn polyline(window: &mut Window, bounds: Bounds<Pixels>, pts: &[(f64, f64)], closed: bool, width: f32, color: Hsla) {
    let at = |(x, y): (f64, f64)| -> Point<Pixels> {
        point(bounds.origin.x + bounds.size.width * x as f32, bounds.origin.y + bounds.size.height * y as f32)
    };
    let mut pb = PathBuilder::stroke(px(width));
    for (i, p) in pts.iter().enumerate() {
        if i == 0 {
            pb.move_to(at(*p))
        } else {
            pb.line_to(at(*p))
        }
    }
    if closed {
        if let Some(p) = pts.first() {
            pb.line_to(at(*p));
        }
    }
    if let Ok(path) = pb.build() {
        window.paint_path(path, color);
    }
}

/// The golden spiral of [`chairphoto_model::editing::GOLDEN_SPIRAL_PATH`] as quarter arcs
/// (centre, radius, start and end angle), in the 0–100 box.
const SPIRAL_ARCS: [((f64, f64), f64, f64, f64); 5] = [
    ((61.8, 61.8), 61.8, 180.0, 270.0),
    ((61.8, 38.2), 38.2, 270.0, 360.0),
    ((76.4, 38.2), 23.6, 0.0, 90.0),
    ((76.4, 47.2), 14.6, 90.0, 180.0),
    ((70.8, 47.2), 9.0, 180.0, 270.0),
];

fn spiral_points() -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    for ((cx_, cy_), r, a0, a1) in SPIRAL_ARCS {
        for i in 0..=16 {
            let a = (a0 + (a1 - a0) * i as f64 / 16.0).to_radians();
            out.push(((cx_ + r * a.cos()) / 100.0, (cy_ + r * a.sin()) / 100.0));
        }
    }
    out
}

/// `CropGuides`: the composition overlay inside the crop box.
fn guides(overlay: CropOverlay) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| match overlay {
            CropOverlay::None => {}
            CropOverlay::Golden => polyline(window, bounds, &spiral_points(), false, 1.0, white(0.6)),
            o => {
                for f in overlay_lines(o) {
                    polyline(window, bounds, &[(*f, 0.0), (*f, 1.0)], false, 1.0, white(0.5));
                    polyline(window, bounds, &[(0.0, *f), (1.0, *f)], false, 1.0, white(0.5));
                }
            }
        },
    )
    .absolute()
    .size_full()
}

const PRESET_CARD: f32 = 116.;

impl DarkroomView {
    // --- following the Darkroom --------------------------------------------------------------

    /// Another photo (or none) is open: overlays and modes belong to the one left.
    pub(super) fn photo_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_overlay(window, cx);
        self.rails.naming = false;
        self.rails.renaming = None;
        self.rails.straighten_mode = false;
    }

    pub(super) fn on_darkroom_event(&mut self, event: &DarkroomEvent, cx: &mut Context<Self>) {
        match event {
            DarkroomEvent::Kept(name) => {
                if let Some(Overlay::Duel(d)) = &self.rails.overlay {
                    d.update(cx, |d, cx| d.kept(name, cx));
                }
            }
        }
    }

    /// The preset cards' renders: every preset's look on this photo while the browser is
    /// open (lazy, as React's on first expand), nothing while it is closed.
    pub(super) fn sync_preset_renders(&mut self, cx: &mut Context<Self>) {
        let d = self.darkroom.read(cx);
        let jobs: Vec<_> = match (self.rails.presets_open, d.variant_source()) {
            (true, Some(source)) => d.presets().iter().map(|p| source.job(&p.edit.value_or_default(), PROOF_EDGE)).collect(),
            _ => Vec::new(),
        };
        if jobs.is_empty() && self.rails.preset_renders.is_none() {
            return;
        }
        let renders = match &self.rails.preset_renders {
            Some(r) => r.clone(),
            None => {
                let pool = d.images().read(cx).pool();
                let r = cx.new(|cx| EditRenders::new(pool, cx));
                self._subscriptions.push(cx.observe(&r, |_, _, cx| cx.notify()));
                self.rails.preset_renders = Some(r.clone());
                r
            }
        };
        renders.update(cx, |r, cx| r.want(&jobs, cx));
    }

    pub fn presets_open(&self) -> bool {
        self.rails.presets_open
    }

    pub fn set_presets_open(&mut self, open: bool, cx: &mut Context<Self>) {
        self.rails.presets_open = open;
        self.sync_preset_renders(cx);
        cx.notify();
    }

    pub fn overlay(&self) -> Option<&Overlay> {
        self.rails.overlay.as_ref()
    }

    pub fn straighten_mode(&self) -> bool {
        self.rails.straighten_mode
    }

    pub fn straighten_slider(&self) -> &Entity<SliderState> {
        &self.rails.straighten
    }

    // --- the frame -----------------------------------------------------------------------------

    /// The original's dimensions as shown (oriented) and the frame's own, for the aspect
    /// maths and the size readout.
    fn dims(&self, cx: &App) -> (Option<(f64, f64)>, Option<(f64, f64)>) {
        let d = self.darkroom.read(cx);
        let Some(open) = d.open.as_ref() else { return (None, None) };
        let shown = open.stage.read(cx).frame().map(|f| {
            let s = f.image.size(0);
            (s.width.0 as f64, s.height.0 as f64)
        });
        let photo = match (open.photo.width, open.photo.height) {
            (Some(w), Some(h)) => Some((w as f64, h as f64)),
            _ => None,
        };
        (geo::oriented_dims(photo, shown), shown)
    }

    /// The aspect maths' frame: the oriented original, else the frame shown.
    pub fn src_dims(&self, cx: &App) -> Option<(f64, f64)> {
        let (oriented, shown) = self.dims(cx);
        oriented.or(shown)
    }

    /// A pointer position as fractions of the frame.
    fn frame_fraction(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let b = self.stage_bounds.get()?;
        let (fx, fy, fw, fh) = self.rails.frame.get()?;
        if fw <= 0.0 || fh <= 0.0 {
            return None;
        }
        Some(((x - f64::from(b.origin.x) - fx) / fw, (y - f64::from(b.origin.y) - fy) / fh))
    }

    /// Enter: frame the view to the crop.
    pub(super) fn zoom_to_crop(&mut self, cx: &mut Context<Self>) {
        let Some(crop) = self.darkroom.read(cx).open.as_ref().and_then(|o| geo::crop_of(&o.working).cloned()) else { return };
        let (Some(b), Some((_, _, fw, fh))) = (self.stage_bounds.get(), self.rails.frame.get()) else { return };
        // The fitted frame's size (the last render's is scaled by the zoom).
        let (fw, fh) = (fw / self.view.scale, fh / self.view.scale);
        let (sw, sh) = (f64::from(b.size.width), f64::from(b.size.height));
        let (cw, ch) = (crop.w * fw, crop.h * fh);
        if cw <= 0.0 || ch <= 0.0 {
            return;
        }
        let scale = (sw / cw).min(sh / ch).min(StageView::MAX_SCALE);
        self.view = StageView {
            scale,
            tx: -scale * fw * (crop.x + crop.w / 2.0 - 0.5),
            ty: -scale * fh * (crop.y + crop.h / 2.0 - 0.5),
        };
        cx.notify();
    }

    /// A frame drag moved to `(x, y)` (window pixels).
    pub(super) fn drag_frame(&mut self, drag: Drag, x: f64, y: f64, cx: &mut Context<Self>) {
        let Some((fx, fy)) = self.frame_fraction(x, y) else { return };
        let src = self.src_dims(cx);
        let frame = self.rails.frame.get();
        match drag {
            Drag::CropMove { x: x0, y: y0, start } => {
                let Some((_, _, fw, fh)) = frame else { return };
                let c = geo::moved_crop(&start, (x - x0) / fw, (y - y0) / fh);
                self.edit(cx, |w, _| geo::with_crop(w, c));
            }
            Drag::CropCorner { anchor } => {
                self.edit(cx, |w, _| match geo::crop_of(w) {
                    Some(c) => {
                        let ratio = geo::ratio_for(&geo::aspect_label(w));
                        geo::with_crop(w, geo::resized_crop(c, anchor, fx, fy, ratio, src))
                    }
                    None => w.clone(),
                });
            }
            Drag::Quad(corner) => self.edit(cx, |w, _| geo::with_quad_corner(w, corner, fx, fy)),
            Drag::Level { x0, y0, .. } => {
                let Some((_, _, fw, fh)) = frame else { return };
                let s = self.view.scale;
                self.drag = Some(Drag::Level { x0, y0, x1: fx * fw / s, y1: fy * fh / s });
                cx.notify();
            }
            Drag::Pan { .. } | Drag::Zone { .. } => {}
        }
    }

    /// The button came up: a level line levels the picture (or, too short, does nothing) and
    /// the mode ends either way.
    pub(super) fn end_drag(&mut self, _e: &MouseUpEvent, cx: &mut Context<Self>) {
        if let Some(Drag::Level { x0, y0, x1, y1 }) = self.drag.take() {
            self.rails.straighten_mode = false;
            if let Some(delta) = geo::level_delta(x0, y0, x1, y1) {
                let src = self.src_dims(cx);
                self.edit(cx, |w, _| geo::apply_straighten(w, geo::straighten_of(w) + delta, src));
            }
            cx.notify();
        }
    }

    /// Start a drag on the frame (a handle's mouse-down), taking the keys back.
    fn start_drag(&mut self, drag: Drag, window: &mut Window, cx: &mut Context<Self>) {
        self.drag = Some(drag);
        self.focus.focus(window, cx);
        cx.stop_propagation();
    }

    /// The crop box, the perspective quad and the level line, over the frame at `rect`.
    pub(super) fn frame_overlays(
        &self,
        d: &Darkroom,
        rect: (f64, f64, f64, f64),
        _shown: (f64, f64),
        colors: Colors,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        self.rails.frame.set(Some(rect));
        let Some(open) = d.open.as_ref() else { return Vec::new() };
        let (x, y, w, h) = rect;
        let mut out = Vec::new();
        let at = |fx: f64, fy: f64| (px((x + fx * w) as f32), px((y + fy * h) as f32));
        if let Some(c) = geo::crop_of(&open.working).filter(|_| !open.perspective_mode) {
            let (l, t) = at(c.x, c.y);
            let start = c.clone();
            let size = geo::crop_px(&open.working, self.dims(cx).0);
            let mut boxed = div()
                .id("dk-crop")
                .absolute()
                .left(l)
                .top(t)
                .w(px((c.w * w) as f32))
                .h(px((c.h * h) as f32))
                .border_1()
                .border_color(white(0.9))
                .cursor_move()
                .child(guides(d.overlay))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                        let (x, y) = (f64::from(e.position.x), f64::from(e.position.y));
                        this.start_drag(Drag::CropMove { x, y, start: start.clone() }, window, cx);
                    }),
                )
                .children(size.map(|(pw, ph)| {
                    div()
                        .id("dk-crop-size")
                        .absolute()
                        .bottom(px(4.))
                        .right(px(4.))
                        .px(px(5.))
                        .rounded(px(3.))
                        .bg(black(0.55))
                        .text_color(white(1.))
                        .text_size(px(10.))
                        .child(format!("{pw} × {ph} px"))
                        .test_support()
                }));
            for corner in CROP_CORNERS {
                let (cx_, cy_) = corner.position(c);
                let anchor = corner.anchor(c);
                boxed = boxed.child(
                    div()
                        .id(SharedString::from(format!("dk-crop-{}", corner.key())))
                        .absolute()
                        .left(px(((cx_ - c.x) * w) as f32 - 6.))
                        .top(px(((cy_ - c.y) * h) as f32 - 6.))
                        .size(px(12.))
                        .bg(white(1.))
                        .border_1()
                        .border_color(black(0.6))
                        .cursor_crosshair()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                                this.start_drag(Drag::CropCorner { anchor }, window, cx)
                            }),
                        )
                        .test_support(),
                );
            }
            out.push(boxed.test_support().into_any_element());
        }
        if let Some(p) = geo::perspective_of(&open.working).filter(|_| open.perspective_mode) {
            let pts: Vec<(f64, f64)> = QUAD_CORNERS.iter().map(|c| (p.corner(*c)[0], p.corner(*c)[1])).collect();
            let (l, t) = at(0.0, 0.0);
            out.push(
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, _| {
                        polyline(window, bounds, &pts, true, 3.0, black(0.5));
                        polyline(window, bounds, &pts, true, 1.5, white(0.95));
                    },
                )
                .absolute()
                .left(l)
                .top(t)
                .w(px(w as f32))
                .h(px(h as f32))
                .into_any_element(),
            );
            for corner in QUAD_CORNERS {
                let [qx, qy] = p.corner(corner);
                let (l, t) = at(qx, qy);
                out.push(
                    div()
                        .id(SharedString::from(format!("dk-quad-{}", corner.key())))
                        .absolute()
                        .left(l - px(7.))
                        .top(t - px(7.))
                        .size(px(14.))
                        .rounded_full()
                        .bg(colors.accent)
                        .border_2()
                        .border_color(white(1.))
                        .cursor_crosshair()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, window, cx| this.start_drag(Drag::Quad(corner), window, cx)),
                        )
                        .test_support()
                        .into_any_element(),
                );
            }
        }
        if self.rails.straighten_mode {
            let (l, t) = at(0.0, 0.0);
            let line = match &self.drag {
                Some(Drag::Level { x0, y0, x1, y1 }) => Some((*x0, *y0, *x1, *y1)),
                _ => None,
            };
            let scale = self.view.scale;
            let mut capture = div()
                .id("dk-level")
                .absolute()
                .left(l)
                .top(t)
                .w(px(w as f32))
                .h(px(h as f32))
                .cursor_crosshair()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                        let Some((fx, fy)) = this.frame_fraction(f64::from(e.position.x), f64::from(e.position.y)) else { return };
                        let (_, _, fw, fh) = this.rails.frame.get().unwrap_or_default();
                        let (px_, py_) = (fx * fw / this.view.scale, fy * fh / this.view.scale);
                        this.start_drag(Drag::Level { x0: px_, y0: py_, x1: px_, y1: py_ }, window, cx);
                    }),
                );
            if let Some((x0, y0, x1, y1)) = line {
                // Fit-frame pixels → fractions of this frame.
                let (fw, fh) = (w / scale, h / scale);
                let pts = [(x0 / fw, y0 / fh), (x1 / fw, y1 / fh)];
                capture = capture.child(
                    canvas(
                        |_, _, _| {},
                        move |bounds, _, window, _| {
                            polyline(window, bounds, &pts, false, 3.0, black(0.6));
                            polyline(window, bounds, &pts, false, 1.5, white(1.));
                        },
                    )
                    .absolute()
                    .size_full(),
                );
            }
            out.push(capture.test_support().into_any_element());
        }
        out
    }

    // --- the bar ---------------------------------------------------------------------------------

    /// The version shelf: Original and every version; a click saves first, then switches. A
    /// pinned face is starred (#252).
    pub(super) fn render_shelf(&self, d: &Darkroom, colors: Colors) -> Option<AnyElement> {
        let open = d.open.as_ref()?;
        let on = |el: gpui_kit::Stateful<gpui_kit::Div>, on: bool| el.when(on, |c| c.border_color(colors.accent).text_color(colors.accent));
        let starred = |name: &str, pinned: bool| if pinned { format!("★ {name}") } else { name.to_string() };
        let mut shelf = div().id("dk-shelf").flex().gap(px(4.)).overflow_x_scroll();
        let dk = self.darkroom.clone();
        let original = starred("Original", open.pin == CoverPin::Original);
        shelf = shelf.child(clickable(
            on(chip("dk-shelf-original", original, open.loaded, colors), open.version_id.is_none())
                .tooltip(crate::shell::title_bar::tooltip("The unedited original. Changing anything here starts a new version.")),
            open.loaded,
            move |_, _, cx| dk.update(cx, |d, cx| d.switch_version(None, cx)),
        ));
        for v in &open.versions {
            let (id, dk) = (v.id, self.darkroom.clone());
            let tip = format!("Edit \"{}\" — every change is saved to it, with history", v.name);
            shelf = shelf.child(clickable(
                on(chip(format!("dk-shelf-{id}"), starred(&v.name, open.pin == CoverPin::Version(id)), true, colors), open.version_id == Some(id))
                    .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx)),
                true,
                move |_, _, cx| dk.update(cx, |d, cx| d.switch_version(Some(id), cx)),
            ));
        }
        Some(shelf.test_support().into_any_element())
    }

    /// The face: whether it is pinned or follows the latest change, the pin toggle for what is
    /// shown (a version, or the Original) (#252); and "+ New version".
    pub(super) fn render_bar_actions(&self, d: &Darkroom, colors: Colors) -> Vec<AnyElement> {
        let Some(open) = d.open.as_ref() else { return Vec::new() };
        let mut out = vec![div().flex_1().into_any_element()];
        let shown = open.version_id.map_or(CoverPin::Original, CoverPin::Version);
        let is_cover = open.pin == shown;
        if open.pin == CoverPin::Auto {
            out.push(
                div()
                    .id("dk-face-auto")
                    .text_size(px(11.))
                    .text_color(colors.dim)
                    .child("Face: latest edit")
                    .tooltip(crate::shell::title_bar::tooltip(
                        "The Library shows the version you changed last. Pin one with \"Use as cover\" to keep it.",
                    ))
                    .test_support()
                    .into_any_element(),
            );
        }
        let dk = self.darkroom.clone();
        let el = chip("dk-cover", if is_cover { "★ Cover" } else { "☆ Use as cover" }, open.loaded, colors)
            .when(is_cover, |c| c.border_color(colors.accent).text_color(colors.accent))
            .tooltip(crate::shell::title_bar::tooltip(match (is_cover, open.version_id) {
                (true, _) => "Pinned: the Library shows this for the photo whatever you edit later — click to unpin, and the face follows your latest edit again",
                (false, Some(_)) => "Pin this version's look as the photo's face in the Library",
                (false, None) => "Pin the unedited original as the photo's face in the Library, whatever the versions hold",
            }));
        out.push(clickable(el, open.loaded, move |_, _, cx| dk.update(cx, |d, cx| d.toggle_cover(cx))));
        let dk = self.darkroom.clone();
        out.push(clickable(
            chip("dk-new-version", "+ New version", open.loaded, colors).tooltip(crate::shell::title_bar::tooltip(
                "Copy these settings into a new version and keep editing there. Changes are saved automatically; only settings are stored, never pixels.",
            )),
            open.loaded,
            move |_, _, cx| dk.update(cx, |d, cx| d.new_version(cx)),
        ));
        out
    }

    // --- the actions row -----------------------------------------------------------------------

    pub(super) fn rails_actions(&self, d: &Darkroom, colors: Colors, cx: &Context<Self>) -> Vec<AnyElement> {
        let Some(open) = d.open.as_ref() else { return Vec::new() };
        let mut out = Vec::new();
        let dealt = open.auto_fragment.is_some();
        out.push(clickable(
            chip("dk-proofs", "▦ Deal a proof sheet", dealt, colors)
                .tooltip(crate::shell::title_bar::tooltip("Your photo developed a dozen ways — pick the one that's closest")),
            dealt,
            cx.listener(|this, _, window, cx| this.open_proof_sheet(window, cx)),
        ));
        out.push(clickable(
            chip("dk-duel", "⚖ Refine by duel", true, colors)
                .tooltip(crate::shell::title_bar::tooltip("Refine by choosing: two prints per round, pick the better one")),
            true,
            cx.listener(|this, _, window, cx| this.open_duel(window, cx)),
        ));
        if self.rails.naming {
            let can = !self.rails.preset_name.read(cx).value().trim().is_empty();
            out.push(
                div()
                    .id("dk-preset-naming")
                    .flex()
                    .gap(px(6.))
                    .items_center()
                    .on_key_down(cx.listener(|this, e: &KeyDownEvent, window, cx| {
                        if e.keystroke.key == "escape" {
                            this.cancel_naming(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(div().w(px(160.)).child(Input::new(&self.rails.preset_name).id("dk-preset-name").small()))
                    .child(clickable(chip("dk-preset-save", "Save", can, colors), can, cx.listener(|this, _, window, cx| this.save_preset(window, cx))))
                    .child(clickable(chip("dk-preset-cancel", "Cancel", true, colors), true, cx.listener(|this, _, window, cx| this.cancel_naming(window, cx))))
                    .into_any_element(),
            );
        } else {
            out.push(clickable(
                chip("dk-save-preset", "☆ Save as preset", true, colors).tooltip(crate::shell::title_bar::tooltip(
                    "Save this look — tone and effects, not the crop or straighten — as a preset",
                )),
                true,
                cx.listener(|this, _, window, cx| this.start_naming(window, cx)),
            ));
        }
        if let Some(n) = &d.notice {
            out.push(div().id("dk-notice").text_size(px(11.)).text_color(colors.ok).child(n.clone()).test_support().into_any_element());
        }
        out
    }

    /// "☆ Save as preset": the inline name field, focused.
    pub fn start_naming(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.rails.naming = true;
        self.rails.preset_name.update(cx, |i, cx| i.set_value("", window, cx));
        let focus = self.rails.preset_name.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    fn cancel_naming(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.rails.naming = false;
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// Enter or Save: a non-empty name saves the look; the field closes.
    pub fn save_preset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.rails.preset_name.read(cx).value().trim().to_string();
        if name.is_empty() {
            return;
        }
        self.darkroom.update(cx, |d, cx| d.save_preset(&name, cx));
        self.cancel_naming(window, cx);
    }

    pub fn preset_name_input(&self) -> &Entity<InputState> {
        &self.rails.preset_name
    }

    pub fn rename_input(&self) -> &Entity<InputState> {
        &self.rails.rename
    }

    // --- overlays --------------------------------------------------------------------------------

    /// "▦ Deal a proof sheet": the spread for the working state, over the view, with the keys.
    pub fn open_proof_sheet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let d = self.darkroom.read(cx);
        let (Some(source), Some(candidates)) = (d.variant_source(), d.proof_candidates()) else { return };
        let images = d.images().clone();
        let shell = d.shell().clone();
        let sheet = cx.new(|cx| ProofSheet::new(&images, shell, source, candidates, window, cx));
        let sub = cx.subscribe_in(&sheet, window, |this, _, e: &ProofEvent, window, cx| {
            if let ProofEvent::Adopt(c) = e {
                let c = c.clone();
                this.darkroom.update(cx, |d, cx| d.adopt_proof(c, cx));
            }
            this.close_overlay(window, cx);
        });
        let focus = sheet.read(cx).focus_handle().clone();
        self.rails.overlay = Some(Overlay::Proof(sheet));
        self.rails.overlay_subs = vec![sub];
        window.focus(&focus, cx);
        cx.notify();
    }

    /// "⚖ Refine by duel": picks stream into the working state; ⑂ banks a variant.
    pub fn open_duel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let d = self.darkroom.read(cx);
        let (Some(source), Some(working)) = (d.variant_source(), d.open.as_ref().map(|o| o.working.clone())) else { return };
        let kelvin = d.kelvin();
        let images = d.images().clone();
        let duel = cx.new(|cx| DuelView::new(&images, source, working, kelvin, cx));
        let sub = cx.subscribe_in(&duel, window, |this, _, e: &DuelEvent, window, cx| match e {
            DuelEvent::Apply(r) => {
                let r = r.clone();
                this.darkroom.update(cx, |d, cx| d.apply(r, Some("Duel pick"), cx));
            }
            DuelEvent::Fork(r, dim) => {
                let (r, dim) = (r.clone(), *dim);
                this.darkroom.update(cx, |d, cx| d.fork_variant(r, dim, cx));
            }
            DuelEvent::Close => this.close_overlay(window, cx),
        });
        let focus = duel.read(cx).focus_handle().clone();
        self.rails.overlay = Some(Overlay::Duel(duel));
        self.rails.overlay_subs = vec![sub];
        window.focus(&focus, cx);
        cx.notify();
    }

    fn close_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.rails.overlay.take() {
            Some(Overlay::Proof(p)) => p.update(cx, |p, cx| p.close(cx)),
            Some(Overlay::Duel(d)) => d.update(cx, |d, cx| d.close(cx)),
            None => return,
        }
        self.rails.overlay_subs.clear();
        self.focus.focus(window, cx);
        cx.notify();
    }

    // --- the right rail ---------------------------------------------------------------------------

    /// `HistoryPanel`: the steps newest first; a click makes one current.
    pub(super) fn render_history(&self, open: &OpenPhoto, colors: Colors, cx: &Context<Self>) -> AnyElement {
        let mut panel = Self::group("History", colors)
            .child(div().text_size(px(10.)).text_color(colors.mute).child("Ctrl+Z · Ctrl+Shift+Z"));
        let steps = open.history.as_ref().map(|h| h.steps.as_slice()).unwrap_or_default();
        let head = open.history.as_ref().and_then(|h| h.head);
        if steps.is_empty() {
            return panel
                .child(div().text_size(px(11.)).text_color(colors.mute).child("Changes are saved as you go and listed here."))
                .into_any_element();
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_millis() as f64);
        let mut list = div().id("dk-history").flex().flex_col().max_h(px(180.)).overflow_y_scroll();
        for st in steps.iter().rev() {
            let state = step_state(st.seq, head);
            let when = when_label(st.created_at as f64, now).text(|s| crate::inspector::publication_date(s as i64));
            let seq = st.seq;
            let (fg, bg) = match state {
                StepState::Current => (colors.accent, colors.well),
                StepState::Undone => (colors.mute, colors.canvas),
                StepState::Done => (colors.txt, colors.canvas),
            };
            let tip = if state == StepState::Undone { "Undone — click to redo up to here" } else { "Go back to this step" };
            let dk = self.darkroom.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("dk-step-{seq}")))
                    .flex()
                    .justify_between()
                    .px(px(6.))
                    .py(px(2.))
                    .rounded(px(4.))
                    .bg(bg)
                    .text_size(px(11.))
                    .cursor_pointer()
                    .child(div().text_color(fg).when(state == StepState::Undone, |d| d.italic()).child(st.label.clone()))
                    .child(div().text_color(colors.mute).child(when))
                    .tooltip(crate::shell::title_bar::tooltip(tip))
                    .on_click(move |_, _, cx| dk.update(cx, |d, cx| d.goto_step(seq, cx)))
                    .test_support(),
            );
        }
        let _ = cx;
        panel = panel.child(list);
        panel.into_any_element()
    }

    /// `PresetBrowser`: by category, each card this photo with that preset; user presets can
    /// be renamed (✎) or deleted (×).
    pub(super) fn render_presets(&self, d: &Darkroom, colors: Colors, cx: &Context<Self>) -> AnyElement {
        let open_ = self.rails.presets_open;
        let mut section = div().flex().flex_col().gap(px(4.)).pb(px(10.)).child(clickable(
            chip("dk-presets-toggle", if open_ { "Presets ▾" } else { "Presets ▸" }, true, colors),
            true,
            cx.listener(move |this, _, _, cx| this.set_presets_open(!open_, cx)),
        ));
        if !open_ {
            return section.into_any_element();
        }
        let Some(open) = d.open.as_ref() else { return section.into_any_element() };
        let source = d.variant_source();
        let renders = self.rails.preset_renders.as_ref().map(|r| r.read(cx));
        let presets = d.presets();
        for cat in PRESET_CATEGORIES {
            let group: Vec<_> = presets.iter().filter(|p| p.category == cat).collect();
            if group.is_empty() {
                continue;
            }
            let name = match cat {
                PresetCategory::Monochrome => "Monochrome",
                PresetCategory::Film => "Film",
                PresetCategory::Color => "Color",
                PresetCategory::User => "User",
            };
            let mut grid = div().flex().flex_wrap().gap(px(6.));
            for p in group {
                let active = preset_is_active(p, &open.working);
                let state = match (&source, renders) {
                    (Some(s), Some(r)) => r.get(&s.job(&p.edit.value_or_default(), PROOF_EDGE)),
                    _ => crate::loupe::edit_renders::RenderState::Rendering,
                };
                let preset = p.clone();
                let mut label = div().flex().justify_between().items_center().px(px(4.)).py(px(2.)).text_size(px(10.5)).child(p.name.clone());
                if p.builtin != Some(true) {
                    let (id1, id2, name) = (p.id.clone(), p.id.clone(), p.name.clone());
                    label = label.child(
                        div()
                            .flex()
                            .gap(px(4.))
                            .child(
                                div()
                                    .id(SharedString::from(format!("dk-preset-rename-{}", p.id)))
                                    .cursor_pointer()
                                    .text_color(colors.dim)
                                    .child("✎")
                                    .tooltip(crate::shell::title_bar::tooltip("Rename preset"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.start_rename(id1.clone(), &name, window, cx);
                                    }))
                                    .test_support(),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!("dk-preset-delete-{}", p.id)))
                                    .cursor_pointer()
                                    .text_color(colors.dim)
                                    .child("×")
                                    .tooltip(crate::shell::title_bar::tooltip("Delete preset"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        let id = id2.clone();
                                        this.darkroom.update(cx, |d, cx| d.delete_preset(id, cx));
                                    }))
                                    .test_support(),
                            ),
                    );
                }
                let tip: SharedString = format!("Apply {}", p.name).into();
                grid = grid.child(
                    div()
                        .id(SharedString::from(format!("dk-preset-{}", p.id)))
                        .flex()
                        .flex_col()
                        .w(px(PRESET_CARD))
                        .rounded(px(5.))
                        .overflow_hidden()
                        .border_2()
                        .border_color(if active { colors.accent } else { colors.border })
                        .cursor_pointer()
                        .child(div().h(px(PRESET_CARD * 0.66)).bg(colors.well).child(variant_image(
                            SharedString::from(format!("dk-preset-image-{}", p.id)),
                            state,
                            ObjectFit::Cover,
                            "…",
                            colors,
                        )))
                        .child(label)
                        .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let p = preset.clone();
                            this.darkroom.update(cx, |d, cx| d.apply_preset(&p, cx));
                        }))
                        .test_support(),
                );
            }
            section = section.child(div().text_size(px(10.)).text_color(colors.mute).child(name)).child(grid);
        }
        if self.rails.renaming.is_some() {
            let can = !self.rails.rename.read(cx).value().trim().is_empty();
            section = section.child(
                div()
                    .id("dk-preset-renaming")
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .p(px(6.))
                    .rounded(px(6.))
                    .bg(colors.panel)
                    .on_key_down(cx.listener(|this, e: &KeyDownEvent, window, cx| {
                        if e.keystroke.key == "escape" {
                            this.cancel_rename(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(div().text_size(px(11.)).child("Rename preset"))
                    .child(Input::new(&self.rails.rename).id("dk-preset-rename-name").small())
                    .child(
                        div()
                            .flex()
                            .gap(px(6.))
                            .child(clickable(chip("dk-rename-cancel", "Cancel", true, colors), true, cx.listener(|this, _, window, cx| this.cancel_rename(window, cx))))
                            .child(clickable(chip("dk-rename-ok", "Rename", can, colors), can, cx.listener(|this, _, window, cx| this.confirm_rename(window, cx)))),
                    )
                    .test_support(),
            );
        }
        section.into_any_element()
    }

    pub fn start_rename(&mut self, id: String, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.rails.renaming = Some(id);
        let name = name.to_string();
        self.rails.rename.update(cx, |i, cx| i.set_value(name, window, cx));
        let focus = self.rails.rename.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    fn cancel_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.rails.renaming = None;
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// Enter or Rename: a non-empty name renames the preset.
    pub fn confirm_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.rails.rename.read(cx).value().trim().to_string();
        let Some(id) = self.rails.renaming.clone() else { return };
        if name.is_empty() {
            return;
        }
        self.darkroom.update(cx, |d, cx| d.rename_preset(id, name, cx));
        self.cancel_rename(window, cx);
    }

    /// `LensRail`: on the RAW, when the file carries a table this build applies.
    pub(super) fn render_lens(&self, open: &OpenPhoto, colors: Colors, cx: &Context<Self>) -> Option<AnyElement> {
        open.source_token()?;
        let info = open.source.lens.as_ref().filter(|l| l.vignetting || l.distortion || l.chromatic)?;
        let on = geo::lens_on(&open.working);
        let el = chip("dk-lens", if on { "Correction on" } else { "Correction off" }, true, colors)
            .when(on, |c| c.border_color(colors.accent).text_color(colors.accent))
            .tooltip(crate::shell::title_bar::tooltip("Apply the correction the camera recorded for this lens, focal length and aperture"));
        Some(
            Self::group("Lens", colors)
                .child(clickable(el, true, cx.listener(move |this, _, _, cx| this.edit(cx, move |w, _| geo::set_lens(w, !on)))))
                .child(div().text_size(px(10.5)).text_color(colors.mute).child(lens_hint(info)))
                .into_any_element(),
        )
    }

    /// `GeometryRail`: aspects, overlays, output size, perspective, straighten.
    pub(super) fn render_geometry(&self, d: &Darkroom, colors: Colors, cx: &Context<Self>) -> AnyElement {
        let mut g = Self::group("Crop & Rotate", colors);
        let Some(open) = d.open.as_ref() else { return g.into_any_element() };
        let w = &open.working;
        let on = |el: gpui_kit::Stateful<gpui_kit::Div>, on: bool| el.when(on, |c| c.border_color(colors.accent).text_color(colors.accent));
        let hint = |t: &str| div().text_size(px(10.5)).text_color(colors.mute).child(t.to_string());
        let sub = |t: &str| div().pt(px(8.)).text_size(px(11.)).text_color(colors.dim).child(t.to_string());
        let aspect = geo::aspect_label(w);
        let mut aspects = div().flex().flex_wrap().gap(px(4.));
        for a in ASPECTS {
            let label = a.label;
            let mut el = on(chip(format!("dk-aspect-{label}"), label, true, colors), aspect == label);
            if let Some(h) = a.hint {
                el = el.tooltip(crate::shell::title_bar::tooltip(h));
            }
            aspects = aspects.child(clickable(el, true, cx.listener(move |this, _, _, cx| this.set_aspect(label, cx))));
        }
        g = g.child(aspects).child(sub("Overlay"));
        let mut overlays = div().flex().flex_wrap().gap(px(4.));
        for (o, label) in OVERLAYS {
            let dk = self.darkroom.clone();
            overlays = overlays.child(clickable(
                on(chip(format!("dk-overlay-{}", o.key()), label, true, colors), d.overlay == o),
                true,
                move |_, _, cx| dk.update(cx, |d, cx| d.set_overlay(o, cx)),
            ));
        }
        g = g.child(overlays);
        if let Some((pw, ph)) = geo::crop_px(w, self.dims(cx).0) {
            g = g.child(div().id("dk-output").text_size(px(10.5)).text_color(colors.dim).child(format!("Output: {pw} × {ph} px")).test_support());
        }
        if geo::crop_of(w).is_some() {
            g = g.child(hint("Drag the box to move · drag a corner to resize."));
        }

        g = g.child(sub("Perspective"));
        let has_quad = w.perspective.is_truthy();
        let mode = open.perspective_mode;
        let label = if mode { "Done" } else if has_quad { "Adjust corners" } else { "Correct perspective" };
        let dk = self.darkroom.clone();
        let mut row = div().flex().gap(px(4.)).child(clickable(
            on(chip("dk-perspective", label, true, colors), mode)
                .tooltip(crate::shell::title_bar::tooltip("Drag the four handles onto the corners of the picture, then press Done")),
            true,
            move |_, _, cx| {
                dk.update(cx, |d, cx| if mode { d.set_perspective_mode(false, cx) } else { d.start_perspective(cx) })
            },
        ));
        if has_quad {
            let dk = self.darkroom.clone();
            row = row.child(clickable(
                chip("dk-perspective-reset", "Reset", true, colors).tooltip(crate::shell::title_bar::tooltip("Reset perspective")),
                true,
                move |_, _, cx| dk.update(cx, |d, cx| d.clear_perspective(cx)),
            ));
        }
        g = g.child(row).child(hint(if mode {
            "Put each handle on the matching corner of the picture, then press Done."
        } else if has_quad {
            "Corners set — the frame is squared up."
        } else {
            "Squares up a picture or document photographed off-axis."
        }));

        g = g.child(sub("Straighten"));
        let s = geo::straighten_of(w);
        let smode = self.rails.straighten_mode;
        let mut row = div().flex().gap(px(4.)).child(clickable(
            on(chip("dk-level-mode", if smode { "Drawing… drag on image" } else { "Draw level line" }, true, colors), smode)
                .tooltip(crate::shell::title_bar::tooltip("Draw a line along something that should be level (a horizon or a vertical edge)")),
            true,
            cx.listener(move |this, _, _, cx| {
                this.rails.straighten_mode = !smode;
                cx.notify();
            }),
        ));
        if s != 0.0 {
            row = row.child(clickable(
                chip("dk-straighten-reset", "Reset", true, colors).tooltip(crate::shell::title_bar::tooltip("Reset straighten")),
                true,
                cx.listener(|this, _, _, cx| this.set_straighten(0.0, cx)),
            ));
        }
        g = g
            .child(row)
            .child(
                div()
                    .id("dk-straighten")
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .py(px(3.))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, e: &MouseDownEvent, _, cx| {
                            if e.click_count >= 2 {
                                this.set_straighten(0.0, cx);
                            }
                        }),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .text_size(px(11.))
                            .child(div().text_color(colors.dim).child("Angle"))
                            .child(div().text_color(colors.txt).child(format!("{}°", chairphoto_model::js_compat::to_fixed(s, 1)))),
                    )
                    .child(Slider::new(&self.rails.straighten))
                    .test_support(),
            )
            .child(hint(if smode {
                "Drag a line along the horizon (or a vertical edge) — the image levels to it."
            } else {
                "Draw a level line or nudge the angle; the crop auto-insets to hide the corners."
            }));
        g.into_any_element()
    }

    /// An aspect chip (a ratio waits until the frame's size is known).
    pub fn set_aspect(&mut self, label: &'static str, cx: &mut Context<Self>) {
        let src = self.src_dims(cx);
        self.darkroom.update(cx, |d, cx| {
            if let Some(next) = d.open.as_ref().and_then(|o| geo::apply_aspect(&o.working, label, src)) {
                d.apply(next, None, cx);
            }
        });
    }

    /// The angle slider (or a reset): straighten, the crop auto-inset.
    pub fn set_straighten(&mut self, deg: f64, cx: &mut Context<Self>) {
        let src = self.src_dims(cx);
        self.edit(cx, |w, _| geo::apply_straighten(w, deg, src));
    }

    /// A quad corner dragged to `(x, y)` (fractions) — what a handle drag does.
    pub fn drag_quad_to(&mut self, corner: QuadCorner, x: f64, y: f64, cx: &mut Context<Self>) {
        self.edit(cx, |w, _| geo::with_quad_corner(w, corner, x, y));
    }
}
