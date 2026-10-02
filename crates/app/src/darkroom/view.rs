//! [`DarkroomView`]: the Darkroom on the stage — DarkroomView.tsx's bar, stage, tone strip,
//! actions and filmstrip, and EditControls.tsx's EditStage (fit, wheel zoom toward the cursor,
//! drag to pan, "Fit NN%", Esc), ToneRail (white balance relative or Kelvin, tone, colour) and
//! EffectsRail (Color/B&W, fade, vignette, grain, split toning, LUT). It draws what the
//! [`Darkroom`] holds and turns input into [`Control`] changes; it owns only presentation
//! state (zoom, drags, the split-toning fold).
//!
//! Keys (the view's focus, not while a text field has it): ← / → step the filmstrip (no
//! wrap, ignored with modifiers), Esc fits the stage, Enter zooms to the crop, Ctrl+S saves
//! now (even in a field), Ctrl+Z undoes, Ctrl+Shift+Z / Ctrl+Y redo.
//!
//! The rails of #112 (`rails`): the version shelf and cover on the bar; History, Presets,
//! Lens and Crop & Rotate on the right rail; the crop box, perspective quad and level line
//! over the frame; the proof sheet and duel (`crate::loupe`) over the whole view.

use super::session::Darkroom;
use super::stage::FrameTier;
use crate::image_store::ImageState;
use crate::shell::style::Colors;
use crate::storage::ui::{chip, clickable};
use chairphoto_core::image_pool::ImageKind;
use chairphoto_model::darkroom::controls::{
    self as ctl, EffectKey, SliderDef, SplitKey, ToneKey, COLOR_SLIDERS, EFFECT_SLIDERS, SPLIT_SLIDERS, TONE_SLIDERS,
};
use chairphoto_model::darkroom::develop_source::{badge_for, BadgeTone};
use chairphoto_model::darkroom::filmstrip::{step_target, KeyTarget};
use chairphoto_model::darkroom::kelvin::{kelvin_to_slider, KelvinContext, WbShown, KELVIN_TINT_RANGE, SLIDER_STEPS};
use chairphoto_model::darkroom::tone_strip::{self, ZONE_COUNT, ZONE_FILLS, ZONE_LABELS};
use chairphoto_model::editing::{bw_filters, VersionEdit};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::prelude::*;
use gpui_kit::{
    canvas, div, img, px, rgb, AnyElement, Bounds, Context, Entity, FocusHandle, FontWeight, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, ObjectFit, PathPromptOptions, Pixels, ScrollDelta, ScrollWheelEvent, SharedString,
    Subscription, TestSupportExt as _, Window,
};
use std::cell::Cell;
use std::rc::Rc;

mod rails;
pub use rails::Overlay;

// --- controls ------------------------------------------------------------------------------

/// A slider on the rails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Tone(ToneKey),
    /// Relative white balance.
    WbTemp,
    WbTint,
    /// Kelvin white balance: the log slider position (0..SLIDER_STEPS) and the tint.
    KelvinTemp,
    KelvinTint,
    Effect(EffectKey),
    Split(SplitKey),
    LutAmount,
}

impl Control {
    /// Every slider, in rail order.
    pub fn all() -> Vec<Control> {
        let mut v = vec![Control::WbTemp, Control::WbTint, Control::KelvinTemp, Control::KelvinTint];
        v.extend(TONE_SLIDERS.iter().chain(&COLOR_SLIDERS).map(|d| Control::Tone(d.key)));
        v.extend(EFFECT_SLIDERS.iter().map(|d| Control::Effect(d.key)));
        v.extend(SPLIT_SLIDERS.iter().map(|d| Control::Split(d.key)));
        v.push(Control::LutAmount);
        v
    }

    /// `(label, min, max, step)`.
    pub fn def(self) -> (&'static str, f64, f64, f64) {
        fn of<K: PartialEq + Copy>(defs: &[SliderDef<K>], k: K) -> (&'static str, f64, f64, f64) {
            let d = defs.iter().find(|d| d.key == k).expect("a defined slider");
            (d.label, d.min, d.max, d.step)
        }
        match self {
            Control::Tone(k) => {
                let all: Vec<SliderDef<ToneKey>> = TONE_SLIDERS.iter().chain(&COLOR_SLIDERS).copied().collect();
                of(&all, k)
            }
            Control::WbTemp => ("Temperature", -1.0, 1.0, 0.05),
            Control::WbTint => ("Tint", -1.0, 1.0, 0.05),
            Control::KelvinTemp => ("Temperature", 0.0, SLIDER_STEPS, 1.0),
            Control::KelvinTint => ("Tint", -KELVIN_TINT_RANGE, KELVIN_TINT_RANGE, 1.0),
            Control::Effect(k) => of(&EFFECT_SLIDERS, k),
            Control::Split(k) => of(&SPLIT_SLIDERS, k),
            Control::LutAmount => ("LUT amount", 0.0, 1.0, 0.05),
        }
    }

    /// The element id of the slider row.
    pub fn id(self) -> SharedString {
        match self {
            Control::Tone(k) => format!("dk-{}", k.key()),
            Control::WbTemp => "dk-wb-temp".into(),
            Control::WbTint => "dk-wb-tint".into(),
            Control::KelvinTemp => "dk-kelvin".into(),
            Control::KelvinTint => "dk-kelvin-tint".into(),
            Control::Effect(k) => format!("dk-{}", k.key()),
            Control::Split(k) => format!("dk-split-{}", k.key()),
            Control::LutAmount => "dk-lut-amount".into(),
        }
        .into()
    }

    /// The slider's value for `working`.
    pub fn value(self, working: &VersionEdit, kelvin: Option<&KelvinContext>) -> f64 {
        let wb = ctl::wb_of(&ctl::tone_of(working));
        match self {
            Control::Tone(k) => ctl::tone_value(&ctl::tone_of(working), k),
            Control::WbTemp => wb.temp.num_or(0.0),
            Control::WbTint => wb.tint.num_or(0.0),
            Control::KelvinTemp | Control::KelvinTint => match ctl::wb_shown_for(working, kelvin) {
                WbShown::Kelvin { kelvin, tint } => {
                    if self == Control::KelvinTemp {
                        kelvin_to_slider(kelvin)
                    } else {
                        tint
                    }
                }
                WbShown::Relative => 0.0,
            },
            Control::Effect(k) => ctl::effect_value(&ctl::look_of(working), k),
            Control::Split(k) => ctl::split_value(&ctl::look_of(working), k),
            Control::LutAmount => ctl::look_of(working).lut.value().map_or(1.0, |l| l.amount),
        }
    }

    /// The record after the slider moved to `v`.
    pub fn apply(self, working: &VersionEdit, kelvin: Option<&KelvinContext>, v: f64) -> VersionEdit {
        match self {
            Control::Tone(k) => ctl::set_tone_key(working, k, v),
            Control::WbTemp => ctl::set_wb_relative(working, false, v),
            Control::WbTint => ctl::set_wb_relative(working, true, v),
            Control::KelvinTemp => ctl::set_kelvin_slider(working, kelvin, v),
            Control::KelvinTint => ctl::set_kelvin_tint(working, kelvin, v),
            Control::Effect(k) => ctl::set_effect(working, k, v),
            Control::Split(k) => ctl::set_split(working, k, v),
            Control::LutAmount => ctl::set_lut_amount(working, v),
        }
    }

    /// A double-click: the slider's reset, or `None` (the hue sliders keep their value).
    pub fn reset(self, working: &VersionEdit, kelvin: Option<&KelvinContext>) -> Option<VersionEdit> {
        Some(match self {
            Control::KelvinTemp | Control::KelvinTint => ctl::wb_as_shot(working),
            Control::Effect(k) => ctl::set_effect(working, k, k.reset_value()),
            Control::Split(k) if k.is_hue() => return None,
            Control::LutAmount => ctl::set_lut_amount(working, 1.0),
            other => other.apply(working, kelvin, 0.0),
        })
    }

    /// The value text beside the label (`toFixed(2)`, Kelvin in K, hues in degrees, a
    /// signed whole tint).
    pub fn label_value(self, v: f64, working: &VersionEdit, kelvin: Option<&KelvinContext>) -> String {
        use chairphoto_model::js_compat::{round, to_fixed};
        match self {
            Control::KelvinTemp => match ctl::wb_shown_for(working, kelvin) {
                WbShown::Kelvin { kelvin, .. } => format!("{} K", round(kelvin)),
                WbShown::Relative => String::new(),
            },
            Control::KelvinTint => format!("{}{}", if v >= 0.0 { "+" } else { "−" }, to_fixed(v.abs(), 0)),
            Control::Split(k) if k.is_hue() => format!("{}°", round(v)),
            _ => to_fixed(v, 2),
        }
    }
}

/// A slider position as the HTML range input reported it: snapped to the step from the
/// minimum, clamped, and spelled with the step's decimals (so 3 × 0.05 is 0.15, not
/// 0.15000000000000002 — the record is written as the user would read it).
pub fn snap(v: f64, min: f64, max: f64, step: f64) -> f64 {
    let decimals = format!("{step}").split_once('.').map_or(0, |(_, f)| f.len()) as i32;
    let k = 10f64.powi(decimals);
    let snapped = min + ((v - min) / step).round() * step;
    (snapped.clamp(min, max) * k).round() / k
}

// --- the stage view ------------------------------------------------------------------------

/// The stage's zoom and pan (EditStage's `view`): `scale` 1–8 over the fitted frame, `tx`/`ty`
/// its offset in pixels from centred.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StageView {
    pub scale: f64,
    pub tx: f64,
    pub ty: f64,
}

impl StageView {
    pub const FIT: StageView = StageView { scale: 1.0, tx: 0.0, ty: 0.0 };
    pub const MAX_SCALE: f64 = 8.0;

    /// One wheel notch toward `(cx, cy)` (pixels from the stage's centre): ×1.15 in, ÷1.15
    /// out, 1–8×, the point under the cursor staying put; back at 1× it is the fit.
    pub fn zoomed(self, zoom_in: bool, cx: f64, cy: f64) -> StageView {
        let next = (self.scale * if zoom_in { 1.15 } else { 1.0 / 1.15 }).clamp(1.0, Self::MAX_SCALE);
        if next <= 1.001 {
            return StageView::FIT;
        }
        StageView {
            scale: next,
            tx: cx - (next / self.scale) * (cx - self.tx),
            ty: cy - (next / self.scale) * (cy - self.ty),
        }
    }

    /// The picture's rectangle `(x, y, w, h)` in a `sw × sh` stage: fitted (contain),
    /// scaled, centred, offset.
    pub fn image_rect(self, sw: f64, sh: f64, iw: f64, ih: f64) -> (f64, f64, f64, f64) {
        if iw <= 0.0 || ih <= 0.0 || sw <= 0.0 || sh <= 0.0 {
            return (0.0, 0.0, sw.max(0.0), sh.max(0.0));
        }
        let fit = (sw / iw).min(sh / ih);
        let (w, h) = (iw * fit * self.scale, ih * fit * self.scale);
        ((sw - w) / 2.0 + self.tx, (sh - h) / 2.0 + self.ty, w, h)
    }

    pub fn is_fit(self) -> bool {
        self.scale <= 1.001
    }

    /// "Fit NN%".
    pub fn fit_label(self) -> String {
        format!("Fit {}%", chairphoto_model::js_compat::round(self.scale * 100.0))
    }
}

/// A drag in progress on the view.
#[derive(Clone, Debug)]
enum Drag {
    /// Panning the zoomed stage: where the pointer started and the offset then.
    Pan { x: f64, y: f64, tx: f64, ty: f64 },
    /// A tone-strip zone: which, where the pointer started, the zones then.
    Zone { zone: usize, y: f64, zones: Option<Vec<f64>> },
    /// The crop body: where the pointer started and the crop then.
    CropMove { x: f64, y: f64, start: chairphoto_model::editing::Crop },
    /// A crop corner, the opposite one fixed.
    CropCorner { anchor: (f64, f64) },
    /// A perspective handle.
    Quad(chairphoto_model::editing::QuadCorner),
    /// The level line, in fit-frame pixels.
    Level { x0: f64, y0: f64, x1: f64, y1: f64 },
}

/// The Darkroom view. See the module docs.
pub struct DarkroomView {
    darkroom: Entity<Darkroom>,
    focus: FocusHandle,
    sliders: Vec<(Control, Entity<SliderState>)>,
    view: StageView,
    drag: Option<Drag>,
    show_split: bool,
    stage_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// The open photo the zoom belongs to: another photo starts at fit.
    view_photo: Option<i64>,
    rails: rails::RailsState,
    _subscriptions: Vec<Subscription>,
}

impl DarkroomView {
    pub fn new(darkroom: Entity<Darkroom>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut subs = vec![cx.observe_in(&darkroom, window, |this, darkroom, window, cx| {
            let photo = darkroom.read(cx).open.as_ref().map(|o| o.photo.id);
            if photo != this.view_photo {
                this.view_photo = photo;
                this.view = StageView::FIT;
                this.photo_changed(window, cx);
            }
            this.sync_preset_renders(cx);
            cx.notify()
        })];
        subs.push(cx.subscribe(&darkroom, |this, _, event: &super::session::DarkroomEvent, cx| this.on_darkroom_event(event, cx)));
        let mut sliders = Vec::new();
        for control in Control::all() {
            let (_, min, max, step) = control.def();
            let state = cx.new(|_| SliderState::new().min(min as f32).max(max as f32).step(step as f32).default_value(0.0));
            subs.push(cx.subscribe_in(&state, window, move |this, _, event: &SliderEvent, _, cx| {
                if let SliderEvent::Change(v) = event {
                    this.set_control(control, v.start() as f64, cx);
                }
            }));
            sliders.push((control, state));
        }
        let (rails, rail_subs) = rails::RailsState::new(window, cx);
        subs.extend(rail_subs);
        DarkroomView {
            darkroom,
            focus: cx.focus_handle(),
            sliders,
            view: StageView::FIT,
            drag: None,
            show_split: false,
            stage_bounds: Rc::default(),
            view_photo: None,
            rails,
            _subscriptions: subs,
        }
    }

    pub fn darkroom(&self) -> &Entity<Darkroom> {
        &self.darkroom
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    pub fn slider(&self, control: Control) -> &Entity<SliderState> {
        &self.sliders.iter().find(|(c, _)| *c == control).expect("every control has a slider").1
    }

    pub fn stage_view(&self) -> StageView {
        self.view
    }

    /// A slider moved to `v`: snapped as the range input would, applied to the record.
    pub fn set_control(&mut self, control: Control, v: f64, cx: &mut Context<Self>) {
        let (_, min, max, step) = control.def();
        let v = snap(v, min, max, step);
        self.darkroom.update(cx, |d, cx| {
            let kelvin = d.kelvin();
            if let Some(open) = &d.open {
                let next = control.apply(&open.working, kelvin.as_ref(), v);
                d.apply(next, None, cx);
            }
        });
    }

    /// A double-click on a slider.
    pub fn reset_control(&mut self, control: Control, cx: &mut Context<Self>) {
        self.darkroom.update(cx, |d, cx| {
            let kelvin = d.kelvin();
            if let Some(next) = d.open.as_ref().and_then(|o| control.reset(&o.working, kelvin.as_ref())) {
                d.apply(next, None, cx);
            }
        });
    }

    /// Apply a record transition to the working record (chips, the strip).
    fn edit(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&VersionEdit, Option<&KelvinContext>) -> VersionEdit) {
        self.darkroom.update(cx, |d, cx| {
            let kelvin = d.kelvin();
            if let Some(next) = d.open.as_ref().map(|o| f(&o.working, kelvin.as_ref())) {
                d.apply(next, None, cx);
            }
        });
    }

    /// ← / →: the filmstrip's neighbour (no wrap). `target` is what has the keys: a slider
    /// or field keeps its arrows.
    pub fn step(&mut self, delta: isize, target: Option<KeyTarget>, cx: &mut Context<Self>) -> bool {
        if chairphoto_model::darkroom::filmstrip::arrows_belong_to_target(target) {
            return false;
        }
        let darkroom = self.darkroom.read(cx);
        let Some(current) = darkroom.open.as_ref().map(|o| o.photo.id) else { return false };
        let ids = darkroom.shell().read(cx).library.photo_ids();
        let Some(next) = step_target(&ids, current, delta) else { return false };
        self.darkroom.update(cx, |d, cx| d.step_to(next, cx));
        true
    }

    fn on_key(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &e.keystroke;
        let m = k.modifiers;
        if (m.control || m.platform) && k.key == "s" {
            self.darkroom.update(cx, |d, cx| d.flush(cx));
            cx.stop_propagation();
            return;
        }
        // The proof sheet and the duel own the keys while they are up (React's filmstrip
        // `keysDisabled`): their arrows and Esc must not also step or fit the stage.
        if self.rails.overlay_open() {
            return;
        }
        // A text field keeps its own undo, and its own Enter and arrows.
        let typing = self.rails.typing(window, cx);
        if (m.control || m.platform) && !m.alt && !typing {
            match k.key.as_str() {
                "z" if m.shift => self.darkroom.update(cx, |d, cx| d.redo(cx)),
                "z" => self.darkroom.update(cx, |d, cx| d.undo(cx)),
                "y" => self.darkroom.update(cx, |d, cx| d.redo(cx)),
                _ => return,
            }
            cx.stop_propagation();
            return;
        }
        if m.control || m.platform || m.alt || typing {
            return;
        }
        match k.key.as_str() {
            "enter" => {
                self.zoom_to_crop(cx);
            }
            "left" | "right" => {
                if self.step(if k.key == "right" { 1 } else { -1 }, None, cx) {
                    cx.stop_propagation();
                }
            }
            "escape" => {
                self.view = StageView::FIT;
                cx.notify();
            }
            _ => {}
        }
    }

    fn on_wheel(&mut self, e: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let dy = match e.delta {
            ScrollDelta::Lines(l) => l.y as f64,
            ScrollDelta::Pixels(p) => f64::from(p.y),
        };
        if dy == 0.0 {
            return;
        }
        let Some(b) = self.stage_bounds.get() else { return };
        let cx_ = f64::from(e.position.x - b.origin.x) - f64::from(b.size.width) / 2.0;
        let cy_ = f64::from(e.position.y - b.origin.y) - f64::from(b.size.height) / 2.0;
        // Wheel up (content scrolls down, delta > 0 here) zooms in, as React's deltaY < 0.
        self.view = self.view.zoomed(dy > 0.0, cx_, cy_);
        cx.notify();
    }

    fn on_mouse_move(&mut self, e: &MouseMoveEvent, cx: &mut Context<Self>) {
        if e.pressed_button != Some(MouseButton::Left) {
            self.drag = None;
            return;
        }
        let (x, y) = (f64::from(e.position.x), f64::from(e.position.y));
        match self.drag.clone() {
            Some(Drag::Pan { x: x0, y: y0, tx, ty }) => {
                self.view = StageView { tx: tx + (x - x0), ty: ty + (y - y0), ..self.view };
                cx.notify();
            }
            Some(Drag::Zone { zone, y: y0, zones }) => {
                let next = tone_strip::apply_zone_drag(zones.as_deref(), zone, tone_strip::drag_delta_ev(y0, y));
                self.edit(cx, |w, _| ctl::set_zones(w, next));
            }
            Some(other) => self.drag_frame(other, x, y, cx),
            None => {}
        }
    }

    fn pick_lut(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose a .cube LUT".into()),
        });
        let darkroom = self.darkroom.clone();
        cx.spawn(async move |_, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(path) = paths.into_iter().next() {
                    let _ = darkroom.update(cx, |d, cx| d.import_lut(path, cx));
                }
            }
        })
        .detach();
    }

    /// Keep each slider's thumb on the record's value (a reset, a chip, a new photo moved
    /// it); a slider being dragged already holds it.
    fn sync_sliders(&self, working: &VersionEdit, kelvin: Option<&KelvinContext>, window: &mut Window, cx: &mut Context<Self>) {
        for (control, state) in &self.sliders {
            let v = control.value(working, kelvin) as f32;
            if (state.read(cx).value().start() - v).abs() > 1e-6 {
                state.update(cx, |s, cx| s.set_value(v, window, cx));
            }
        }
    }
}

// --- render ----------------------------------------------------------------------------------

impl DarkroomView {
    fn render_bar(&self, d: &Darkroom, colors: Colors, cx: &Context<Self>) -> AnyElement {
        let open = d.open.as_ref();
        let stage = open.map(|o| o.stage.read(cx));
        let rendering = stage.is_some_and(|s| match s.frame() {
            Some(f) => f.generation < s.generation() || f.tier == FrameTier::Fast,
            None => s.failure().is_none(),
        });
        let hint = if open.is_some_and(|o| o.saving) {
            "saving…"
        } else if rendering {
            "rendering…"
        } else {
            ""
        };
        let shell = d.shell().clone();
        let mut bar = div()
            .id("dk-bar")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .h(px(40.))
            .px(px(12.))
            .border_b_1()
            .border_color(colors.line)
            .child(clickable(chip("dk-back", "← Library", true, colors), true, move |_, _, cx| {
                shell.update(cx, |s, cx| s.show_library(cx))
            }))
            .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).child("Darkroom"))
            .children(self.render_shelf(d, colors))
            .child(div().id("dk-hint").text_size(px(11.)).text_color(colors.mute).child(hint).test_support());
        if let Some(open) = open {
            if let Some(source) = &open.source.source {
                let (label, title, tone) = if open.engine1_version && open.source.token.is_some() {
                    (
                        "camera preview · this version's engine".to_string(),
                        "This version was developed on the camera preview. It keeps rendering exactly as it was saved; the RAW is ready for a new version.".to_string(),
                        BadgeTone::Warn,
                    )
                } else {
                    let b = badge_for(source);
                    (b.label, b.title, b.tone)
                };
                let color = match tone {
                    BadgeTone::Raw => colors.ok,
                    BadgeTone::Warn => colors.rating,
                    BadgeTone::Plain => colors.dim,
                };
                let title: SharedString = title.into();
                bar = bar.child(
                    div()
                        .id("dk-source")
                        .px(px(8.))
                        .py(px(2.))
                        .rounded_full()
                        .border_1()
                        .border_color(color)
                        .text_color(color)
                        .text_size(px(11.))
                        .child(label)
                        .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(title.clone()).build(window, cx))
                        .test_support(),
                );
                if open.engine1_version && open.source.token.is_some() {
                    let dk = self.darkroom.clone();
                    bar = bar.child(clickable(
                        chip("dk-new-engine", "Develop with the new engine", true, colors).tooltip(crate::shell::title_bar::tooltip(
                            "Start a new version on the RAW with this version's framing (tone and look start fresh — the engines read sliders differently)",
                        )),
                        true,
                        move |_, _, cx| dk.update(cx, |d, cx| d.develop_with_new_engine(cx)),
                    ));
                }
            }
            if open.source_token().is_some() {
                let on = open.show_clipping;
                let dk = self.darkroom.clone();
                let el = chip("dk-clipping", "◩ Clipping", true, colors)
                    .when(on, |c| c.border_color(colors.accent).text_color(colors.accent))
                    .tooltip(crate::shell::title_bar::tooltip(
                        "Mark where the sensor itself clipped — the only white no slider can bring back",
                    ));
                bar = bar.child(clickable(el, true, move |_, _, cx| dk.update(cx, |d, cx| d.set_clipping(!on, cx))));
            }
        }
        bar = bar.children(self.render_bar_actions(d, colors));
        bar.into_any_element()
    }

    fn render_stage(&self, d: &Darkroom, colors: Colors, cx: &Context<Self>) -> AnyElement {
        // The stage's size is known only at layout: when it changes (the first frame, a
        // window resize), lay the picture out again.
        let bounds = self.stage_bounds.clone();
        let this = cx.entity().downgrade();
        let measure = canvas(
            move |b, _, cx| {
                if bounds.replace(Some(b)) != Some(b) {
                    let this = this.clone();
                    cx.defer(move |cx| {
                        this.update(cx, |_, cx| cx.notify()).ok();
                    });
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        let mut stage = div()
            .id("dk-stage")
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .bg(colors.well)
            .child(measure)
            .on_scroll_wheel(cx.listener(|this, e: &ScrollWheelEvent, _, cx| this.on_wheel(e, cx)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, _, _| {
                    if !this.view.is_fit() {
                        let (x, y) = (f64::from(e.position.x), f64::from(e.position.y));
                        this.drag = Some(Drag::Pan { x, y, tx: this.view.tx, ty: this.view.ty });
                    }
                }),
            );
        let Some(open) = d.open.as_ref() else {
            return stage.into_any_element();
        };
        let s = open.stage.read(cx);
        let (sw, sh) = self.stage_bounds.get().map_or((0.0, 0.0), |b| (f64::from(b.size.width), f64::from(b.size.height)));
        match s.frame() {
            Some(frame) => {
                let size = frame.image.size(0);
                let (x, y, w, h) = self.view.image_rect(sw, sh, size.width.0 as f64, size.height.0 as f64);
                let place = |id: &'static str, image| {
                    div()
                        .id(id)
                        .absolute()
                        .left(px(x as f32))
                        .top(px(y as f32))
                        .w(px(w as f32))
                        .h(px(h as f32))
                        .child(img(image).size_full().object_fit(ObjectFit::Contain))
                        .test_support()
                };
                stage = stage.child(place("dk-frame", frame.image.clone()));
                if let Some(clip) = open.clip_stage.as_ref().and_then(|c| c.read(cx).frame().cloned()) {
                    stage = stage.child(place("dk-clip-layer", clip.image));
                }
                let shown = (size.width.0 as f64, size.height.0 as f64);
                stage = stage.children(self.frame_overlays(d, (x, y, w, h), shown, colors, cx));
            }
            None if s.failure().is_none() => {
                stage = stage.items_center().justify_center().flex().child(
                    div().text_size(px(12.)).text_color(colors.mute).child("Rendering…"),
                );
            }
            None => {}
        }
        if let Some(f) = s.failure() {
            stage = stage.child(
                div()
                    .id("dk-render-failed")
                    .absolute()
                    .top(px(8.))
                    .left(px(8.))
                    .px(px(8.))
                    .py(px(3.))
                    .rounded(px(6.))
                    .bg(colors.danger)
                    .text_color(colors.onaccent)
                    .text_size(px(11.))
                    .child(format!("Render failed — the stage shows an older frame ({})", f.message))
                    .test_support(),
            );
        }
        if !self.view.is_fit() {
            stage = stage.child(
                div()
                    .absolute()
                    .bottom(px(8.))
                    .right(px(8.))
                    .child(clickable(chip("dk-fit", self.view.fit_label(), true, colors).bg(colors.panel), true, {
                        let this = cx.entity().downgrade();
                        move |_, _, cx| {
                            this.update(cx, |v, cx| {
                                v.view = StageView::FIT;
                                cx.notify();
                            })
                            .ok();
                        }
                    })),
            );
        }
        stage.into_any_element()
    }

    fn render_tone_strip(&self, d: &Darkroom, colors: Colors, cx: &Context<Self>) -> AnyElement {
        let Some(open) = d.open.as_ref() else { return div().into_any_element() };
        let heights = tone_strip::fill_heights(&open.masses);
        let zones = ctl::zones_of(&open.working).map(<[f64]>::to_vec);
        let mut strip = div().id("dk-tone-strip").flex().flex_row().gap(px(2.)).h(px(64.)).px(px(12.)).pt(px(6.));
        for i in 0..ZONE_COUNT {
            let dz = zones.as_ref().filter(|z| z.len() == ZONE_COUNT).map_or(0.0, |z| z[i]);
            let zones_now = zones.clone();
            strip = strip.child(
                div()
                    .id(SharedString::from(format!("dk-zone-{i}")))
                    .relative()
                    .flex_1()
                    .h_full()
                    .flex()
                    .flex_col()
                    .justify_end()
                    .rounded(px(3.))
                    .bg(colors.well)
                    .cursor_ns_resize()
                    .tooltip(crate::shell::title_bar::tooltip(ZONE_TOOLTIPS[i]))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                            if e.click_count >= 2 {
                                let z = zones_now.clone();
                                this.edit(cx, |w, _| ctl::set_zones(w, tone_strip::reset_zone(z.as_deref(), i)));
                                this.drag = None;
                            } else {
                                this.drag = Some(Drag::Zone { zone: i, y: f64::from(e.position.y), zones: zones_now.clone() });
                            }
                            cx.stop_propagation();
                        }),
                    )
                    .children(tone_strip::delta_label(dz).map(|l| {
                        div().absolute().top(px(2.)).w_full().flex().justify_center().text_size(px(10.)).text_color(colors.txt).child(l)
                    }))
                    .child(div().w_full().h(gpui_kit::relative(heights[i] as f32)).rounded(px(3.)).bg(rgb(ZONE_FILLS[i])))
                    .test_support(),
            );
        }
        let labels = div()
            .flex()
            .flex_row()
            .gap(px(2.))
            .px(px(12.))
            .children(ZONE_LABELS.iter().map(|l| div().flex_1().text_size(px(9.)).text_color(colors.mute).child(*l)));
        div().flex().flex_col().flex_none().child(strip).child(labels).into_any_element()
    }

    fn render_actions(&self, d: &Darkroom, colors: Colors, cx: &Context<Self>) -> AnyElement {
        let dk = self.darkroom.clone();
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .px(px(12.))
            .py(px(6.))
            .children(self.rails_actions(d, colors, cx))
            .child(clickable(
                chip("dk-reset", "Reset", true, colors)
                    .tooltip(crate::shell::title_bar::tooltip("Back to as shot — clears every adjustment, framing included")),
                true,
                move |_, _, cx| dk.update(cx, |d, cx| d.reset(cx)),
            ))
            .into_any_element()
    }

    fn render_filmstrip(&self, d: &Darkroom, colors: Colors, cx: &Context<Self>) -> Option<AnyElement> {
        let (start, shown, total) = d.strip(cx);
        if total <= 1 {
            return None;
        }
        let current = d.open.as_ref().map(|o| o.photo.id);
        let images: Vec<ImageState> = shown.iter().map(|p| d.images().read(cx).peek(p.id, ImageKind::Thumb)).collect();
        let mut strip = div().id("dk-filmstrip").flex().flex_row().flex_none().gap(px(4.)).h(px(64.)).px(px(12.)).py(px(4.)).overflow_x_scroll();
        for (k, (p, image)) in shown.iter().zip(images).enumerate() {
            let id = p.id;
            let name = p.path.rsplit('/').next().unwrap_or(&p.path).to_string();
            let title: SharedString = format!("{name} ({} of {total})", start + k + 1).into();
            let is_current = Some(id) == current;
            let thumb = match image {
                ImageState::Ready(loaded) => img(loaded.image).size_full().object_fit(ObjectFit::Cover).into_any_element(),
                _ => div().size_full().bg(colors.well).into_any_element(),
            };
            let dk = self.darkroom.clone();
            strip = strip.child(
                div()
                    .id(SharedString::from(format!("dk-strip-{id}")))
                    .flex_none()
                    .w(px(72.))
                    .h_full()
                    .rounded(px(4.))
                    .overflow_hidden()
                    .border_2()
                    .border_color(if is_current { colors.accent } else { colors.canvas })
                    .cursor_pointer()
                    .child(thumb)
                    .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(title.clone()).build(window, cx))
                    .on_click(move |_, _, cx| {
                        if !is_current {
                            dk.update(cx, |d, cx| d.step_to(id, cx));
                        }
                    })
                    .test_support(),
            );
        }
        Some(strip.into_any_element())
    }

    fn slider_row(
        &self,
        control: Control,
        working: &VersionEdit,
        kelvin: Option<&KelvinContext>,
        colors: Colors,
        cx: &Context<Self>,
    ) -> AnyElement {
        let (label, ..) = control.def();
        let v = control.value(working, kelvin);
        div()
            .id(control.id())
            .flex()
            .flex_col()
            .gap(px(2.))
            .py(px(3.))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                    if e.click_count >= 2 {
                        this.reset_control(control, cx);
                    }
                }),
            )
            .child(
                div()
                    .flex()
                    .justify_between()
                    .text_size(px(11.))
                    .child(div().text_color(colors.dim).child(label))
                    .child(div().text_color(colors.txt).child(control.label_value(v, working, kelvin))),
            )
            .child(Slider::new(self.slider(control)))
            .test_support()
            .into_any_element()
    }

    fn group(title: &str, colors: Colors) -> gpui_kit::Div {
        div().flex().flex_col().gap(px(2.)).pb(px(10.)).child(
            div().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(colors.dim).pb(px(4.)).child(title.to_string()),
        )
    }

    fn render_rail(&self, d: &Darkroom, colors: Colors, cx: &Context<Self>) -> AnyElement {
        let mut rail = div().id("dk-rail").flex().flex_col().flex_none().w(px(280.)).h_full().p(px(12.)).overflow_y_scroll().border_l_1().border_color(colors.line);
        let Some(open) = d.open.as_ref() else { return rail.into_any_element() };
        let working = open.working.clone();
        let kelvin = d.kelvin();
        let k = kelvin.as_ref();
        let look = ctl::look_of(&working);

        rail = rail.child(self.render_history(open, colors, cx));

        // White balance.
        let shown = ctl::wb_shown_for(&working, k);
        let mut wb = Self::group("White Balance", colors);
        if kelvin.is_some() {
            let label = if matches!(shown, WbShown::Kelvin { .. }) { "K" } else { "±" };
            wb = wb.child(clickable(chip("dk-wb-mode", label, true, colors), true, {
                let this = cx.entity().downgrade();
                move |_, _, cx| {
                    this.update(cx, |v, cx| v.edit(cx, |w, k| ctl::toggle_wb_mode(w, k))).ok();
                }
            }));
        }
        let wb_controls = match shown {
            WbShown::Kelvin { .. } => [Control::KelvinTemp, Control::KelvinTint],
            WbShown::Relative => [Control::WbTemp, Control::WbTint],
        };
        for c in wb_controls {
            wb = wb.child(self.slider_row(c, &working, k, colors, cx));
        }
        rail = rail.child(wb);

        // Tone and colour.
        let mut tone = Self::group("Tone", colors);
        for d in TONE_SLIDERS {
            tone = tone.child(self.slider_row(Control::Tone(d.key), &working, k, colors, cx));
        }
        tone = tone.child(div().text_size(px(10.)).text_color(colors.mute).child("Double-click a slider to reset it."));
        rail = rail.child(tone);
        let mut colour = Self::group("Color", colors);
        for d in COLOR_SLIDERS {
            colour = colour.child(self.slider_row(Control::Tone(d.key), &working, k, colors, cx));
        }
        rail = rail.child(colour);
        rail = rail.child(self.render_presets(d, colors, cx));

        // Effects.
        let mut fx = Self::group("Effects", colors);
        let mut chips = div().flex().flex_wrap().gap(px(4.)).pb(px(4.));
        let on = |el: gpui_kit::Stateful<gpui_kit::Div>, on: bool| el.when(on, |c| c.border_color(colors.accent).text_color(colors.accent));
        let this = cx.entity().downgrade();
        chips = chips.child(clickable(on(chip("dk-bw-color", "Color", true, colors), ctl::is_colour(&look)), true, {
            let this = this.clone();
            move |_, _, cx| {
                this.update(cx, |v, cx| v.edit(cx, |w, _| ctl::set_bw(w, None))).ok();
            }
        }));
        for f in bw_filters() {
            let active = ctl::bw_filter_active(&look, &f.bw);
            let id = format!("dk-bw-{}", f.label.to_lowercase());
            let this = this.clone();
            chips = chips.child(clickable(on(chip(id, format!("B&W {}", f.label), true, colors), active), true, move |_, _, cx| {
                let bw = f.bw.clone();
                this.update(cx, |v, cx| v.edit(cx, move |w, _| ctl::set_bw(w, Some(bw)))).ok();
            }));
        }
        fx = fx.child(chips);
        for d in EFFECT_SLIDERS {
            fx = fx.child(self.slider_row(Control::Effect(d.key), &working, k, colors, cx));
        }
        fx = fx.child(clickable(
            chip("dk-split-toggle", if self.show_split { "Split toning ▾" } else { "Split toning ▸" }, true, colors),
            true,
            {
                let this = this.clone();
                move |_, _, cx| {
                    this.update(cx, |v, cx| {
                        v.show_split = !v.show_split;
                        cx.notify();
                    })
                    .ok();
                }
            },
        ));
        if self.show_split {
            for d in SPLIT_SLIDERS {
                fx = fx.child(self.slider_row(Control::Split(d.key), &working, k, colors, cx));
            }
        }

        // LUT.
        fx = fx.child(div().pt(px(8.)).text_size(px(11.)).text_color(colors.dim).child("LUT (.cube)"));
        let mut luts = div().flex().flex_wrap().gap(px(4.));
        let current = look.lut.value().map(|l| l.file.clone());
        luts = luts.child(clickable(on(chip("dk-lut-none", "None", true, colors), current.is_none()), true, {
            let this = this.clone();
            move |_, _, cx| {
                this.update(cx, |v, cx| v.edit(cx, |w, _| ctl::set_lut(w, None))).ok();
            }
        }));
        for (i, (file, label)) in ctl::lut_options(&d.luts, &look).into_iter().enumerate() {
            let active = current.as_deref() == Some(file.as_str());
            let this = this.clone();
            luts = luts.child(clickable(on(chip(format!("dk-lut-{i}"), label, true, colors), active), true, move |_, _, cx| {
                let file = file.clone();
                this.update(cx, move |v, cx| v.edit(cx, move |w, _| ctl::set_lut(w, Some(&file)))).ok();
            }));
        }
        luts = luts.child(clickable(
            chip("dk-lut-import", "Import…", true, colors).tooltip(crate::shell::title_bar::tooltip("Copy a .cube file into the LUT folder")),
            true,
            {
                let this = this.clone();
                move |_, _, cx| {
                    this.update(cx, |v, cx| v.pick_lut(cx)).ok();
                }
            },
        ));
        fx = fx.child(luts);
        if current.is_some() {
            fx = fx.child(self.slider_row(Control::LutAmount, &working, k, colors, cx));
        }
        rail = rail.child(fx);
        rail = rail.children(self.render_lens(open, colors, cx));
        rail = rail.child(self.render_geometry(d, colors, cx));
        rail.into_any_element()
    }
}

const ZONE_TOOLTIPS: [&str; ZONE_COUNT] = [
    "blacks — drag up/down (±2 EV), double-click resets",
    "deep shadows — drag up/down (±2 EV), double-click resets",
    "shadows — drag up/down (±2 EV), double-click resets",
    "low mids — drag up/down (±2 EV), double-click resets",
    "high mids — drag up/down (±2 EV), double-click resets",
    "lights — drag up/down (±2 EV), double-click resets",
    "highlights — drag up/down (±2 EV), double-click resets",
    "whites — drag up/down (±2 EV), double-click resets",
];

impl Render for DarkroomView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let darkroom = self.darkroom.clone();
        if let Some((working, kelvin)) = darkroom.read(cx).open.as_ref().map(|o| (o.working.clone(), darkroom.read(cx).kelvin())) {
            self.sync_sliders(&working, kelvin.as_ref(), window, cx);
            self.rails.sync_straighten(&working, window, cx);
        }
        let d = darkroom.read(cx);
        let bar = self.render_bar(d, colors, cx);
        let error = d.error.clone();
        let stage = self.render_stage(d, colors, cx);
        let strip = self.render_tone_strip(d, colors, cx);
        let actions = self.render_actions(d, colors, cx);
        let film = self.render_filmstrip(d, colors, cx);
        let rail = self.render_rail(d, colors, cx);
        let empty = d.open.is_none();
        let overlay = self.rails.overlay_element();
        div()
            .id("darkroom")
            .key_context("Darkroom")
            .relative()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, e: &KeyDownEvent, window, cx| this.on_key(e, window, cx)))
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| this.on_mouse_move(e, cx)))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, e: &gpui_kit::MouseUpEvent, _, cx| this.end_drag(e, cx)))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| this.focus.focus(window, cx)))
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.canvas)
            .text_color(colors.txt)
            .child(bar)
            .children(error.map(|e| {
                div().id("dk-error").px(px(12.)).py(px(4.)).bg(colors.danger).text_color(colors.onaccent).text_size(px(11.)).child(e).test_support()
            }))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(stage)
                            .when(!empty, |c| c.child(strip).child(actions))
                            .children(film),
                    )
                    .child(rail),
            )
            .children(overlay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snap_follows_the_range_inputs_step_and_spelling() {
        assert_eq!(snap(0.149999, -1.0, 1.0, 0.05), 0.15);
        assert_eq!(snap(0.151, -1.0, 1.0, 0.05), 0.15);
        assert_eq!(snap(2.0, -1.0, 1.0, 0.05), 1.0);
        assert_eq!(snap(-3.2, -3.0, 3.0, 0.05), -3.0);
        assert_eq!(snap(0.5000000074505806, 0.0, 1.0, 0.02), 0.5);
        assert_eq!(snap(182.4, 0.0, 360.0, 5.0), 180.0);
        assert_eq!(snap(412.6, 0.0, 1000.0, 1.0), 413.0);
    }

    #[test]
    fn the_wheel_zooms_toward_the_cursor_between_fit_and_8x() {
        let v = StageView::FIT.zoomed(true, 100.0, 0.0);
        assert!((v.scale - 1.15).abs() < 1e-9);
        // The point under the cursor stays put: x' = tx + scale·(x − 0)… for the frame centre.
        assert!((v.tx - (100.0 - 1.15 * 100.0)).abs() < 1e-9);
        assert_eq!(v.zoomed(false, 100.0, 0.0), StageView::FIT, "back to 1× is the fit");
        let mut v = StageView::FIT;
        for _ in 0..40 {
            v = v.zoomed(true, 0.0, 0.0);
        }
        assert_eq!(v.scale, StageView::MAX_SCALE);
        assert_eq!(v.fit_label(), "Fit 800%");
    }

    #[test]
    fn the_frame_is_fitted_centred_scaled_and_offset() {
        // A 3:2 frame in a 1000×500 stage: height-limited, 750×500, centred.
        assert_eq!(StageView::FIT.image_rect(1000.0, 500.0, 1500.0, 1000.0), (125.0, 0.0, 750.0, 500.0));
        let v = StageView { scale: 2.0, tx: 10.0, ty: -5.0 };
        assert_eq!(v.image_rect(1000.0, 500.0, 1500.0, 1000.0), (-240.0, -255.0, 1500.0, 1000.0));
    }

    #[test]
    fn every_control_has_a_range_an_id_and_a_reset() {
        let w = VersionEdit::default();
        let all = Control::all();
        assert_eq!(all.len(), 4 + 8 + 4 + 5 + 1);
        let ids: std::collections::HashSet<_> = all.iter().map(|c| c.id()).collect();
        assert_eq!(ids.len(), all.len(), "ids are unique");
        for c in all {
            let (_, min, max, step) = c.def();
            assert!(min < max && step > 0.0, "{c:?}");
            let hue = matches!(c, Control::Split(k) if k.is_hue());
            assert_eq!(c.reset(&w, None).is_none(), hue, "{c:?}");
        }
    }

    #[test]
    fn labels_read_as_the_rail_printed_them() {
        let w = VersionEdit::default();
        assert_eq!(Control::Tone(ToneKey::Ev).label_value(0.5, &w, None), "0.50");
        assert_eq!(Control::KelvinTint.label_value(-3.0, &w, None), "−3");
        assert_eq!(Control::KelvinTint.label_value(2.0, &w, None), "+2");
        assert_eq!(Control::Split(SplitKey::ShadowHue).label_value(35.0, &w, None), "35°");
    }
}
