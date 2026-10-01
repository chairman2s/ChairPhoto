//! The Trash (`TrashDialog.tsx`): "N photos, most recently trashed first", a selectable
//! thumbnail grid, **Restore** (the selection, or all; no confirmation — trashing touched no
//! bytes) and **Delete permanently…**, the one action in the app with no undo, which asks for
//! the word `delete` typed (Enter deletes only then; Escape cancels the confirmation).
//!
//! Delete is the core's `storage::empty_trash` with `confirm: true` on the [`Runner`]: every
//! known copy must be reachable, or the photo is left alone and reported. The report names
//! what was deleted, what was skipped for an unreachable disk, what was restored meanwhile,
//! a run that stopped early, and the failures. Restore trips the trash generation first, so a
//! delete already walking the filesystem stands down. A result that comes back after a
//! catalog switch is dropped: its photo ids name another catalog's photos.
//!
//! **Catalog switches.** The list is read together with its catalog's identity, and Restore
//! and Delete send that identity with the ids (`restore_trashed_as`, `empty_trash_as`), so the
//! core refuses them once another catalog is open — even when the switch has happened but
//! `catalog:switched` has not reached this dialog yet. When the event does arrive the dialog
//! drops the old list, selection and confirmation and reads the new catalog's trash.

use super::ui;
use super::state::StorageEvent;
use super::{CloseDialog, Runner, StorageState};
use crate::image_store::{ImageState, ImageStore};
use crate::model::AppModel;
use crate::shell::style::Colors;
use chairphoto_core::app::storage::EmptyTrashReport;
use chairphoto_core::app::{with_catalog_identified, AppState, CatalogIdentity};
use chairphoto_core::catalog::Photo;
use chairphoto_core::image_pool::ImageKind;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::*;
use gpui_kit::TestSupportExt as _;
use gpui_kit::{div, img, px, Context, Entity, EventEmitter, ObjectFit, SharedString, Subscription, Window};

/// What must be typed to confirm.
pub const CONFIRM_WORD: &str = "delete";

pub struct TrashDialog {
    app: AppState,
    storage: Entity<StorageState>,
    model: Entity<AppModel>,
    images: Entity<ImageStore>,
    pub photos: Option<Vec<Photo>>,
    /// The catalog `photos` (and so every id in `selected`) was read from.
    pub loaded_from: Option<CatalogIdentity>,
    /// In click order.
    pub selected: Vec<i64>,
    pub error: Option<String>,
    pub confirming: bool,
    pub typed: Entity<InputState>,
    pub report: Option<EmptyTrashReport>,
    pub busy: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<CloseDialog> for TrashDialog {}

impl TrashDialog {
    pub fn new(
        storage: Entity<StorageState>,
        model: Entity<AppModel>,
        images: Entity<ImageStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let app = storage.read(cx).app_state().clone();
        let typed = cx.new(|cx| InputState::new(window, cx));
        let enter = cx.subscribe(&typed, |this: &mut Self, input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) && input.read(cx).value() == CONFIRM_WORD {
                this.destroy(cx);
            }
        });
        // A switch: the list names another catalog's photos. Drop it, and everything
        // chosen from it, and read the new catalog's trash.
        let switched = cx.subscribe(&storage, |this: &mut Self, _, event: &StorageEvent, cx| {
            if let StorageEvent::CatalogSwitched = event {
                this.photos = None;
                this.loaded_from = None;
                this.selected.clear();
                this.confirming = false;
                this.report = None;
                this.error = None;
                this.reload(cx);
                cx.notify();
            }
        });
        let mut this = TrashDialog {
            app,
            storage,
            model,
            images,
            photos: None,
            loaded_from: None,
            selected: Vec::new(),
            error: None,
            confirming: false,
            typed,
            report: None,
            busy: false,
            _subscriptions: vec![enter, switched],
        };
        this.reload(cx);
        this
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let state = self.app.clone();
        let epoch = self.storage.read(cx).epoch();
        let rx = Runner::get(cx).run(move || with_catalog_identified(&state, |c| c.list_trash()));
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else { return };
            this.update(cx, |s, cx| {
                if s.storage.read(cx).epoch() != epoch {
                    return;
                }
                match result {
                    Ok((from, photos)) => {
                        s.photos = Some(photos);
                        s.loaded_from = Some(from);
                        s.selected.clear();
                    }
                    Err(e) => s.error = Some(e),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn toggle(&mut self, id: i64, cx: &mut Context<Self>) {
        match self.selected.iter().position(|&s| s == id) {
            Some(i) => {
                self.selected.remove(i);
            }
            None => self.selected.push(id),
        }
        cx.notify();
    }

    /// The selection, or every photo in the trash.
    pub fn targets(&self) -> Vec<i64> {
        if self.selected.is_empty() {
            self.photos.iter().flatten().map(|p| p.id).collect()
        } else {
            self.selected.clone()
        }
    }

    /// Something left the trash: the counts and the grid re-read.
    fn changed(&self, cx: &mut Context<Self>) {
        self.model.update(cx, |m, cx| m.refresh(cx));
    }

    pub fn restore(&mut self, cx: &mut Context<Self>) {
        let Some(from) = self.loaded_from else { return };
        if self.busy {
            return;
        }
        self.busy = true;
        let ids = self.targets();
        let state = self.app.clone();
        let epoch = self.storage.read(cx).epoch();
        let rx =
            Runner::get(cx).run(move || chairphoto_core::app::storage::restore_trashed_as(&state, from, &ids));
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("the restore worker stopped".into()));
            this.update(cx, |s, cx| {
                s.busy = false;
                if s.storage.read(cx).epoch() != epoch {
                    return;
                }
                if let Err(e) = result {
                    s.error = Some(e);
                }
                s.reload(cx);
                s.changed(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    pub fn start_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.confirming = true;
        self.typed.update(cx, |i, cx| i.set_value("", window, cx));
        let focus = gpui_kit::Focusable::focus_handle(self.typed.read(cx), cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// The dialog's Cancel (Escape, the close button): while the typed confirmation is open
    /// it cancels only that, as React's input keydown did, and keeps the dialog. Returns
    /// whether the dialog may close.
    pub fn on_dialog_cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.confirming {
            self.cancel_confirm(window, cx);
            return false;
        }
        true
    }

    pub fn cancel_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.confirming = false;
        self.typed.update(cx, |i, cx| i.set_value("", window, cx));
        cx.notify();
    }

    /// Delete the targets permanently. Only with the confirmation word typed.
    pub fn destroy(&mut self, cx: &mut Context<Self>) {
        if self.busy || !self.confirming || self.typed.read(cx).value() != CONFIRM_WORD {
            return;
        }
        let Some(from) = self.loaded_from else { return };
        self.busy = true;
        let ids = self.targets();
        let state = self.app.clone();
        let epoch = self.storage.read(cx).epoch();
        let rx = Runner::get(cx)
            .run(move || chairphoto_core::app::storage::empty_trash_as(&state, Some(from), Some(ids), None, true));
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("the delete worker stopped".into()));
            this.update(cx, |s, cx| {
                s.busy = false;
                if s.storage.read(cx).epoch() != epoch {
                    return; // another catalog's report
                }
                match result {
                    Ok(report) => {
                        s.report = Some(report);
                        s.confirming = false;
                    }
                    Err(e) => s.error = Some(e),
                }
                s.reload(cx);
                s.changed(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }
}

/// The report, in React's words.
pub fn report_lines(r: &EmptyTrashReport) -> Vec<String> {
    let mut lines = vec![format!(
        "Deleted {} and {}.",
        ui::plural(r.deleted, "photo", "photos"),
        ui::plural(r.files_deleted, "file", "files")
    )];
    if !r.skipped_unreachable.is_empty() {
        lines.push(format!(
            "{} left alone — a disk holding a copy could not be reached. Nothing was deleted for those: \
             removing the copies we can see would leave one behind that nothing points at. Reconnect the disk \
             and try again.",
            r.skipped_unreachable.len()
        ));
    }
    if !r.restored_meanwhile.is_empty() {
        lines.push(format!("{} were restored while this was running and were left alone.", r.restored_meanwhile.len()));
    }
    if r.aborted {
        lines.push(
            "Stopped early. Something else took over — a restore, or the library being switched — so the rest \
             of the trash was not touched."
                .into(),
        );
    }
    if !r.failed.is_empty() {
        lines.push(format!(
            "{} could not be fully deleted. Those photos keep their place in the trash so you can retry — a \
             file we could not remove is recoverable, one with no catalog entry is not.",
            r.failed.len()
        ));
        lines.extend(r.failed.iter().map(|(_, why)| format!("• {why}")));
    }
    lines
}

impl Render for TrashDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let mut body = ui::body().id("trash-dialog");
        if let Some(e) = &self.error {
            body = body.child(ui::error("trash-error", e.clone(), colors));
        }
        if let Some(r) = &self.report {
            body = body.child(
                div()
                    .id("trash-report")
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .children(report_lines(r).into_iter().map(|l| ui::sub(l, colors)))
                    .test_support(),
            );
        }
        let Some(photos) = &self.photos else {
            return body.child(ui::empty("trash-loading", "Loading…", colors));
        };
        let count = photos.len();
        if count == 0 {
            return body.child(ui::empty(
                "trash-empty",
                "The trash is empty. Photos you trash are hidden everywhere but keep every tag, rating and edit until you delete them here.",
                colors,
            ));
        }
        let ids: Vec<i64> = photos.iter().map(|p| p.id).collect();
        let thumbs: Vec<ImageState> = self.images.update(cx, |store, _| {
            let wanted: Vec<_> = ids.iter().map(|&id| (id, ImageKind::Thumb)).collect();
            store.request_batch(&wanted);
            ids.iter().map(|&id| store.get(id, ImageKind::Thumb)).collect()
        });
        let n_sel = self.selected.len();
        let which = if n_sel > 0 { n_sel.to_string() } else { "all".to_string() };
        let acting = if n_sel > 0 { n_sel } else { count };
        let busy = self.busy;
        body = body
            .child(ui::sub(
                format!(
                    "{}, most recently trashed first. Nothing here has been changed on disk — trashing hides, it does not delete.",
                    ui::plural(count, "photo", "photos")
                ),
                colors,
            ))
            .child(
                div()
                    .id("trash-grid")
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap(px(6.))
                    .max_h(px(340.))
                    .overflow_y_scroll()
                    .children(photos.iter().zip(thumbs).map(|(p, thumb)| {
                        let id = p.id;
                        let sel = self.selected.contains(&id);
                        let name = p.path.rsplit('/').next().unwrap_or(&p.path).to_string();
                        let tile = div()
                            .id(SharedString::from(format!("trash-{id}")))
                            .relative()
                            .w(px(112.))
                            .h(px(92.))
                            .rounded(px(6.))
                            .overflow_hidden()
                            .bg(colors.well)
                            .border_2()
                            .border_color(if sel { colors.accent } else { colors.border })
                            .cursor_pointer()
                            .child(match thumb {
                                ImageState::Ready(l) => img(l.image).size_full().object_fit(ObjectFit::Cover).into_any_element(),
                                _ => div().size_full().into_any_element(),
                            })
                            .child(
                                div()
                                    .absolute()
                                    .bottom_0()
                                    .left_0()
                                    .right_0()
                                    .px(px(4.))
                                    .bg(colors.scrim)
                                    .text_size(px(10.))
                                    .truncate()
                                    .child(name),
                            );
                        ui::clickable(tile, true, cx.listener(move |s, _, _, cx| s.toggle(id, cx)))
                    })),
            );
        let mut actions = ui::row().child(ui::clickable(
            ui::primary("trash-restore", format!("Restore {which}"), !busy, colors),
            !busy,
            cx.listener(|s, _, _, cx| s.restore(cx)),
        ));
        if !self.confirming {
            actions = actions.child(ui::clickable(
                ui::danger_chip("trash-delete", format!("Delete {which} permanently…"), !busy, colors),
                !busy,
                cx.listener(|s, _, window, cx| s.start_confirm(window, cx)),
            ));
        } else {
            let ready = self.typed.read(cx).value() == CONFIRM_WORD && !busy;
            actions = actions.child(
                div()
                    .id("trash-confirm")
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap(px(6.))
                    .child(ui::sub(
                        format!(
                            "This destroys {} and every copy of {}. Type {CONFIRM_WORD} to confirm:",
                            ui::plural(acting, "photo", "photos"),
                            if acting == 1 { "it" } else { "them" }
                        ),
                        colors,
                    ))
                    .child(div().w(px(120.)).child(Input::new(&self.typed).id("trash-typed")))
                    .child(ui::clickable(
                        ui::danger_chip("trash-confirm-delete", "Delete", ready, colors),
                        ready,
                        cx.listener(|s, _, _, cx| s.destroy(cx)),
                    ))
                    .child(ui::clickable(
                        ui::chip("trash-confirm-cancel", "Cancel", true, colors),
                        true,
                        cx.listener(|s, _, window, cx| s.cancel_confirm(window, cx)),
                    ))
                    .test_support(),
            );
        }
        body.child(actions).child(ui::sub(
            "Deleting removes every copy ChairPhoto can reach, and its sidecars with it. A photo whose copies are not \
             all reachable is skipped rather than partly deleted.",
            colors,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_says_what_did_not_happen_too() {
        let r = EmptyTrashReport {
            deleted: 1,
            files_deleted: 3,
            skipped_unreachable: vec![7, 8],
            failed: vec![(9, "/x/a.ARW could not be removed".into())],
            restored_meanwhile: vec![],
            aborted: true,
        };
        let lines = report_lines(&r);
        assert_eq!(lines[0], "Deleted 1 photo and 3 files.");
        assert!(lines[1].starts_with("2 left alone"));
        assert!(lines[2].starts_with("Stopped early."));
        assert!(lines[3].starts_with("1 could not be fully deleted."));
        assert_eq!(lines[4], "• /x/a.ARW could not be removed");
    }
}
