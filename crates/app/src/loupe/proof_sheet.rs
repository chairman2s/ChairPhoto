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
//!
//! **The pop-out's preview (#250).** While the pointer is over a cell, or — nothing hovered —
//! a cell has Tab focus, this publishes a [`LoupeProofPreview`] to [`ShellState`], which the
//! pop-out's `LoupeView` renders at loupe size in place of whatever it would otherwise show —
//! outranking even the Darkroom's print — (its own 320 px render stands in until that lands).
//! Hover beats focus; neither clears it to "the photo as it is" — [`Self::sync_preview`], run
//! from any click on the backdrop or the panel too (not only Tab/hover), since the backdrop's
//! own `track_focus` moves focus there on the matching mouse down, off a Tab-focused cell,
//! before any of this entity's own listeners do; and from every render, as a catch-all for
//! whatever that list still misses, through [`Self::resync_in_render`] (which steps aside
//! whenever another live sheet currently owns the slot, so two mounted sheets settle instead
//! of looping — #250 review, probe P5).
//! Cleared, by this sheet's own token, on adopt, decline, and whenever this entity is released
//! (its window closed, the overlay replaced); a catalog switch clears it in `ShellState`
//! itself, the same way it clears the Darkroom's print.

use crate::image_store::ImageStore;
use crate::keymap::contexts;
use crate::loupe::duel::{variant_image, VariantSource};
use crate::loupe::edit_renders::EditRenders;
use crate::loupe::{ProofClose, ProofNext, ProofPrevious};
use crate::shell::state::{LoupeProofPreview, ShellState};
use crate::shell::style::Colors;
use chairphoto_core::image_pool::EditJob;
use chairphoto_model::darkroom::spreads::{ProofCandidate, ProofGroup};
use gpui_kit::prelude::*;
use gpui_kit::{
    div, px, App, ClickEvent, Context, Entity, EntityId, EventEmitter, FocusHandle, ObjectFit, SharedString,
    Subscription, TestSupportExt as _, Window,
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
    shell: Entity<ShellState>,
    focus: FocusHandle,
    /// One per proof: Tab moves among them, Enter / Space adopts the focused one.
    cell_focus: Vec<FocusHandle>,
    /// The pointer is over this cell (`Self::sync_preview`'s precedence: hover beats focus).
    hovered: Option<usize>,
    /// This entity's id: stamped on every [`LoupeProofPreview`] this sheet publishes, so
    /// [`Self::clear_preview`] takes down only its own (#250 review: two live sheets is not
    /// reachable in-app today — one overlay, replaced before a new sheet can publish — but
    /// `clear_preview` should not depend on that staying true).
    token: EntityId,
    closed: bool,
    _observers: [Subscription; 1],
}

impl EventEmitter<ProofEvent> for ProofSheet {}

impl ProofSheet {
    pub fn new(
        images: &Entity<ImageStore>,
        shell: Entity<ShellState>,
        source: VariantSource,
        candidates: Vec<ProofCandidate>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let pool = images.read(cx).pool();
        let renders = cx.new(|cx| EditRenders::new(pool, cx));
        let jobs: Vec<EditJob> = candidates.iter().map(|c| source.job(&c.record, PROOF_EDGE)).collect();
        renders.update(cx, |r, cx| r.want(&jobs, cx));
        // Keeps the 320 px placeholder the pop-out's preview shows fresh as it settles, on
        // top of the plain re-render every other `EditRenders` observer does (#250).
        let _observers = [cx.observe_in(&renders, window, |this, _, window, cx| {
            this.sync_preview(window, cx);
            cx.notify();
        })];
        let cell_focus = candidates.iter().map(|_| cx.focus_handle()).collect();
        let this = ProofSheet {
            source,
            candidates,
            renders,
            shell,
            focus: cx.focus_handle(),
            cell_focus,
            hovered: None,
            token: cx.entity_id(),
            closed: false,
            _observers,
        };
        // Whatever ends this entity's life without going through `close`/`adopt` — the
        // overlay dropped from under it, the window it was mounted in closing — still takes
        // the preview down (#250).
        cx.on_release(|this: &mut Self, cx| this.clear_preview(cx)).detach();
        this
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

    /// The pointer entered (`hovering`) or left cell `i` (`on_hover`'s own bookkeeping, then
    /// [`Self::sync_preview`]).
    fn set_hovered(&mut self, i: usize, hovering: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.hovered = if hovering {
            Some(i)
        } else if self.hovered == Some(i) {
            None
        } else {
            self.hovered
        };
        self.sync_preview(window, cx);
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
        self.sync_preview(window, cx);
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
        self.clear_preview(cx);
    }

    /// The pop-out's preview: the hovered cell, else the Tab-focused one, else none (the
    /// photo as it is) — published to [`ShellState`] (#250). Called from the hover/cycle
    /// handlers, and from a click on the backdrop or the panel: its backdrop tracks focus
    /// too, and the matching mouse down can move focus off a Tab-focused cell before either
    /// of those runs (#250 review, probe E). `ShellState::set_loupe_proof_preview` skips the
    /// notify when nothing actually changed, so a same-answer call here is cheap.
    fn sync_preview(&mut self, window: &Window, cx: &mut Context<Self>) {
        let i = if self.closed { None } else { self.hovered.or_else(|| self.focused(window)) };
        let preview = i.and_then(|i| self.candidates.get(i)).map(|c| {
            let cell = self.renders.read(cx).get(&self.source.job(&c.record, PROOF_EDGE));
            LoupeProofPreview {
                sheet: self.token,
                photo_id: self.source.photo_id,
                source: self.source.clone(),
                candidate: c.clone(),
                cell,
            }
        });
        match preview {
            Some(preview) => self.shell.update(cx, |s, cx| s.set_loupe_proof_preview(Some(preview), cx)),
            // Nothing of this sheet's own to show: take only this sheet's preview down, the
            // same as `clear_preview` (#250 review) — a non-hovered, non-focused sheet whose
            // own 320 px render lands (the `renders` observer calls this too) must not clobber
            // whichever other sheet is currently shown.
            None => self.clear_preview(cx),
        }
    }

    /// Takes the preview down, but only if it is still this sheet's own (#250 review: `token`).
    fn clear_preview(&mut self, cx: &mut App) {
        let token = self.token;
        self.shell.update(cx, |s, cx| {
            if s.loupe_proof_preview().is_some_and(|p| p.sheet == token) {
                s.set_loupe_proof_preview(None, cx);
            }
        });
    }

    /// [`Self::sync_preview`]'s render-time catch-all (probe E): skipped whenever another
    /// live sheet currently owns the published preview. Resyncing unconditionally here, with
    /// two mounted sheets each having a hovered or focused cell, has each one's own render
    /// republish its own answer and notify the other's — forever (#250 review, probe P5, not
    /// reachable with today's single Darkroom overlay, but `render` must not rely on that).
    /// An actual hover, focus change or click on THIS sheet still calls `sync_preview`
    /// directly (`set_hovered`, `cycle`, the two `on_click`s, the `renders` observer) and
    /// takes the slot over regardless of who held it.
    fn resync_in_render(&mut self, window: &Window, cx: &mut Context<Self>) {
        let owned_by_another = self.shell.read(cx).loupe_proof_preview().is_some_and(|p| p.sheet != self.token);
        if !owned_by_another {
            self.sync_preview(window, cx);
        }
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
        // Catches a focus change that didn't go through `cycle`/`set_hovered`/an `on_click`
        // (#250 review, probe E) — see `Self::resync_in_render`'s own docs for why this is
        // not a plain `sync_preview` call.
        self.resync_in_render(window, cx);
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
                    .on_hover(cx.listener(move |this, hovering: &bool, window, cx| {
                        this.set_hovered(i, *hovering, window, cx)
                    }))
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
            // click here: not a decline (only a focused proof takes Enter, and adopts). Either
            // way, a mouse click here already focused the backdrop on its mouse down (GPUI's
            // own `track_focus` behaviour) — which can move focus off a Tab-focused cell
            // without going through `cycle` (#250 review, probe E) — so resync; a no-op when
            // the answer hasn't changed (and `close` below clears it anyway on a decline).
            .on_click(cx.listener(|this, e: &ClickEvent, window, cx| {
                this.sync_preview(window, cx);
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
                    // Stops the backdrop's decline; the click still moved focus here (#250
                    // review, probe E), so resync the same way.
                    .on_click(cx.listener(|this, _, window, cx| {
                        cx.stop_propagation();
                        this.sync_preview(window, cx)
                    }))
                    .test_support()
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
