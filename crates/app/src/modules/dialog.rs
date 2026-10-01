//! What the action dialogs of the Slideshow and Collage modules share: the selection they
//! snapshot when they open (with the catalog its ids were read from), opening themselves at
//! their own width, and closing on a catalog switch.
//!
//! **Catalog identity** (#92's standing rule). A dialog's photo ids are the Library's, read
//! under [`ShellState::rows_from`]; the dialog keeps that identity and every backend call it
//! makes is bound to it (core fails closed with `CATALOG_CHANGED` once another catalog is
//! open, even before `catalog:switched` reaches the UI). When the event does arrive the dialog
//! closes ([`close_on_switch`]).

use crate::image_store::ImageStore;
use crate::model::{AppModel, AppModelEvent};
use crate::shell::ShellState;
use chairphoto_core::app::{AppState, CatalogIdentity, CoreEvent};
use gpui_kit::component::WindowExt as _;
use gpui_kit::prelude::*;
use gpui_kit::{px, App, Context, Entity, SharedString, Subscription, Window};

/// The app entities a dialog works with.
#[derive(Clone)]
pub struct DialogHost {
    pub app: AppState,
    pub model: Entity<AppModel>,
    pub shell: Entity<ShellState>,
    pub images: Option<Entity<ImageStore>>,
}

impl DialogHost {
    pub fn new(model: &Entity<AppModel>, shell: &Entity<ShellState>, images: Option<Entity<ImageStore>>, cx: &App) -> Self {
        DialogHost { app: model.read(cx).state().clone(), model: model.clone(), shell: shell.clone(), images }
    }

    /// Put `line` on the status line (React's `showToast`).
    pub fn status(&self, line: impl Into<SharedString>, cx: &mut App) {
        let line = line.into();
        self.model.update(cx, |m, cx| m.set_status(line, cx));
    }
}

/// One photo of the snapshot: its id and file name (the tile's title).
#[derive(Debug, Clone, PartialEq)]
pub struct Picked {
    pub id: i64,
    pub name: SharedString,
}

/// The selection when the dialog opened (`api.getSelectedPhotos()`, snapshotted once so the
/// dialog's order and layout stay stable), in grid order, and the catalog its ids came from.
#[derive(Debug, Clone, Default)]
pub struct SelectionSnapshot {
    pub photos: Vec<Picked>,
    /// `None` when no rows have been read: every backend call then refuses.
    pub catalog: Option<CatalogIdentity>,
}

impl SelectionSnapshot {
    pub fn take(shell: &Entity<ShellState>, cx: &App) -> Self {
        let shell = shell.read(cx);
        let sel = shell.library.selection();
        let name = |path: &str| SharedString::from(path.rsplit('/').next().unwrap_or(path).to_string());
        let photos = if sel.photos.len() == sel.targets.len() {
            sel.photos.iter().map(|p| Picked { id: p.id, name: name(&p.path) }).collect()
        } else {
            // Some targets are not on the loaded page: keep the selection's order, names
            // where the page has them.
            sel.targets
                .iter()
                .map(|&id| Picked {
                    id,
                    name: sel.photos.iter().find(|p| p.id == id).map(|p| name(&p.path)).unwrap_or_default(),
                })
                .collect()
        };
        SelectionSnapshot { photos, catalog: shell.rows_from() }
    }

    pub fn ids(&self) -> Vec<i64> {
        self.photos.iter().map(|p| p.id).collect()
    }
}

/// What a dialog answers when it has no catalog to bind its ids to.
pub const NO_ROWS: &str = "The library has not been read yet; select photos and open this again.";

/// Open `view` as a gpui-component dialog `width` wide. `overlay_closable`: whether a click on
/// the backdrop closes it (Slideshow yes; Collage no — a drag ending outside the canvas must
/// not discard the layout).
pub fn open<V: Render>(
    title: &'static str,
    width: f32,
    overlay_closable: bool,
    view: Entity<V>,
    window: &mut Window,
    cx: &mut App,
) {
    window.open_dialog(cx, move |dialog, _, _| {
        dialog.title(title).w(px(width)).overlay_closable(overlay_closable).child(view.clone())
    });
}

/// Close the dialog showing this view when another catalog is announced: its ids, layout
/// and output choices were the old catalog's.
pub fn close_on_switch<V: 'static>(model: &Entity<AppModel>, window: &mut Window, cx: &mut Context<V>) -> Subscription {
    cx.subscribe_in(model, window, |_, _, event: &AppModelEvent, window, cx| {
        if matches!(event, AppModelEvent::Core(CoreEvent::CatalogSwitched(_))) {
            window.close_dialog(cx);
        }
    })
}

/// Browse… for a folder (the portal picker), then `set` it. A picker failure is reported
/// through `fail`.
pub fn pick_folder<V: 'static>(
    prompt: &'static str,
    window: &mut Window,
    cx: &mut Context<V>,
    set: impl FnOnce(&mut V, String, &mut Window, &mut Context<V>) + 'static,
    fail: impl FnOnce(&mut V, String, &mut Context<V>) + 'static,
) {
    let rx = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some(prompt.into()),
    });
    cx.spawn_in(window, async move |this, cx| match rx.await {
        Ok(Ok(Some(paths))) => {
            if let Some(path) = paths.into_iter().next() {
                this.update_in(cx, |v, window, cx| set(v, path.to_string_lossy().to_string(), window, cx)).ok();
            }
        }
        Ok(Err(e)) => {
            this.update(cx, |v, cx| fail(v, format!("Couldn't open the folder picker: {e}"), cx)).ok();
        }
        _ => {}
    })
    .detach();
}
