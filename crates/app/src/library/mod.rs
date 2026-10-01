//! The Library view (#106): the virtualised photo grid on the Library surface, its keys,
//! and the "Stack bursts" dialog. Ports of `CatalogGrid.tsx`, `Thumbnail.tsx`,
//! `StackProposalsDialog.tsx` and the grid branch of App.tsx's key handler;
//! `docs/plans/gpui/parity.md` is the acceptance list.
//!
//! - [`grid::LibraryView`] draws the rows of the Library session (`ShellState::library`)
//!   in a `uniform_list` whose column count follows the list's width and the command pill's
//!   size slider. Thumbnails come from the image layer (`ImageStore`): only the visible
//!   rows plus a small overscan are requested, and what scrolls away is released.
//! - The rows themselves, the culling write path and deep links live in
//!   [`crate::shell::ShellState`], which owns the session.
//! - [`stacks::StackDialog`] is the "Stack bursts" modal.
//!
//! **Paging.** The React grid did not page: it listed every matching row and virtualised
//! the drawing, fetching only the storage badges per window (libraryQuery.ts). So does
//! this port — the session's own note ("Not yet windowed") says why: Shift ranges,
//! select-all and stepping index into the full row list.

pub mod grid;
pub mod layout;
pub mod stacks;
#[cfg(test)]
mod tests;

use crate::keymap::contexts;
use gpui_kit::{actions, KeyBinding};

actions!(
    library,
    [
        /// →/↓: the next photo (Shift extends the selection).
        SelectNext,
        /// ←/↑: the previous photo.
        SelectPrevious,
        /// Shift+→/↓.
        ExtendNext,
        /// Shift+←/↑.
        ExtendPrevious,
        /// Home: the first photo.
        SelectFirst,
        /// End: the last photo.
        SelectLast,
        /// Page Down: a screenful of photos on.
        PageDown,
        /// Page Up: a screenful of photos back.
        PageUp,
        /// Ctrl+A: every photo in the view.
        SelectAll,
        /// 0–5: rate.
        Rate0,
        Rate1,
        Rate2,
        Rate3,
        Rate4,
        Rate5,
        /// P / X / U: pick, reject, unflag.
        MarkPick,
        MarkReject,
        MarkUnflag,
        /// R / Y / G / B / V: colour label; N clears it.
        LabelRed,
        LabelYellow,
        LabelGreen,
        LabelBlue,
        LabelPurple,
        LabelNone,
        /// Enter: open the active photo (the loupe, #109).
        OpenActive,
        /// C: Compare, with two or more selected (#109).
        CompareSelection,
        /// Escape in the "Stack bursts" dialog.
        CloseDialog,
    ]
);

/// The Library's keys (App.tsx's window handler, grid branch), plus Home/End/Page keys,
/// which React's grid did not have. Bound in the [`contexts::LIBRARY`] context, so they
/// fire only while the grid has focus — never from a menu or a text input, which are not
/// inside it. React ignored modifiers (Ctrl+P also picked); here only the plain keys (and
/// Shift for the arrows) are bound.
pub fn bindings() -> Vec<KeyBinding> {
    let c = Some(contexts::LIBRARY);
    let mut b = vec![
        KeyBinding::new("right", SelectNext, c),
        KeyBinding::new("down", SelectNext, c),
        KeyBinding::new("left", SelectPrevious, c),
        KeyBinding::new("up", SelectPrevious, c),
        KeyBinding::new("shift-right", ExtendNext, c),
        KeyBinding::new("shift-down", ExtendNext, c),
        KeyBinding::new("shift-left", ExtendPrevious, c),
        KeyBinding::new("shift-up", ExtendPrevious, c),
        KeyBinding::new("home", SelectFirst, c),
        KeyBinding::new("end", SelectLast, c),
        KeyBinding::new("pagedown", PageDown, c),
        KeyBinding::new("pageup", PageUp, c),
        KeyBinding::new("ctrl-a", SelectAll, c),
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
        KeyBinding::new("enter", OpenActive, c),
        KeyBinding::new("c", CompareSelection, c),
    ];
    b.push(KeyBinding::new("escape", CloseDialog, Some(contexts::STACK_DIALOG)));
    b
}
