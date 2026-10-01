//! Actions, key contexts and the keymap.
//!
//! **Contexts.** Every view that owns keys sets one of the [`contexts`] with `.key_context(..)`
//! and tracks focus; GPUI matches bindings against the focused element's context stack and
//! the deepest context wins (shell-apis.md § 3). So an overlay (Duel, Proof sheet, a menu)
//! outranks the view under it only while it is focused: it focuses itself on open and gives
//! focus back on close. App-wide keys bind in [`contexts::ROOT`], never with no context — a
//! context-less binding ties with the deepest context and wins on load order.
//!
//! **Adding a key.** Declare the action in the `actions!` list of the module that handles it,
//! add one row to [`bindings`] with its context, and handle it with `.on_action` on the view
//! that sets that context (or `cx.on_action` for app-global ones such as [`Quit`]).

use crate::shell::actions::{ToggleLeftPanel, ToggleRightPanel};
use gpui_kit::{actions, KeyBinding, NoAction};

/// Key-context names, one per surface that owns keys. Ported surfaces use these names so
/// bindings and views agree; the React key handlers they replace are listed per row in
/// `docs/plans/gpui/parity.md`.
pub mod contexts {
    /// The shell's root view: app-wide keys (quit, reload theme, panel toggles).
    pub const ROOT: &str = "ChairPhoto";
    /// The library grid (culling keys, selection).
    pub const LIBRARY: &str = "Library";
    /// The "Stack bursts" dialog over the Library.
    pub const STACK_DIALOG: &str = "StackProposals";
    /// The inline loupe over the grid, and the pop-out loupe window.
    pub const LOUPE: &str = "Loupe";
    /// Compare (duel/grid modes).
    pub const COMPARE: &str = "Compare";
    /// The full-screen, keyboard-only cull session.
    pub const CULL: &str = "Cull";
    /// The Darkroom (develop) stage.
    pub const DARKROOM: &str = "Darkroom";
    /// The Darkroom's Duel overlay.
    pub const DUEL: &str = "Duel";
    /// The Darkroom's Proof sheet overlay.
    pub const PROOF_SHEET: &str = "ProofSheet";
    /// An open dropdown menu: gpui-component's `PopupMenu` context (its own bindings: Escape,
    /// ↑/↓/←/→, Enter). The keymap mutes the shell's keys in it, as React's open menu
    /// swallowed every key but its own.
    pub const MENU: &str = "PopupMenu";
    /// The Tag graph's main view (`modules::tag_graph`).
    pub const TAG_GRAPH: &str = "TagGraph";
    /// A focused text input (gpui-base's `Input` context): typed characters are text, not
    /// shortcuts (React's `INPUT`/`TEXTAREA` guard).
    pub const INPUT: &str = "Input";
}

/// Single-key shell shortcuts that must not fire while a menu or a text input has focus.
/// The Library's culling keys need no entry: they bind in [`contexts::LIBRARY`], which no
/// menu or input sits inside.
const MUTED_IN_MENUS_AND_INPUTS: &[&str] = &["[", "]"];

actions!(
    chairphoto,
    [
        /// Quit the app (clean exit: crash markers cleared).
        Quit,
        /// Re-read the system theme now and apply it.
        ReloadTheme,
    ]
);

/// The keymap. One row per binding: keystroke, action, context.
///
/// The shell's own keys are App.tsx's panel toggles (`[`, `]`; they also work in Compare,
/// and a Darkroom/module/cull context mutes them with `NoAction` when those land) and
/// Menu.tsx's menu keys, which gpui-component's `PopupMenu` binds itself. The rest of
/// App.tsx's window handler — culling, Compare, loupe — binds with its views (#106, #109).
pub fn bindings() -> Vec<KeyBinding> {
    let mut bindings = vec![
        KeyBinding::new("ctrl-q", Quit, Some(contexts::ROOT)),
        KeyBinding::new("ctrl-shift-r", ReloadTheme, Some(contexts::ROOT)),
        KeyBinding::new("[", ToggleLeftPanel, Some(contexts::ROOT)),
        KeyBinding::new("]", ToggleRightPanel, Some(contexts::ROOT)),
    ];
    // The Tag graph's window keydown handler (Escape) moves to its focused view.
    #[cfg(feature = "tag-graph")]
    bindings.push(KeyBinding::new("escape", crate::modules::tag_graph::Back, Some(contexts::TAG_GRAPH)));
    // A deeper `NoAction` outranks the root binding for the same keystroke.
    for key in MUTED_IN_MENUS_AND_INPUTS {
        bindings.push(KeyBinding::new(key, NoAction, Some(contexts::MENU)));
        bindings.push(KeyBinding::new(key, NoAction, Some(contexts::INPUT)));
    }
    bindings.extend(crate::library::bindings());
    bindings
}
