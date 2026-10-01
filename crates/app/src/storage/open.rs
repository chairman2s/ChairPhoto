//! The root view's side of Storage and import: the action handlers, and opening each dialog
//! view as a gpui-component `Dialog` (the window's Base `Root` renders the dialog layer).
//! A dialog view asks to close with [`CloseDialog`]; Escape, the overlay and the close button
//! close it as any `Dialog`.

use super::bundle_import::BundleImport;
use super::catalog_switcher::CatalogSwitcher;
use super::identity_debt::IdentityDebtPanel;
use super::import_panel::ImportPanel;
use super::trash::TrashDialog;
use super::volumes::VolumesPanel;
use super::CloseDialog;
use crate::view::RootView;
use gpui_kit::component::WindowExt as _;
use gpui_kit::prelude::*;
use gpui_kit::{px, Context, Entity, EventEmitter, Window};

/// The storage dialog opened last — what a test drives. Replaced by the next one.
#[derive(Clone)]
pub enum StorageDialog {
    Catalogs(Entity<CatalogSwitcher>),
    ImportCard(Entity<ImportPanel>),
    ImportBundle(Entity<BundleImport>),
    IdentityDebt(Entity<IdentityDebtPanel>),
    Trash(Entity<TrashDialog>),
    Volumes(Entity<VolumesPanel>),
}

impl RootView {
    fn show_dialog<V: Render + EventEmitter<CloseDialog>>(
        &mut self,
        title: &'static str,
        width: f32,
        view: Entity<V>,
        handle: StorageDialog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dialog_close = Some(cx.subscribe_in(&view, window, |_, _, _: &CloseDialog, window, cx| {
            window.close_dialog(cx);
        }));
        self.storage.update(cx, |s, _| s.last_dialog = Some(handle));
        window.open_dialog(cx, move |dialog, _, _| dialog.title(title).w(px(width)).child(view.clone()));
    }

    pub(crate) fn open_catalogs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let app = self.model.read(cx).state().clone();
        let view = cx.new(|cx| CatalogSwitcher::new(app, window, cx));
        self.show_dialog("Open catalog", 560., view.clone(), StorageDialog::Catalogs(view), window, cx);
    }

    pub(crate) fn open_import_card(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let storage = self.storage.clone();
        let view = cx.new(|cx| ImportPanel::new(storage, window, cx));
        self.show_dialog("Import from card", 760., view.clone(), StorageDialog::ImportCard(view), window, cx);
    }

    pub(crate) fn open_import_bundle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let storage = self.storage.clone();
        let view = cx.new(|cx| BundleImport::new(storage, window, cx));
        self.show_dialog("Import bundle", 600., view.clone(), StorageDialog::ImportBundle(view), window, cx);
    }

    pub(crate) fn open_identity_debt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let storage = self.storage.clone();
        let view = cx.new(|cx| IdentityDebtPanel::new(storage, cx));
        self.show_dialog("Identity debt", 1100., view.clone(), StorageDialog::IdentityDebt(view), window, cx);
    }

    pub(crate) fn open_trash(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (storage, model, images) = (self.storage.clone(), self.model.clone(), self.images.clone());
        let view = cx.new(|cx| TrashDialog::new(storage, model, images, window, cx));
        self.show_dialog("Trash", 760., view.clone(), StorageDialog::Trash(view), window, cx);
    }

    pub(crate) fn open_volumes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let app = self.model.read(cx).state().clone();
        let view = cx.new(|cx| VolumesPanel::new(app, window, cx));
        self.show_dialog("Storage volumes", 640., view.clone(), StorageDialog::Volumes(view), window, cx);
    }

    /// The bench's Back up: the selection's targets.
    pub(crate) fn back_up_selection(&mut self, cx: &mut Context<Self>) {
        let targets = self.shell.read(cx).library.selection().targets.clone();
        self.storage.update(cx, |s, cx| s.back_up(targets, cx));
    }
}
