//! Storage and import (#114): the catalog switcher and recent catalogs, Import ▾ (card,
//! bundle, rescan), the back-up drain with reconcile on launch and on window focus, the
//! identity-debt panel with its repair job, the Trash dialog, the volumes panel and the
//! import-batches section. Ports of `CatalogSwitcher.tsx`, `ImportPanel.tsx`,
//! `BundleImportDialog.tsx`, `IdentityDebtPanel.tsx`, `TrashDialog.tsx`, `VolumesPanel.tsx`,
//! `BatchesPanel.tsx` and the matching parts of `App.tsx`; `docs/plans/gpui/parity.md` §
//! Storage and import is the acceptance list.
//!
//! Every read and job goes through the core (`chairphoto_core::app::{catalogs, scans,
//! bundles, identity, storage}`), on a [`runner::Runner`] — never the UI thread.
//! [`state::StorageState`] owns the background jobs and decides whose result lands; each
//! dialog is its own view entity, opened as a gpui-component `Dialog` by the root view
//! ([`open`]).

pub mod batches;
pub mod bundle_import;
pub mod catalog_switcher;
pub mod identity_debt;
pub mod import_panel;
pub mod open;
pub mod runner;
pub mod state;
pub mod trash;
pub mod ui;
pub mod volumes;

pub use runner::Runner;
pub use state::StorageState;

use gpui_kit::{App, Global};
use std::path::PathBuf;

/// Where the recent-catalogs registry lives. Absent: the app data dir. Tests point it at a
/// scratch directory so a switch never writes the user's `recent_catalogs.json`.
#[derive(Clone, Default)]
pub struct RecentRegistry(pub Option<PathBuf>);

impl Global for RecentRegistry {}

impl RecentRegistry {
    pub fn get(cx: &App) -> Option<PathBuf> {
        cx.try_global::<RecentRegistry>().and_then(|r| r.0.clone())
    }
}

/// A dialog view asks its opener to close it (after a switch, Close, Escape).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloseDialog;
