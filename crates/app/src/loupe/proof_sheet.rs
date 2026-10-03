//! [`ProofSheet`] (`ProofSheet.tsx`): the photo developed a dozen ways — real renders of the
//! `spreads::proof_spread` candidates, from the Darkroom's own source. Click a proof to adopt
//! its record ([`ProofEvent::Adopt`]); Esc, ✕ or the backdrop declines.
//!
//! Keys, as React's proof cells were buttons: Tab / Shift+Tab move focus through the proofs
//! (wrapping), and Enter or Space adopts the focused one. The backdrop is focused while no
//! proof is, but only a pointer click on it declines: Enter there does nothing (it used to be
//! the backdrop's keyboard click, so Enter declined the sheet).
//!
//! The Darkroom (#111/#112) deals it, focuses it (its [`contexts::PROOF_SHEET`] Escape outranks
//! the Darkroom's own while it has focus, as React's capture-phase listener did) and gives
//! focus back on [`ProofEvent::Close`] or after an adopt.

use crate::image_store::ImageStore;
use crate::keymap::contexts;
use crate::loupe::duel::{variant_image, VariantSource};
use crate::loupe::edit_renders::EditRenders;
use crate::loupe::{ProofClose, ProofNext, ProofPrevious};
use crate::shell::style::Colors;
use chairphoto_core::image_pool::EditJob;
use chairphoto_model::darkroom::spreads::{ProofCandidate, ProofGroup};
use gpui_kit::prelude::*;
use gpui_kit::{
    div, px, ClickEvent, Context, Entity, EventEmitter, FocusHandle, ObjectFit, SharedString, Subscription,
    TestSupportExt as _, Window,
};

/// A proof's long edge (React rendered 320 px cells).
pub const PROOF_EDGE: u32 = 320;

#[derive(Debug, Clone, PartialEq)]
pub enum ProofEvent {
    /// Adopt this candidate as the working state (the Darkroom names the history step
    /// "Proof: <label>" unless it is the as-shot cell).
    Adopt(ProofCandidate),
    Close,
}

/// See the module docs.
pub struct ProofSheet {
    source: VariantSource,
    candidates: Vec<ProofCandidate>,
    renders: Entity<EditRenders>,
    focus: FocusHandle,
    /// One per proof: Tab moves among them, Enter / Space adopts the focused one.
    cell_focus: Vec<FocusHandle>,
    closed: bool,
    _observers: [Subscription; 1],
}

impl EventEmitter<ProofEvent> for ProofSheet {}

impl ProofSheet {
    pub fn new(
        images: &Entity<ImageStore>,
        source: VariantSource,
        candidates: Vec<ProofCandidate>,
        cx: &mut Context<Self>,
    ) -> Self {
        let pool = images.read(cx).pool();
        let renders = cx.new(|cx| EditRenders::new(pool, cx));
        let jobs: Vec<EditJob> = candidates.iter().map(|c| source.job(&c.record, PROOF_EDGE)).collect();
        renders.update(cx, |r, cx| r.want(&jobs, cx));
        let _observers = [cx.observe(&renders, |_, _, cx| cx.notify())];
        let cell_focus = candidates.iter().map(|_| cx.focus_handle()).collect();
        ProofSheet { source, candidates, renders, focus: cx.focus_handle(), cell_focus, closed: false, _observers }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    pub fn candidates(&self) -> &[ProofCandidate] {
        &self.candidates
    }

    pub fn renders(&self) -> &Entity<EditRenders> {
        &self.renders
    }

    /// The proof that has focus, if any.
    pub fn focused(&self, window: &Window) -> Option<usize> {
        self.cell_focus.iter().position(|f| f.is_focused(window))
    }

    /// Tab / Shift+Tab: focus the next (previous) proof, wrapping; from the backdrop, the
    /// first (last).
    fn cycle(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.cell_focus.len();
        if n == 0 {
            return;
        }
        let next = match (self.focused(window), forward) {
            (Some(i), true) => (i + 1) % n,
            (Some(i), false) => (i + n - 1) % n,
            (None, true) => 0,
            (None, false) => n - 1,
        };
        window.focus(&self.cell_focus[next], cx);
        cx.notify();
    }

    pub fn adopt(&mut self, i: usize, cx: &mut Context<Self>) {
        if self.closed {
            return;
        }
        if let Some(c) = self.candidates.get(i).cloned() {
            self.end(cx);
            cx.emit(ProofEvent::Adopt(c));
        }
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        if !self.closed {
            self.end(cx);
            cx.emit(ProofEvent::Close);
        }
    }

    fn end(&mut self, cx: &mut Context<Self>) {
        self.closed = true;
        self.renders.update(cx, |r, cx| r.want(&[], cx));
    }
}

fn group_name(g: ProofGroup) -> Option<&'static str> {
    match g {
        ProofGroup::AsShot | ProofGroup::Auto => None,
        ProofGroup::Film => Some("film"),
        ProofGroup::Bw => Some("bw"),
        ProofGroup::Look => Some("look"),
    }
}

impl Render for ProofSheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let renders = self.renders.read(cx);
        let cells: Vec<_> = self
            .candidates
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let state = renders.get(&self.source.job(&c.record, PROOF_EDGE));
                let current = c.group == ProofGroup::AsShot;
                let focused = self.cell_focus[i].is_focused(window);
                div()
                    .id(("proof-cell", i as u64))
                    .track_focus(&self.cell_focus[i])
                    .flex()
                    .flex_col()
                    .w(px(PROOF_EDGE as f32 * 0.75))
                    .rounded(px(6.))
                    .border_2()
                    .border_color(if focused {
                        colors.txt
                    } else if current {
                        colors.accent
                    } else {
                        colors.border
                    })
                    .overflow_hidden()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.adopt(i, cx)))
                    .child(div().h(px(160.)).bg(colors.well).child(variant_image(("proof-image", i as u64), state, ObjectFit::Cover, "…", colors)))
                    .child(
                        div()
                            .flex()
                            .gap(px(6.))
                            .px(px(8.))
                            .py(px(5.))
                            .text_size(px(11.5))
                            .child(div().text_color(colors.txt).child(c.label.clone()))
                            .children(group_name(c.group).map(|g| div().text_color(colors.mute).child(g))),
                    )
                    .test_support()
            })
            .collect();
        let n = self.candidates.len();
        div()
            .id("proof-backdrop")
            .key_context(contexts::PROOF_SHEET)
            .track_focus(&self.focus)
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(colors.scrim)
            .on_action(cx.listener(|this, _: &ProofClose, _, cx| this.close(cx)))
            .on_action(cx.listener(|this, _: &ProofNext, window, cx| this.cycle(true, window, cx)))
            .on_action(cx.listener(|this, _: &ProofPrevious, window, cx| this.cycle(false, window, cx)))
            // A pointer click declines. Enter / Space on the focused backdrop is a keyboard
            // click here: not a decline (only a focused proof takes Enter, and adopts).
            .on_click(cx.listener(|this, e: &ClickEvent, _, cx| {
                if !e.is_keyboard() {
                    this.close(cx)
                }
            }))
            .child(
                div()
                    .id("proof-sheet")
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .p(px(16.))
                    .max_w(px(1100.))
                    .rounded(px(10.))
                    .bg(colors.panel)
                    .text_color(colors.txt)
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .child(div().text_size(px(15.)).child("Proof sheet"))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(colors.mute)
                                    .child(format!("your photo, developed {n} ways — real renders")),
                            )
                            .child(div().flex_1())
                            .child(
                                div()
                                    .id("proof-close")
                                    .cursor_pointer()
                                    .text_color(colors.dim)
                                    .child("✕")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.close(cx)
                                    }))
                                    .test_support(),
                            ),
                    )
                    .child(div().flex().flex_wrap().gap(px(8.)).children(cells))
                    .child(div().text_size(px(11.)).text_color(colors.mute).child(SharedString::from(
                        "Click a proof to adopt it · the current state is always dealt, so declining is a click · framing never changes on a proof",
                    ))),
            )
            .test_support()
    }
}
