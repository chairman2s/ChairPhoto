//! The Darkroom's variant overlays — [`DuelView`] (`DuelView.tsx`) here and the Proof sheet
//! (`proof_sheet.rs`) — and what both render from ([`VariantSource`]).
//!
//! **Duel refinement.** Two prints of the working state, symmetric around it in one dimension
//! per round (exposure, warmth, contrast, shadows: `spreads::duel_pair`); pick the better
//! one with a click or ←/→. The winner becomes the working state at once
//! ([`DuelEvent::Apply`]); ↓ says "same" and skips the dimension; Esc keeps the standing
//! winner and leaves. Either pane can be banked as a version (⑂, [`DuelEvent::Fork`]) without
//! ending the round; the Darkroom answers with [`DuelView::kept`].
//!
//! The Darkroom (#111/#112) opens it over its stage, focuses it (its [`contexts::DUEL`] keys
//! outrank the Darkroom's own while it has focus, as React's capture-phase listener did) and
//! gives focus back when it emits [`DuelEvent::Close`].

use crate::image_store::ImageStore;
use crate::keymap::contexts;
use crate::loupe::edit_renders::{EditRenders, RenderState};
use crate::loupe::zoom::fitted;
use crate::loupe::*;
use crate::shell::style::Colors;
use chairphoto_core::image_pool::EditJob;
use chairphoto_core::plugins::edit::SourceToken;
use chairphoto_model::darkroom::kelvin::KelvinContext;
use chairphoto_model::darkroom::spreads::{duel_pair, DuelDim, DUEL_DIMS};
use chairphoto_model::editing::VersionEdit;
use gpui_kit::prelude::*;
use gpui_kit::{
    div, px, AnyElement, Context, ElementId, Entity, EventEmitter, FocusHandle, ObjectFit, SharedString, Subscription,
    TestSupportExt as _, Window,
};
use std::rc::Rc;

/// What a variant renders from: the Darkroom's photo, source and engine stamp.
#[derive(Clone)]
pub struct VariantSource {
    pub photo_id: i64,
    /// The catalog the Darkroom was opened under (`AppModel::catalog_epoch`).
    pub catalog_epoch: u64,
    /// The stage's own pixels: the camera preview, or the resident RAW working image.
    pub source: SourceToken,
    /// The record as the renderer gets it (the Darkroom stamps the engine; React's
    /// `stamped(record)`). Defaults to [`VersionEdit::to_json`].
    pub encode: Rc<dyn Fn(&VersionEdit) -> String>,
}

impl VariantSource {
    pub fn new(photo_id: i64, catalog_epoch: u64, source: SourceToken) -> Self {
        VariantSource { photo_id, catalog_epoch, source, encode: Rc::new(|r: &VersionEdit| r.to_json()) }
    }

    /// The render job for `record` at `max_edge`.
    pub fn job(&self, record: &VersionEdit, max_edge: u32) -> EditJob {
        EditJob {
            photo_id: self.photo_id,
            edit_json: (self.encode)(record),
            max_edge,
            hi_res: false,
            base_only: false,
            source: self.source.clone(),
            clip: false,
            catalog_epoch: self.catalog_epoch,
        }
    }
}

/// What a variant cell shows in place of its picture (`RenderedImage.tsx`): `loading` while it
/// renders, a quiet "—" once it failed (React showed no error text there), nothing once ready.
pub fn variant_placeholder(state: &RenderState, loading: &'static str) -> Option<&'static str> {
    match state {
        RenderState::Ready(_) => None,
        RenderState::Failed(_) => Some("—"),
        RenderState::Rendering | RenderState::Absent => Some(loading),
    }
}

/// A variant cell's picture, `id` once drawn: the render fitted to the cell with `fit`
/// ([`fitted`]; React: the duel `contain`, proof cells and preset cards `cover`), or its
/// placeholder ([`variant_placeholder`]).
pub fn variant_image(
    id: impl Into<ElementId>,
    state: RenderState,
    fit: ObjectFit,
    loading: &'static str,
    colors: Colors,
) -> AnyElement {
    match (variant_placeholder(&state, loading), state) {
        (_, RenderState::Ready(image)) => fitted(id, image, fit).into_any_element(),
        (text, _) => div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(12.))
            .text_color(colors.mute)
            .child(text.unwrap_or_default())
            .into_any_element(),
    }
}

/// The duel's long edge per pane (React rendered 1024 px variants).
pub const DUEL_EDGE: u32 = 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum DuelEvent {
    /// This variant is the working state now.
    Apply(VersionEdit),
    /// Bank this variant as a version ("What-if — <dimension>"); answer with
    /// [`DuelView::kept`].
    Fork(VersionEdit, DuelDim),
    /// Done (Esc, or the last round decided).
    Close,
}

/// See the module docs.
pub struct DuelView {
    source: VariantSource,
    working: VersionEdit,
    kelvin: Option<KelvinContext>,
    dim_idx: usize,
    note: Option<String>,
    renders: Entity<EditRenders>,
    focus: FocusHandle,
    closed: bool,
    _observers: [Subscription; 1],
}

impl EventEmitter<DuelEvent> for DuelView {}

impl DuelView {
    pub fn new(
        images: &Entity<ImageStore>,
        source: VariantSource,
        working: VersionEdit,
        kelvin: Option<KelvinContext>,
        cx: &mut Context<Self>,
    ) -> Self {
        let pool = images.read(cx).pool();
        let renders = cx.new(|cx| EditRenders::new(pool, cx));
        let _observers = [cx.observe(&renders, |_, _, cx| cx.notify())];
        let mut view = DuelView {
            source,
            working,
            kelvin,
            dim_idx: 0,
            note: None,
            renders,
            focus: cx.focus_handle(),
            closed: false,
            _observers,
        };
        view.request(cx);
        view
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    pub fn round(&self) -> usize {
        self.dim_idx + 1
    }

    pub fn dim(&self) -> DuelDim {
        DUEL_DIMS[self.dim_idx.min(DUEL_DIMS.len() - 1)]
    }

    /// This round's two variants.
    pub fn pair(&self) -> [VersionEdit; 2] {
        duel_pair(&self.working, self.dim(), 0, self.kelvin.as_ref())
    }

    pub fn renders(&self) -> &Entity<EditRenders> {
        &self.renders
    }

    /// The working state changed outside the duel (the Darkroom applied the pick).
    pub fn set_working(&mut self, working: VersionEdit, cx: &mut Context<Self>) {
        if self.working != working {
            self.working = working;
            self.request(cx);
            cx.notify();
        }
    }

    /// The Darkroom banked a variant as `name`.
    pub fn kept(&mut self, name: &str, cx: &mut Context<Self>) {
        self.note = Some(format!("Kept as “{name}”"));
        cx.notify();
    }

    fn request(&mut self, cx: &mut Context<Self>) {
        if self.closed {
            return;
        }
        let jobs: Vec<EditJob> = self.pair().iter().map(|r| self.source.job(r, DUEL_EDGE)).collect();
        self.renders.update(cx, |r, cx| r.want(&jobs, cx));
    }

    /// Variant `i` (0 left, 1 right) wins the round.
    pub fn pick(&mut self, i: usize, cx: &mut Context<Self>) {
        if self.closed {
            return;
        }
        let record = self.pair()[i.min(1)].clone();
        self.working = record.clone();
        cx.emit(DuelEvent::Apply(record));
        self.advance(cx);
    }

    /// ↓ "same": the next dimension; the last round closes.
    pub fn advance(&mut self, cx: &mut Context<Self>) {
        if self.closed {
            return;
        }
        self.note = None;
        if self.dim_idx + 1 >= DUEL_DIMS.len() {
            self.close(cx);
        } else {
            self.dim_idx += 1;
            self.request(cx);
            cx.notify();
        }
    }

    pub fn fork(&mut self, i: usize, cx: &mut Context<Self>) {
        if !self.closed {
            cx.emit(DuelEvent::Fork(self.pair()[i.min(1)].clone(), self.dim()));
        }
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        if self.closed {
            return;
        }
        self.closed = true;
        // Nothing renders for a closed duel.
        self.renders.update(cx, |r, cx| r.want(&[], cx));
        cx.emit(DuelEvent::Close);
    }
}

impl Render for DuelView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let pair = self.pair();
        let mut rounds = div().flex().gap(px(6.));
        for (i, d) in DUEL_DIMS.iter().enumerate() {
            let (fg, bg) = if i == self.dim_idx {
                (colors.onaccent, colors.accent)
            } else if i < self.dim_idx {
                (colors.mute, colors.well)
            } else {
                (colors.dim, colors.well)
            };
            rounds = rounds.child(
                div()
                    .id(SharedString::from(format!("duel-round-{i}")))
                    .px(px(8.))
                    .h(px(20.))
                    .flex()
                    .items_center()
                    .rounded_full()
                    .text_size(px(11.))
                    .text_color(fg)
                    .bg(bg)
                    .child(d.label())
                    .test_support(),
            );
        }
        let title = format!("⚖ Duel — round {}", self.round());
        let head = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(12.))
            .h(px(44.))
            .px(px(14.))
            .border_b_1()
            .border_color(colors.border)
            .child(div().id("duel-title").text_size(px(14.)).child(title.clone()).aria_label(title).test_support())
            .child(rounds)
            .children(self.note.clone().map(|n| {
                div().id("duel-note").text_size(px(12.)).text_color(colors.ok).child(n.clone()).aria_label(n).test_support()
            }))
            .child(div().flex_1())
            .child(div().text_size(px(11.)).text_color(colors.mute).child("↓ same · Esc done"));
        let renders = self.renders.read(cx);
        let states: Vec<RenderState> = pair.iter().map(|r| renders.get(&self.source.job(r, DUEL_EDGE))).collect();
        // `darkroom.css` `.dk-duel-panes` / `.dk-duel-pane`: each pane a column, the variant
        // taking what the buttons leave (fitted inside it, never under them), the buttons below.
        let panes = div().flex().flex_row().gap(px(10.)).flex_1().min_h_0().p(px(14.)).children(
            states.into_iter().enumerate().map(|(i, state)| {
                let pick_label = if i == 0 { "← This one" } else { "This one →" };
                div()
                    .id(("duel-pane", i as u64))
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .flex_1()
                    .min_w_0()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.pick(i, cx)))
                    .child(
                        div()
                            .id(("duel-image-box", i as u64))
                            .flex_1()
                            .min_h_0()
                            .bg(colors.well)
                            .child(variant_image(("duel-image", i as u64), state, ObjectFit::Contain, "Rendering…", colors))
                            .test_support(),
                    )
                    .child(
                        div()
                            .id(("duel-pane-bar", i as u64))
                            .flex()
                            .flex_none()
                            .h(px(36.))
                            .items_center()
                            .justify_center()
                            .gap(px(8.))
                            .child(
                                crate::storage::ui::chip(SharedString::from(format!("duel-pick-{i}")), pick_label, true, colors)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.pick(i, cx)
                                    }))
                                    .test_support(),
                            )
                            .child(
                                crate::storage::ui::chip(SharedString::from(format!("duel-fork-{i}")), "⑂", true, colors)
                                    .tooltip(|window, cx| {
                                        gpui_kit::component::tooltip::Tooltip::new(
                                            "Keep this variant as a version and continue",
                                        )
                                        .build(window, cx)
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.fork(i, cx)
                                    }))
                                    .test_support(),
                            )
                            .test_support(),
                    )
                    .test_support()
            }),
        );
        div()
            .id("duel")
            .key_context(contexts::DUEL)
            .track_focus(&self.focus)
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.canvas)
            .text_color(colors.txt)
            .on_action(cx.listener(|this, _: &DuelLeft, _, cx| this.pick(0, cx)))
            .on_action(cx.listener(|this, _: &DuelRight, _, cx| this.pick(1, cx)))
            .on_action(cx.listener(|this, _: &DuelSame, _, cx| this.advance(cx)))
            .on_action(cx.listener(|this, _: &DuelClose, _, cx| this.close(cx)))
            .child(head)
            .child(panes)
            .test_support()
    }
}

#[cfg(test)]
mod placeholder_tests {
    use super::*;

    /// `RenderedImage.tsx`: the loading text while a variant renders (or is not wanted yet), a
    /// quiet "—" when it failed — never the error text (#161) — and nothing once it is ready.
    #[test]
    fn a_failed_variant_shows_a_dash_not_its_error() {
        assert_eq!(variant_placeholder(&RenderState::Failed("decode failed: boom".into()), "Rendering…"), Some("—"));
        assert_eq!(variant_placeholder(&RenderState::Rendering, "Rendering…"), Some("Rendering…"));
        assert_eq!(variant_placeholder(&RenderState::Absent, "…"), Some("…"));
    }
}
