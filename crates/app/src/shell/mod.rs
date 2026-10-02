//! The shell chrome (#105): the title bar and its menus, the command pill, the bench, the
//! icon rail, the collection browser and the inspector column, around the main-area slot the
//! Library, Loupe and Darkroom views fill. Ports of `src/components/shell/*` and the
//! sidebar parts of `src/App.tsx`; `docs/plans/gpui/parity.md` § Shell is the acceptance list.
//!
//! [`state::ShellState`] holds what the chrome shows and edits; each component is a
//! `RootView::render_*` method in its own file; [`actions`] are what the chrome dispatches.

pub mod actions;
pub mod bench;
pub mod command_pill;
pub mod inspector;
pub mod layout_prefs;
pub mod sidebar;
pub mod state;
pub mod style;
pub mod timing;
pub mod title_bar;

pub use state::ShellState;

#[cfg(test)]
mod tests;
