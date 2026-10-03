//! The grid's right-click menu (App.tsx's `ctxMenu`): a header with the file name and its
//! storage state, then **Move to trash**, **Reveal in Files**, **Relocate…**, **Retrieve from
//! NAS** and **Remove from catalog**.
//!
//! - **What it acts on.** Move to trash takes the selection when the clicked tile is part of
//!   it, else just that tile; the other four always act on the clicked photo. A right-click
//!   on a selected tile keeps the selection (so a multi-selection can be trashed); one on an
//!   unselected tile selects it alone first.
//! - **Enablement.** Retrieve from NAS is disabled, with "No NAS backup to retrieve", unless
//!   the photo's storage state says a backup may exist (backed up, on the NAS, or on the NAS
//!   offline). An unknown state (its badge not read yet) counts as no backup, as in React.
//! - **Catalog identity.** The menu captures the identity of the catalog the rows were read
//!   from when it opens, and every command it sends carries it: the write fails closed
//!   (`CATALOG_CHANGED`) once another catalog is open, even before `catalog:switched` arrives.
//!   The menu closes when the rows change catalog, when the stage leaves the grid, on Escape,
//!   on a click outside it and when a command is chosen.
//!
//! The grid only gathers and sends: the commands run in the root view
//! ([`crate::library::photo_actions`]), which the loupe's unavailable state shares.

use super::grid::LibraryView;
use crate::shell::state::StageView;
use crate::shell::style::Colors;
use chairphoto_core::app::CatalogIdentity;
use chairphoto_core::catalog::StorageStatus;
use gpui_kit::prelude::*;
use gpui_kit::{
    anchored, deferred, div, px, AnyElement, Context, Div, EventEmitter, Pixels, Point, SharedString, Stateful,
    TestSupportExt as _,
};

/// The open menu: what it was opened on, and the catalog that was read from.
#[derive(Debug, Clone)]
pub struct GridMenu {
    /// The clicked photo.
    pub photo: i64,
    /// Its file name, for the header and the Remove confirm.
    pub name: SharedString,
    /// What Move to trash takes: the selection when the clicked photo is in it, else the photo.
    pub trash: Vec<i64>,
    /// The catalog the rows (and so every id above) were read from.
    pub from: CatalogIdentity,
    /// Where the pointer was, in window coordinates.
    pub position: Point<Pixels>,
}

/// One of the menu's commands, sent to the root view with the catalog it is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhotoCommand {
    Trash { ids: Vec<i64>, from: CatalogIdentity },
    Reveal { id: i64, from: CatalogIdentity },
    Relocate { id: i64, from: CatalogIdentity },
    Retrieve { id: i64, from: CatalogIdentity },
    Remove { id: i64, name: SharedString, from: CatalogIdentity },
}

impl EventEmitter<PhotoCommand> for LibraryView {}

/// React's `storageLabel`: the header's storage line.
pub fn storage_label(status: Option<StorageStatus>) -> &'static str {
    match status {
        Some(StorageStatus::LocalOnly) => "On local disk",
        Some(StorageStatus::BackedUp) => "Local + NAS backup",
        Some(StorageStatus::Archived) => "On NAS",
        Some(StorageStatus::Offline) => "On NAS (offline)",
        Some(StorageStatus::Missing) => "Missing — no copy found",
        None => "",
    }
}

/// Whether a NAS backup may exist to retrieve. The status is derived from volume kinds, so a
/// photo whose local copy was deleted still reads "backed up": Retrieve stays enabled for it.
pub fn can_retrieve(status: Option<StorageStatus>) -> bool {
    matches!(status, Some(StorageStatus::BackedUp | StorageStatus::Archived | StorageStatus::Offline))
}

impl LibraryView {
    /// Right-click on a tile: select it unless it is already part of the selection, and open
    /// the menu on it, bound to the catalog the rows came from.
    pub(crate) fn open_menu(&mut self, id: i64, position: Point<Pixels>, cx: &mut Context<Self>) {
        let in_selection = self.shell.read(cx).library.selection().ids.contains(&id);
        if !in_selection {
            self.shell.update(cx, |s, cx| {
                s.select_with(cx, |l| l.select(id, chairphoto_model::library::session::SelectMods::default()))
            });
        }
        let shell = self.shell.read(cx);
        let Some(from) = shell.rows_from() else {
            self.menu = None;
            return;
        };
        let ids = shell.library.selection().ids;
        let trash = if ids.contains(&id) { ids.to_vec() } else { vec![id] };
        let name = shell
            .library
            .photos()
            .iter()
            .find(|p| p.id == id)
            .map(|p| crate::loupe::view::file_name(&p.path))
            .unwrap_or_else(|| format!("photo {id}").into());
        self.menu = Some(GridMenu { photo: id, name, trash, from, position });
        cx.notify();
    }

    pub fn menu(&self) -> Option<&GridMenu> {
        self.menu.as_ref()
    }

    pub(crate) fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            cx.notify();
        }
    }

    /// The shell changed: a menu over rows read from another catalog, or over a grid no longer
    /// on the stage, closes.
    pub(crate) fn check_menu(&mut self, cx: &mut Context<Self>) {
        let Some(menu) = &self.menu else { return };
        let shell = self.shell.read(cx);
        let gone = shell.rows_from() != Some(menu.from)
            || shell.surface != crate::shell::state::Surface::Library
            || shell.stage_view() != StageView::Grid;
        if gone {
            self.menu = None;
        }
    }

    /// Close the menu and send `command` to the root view.
    fn choose(&mut self, command: PhotoCommand, cx: &mut Context<Self>) {
        self.close_menu(cx);
        cx.emit(command);
    }

    pub(crate) fn render_menu(&self, colors: Colors, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.menu.clone()?;
        let status = self.shell.read(cx).library.statuses().get(&menu.photo).copied();
        let retrieve = can_retrieve(status);
        let (id, from) = (menu.photo, menu.from);
        let item = |el_id: &'static str, label: &'static str, enabled: bool, danger: bool| -> Stateful<Div> {
            div()
                .id(el_id)
                .flex()
                .items_center()
                .h(px(28.))
                .px(px(12.))
                .text_size(px(12.5))
                .text_color(if danger { colors.danger } else { colors.txt })
                .child(label)
                .when(enabled, |d| d.cursor_pointer().hover(|s| s.bg(colors.sel)))
                .when(!enabled, |d| d.opacity(0.4))
        };
        let trash_ids = menu.trash.clone();
        let name = menu.name.clone();
        let trash_label = if menu.trash.len() > 1 { "Move selection to trash" } else { "Move to trash" };
        let body = div()
            .id("grid-menu")
            .occlude()
            .flex()
            .flex_col()
            .min_w(px(220.))
            .max_w(px(320.))
            .py(px(4.))
            .bg(colors.elev)
            .border_1()
            .border_color(colors.border)
            .rounded(px(8.))
            .shadow_lg()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_menu(cx)))
            .child(
                div()
                    .id("grid-menu-header")
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .px(px(12.))
                    .py(px(6.))
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(colors.txt)
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(menu.name.clone()),
                    )
                    .when(status.is_some(), |d| {
                        d.child(
                            div()
                                .id("grid-menu-status")
                                .text_size(px(11.))
                                .text_color(colors.mute)
                                .child(storage_label(status))
                                .aria_label(storage_label(status))
                                .test_support(),
                        )
                    })
                    .aria_label(menu.name.clone())
                    .test_support(),
            )
            .child(
                item("grid-menu-trash", trash_label, true, false)
                    .tooltip(crate::shell::title_bar::tooltip(
                        "Hide it everywhere, reversibly. Nothing is deleted and nothing is written to disk.",
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.choose(PhotoCommand::Trash { ids: trash_ids.clone(), from }, cx)
                    }))
                    .aria_label(trash_label)
                    .test_support(),
            )
            .child(
                item("grid-menu-reveal", "Reveal in Files", true, false)
                    .on_click(cx.listener(move |this, _, _, cx| this.choose(PhotoCommand::Reveal { id, from }, cx)))
                    .test_support(),
            )
            .child(
                item("grid-menu-relocate", "Relocate…", true, false)
                    .on_click(cx.listener(move |this, _, _, cx| this.choose(PhotoCommand::Relocate { id, from }, cx)))
                    .test_support(),
            )
            .child({
                let el = item("grid-menu-retrieve", "Retrieve from NAS", retrieve, false);
                let el = if retrieve {
                    el.on_click(cx.listener(move |this, _, _, cx| this.choose(PhotoCommand::Retrieve { id, from }, cx)))
                } else {
                    el.tooltip(crate::shell::title_bar::tooltip("No NAS backup to retrieve"))
                };
                el.test_support()
            })
            .child(div().h(px(1.)).my(px(4.)).bg(colors.border))
            .child(
                item("grid-menu-remove", "Remove from catalog", true, true)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.choose(PhotoCommand::Remove { id, name: name.clone(), from }, cx)
                    }))
                    .test_support(),
            )
            .test_support();
        Some(
            deferred(anchored().position(menu.position).snap_to_window_with_margin(px(8.)).child(body))
                .with_priority(1)
                .into_any_element(),
        )
    }
}
