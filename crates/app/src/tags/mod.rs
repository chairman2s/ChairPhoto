//! Tags (#107): the collection browser's tag panel, the tag editor and the tag-maintenance
//! dialogs. Ports of `TagPanel.tsx`, `TagEditor.tsx`, `TagCreateModal.tsx`,
//! `TagMergeModal.tsx`, `TagSplitModal.tsx` and the tag wiring of `App.tsx`; `docs/plans/gpui/parity.md` § Tag
//! panel is the acceptance list, `docs/taxonomy.md` the model.
//!
//! - [`state::TagsState`] owns the tag tree and the tag clipboard, and every write goes
//!   through [`state::run`] — off the UI thread, fenced against a catalog switch.
//! - [`panel::TagPanel`]: the tree with search, collapse, drag-to-reparent (GPUI
//!   `on_drag`/`on_drop`), the context menu, and Library tag filtering (the shell's
//!   `LibrarySession::select_tag`).
//! - [`editor::TagEditor`]: one tag's name, description, export gate, translations, synonyms
//!   and export preview, plus the `tag-editor` module slot.
//! - The dialogs: [`create`], [`move_tag`], [`merge`], [`split`].
//!
//! The tree logic (search ranking, visibility, drop and move targets, report wording) is the
//! sans-IO `chairphoto_model::tag_tree`; pasted hierarchies are `chairphoto_model::tag_paste`.

pub mod create;
pub mod editor;
pub mod merge;
pub mod move_tag;
pub mod panel;
pub mod split;
pub mod state;

pub use state::TagsState;

use crate::storage::CloseDialog;
use gpui_kit::component::WindowExt as _;
use gpui_kit::prelude::*;
use gpui_kit::{px, Context, Entity, EventEmitter, Subscription, Window};

/// Open `view` as a gpui-component `Dialog` (the window's Base `Root` renders the layer).
/// The view closes it by emitting [`CloseDialog`]; Escape, the overlay and the close button
/// close it as any `Dialog`. Enter in a field must not reach a Confirm button, so there is
/// none. The returned subscription is the close request's listener: the opener holds it.
pub fn open_dialog<O: 'static, V: Render + EventEmitter<CloseDialog>>(
    title: &'static str,
    width: f32,
    view: Entity<V>,
    window: &mut Window,
    cx: &mut Context<O>,
) -> Subscription {
    let close = cx.subscribe_in(&view, window, |_, _, _: &CloseDialog, window, cx| window.close_dialog(cx));
    window.open_dialog(cx, move |dialog, _, _| dialog.title(title).w(px(width)).child(view.clone()).on_ok(|_, _, _| false));
    close
}

/// A small text toggle (`☑ label` / `☐ label`) — React's checkbox rows. Findable in tests
/// by `id`.
pub fn toggle(
    id: impl Into<gpui_kit::SharedString>,
    on: bool,
    label: impl Into<gpui_kit::SharedString>,
    colors: crate::shell::style::Colors,
    f: impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut gpui_kit::App) + 'static,
) -> gpui_kit::AnyElement {
    use gpui_kit::TestSupportExt as _;
    gpui_kit::div()
        .id(id.into())
        .flex()
        .flex_none()
        .items_center()
        .gap(px(5.))
        .cursor_pointer()
        .text_size(px(11.5))
        .text_color(colors.dim)
        .child(if on { "☑" } else { "☐" })
        .child(label.into())
        .on_click(f)
        .test_support()
        .into_any_element()
}

/// A tooltip with owned text (the shell's `title_bar::tooltip` takes `&'static str`).
pub fn tip(text: impl Into<gpui_kit::SharedString>) -> impl Fn(&mut Window, &mut gpui_kit::App) -> gpui_kit::AnyView + 'static {
    let text = text.into();
    move |window, cx| gpui_kit::component::tooltip::Tooltip::new(text.clone()).build(window, cx)
}
