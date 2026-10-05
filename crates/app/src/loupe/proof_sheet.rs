//! [`ProofSheet`] (`ProofSheet.tsx`): the photo developed a dozen ways — real renders of the
//! `spreads::proof_spread` candidates, from the Darkroom's own source. Click a proof to adopt
//! its record ([`ProofEvent::Adopt`]); Esc, ✕ or the backdrop declines.
//!
//! Keys, as React's proof cells were buttons: Tab / Shift+Tab, or ← / →, move focus through
//! the proofs in order (wrapping); ↑ / ↓ move to the cell above/below in the grid as it
//! actually renders ([`Self::columns`], [`Self::move_row`]) — the nearest cell when the target
//! row is shorter (the last, uneven one), and staying put at the top/bottom edge rather than
//! wrapping (#250 follow-up: unlike the flat Tab order, a vertical wrap would land on an
//! arbitrary column in the opposite edge row). Enter or Space adopts the focused one. The
//! backdrop is focused while no proof is, but only a pointer click on it declines: Enter there
//! does nothing (it used to be the backdrop's keyboard click, so Enter declined the sheet).
//!
//! The Darkroom (#111/#112) deals it, focuses it (its [`contexts::PROOF_SHEET`] Escape outranks
//! the Darkroom's own while it has focus, as React's capture-phase listener did) and gives
//! focus back on [`ProofEvent::Close`] or after an adopt. The Darkroom's own ←/→ filmstrip
//! step and ↑/↓ (unbound there) never double-fire under this sheet: its key context outranks
//! the Darkroom's for the keystrokes both would otherwise match, and the Darkroom's own
//! `on_key` stands down outright while any overlay (this sheet or the duel) is up.
//!
//! **The pop-out's preview (#250).** While the pointer is over a cell, or — nothing hovered —
//! a cell has focus, this publishes a [`LoupeProofPreview`] to [`ShellState`], which the
//! pop-out's `LoupeView` renders at loupe size in place of whatever it would otherwise show —
//! outranking even the Darkroom's print — (its own 320 px render stands in until that lands).
//! Hover beats focus — except that a keyboard move (an arrow key or Tab/Shift+Tab) wins over
//! whatever is merely hovered, until the pointer itself actually moves again (#250 follow-up,
//! "last input wins"): [`Self::keyboard_wins`] flips on in [`Self::cycle`] and
//! [`Self::move_row`], and off in [`Self::set_hovered`], which runs on every real hover change
//! regardless of its direction. Neither clears the preview to "the photo as it is" —
//! [`Self::sync_preview`], run from any click on the backdrop or the panel too (not only
//! Tab/hover), since the backdrop's own `track_focus` moves focus there on the matching mouse
//! down, off a focused cell, before any of this entity's own listeners do; and from every
//! render, as a catch-all for whatever that list still misses, through
//! [`Self::resync_in_render`] (which steps aside whenever another live sheet currently owns the
//! slot, so two mounted sheets settle instead of looping — #250 review, probe P5).
//! Cleared, by this sheet's own token, on adopt, decline, and whenever this entity is released
//! (its window closed, the overlay replaced); a catalog switch clears it in `ShellState`
//! itself, the same way it clears the Darkroom's print.

use crate::image_store::ImageStore;
use crate::keymap::contexts;
use crate::loupe::duel::{variant_image, VariantSource};
use crate::loupe::edit_renders::EditRenders;
use crate::loupe::{ProofClose, ProofDown, ProofNext, ProofPrevious, ProofUp};
use crate::shell::state::{LoupeProofPreview, ShellState};
use crate::shell::style::Colors;
use chairphoto_core::image_pool::EditJob;
use chairphoto_model::darkroom::spreads::{ProofCandidate, ProofGroup};
use gpui_kit::prelude::*;
use gpui_kit::{
    canvas, div, px, App, Bounds, ClickEvent, Context, Entity, EntityId, EventEmitter, FocusHandle, ObjectFit, Pixels,
    SharedString, Subscription, TestSupportExt as _, Window,
};
use std::cell::Cell;
use std::rc::Rc;

/// A proof's long edge (React rendered 320 px cells).
pub const PROOF_EDGE: u32 = 320;

/// A cell's rendered width (`.w(..)` below) and the wrap row's gap (`.gap(..)` below): the
/// magic numbers [`columns_for`] turns a measured row width into a column count with — kept
/// here, next to [`PROOF_EDGE`], so a future resize of either stays in step with the layout it
/// describes (#250 follow-up).
const CELL_W: f32 = PROOF_EDGE as f32 * 0.75;
const CELL_GAP: f32 = 8.;

/// How many `CELL_W` px cells, `CELL_GAP` px apart, fit `width` px of the sheet's actual
/// flex-wrap row — the same shape as `crate::library::layout::columns`'s CSS-grid `minmax`
/// math, but this row's cells are a fixed width and its own gap, not the grid's variable tile
/// and 3 px gap. Nothing measured yet (`width <= 0`) is one column, not zero.
fn columns_for(width: f32) -> usize {
    if width <= 0. {
        return 1;
    }
    (((width + CELL_GAP) / (CELL_W + CELL_GAP)).floor() as usize).max(1)
}

/// [`ProofSheet::move_row`]'s pure arithmetic (#250 follow-up): `delta` rows from index `i`
/// of `n` cells laid out `cols` wide — row-major, the same order [`ProofSheet::render`] hands
/// out `cell_focus`/`cell` ids in — clamped to the nearest cell when the target row is shorter
/// (the last, uneven one: `n` need not be a multiple of `cols`). `None` at the top/bottom
/// edge, where `move_row` stays put rather than wrapping (see the module docs for why a
/// vertical wrap would be more surprising than useful here, unlike Tab's flat order).
pub(crate) fn row_target(i: usize, n: usize, cols: usize, delta: i32) -> Option<usize> {
    let cols = cols.max(1);
    let row = (i / cols) as i32 + delta;
    if row < 0 {
        return None;
    }
    let row_start = row as usize * cols;
    if row_start >= n {
        return None;
    }
    let row_last = (row_start + cols).min(n) - 1;
    Some(row_start + (i % cols).min(row_last - row_start))
}

#[cfg(test)]
mod layout_tests {
    use super::{columns_for, row_target};

    #[test]
    fn columns_follow_the_measured_width() {
        assert_eq!(columns_for(0.), 1);
        assert_eq!(columns_for(-5.), 1);
        assert_eq!(columns_for(100.), 1, "narrower than one cell is still one column");
        // floor((w + 8) / (240 + 8)): a second column needs 2 × 240 + 8 = 488 px.
        assert_eq!(columns_for(487.), 1);
        assert_eq!(columns_for(488.), 2);
        assert_eq!(columns_for(1068.), 4, "the 1100 px panel cap, minus its 16 px padding twice");
    }

    #[test]
    fn row_target_moves_by_row_and_clamps_to_the_nearest_cell_in_a_shorter_row() {
        // 3 columns, 7 cells: rows [0,1,2], [3,4,5], [6] — the last row has only one cell.
        assert_eq!(row_target(0, 7, 3, 1), Some(3), "row 0 col 0 down to row 1 col 0");
        assert_eq!(row_target(2, 7, 3, 1), Some(5), "row 0 col 2 down to row 1 col 2");
        assert_eq!(row_target(3, 7, 3, 1), Some(6), "row 1 col 0 down into row 2's only cell: clamped");
        assert_eq!(row_target(5, 7, 3, 1), Some(6), "row 1 col 2 down into row 2's only cell too: clamped the same");
        assert_eq!(row_target(6, 7, 3, 1), None, "row 2 is the last row: Down stays put");
        assert_eq!(row_target(6, 7, 3, -1), Some(3), "row 2's only cell up to row 1's own column 0");
        assert_eq!(row_target(0, 7, 3, -1), None, "row 0 is the first row: Up stays put");
        assert_eq!(row_target(4, 7, 3, -1), Some(1), "row 1 col 1 up to row 0 col 1");
        // One column: every row has exactly one cell, so Up/Down are a plain ±1 step.
        assert_eq!(row_target(0, 4, 1, 1), Some(1));
        assert_eq!(row_target(3, 4, 1, 1), None, "the last cell: Down stays put");
        assert_eq!(row_target(0, 4, 1, -1), None, "the first cell: Up stays put");
    }
}

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
    /// One per proof: Tab/arrows move among them, Enter / Space adopts the focused one.
    cell_focus: Vec<FocusHandle>,
    /// The pointer is over this cell (`Self::sync_preview`'s precedence: hover beats focus,
    /// unless `keyboard_wins`).
    hovered: Option<usize>,
    /// An arrow key or Tab/Shift+Tab moved focus more recently than the pointer did: outranks
    /// `hovered` in `Self::sync_preview` until `Self::set_hovered` runs again — a real hover
    /// change, in either direction (#250 follow-up, "last input wins"). Sticks across renders;
    /// only a fresh hover event clears it, not time or a click.
    keyboard_wins: bool,
    /// The cells' own flex-wrap row, measured after layout the way the Darkroom's own
    /// `stage_bounds` is: `Self::columns` turns its width into the column count `Self::move_row`
    /// steps by — the sheet's actual rendered layout, not a guessed one (#250 follow-up).
    grid_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
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
            keyboard_wins: false,
            grid_bounds: Rc::default(),
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
    /// [`Self::sync_preview`]). A real hover change, so it is also where `keyboard_wins` lets
    /// go: the pointer itself just moved, in either direction (#250 follow-up).
    fn set_hovered(&mut self, i: usize, hovering: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.keyboard_wins = false;
        self.hovered = if hovering {
            Some(i)
        } else if self.hovered == Some(i) {
            None
        } else {
            self.hovered
        };
        self.sync_preview(window, cx);
    }

    /// Tab / Shift+Tab, or ← / →: focus the next (previous) proof, wrapping; from the
    /// backdrop, the first (last).
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
        self.keyboard_wins = true;
        window.focus(&self.cell_focus[next], cx);
        self.sync_preview(window, cx);
        cx.notify();
    }

    /// ↑ / ↓ (`delta` −1 / +1): focus the cell one row up/down in [`Self::columns`]'s grid,
    /// the nearest one when the target row is shorter (the last, uneven row) — clamped to
    /// that row's own last column, never spilling into the row after it. Stays put at the
    /// top/bottom edge (see the module docs for why that beats wrapping here). From the
    /// backdrop (nothing focused), `Down` starts at the first cell and `Up` at the last,
    /// `Self::cycle`'s own convention for "no focus yet".
    fn move_row(&mut self, delta: i32, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.cell_focus.len();
        if n == 0 {
            return;
        }
        let next = match self.focused(window) {
            Some(i) => match row_target(i, n, self.columns(), delta) {
                Some(next) => next,
                None => return,
            },
            None if delta > 0 => 0,
            None => n - 1,
        };
        self.keyboard_wins = true;
        window.focus(&self.cell_focus[next], cx);
        self.sync_preview(window, cx);
        cx.notify();
    }

    /// How many cells fit one row of the wrap's measured width (`Self::grid_bounds`) —
    /// [`columns_for`], the sheet's own actual layout rather than the library grid's CSS-grid
    /// `minmax` math, which uses a different gap and a variable tile width. `pub(crate)` so
    /// tests can predict `Self::move_row`'s targets from the same measured width, rather than
    /// guessing a column count of their own (#250 follow-up).
    pub(crate) fn columns(&self) -> usize {
        columns_for(self.grid_bounds.get().map_or(0., |b| f32::from(b.size.width)))
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

    /// The pop-out's preview: the hovered cell, else the focused one, else none (the photo as
    /// it is) — published to [`ShellState`] (#250). While `keyboard_wins` (an arrow key or
    /// Tab/Shift+Tab moved focus more recently than the pointer did), the focused cell wins
    /// instead, so a pointer merely resting on another cell cannot block keyboard navigation's
    /// own preview (#250 follow-up). Called from the hover/cycle/move_row handlers, and from a
    /// click on the backdrop or the panel: its backdrop tracks focus too, and the matching
    /// mouse down can move focus off a focused cell before either of those runs (#250 review,
    /// probe E). `ShellState::set_loupe_proof_preview` skips the notify when nothing actually
    /// changed, so a same-answer call here is cheap.
    fn sync_preview(&mut self, window: &Window, cx: &mut Context<Self>) {
        let i = if self.closed {
            None
        } else if self.keyboard_wins {
            self.focused(window).or(self.hovered)
        } else {
            self.hovered.or_else(|| self.focused(window))
        };
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
                    .w(px(CELL_W))
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
            .on_action(cx.listener(|this, _: &ProofUp, window, cx| this.move_row(-1, window, cx)))
            .on_action(cx.listener(|this, _: &ProofDown, window, cx| this.move_row(1, window, cx)))
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
                    .child({
                        // Measures this row's actual rendered width, the way the Darkroom's
                        // own `stage_bounds` does, so `Self::columns` reflects the layout GPUI
                        // settled on — not a guess independent of it (#250 follow-up).
                        let bounds = self.grid_bounds.clone();
                        let measure = canvas(move |b, _, _| bounds.set(Some(b)), |_, _, _, _| {}).absolute().size_full();
                        div().relative().flex().flex_wrap().gap(px(CELL_GAP)).child(measure).children(cells)
                    })
                    .child(div().text_size(px(11.)).text_color(colors.mute).child(SharedString::from(
                        "Click a proof to adopt it · the current state is always dealt, so declining is a click · framing never changes on a proof",
                    ))),
            )
            .test_support()
    }
}
