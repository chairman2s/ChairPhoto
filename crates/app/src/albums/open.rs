//! The root view's side of the albums sections: opening the name prompt and the rule editor
//! as gpui-component `Dialog`s, the delete confirms, and running a submitted name.

use super::prompt::{NamePrompt, PromptFor, Submitted};
use super::smart_editor::SmartAlbumEditor;
use super::state::AlbumDialog;
use crate::storage::{ui, CloseDialog};
use crate::view::RootView;
use chairphoto_core::catalog::{Album, SmartAlbum};
use gpui_kit::component::WindowExt as _;
use gpui_kit::prelude::*;
use gpui_kit::{px, Context, Entity, EventEmitter, Window};

impl RootView {
    /// Open `view` as a dialog; its [`CloseDialog`] closes it. No OK button: Enter in a field
    /// must not reach the Dialog's Confirm (as the storage dialogs).
    pub(crate) fn show_view_dialog<V: Render + EventEmitter<CloseDialog>>(
        &mut self,
        title: &'static str,
        width: f32,
        view: Entity<V>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dialog_close = Some(cx.subscribe_in(&view, window, |_, _, _: &CloseDialog, window, cx| {
            window.close_dialog(cx);
        }));
        window.open_dialog(cx, move |dialog, _, _| dialog.title(title).w(px(width)).child(view.clone()).on_ok(|_, _, _| false));
    }

    fn open_prompt(&mut self, title: &'static str, what: PromptFor, initial: String, window: &mut Window, cx: &mut Context<Self>) {
        let from = self.shell.read(cx).lists_from();
        let albums = self.albums.clone();
        let view = cx.new(|cx| NamePrompt::new(&albums, from, what, initial, window, cx));
        self.album_prompt = Some(cx.subscribe(&view, |this, _, Submitted(what, name): &Submitted, cx| {
            let name = name.clone();
            this.albums.update(cx, |a, cx| match what {
                PromptFor::NewAlbum => a.create_album(name, cx),
                PromptFor::RenameAlbum(id) => a.rename_album(*id, name, cx),
                PromptFor::RenameSmartAlbum(id) => a.rename_smart_album(*id, name, cx),
            });
        }));
        self.albums.update(cx, |a, _| a.last_dialog = Some(AlbumDialog::Prompt(view.downgrade())));
        self.show_view_dialog(title, 420., view, window, cx);
    }

    pub(crate) fn open_new_album(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_prompt("New album name", PromptFor::NewAlbum, String::new(), window, cx);
    }

    pub(crate) fn open_rename_album(&mut self, album: &Album, window: &mut Window, cx: &mut Context<Self>) {
        self.open_prompt("Rename album", PromptFor::RenameAlbum(album.id), album.name.clone(), window, cx);
    }

    pub(crate) fn open_rename_smart_album(&mut self, album: &SmartAlbum, window: &mut Window, cx: &mut Context<Self>) {
        self.open_prompt("Rename smart album", PromptFor::RenameSmartAlbum(album.id), album.name.clone(), window, cx);
    }

    /// ＋ (new) or ⚙ Edit rule: the rule editor, bound to the catalog the list came from.
    pub(crate) fn open_smart_editor(&mut self, album: Option<SmartAlbum>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(from) = self.shell.read(cx).lists_from() else {
            self.model.update(cx, |m, cx| m.set_status("The albums are still loading; try again.", cx));
            return;
        };
        let title = if album.is_some() { "Edit smart album" } else { "New smart album" };
        let albums = self.albums.clone();
        let view = cx.new(|cx| SmartAlbumEditor::new(albums, from, album, window, cx));
        self.albums.update(cx, |a, _| a.last_dialog = Some(AlbumDialog::SmartEditor(view.downgrade())));
        self.show_view_dialog(title, 720., view, window, cx);
    }

    pub(crate) fn confirm_delete_album(&mut self, album: &Album, window: &mut Window, cx: &mut Context<Self>) {
        let (id, body) = (album.id, format!("Delete album \"{}\"? Photos are not deleted.", album.name));
        // The catalog the ✕'s id came from, captured now: an OK after a switch fails closed.
        let from = self.shell.read(cx).lists_from();
        let answer = ui::confirm(window, cx, "Delete album".into(), body.into(), "Delete");
        let albums = self.albums.clone();
        cx.spawn(async move |_, cx| {
            if answer.await == Ok(true) {
                albums.update(cx, |a, cx| a.delete_album(id, from, cx));
            }
        })
        .detach();
    }

    pub(crate) fn confirm_delete_smart_album(&mut self, album: &SmartAlbum, window: &mut Window, cx: &mut Context<Self>) {
        let (id, body) = (album.id, format!("Delete smart album \"{}\"? Photos are not deleted.", album.name));
        let from = self.shell.read(cx).lists_from();
        let answer = ui::confirm(window, cx, "Delete smart album".into(), body.into(), "Delete");
        let albums = self.albums.clone();
        cx.spawn(async move |_, cx| {
            if answer.await == Ok(true) {
                albums.update(cx, |a, cx| a.delete_smart_album(id, from, cx));
            }
        })
        .detach();
    }
}
