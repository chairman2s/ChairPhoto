//! The recovery and housekeeping commands on one photo (App.tsx's `relocatePhotoAction`,
//! `retrieveFromNasAction`, `removeFromCatalogAction`, and the right-click menu's trash and
//! reveal), run by the root view for the grid's menu ([`crate::library::grid_menu`]) and the
//! loupe's "unavailable" state (`RelocatePhoto`, `RetrieveFromNas`, `RemoveFromCatalog`).
//!
//! - **Move to trash** (`Catalog::trash_photos`): hides the photos (and their stacks)
//!   everywhere, reversibly; nothing is deleted and nothing is written to disk. The Trash
//!   dialog restores them or deletes them for good.
//! - **Reveal in Files**: resolves the photo's best reachable copy
//!   (`Catalog::require_photo_path`, never `photos.path`) on the storage runner, and hands it
//!   to the file manager.
//! - **Relocate…**: a file picker, then the core's `storage::relocate_photo` (the file must be
//!   under the library root; its sidecar is bound to the photo's UUID). GPUI's picker takes no
//!   starting folder, so it does not open at the library root as React's did.
//! - **Retrieve from NAS**: the core's `storage::restore_photo_as`, a hash-verified copy back
//!   to the local volume.
//! - **Remove from catalog**: behind a confirm; deletes the catalog row only, never a file.
//!
//! Each command carries the identity of the catalog its id was read from and writes through
//! `with_catalog_as` (or the core's `_as` bodies), so it fails closed with `CATALOG_CHANGED`
//! once another catalog is open — also in the window where the core has switched and
//! `catalog:switched` has not arrived. The Remove confirm binds the identity when it opens,
//! and closes when `catalog:switched` arrives. Every result is a status line; a change
//! re-reads the catalog (rows, counts, trash count), and a recovered file's cached images are
//! dropped so its tile and loupe render again. Work that can block on a mount (sidecar IO, a
//! NAS copy, stat-ing copies for Reveal) runs on the storage [`Runner`]; short catalog writes
//! (trash, remove) on GPUI's background executor.

use super::grid_menu::PhotoCommand;
use crate::storage::{ui, Runner};
use crate::view::RootView;
use chairphoto_core::app::{with_catalog_as, CatalogIdentity};
use gpui_kit::{App, Context, FocusHandle, Global, PathPromptOptions, SharedString, Window};
use std::path::Path;
use std::rc::Rc;

/// Shows a file in the desktop's file manager: `App::reveal_path` in the app; a recorder in
/// tests (GPUI's test platform does not implement it).
#[derive(Clone)]
pub struct SystemRevealer(pub Rc<dyn Fn(&Path, &mut App)>);

impl Global for SystemRevealer {}

fn reveal_path(path: &Path, cx: &mut App) {
    match cx.try_global::<SystemRevealer>().cloned() {
        Some(revealer) => (revealer.0)(path, cx),
        None => cx.reveal_path(path),
    }
}

/// The open Remove confirm, so a catalog switch can close **it** — not whatever dialog is on
/// top (gpui-component's `close_dialog` pops the top one and has no per-dialog handle).
///
/// gpui-component focuses a new dialog's own focus handle as it opens it, and focus stays
/// inside the top dialog until that closes; the handle captured right after opening therefore
/// names this dialog. Closing pops only while focus is inside it, i.e. while it is the top
/// dialog. Once it is answered (and gone) or another dialog lies over it, a switch closes
/// nothing; the confirm's write is bound to its catalog anyway, so a late OK fails closed.
pub struct RemoveConfirm {
    pub(crate) serial: u64,
    dialog: Option<FocusHandle>,
}

impl RemoveConfirm {
    pub(crate) fn close(self, window: &mut Window, cx: &mut App) {
        if self.dialog.is_some_and(|d| d.contains_focused(window, cx)) {
            gpui_kit::component::WindowExt::close_dialog(window, cx);
        }
    }
}

/// React's Remove confirm text.
pub fn remove_confirm_body(name: &str) -> String {
    format!(
        "Remove \"{name}\" from the catalog? This deletes its catalog entry (tags, rating, versions) \
         but never deletes the file on disk or the NAS."
    )
}

impl RootView {
    pub(crate) fn run_photo_command(&mut self, command: &PhotoCommand, window: &mut Window, cx: &mut Context<Self>) {
        match command.clone() {
            PhotoCommand::Trash { ids, from } => self.trash_photos(ids, from, cx),
            PhotoCommand::Reveal { id, from } => self.reveal_photo(id, from, cx),
            PhotoCommand::Relocate { id, from } => self.relocate_photo(id, from, cx),
            PhotoCommand::Retrieve { id, from } => self.retrieve_photo(id, from, cx),
            PhotoCommand::Remove { id, name, from } => self.confirm_remove_photo(id, name, from, window, cx),
        }
    }

    /// The loupe's unavailable-state buttons: the photo the inline loupe shows, bound to the
    /// catalog the rows were read from.
    pub(crate) fn loupe_photo_command(
        &mut self,
        make: impl FnOnce(i64, SharedString, CatalogIdentity) -> PhotoCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shell = self.shell.read(cx);
        let Some(photo) = shell.loupe_target() else { return };
        let (id, name) = (photo.id, crate::loupe::view::file_name(&photo.path));
        let Some(from) = shell.rows_from() else {
            self.photo_status("The photos are still loading; try again.".into(), cx);
            return;
        };
        let command = make(id, name, from);
        self.run_photo_command(&command, window, cx);
    }

    fn photo_status(&self, line: String, cx: &mut App) {
        self.model.update(cx, |m, cx| m.set_status(line, cx));
    }

    /// Something changed in the catalog: re-read the rows, the lists and the counts.
    fn photos_changed(&self, cx: &mut App) {
        self.model.update(cx, |m, cx| m.refresh(cx));
    }

    /// Move to trash: `ids` (the selection, or the clicked photo).
    pub(crate) fn trash_photos(&mut self, ids: Vec<i64>, from: CatalogIdentity, cx: &mut Context<Self>) {
        let state = self.model.read(cx).state().clone();
        let ids_done = ids.clone();
        let run = cx.background_executor().spawn(async move { with_catalog_as(&state, from, |c| c.trash_photos(&ids)) });
        cx.spawn(async move |this, cx| {
            let result = run.await;
            this.update(cx, |this, cx| match result {
                Ok(s) => {
                    // Gone from the library: unselected now, before the refresh lands, so the
                    // next rating or flag key cannot reach them.
                    this.shell.update(cx, |sh, cx| sh.select_with(cx, |l| l.unselect(&ids_done)));
                    let extra = if s.cascaded > 0 { format!(" (+{} stacked)", s.cascaded) } else { String::new() };
                    this.photo_status(format!("Moved {} to the trash{extra}.", s.trashed), cx);
                    this.photos_changed(cx);
                }
                Err(e) => this.photo_status(format!("Could not trash: {e}"), cx),
            })
            .ok();
        })
        .detach();
    }

    /// Reveal in Files: the best reachable copy, resolved on the storage [`Runner`] — it stats
    /// each copy under the catalog lock, which can block on a hung NAS mount, so it must not
    /// park one of GPUI's background executor threads.
    pub(crate) fn reveal_photo(&mut self, id: i64, from: CatalogIdentity, cx: &mut Context<Self>) {
        let state = self.model.read(cx).state().clone();
        let rx = Runner::get(cx).run(move || with_catalog_as(&state, from, |c| c.require_photo_path(id)));
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("the reveal worker stopped".into()));
            this.update(cx, |this, cx| match result {
                Ok(path) => reveal_path(&path, cx),
                Err(e) => this.photo_status(format!("Couldn't reveal: {e} (the file may be offline)"), cx),
            })
            .ok();
        })
        .detach();
    }

    /// Relocate…: pick the moved file, then re-point the photo at it.
    pub(crate) fn relocate_photo(&mut self, id: i64, from: CatalogIdentity, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Relocate".into()),
        });
        let state = self.model.read(cx).state().clone();
        cx.spawn(async move |this, cx| {
            let path = match picked.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                _ => None,
            };
            let Some(path) = path else { return };
            let rx = cx.update(|cx| {
                Runner::get(cx).run(move || chairphoto_core::app::storage::relocate_photo(&state, Some(from), id, &path))
            });
            let result = rx.await.unwrap_or_else(|_| Err("the relocate worker stopped".into()));
            this.update(cx, |this, cx| match result {
                Ok(()) => {
                    this.images.update(cx, |s, cx| s.invalidate(id, cx));
                    this.photos_changed(cx);
                    this.photo_status("Photo relocated to its new file.".into(), cx);
                }
                Err(e) => this.photo_status(format!("Couldn't relocate: {e}"), cx),
            })
            .ok();
        })
        .detach();
    }

    /// Retrieve from NAS: copy the backup back to the local volume, hash-verified.
    pub(crate) fn retrieve_photo(&mut self, id: i64, from: CatalogIdentity, cx: &mut Context<Self>) {
        self.photo_status("Retrieving from NAS…".into(), cx);
        let state = self.model.read(cx).state().clone();
        let rx = Runner::get(cx).run(move || chairphoto_core::app::storage::restore_photo_as(&state, from, id));
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("the retrieve worker stopped".into()));
            this.update(cx, |this, cx| match result {
                Ok(()) => {
                    this.images.update(cx, |s, cx| s.invalidate(id, cx));
                    this.photos_changed(cx);
                    this.photo_status("Retrieved from NAS.".into(), cx);
                }
                Err(e) => this.photo_status(format!("Couldn't retrieve from NAS: {e}"), cx),
            })
            .ok();
        })
        .detach();
    }

    /// Remove from catalog asks first. The confirm is bound to `from` (an OK after a switch
    /// fails closed) and closes when `catalog:switched` arrives (`RootView::new`'s observer).
    pub(crate) fn confirm_remove_photo(
        &mut self,
        id: i64,
        name: SharedString,
        from: CatalogIdentity,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let answer =
            ui::confirm(window, cx, "Remove from catalog".into(), remove_confirm_body(&name).into(), "Remove");
        self.confirm_serial += 1;
        let serial = self.confirm_serial;
        self.remove_confirm = Some(RemoveConfirm { serial, dialog: window.focused(cx) });
        cx.spawn(async move |this, cx| {
            let ok = answer.await == Ok(true);
            this.update(cx, |this, cx| {
                if this.remove_confirm.as_ref().is_some_and(|c| c.serial == serial) {
                    this.remove_confirm = None;
                }
                if ok {
                    this.remove_photo(id, from, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Delete the photo's catalog row (tags, rating, versions); never a file.
    pub(crate) fn remove_photo(&mut self, id: i64, from: CatalogIdentity, cx: &mut Context<Self>) {
        let state = self.model.read(cx).state().clone();
        let run = cx.background_executor().spawn(async move { with_catalog_as(&state, from, |c| c.remove_photo(id)) });
        cx.spawn(async move |this, cx| {
            let result = run.await;
            this.update(cx, |this, cx| match result {
                Ok(()) => {
                    this.shell.update(cx, |sh, cx| sh.select_with(cx, |l| l.unselect(&[id])));
                    this.photos_changed(cx);
                    this.photo_status("Removed from catalog (files left untouched).".into(), cx);
                }
                Err(e) => this.photo_status(format!("Couldn't remove: {e}"), cx),
            })
            .ok();
        })
        .detach();
    }
}
