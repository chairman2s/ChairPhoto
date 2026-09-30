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

use gpui_kit::{actions, KeyBinding};

/// Key-context names, one per surface that owns keys. Ported surfaces use these names so
/// bindings and views agree; the React key handlers they replace are listed per row in
/// `docs/plans/gpui/parity.md`.
pub mod contexts {
    /// The shell's root view: app-wide keys (quit, reload theme, panel toggles).
    pub const ROOT: &str = "ChairPhoto";
    /// The library grid (culling keys, selection).
    pub const LIBRARY: &str = "Library";
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
    /// An open dropdown menu (swallows the culling keys while open).
    pub const MENU: &str = "Menu";
}

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
pub fn bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("ctrl-q", Quit, Some(contexts::ROOT)),
        KeyBinding::new("ctrl-shift-r", ReloadTheme, Some(contexts::ROOT)),
    ]
}
