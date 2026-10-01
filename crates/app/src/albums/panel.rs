//! The collection browser's "albums" and "smart albums" sections (`AlbumsPanel.tsx`,
//! `SmartAlbumsPanel.tsx`). Albums: ＋ New album (a name prompt); each row's name toggles the
//! album filter, "+N" adds the selection (only with one), its count, ⚙ Rename, ✕ Delete (a
//! confirm; photos are not deleted, an active filter on it clears); "No albums yet". Smart
//! albums: ＋ opens the rule editor; each row's name toggles its filter, its live count, ⚙
//! Edit rule, ✎ Rename, ✕ Delete; "No smart albums yet". The lists are the shell's.

use crate::shell::state::ShellState;
use crate::shell::style::{grouped, Colors};
use crate::view::RootView;
use chairphoto_core::catalog::{Album, SmartAlbum};
use gpui_kit::prelude::*;
use gpui_kit::{div, px, AnyElement, Context, SharedString, Stateful, Div, TestSupportExt as _};

/// A row's small icon button (`.tag-edit`).
fn icon(id: String, glyph: &'static str, tip: String, colors: Colors) -> Stateful<Div> {
    div()
        .id(SharedString::from(id))
        .flex_none()
        .px(px(2.))
        .text_size(px(11.))
        .text_color(colors.mute)
        .cursor_pointer()
        .hover(|s| s.text_color(colors.txt))
        .child(glyph)
        .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx))
}

fn section_row(id: String, on: bool, colors: Colors) -> Stateful<Div> {
    div()
        .id(SharedString::from(id))
        .flex()
        .items_center()
        .gap(px(6.))
        .h(px(25.))
        .px(px(14.))
        .text_color(if on { colors.txt } else { colors.dim })
        .when(on, |r| r.bg(colors.sel))
        .hover(|s| s.bg(colors.elev))
}

fn new_row(id: &'static str, label: &'static str, colors: Colors) -> Stateful<Div> {
    div()
        .id(id)
        .px(px(14.))
        .h(px(22.))
        .flex()
        .items_center()
        .text_size(px(11.))
        .text_color(colors.mute)
        .cursor_pointer()
        .hover(|s| s.text_color(colors.txt))
        .child(label)
}

fn empty(id: &'static str, text: &'static str, colors: Colors) -> AnyElement {
    div().id(id).px(px(14.)).py(px(4.)).text_size(px(11.)).text_color(colors.mute).child(text).test_support().into_any_element()
}

impl RootView {
    pub(crate) fn render_albums(&self, shell: &ShellState, colors: Colors, cx: &Context<Self>) -> AnyElement {
        let active = shell.library.scope().album_id;
        let selected = shell.library.selection().ids.len();
        let mut list = div().id("albums").flex().flex_col().child(
            new_row("album-new", "＋ New album", colors)
                .on_click(cx.listener(|this, _, window, cx| this.open_new_album(window, cx)))
                .test_support(),
        );
        if shell.lists.albums.is_empty() {
            list = list.child(empty("albums-empty", "No albums yet", colors));
        }
        for album in &shell.lists.albums {
            list = list.child(album_row(album, active == Some(album.id), selected, colors, cx));
        }
        list.into_any_element()
    }

    pub(crate) fn render_smart_albums(&self, shell: &ShellState, colors: Colors, cx: &Context<Self>) -> AnyElement {
        let active = shell.library.scope().smart_album_id;
        let mut list = div().id("smart-albums").flex().flex_col().child(
            new_row("smart-album-new", "＋ New smart album", colors)
                .on_click(cx.listener(|this, _, window, cx| this.open_smart_editor(None, window, cx)))
                .test_support(),
        );
        if shell.lists.smart_albums.is_empty() {
            list = list.child(empty("smart-albums-empty", "No smart albums yet", colors));
        }
        for album in &shell.lists.smart_albums {
            list = list.child(smart_row(album, active == Some(album.id), colors, cx));
        }
        list.into_any_element()
    }
}

fn album_row(album: &Album, on: bool, selected: usize, colors: Colors, cx: &Context<RootView>) -> impl IntoElement {
    let id = album.id;
    let (a1, a2, a3) = (album.clone(), album.clone(), album.clone());
    section_row(format!("album-{id}"), on, colors)
        .child(
            div()
                .id(SharedString::from(format!("album-name-{id}")))
                .flex_1()
                .min_w_0()
                .truncate()
                .cursor_pointer()
                .child(album.name.clone())
                .on_click(cx.listener(move |this, _, _, cx| {
                    let next = if on { None } else { Some(id) };
                    this.shell.update(cx, |s, cx| s.update_scope(cx, |l| l.select_album(next)));
                }))
                .test_support(),
        )
        .when(selected > 0, |r| {
            r.child(
                icon(format!("album-add-{id}"), "", format!("Add {selected} selected photo(s) to {}", a1.name), colors)
                    .child(format!("+{selected}"))
                    .text_color(colors.accent)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.albums.update(cx, |a, cx| a.add_selection(id, cx));
                    }))
                    .test_support(),
            )
        })
        .child(div().flex_none().text_size(px(10.5)).text_color(colors.mute).child(grouped(album.photo_count.max(0) as usize)))
        .child(
            icon(format!("album-rename-{id}"), "⚙", "Rename".into(), colors)
                .on_click(cx.listener(move |this, _, window, cx| this.open_rename_album(&a2, window, cx)))
                .test_support(),
        )
        .child(
            icon(format!("album-delete-{id}"), "✕", "Delete album".into(), colors)
                .on_click(cx.listener(move |this, _, window, cx| this.confirm_delete_album(&a3, window, cx)))
                .test_support(),
        )
        .test_support()
}

fn smart_row(album: &SmartAlbum, on: bool, colors: Colors, cx: &Context<RootView>) -> impl IntoElement {
    let id = album.id;
    let (a1, a2, a3) = (album.clone(), album.clone(), album.clone());
    section_row(format!("smart-album-{id}"), on, colors)
        .child(
            div()
                .id(SharedString::from(format!("smart-album-name-{id}")))
                .flex_1()
                .min_w_0()
                .truncate()
                .cursor_pointer()
                .child(album.name.clone())
                .on_click(cx.listener(move |this, _, _, cx| {
                    let next = if on { None } else { Some(id) };
                    this.shell.update(cx, |s, cx| s.update_scope(cx, |l| l.select_smart_album(next)));
                }))
                .test_support(),
        )
        .child(div().flex_none().text_size(px(10.5)).text_color(colors.mute).child(grouped(album.photo_count.max(0) as usize)))
        .child(
            icon(format!("smart-album-edit-{id}"), "⚙", "Edit rule".into(), colors)
                .on_click(cx.listener(move |this, _, window, cx| this.open_smart_editor(Some(a1.clone()), window, cx)))
                .test_support(),
        )
        .child(
            icon(format!("smart-album-rename-{id}"), "✎", "Rename".into(), colors)
                .on_click(cx.listener(move |this, _, window, cx| this.open_rename_smart_album(&a2, window, cx)))
                .test_support(),
        )
        .child(
            icon(format!("smart-album-delete-{id}"), "✕", "Delete smart album".into(), colors)
                .on_click(cx.listener(move |this, _, window, cx| this.confirm_delete_smart_album(&a3, window, cx)))
                .test_support(),
        )
        .test_support()
}
