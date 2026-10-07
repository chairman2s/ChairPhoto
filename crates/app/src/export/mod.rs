//! Export (#115): the Export dialog (photos to a folder, with the reach-hashtag bundle and its
//! clipboard Copy) and the `.chairphoto` bundle export, both owned background jobs
//! ([`state::ExportState`]) over the core's `app::exports`. Ports of `ExportPanel.tsx` and
//! `BundleExportDialog.tsx`; `docs/plans/gpui/parity.md` § Albums and export is the
//! acceptance list.
//!
//! Export is one-way: it writes copies to the destination and never touches the originals or
//! the catalog (docs/storage-and-import.md).

pub mod bundle;
pub mod open;
pub mod panel;
pub mod state;

pub use state::ExportState;
