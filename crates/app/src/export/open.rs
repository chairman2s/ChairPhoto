//! The root view's side of export: the title bar's and the bench's Export, the bundle export
//! from Import ▾ → "Export a bundle", the import-batches section's ⬇, and the Export dialog's
//! "Export as bundle…".

use super::bundle::BundleExport;
use super::panel::{ActiveVersion, ExportBatch, ExportPanel};
use crate::view::RootView;
use chairphoto_core::catalog::ImportBatch;
use gpui_kit::prelude::*;
use gpui_kit::component::WindowExt as _;
use gpui_kit::{Context, WeakEntity, Window};

/// The export dialog opened last — what a test drives.
#[derive(Clone)]
pub enum ExportDialog {
    Photos(WeakEntity<ExportPanel>),
    Bundle(WeakEntity<BundleExport>),
}

impl RootView {
    /// Export: the selection's targets (else nothing — the button is disabled then), bound to
    /// the catalog the rows came from; Show off renders the inspector's active version.
    pub(crate) fn open_export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (targets, from, version, batch) = {
            let s = self.shell.read(cx);
            let version = s.active_version().map(|v| ActiveVersion { id: v.id, name: v.name.clone() });
            (s.library.selection().targets.clone(), s.rows_from(), version, s.library.scope().batch.clone())
        };
        let Some(from) = from.filter(|_| !targets.is_empty()) else {
            self.model.update(cx, |m, cx| m.set_status("Select photos to export.", cx));
            return;
        };
        let (albums, exports) = (self.albums.clone(), self.exports.clone());
        let view = cx.new(|cx| ExportPanel::new(albums, exports, from, targets, version, batch, window, cx));
        self.export_batch = Some(cx.subscribe_in(&view, window, |_, _, ExportBatch(batch): &ExportBatch, window, cx| {
            let batch = batch.clone();
            window.close_dialog(cx);
            cx.defer_in(window, move |this, window, cx| this.open_bundle_export(batch, window, cx));
        }));
        self.exports.update(cx, |e, _| e.last_dialog = Some(ExportDialog::Photos(view.downgrade())));
        self.show_view_dialog("Export", 560., view, window, cx);
    }

    /// The bundle export for `batch`, bound to the catalog the batch list came from.
    pub(crate) fn open_bundle_export(&mut self, batch: ImportBatch, window: &mut Window, cx: &mut Context<Self>) {
        let Some(from) = self.shell.read(cx).lists_from() else {
            self.model.update(cx, |m, cx| m.set_status("The import batches are still loading; try again.", cx));
            return;
        };
        let (albums, exports) = (self.albums.clone(), self.exports.clone());
        let view = cx.new(|cx| BundleExport::new(&albums, exports, from, batch, window, cx));
        self.exports.update(cx, |e, _| e.last_dialog = Some(ExportDialog::Bundle(view.downgrade())));
        self.show_view_dialog("Export batch as bundle", 560., view, window, cx);
    }
}
