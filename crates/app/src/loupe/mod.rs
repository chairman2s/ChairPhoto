//! The loupe and its relatives (#109): ports of `ZoomableImage.tsx`, `PreviewImage.tsx`,
//! `CompareView.tsx`, `CullSession.tsx`, `DuelView.tsx`, `ProofSheet.tsx` and the loupe,
//! Compare and cull branches of App.tsx's key handler. `docs/plans/gpui/parity.md` is the
//! acceptance list.
//!
//! - [`zoom::ZoomImage`] — the zoomable image: preview at fit, the full-resolution tier once
//!   zoomed, a transform that Compare's panes share.
//! - [`view::LoupeView`] — the inline loupe on the Library stage. It follows
//!   [`ShellState::loupe_target`](crate::shell::ShellState::loupe_target) and holds no state a
//!   second window could not have too.
//! - [`window`] — the pop-out loupe (#110): another `LoupeView` over the same `ShellState`,
//!   `ImageStore` and `ModuleRegistry` entities, in its own window.
//! - [`compare_view::CompareView`] over [`compare::CompareSession`] — two to four frames with
//!   one pan/zoom, in grid or duel mode.
//! - [`cull::CullView`] — the full-screen, keyboard-only cull session over a frozen list.
//! - `duel::DuelView` and `proof_sheet::ProofSheet` (`edit` feature) — the Darkroom's variant
//!   overlays, rendered through [`edit_renders::EditRenders`]. Their entry points are the
//!   Darkroom's (#111/#112), which mounts them.
//!
//! **Latency.** Navigation asks the image layer for the requested photo first, then N+1 and
//! N−1, then the rest of the preload window, as one pool batch (`ImageStore::navigate_window`);
//! nothing decodes on the UI thread. `examples/loupe_bench.rs` measures stepping latency
//! against the AGENTS.md targets (< 50 ms preloaded, < 500 ms cold).
//!
//! **Keys.** Each surface has its key context ([`crate::keymap::contexts`]) and takes focus
//! when it opens; the root hands focus to whichever stage view is showing. The culling keys
//! (0–5, P/X/U, R/Y/G/B/V/N) are the Library's actions, bound in every context that culls,
//! so the keymap stays one list.

pub mod compare;
pub mod compare_view;
pub mod cull;
#[cfg(feature = "edit")]
pub mod duel;
#[cfg(feature = "edit")]
pub mod edit_renders;
#[cfg(feature = "edit")]
pub mod proof_sheet;
pub mod view;
pub mod window;
pub mod zoom;
#[cfg(test)]
mod popout_tests;
#[cfg(test)]
mod tests;

use crate::keymap::contexts;
use crate::library::*;
use gpui_kit::{actions, KeyBinding, NoAction};

actions!(
    loupe,
    [
        /// Escape in the loupe: back to the grid.
        CloseLoupe,
        /// Escape or C in Compare: back to the grid.
        CloseCompare,
        /// ← in Compare: the duel's left verdict, else the previous pane.
        CompareLeft,
        /// → in Compare: the duel's right verdict, else the next pane.
        CompareRight,
        /// ↑ in Compare: the previous pane.
        ComparePrevious,
        /// ↓ in Compare: the next pane.
        CompareNext,
        /// Page Down / Page Up in Compare's grid: the next / previous batch.
        ComparePageDown,
        ComparePageUp,
        /// K in Compare: keep the focused pane.
        CompareKeep,
        /// →/↓/Space in a cull session: the next photo without deciding.
        CullNext,
        /// ←/↑: back one.
        CullPrevious,
        /// Escape: close the help, else end the session; on the summary, back to the grid.
        CullEscape,
        /// Enter on the summary: back to the grid.
        CullConfirm,
        /// H or ?: the keys.
        CullHelp,
        /// ← / → in the Darkroom's Duel: the left / right variant wins the round.
        DuelLeft,
        DuelRight,
        /// ↓: same — skip this dimension.
        DuelSame,
        /// Escape: keep the standing winner and leave.
        DuelClose,
        /// Escape over the Proof sheet: decline.
        ProofClose,
    ]
);

/// The culling keys, which the grid, the loupe, Compare and the cull session share.
fn culling_keys(context: &'static str) -> Vec<KeyBinding> {
    let c = Some(context);
    vec![
        KeyBinding::new("0", Rate0, c),
        KeyBinding::new("1", Rate1, c),
        KeyBinding::new("2", Rate2, c),
        KeyBinding::new("3", Rate3, c),
        KeyBinding::new("4", Rate4, c),
        KeyBinding::new("5", Rate5, c),
        KeyBinding::new("p", MarkPick, c),
        KeyBinding::new("x", MarkReject, c),
        KeyBinding::new("u", MarkUnflag, c),
        KeyBinding::new("r", LabelRed, c),
        KeyBinding::new("y", LabelYellow, c),
        KeyBinding::new("g", LabelGreen, c),
        KeyBinding::new("b", LabelBlue, c),
        KeyBinding::new("v", LabelPurple, c),
        KeyBinding::new("n", LabelNone, c),
    ]
}

/// The loupe's, Compare's, the cull session's and the Darkroom overlays' keys.
pub fn bindings() -> Vec<KeyBinding> {
    let loupe = Some(contexts::LOUPE);
    let compare = Some(contexts::COMPARE);
    let cull = Some(contexts::CULL);
    let duel = Some(contexts::DUEL);
    let proof = Some(contexts::PROOF_SHEET);
    let mut b = vec![
        // The loupe: App.tsx's grid branch, which also ran while the loupe was inline.
        KeyBinding::new("right", SelectNext, loupe),
        KeyBinding::new("down", SelectNext, loupe),
        KeyBinding::new("left", SelectPrevious, loupe),
        KeyBinding::new("up", SelectPrevious, loupe),
        KeyBinding::new("shift-right", ExtendNext, loupe),
        KeyBinding::new("shift-down", ExtendNext, loupe),
        KeyBinding::new("shift-left", ExtendPrevious, loupe),
        KeyBinding::new("shift-up", ExtendPrevious, loupe),
        KeyBinding::new("ctrl-a", SelectAll, loupe),
        KeyBinding::new("enter", CloseLoupe, loupe),
        KeyBinding::new("escape", CloseLoupe, loupe),
        KeyBinding::new("c", CompareSelection, loupe),
        // Compare.
        KeyBinding::new("escape", CloseCompare, compare),
        KeyBinding::new("c", CloseCompare, compare),
        KeyBinding::new("left", CompareLeft, compare),
        KeyBinding::new("right", CompareRight, compare),
        KeyBinding::new("up", ComparePrevious, compare),
        KeyBinding::new("down", CompareNext, compare),
        KeyBinding::new("pagedown", ComparePageDown, compare),
        KeyBinding::new("pageup", ComparePageUp, compare),
        KeyBinding::new("k", CompareKeep, compare),
        // The cull session.
        KeyBinding::new("right", CullNext, cull),
        KeyBinding::new("down", CullNext, cull),
        KeyBinding::new("space", CullNext, cull),
        KeyBinding::new("left", CullPrevious, cull),
        KeyBinding::new("up", CullPrevious, cull),
        KeyBinding::new("escape", CullEscape, cull),
        KeyBinding::new("enter", CullConfirm, cull),
        KeyBinding::new("h", CullHelp, cull),
        KeyBinding::new("?", CullHelp, cull),
        KeyBinding::new("shift-/", CullHelp, cull),
        // The Darkroom's overlays.
        KeyBinding::new("left", DuelLeft, duel),
        KeyBinding::new("right", DuelRight, duel),
        KeyBinding::new("down", DuelSame, duel),
        KeyBinding::new("escape", DuelClose, duel),
        KeyBinding::new("escape", ProofClose, proof),
    ];
    for context in [contexts::LOUPE, contexts::COMPARE, contexts::CULL] {
        b.extend(culling_keys(context));
    }
    // The cull session and the Darkroom's overlays own the keyboard: React's window handler
    // stood down for them, so the panel toggles do not fire there.
    for context in [contexts::CULL, contexts::DUEL, contexts::PROOF_SHEET] {
        b.push(KeyBinding::new("[", NoAction, Some(context)));
        b.push(KeyBinding::new("]", NoAction, Some(context)));
    }
    b
}
