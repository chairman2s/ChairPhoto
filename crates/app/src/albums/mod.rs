//! Albums and smart albums (#115): the collection browser's two sections, the name prompt,
//! the rule editor and the write path behind them. Ports of `AlbumsPanel.tsx`,
//! `SmartAlbumsPanel.tsx` and `SmartAlbumEditor.tsx`; `docs/plans/gpui/parity.md` § Albums
//! and export is the acceptance list. Export is [`crate::export`].

pub mod open;
pub mod panel;
pub mod prompt;
pub mod rule;
pub mod smart_editor;
pub mod state;

pub use state::AlbumsState;
