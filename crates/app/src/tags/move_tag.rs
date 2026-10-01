//! "Move “name”" (the tag panel's Move to…): a filter field and the tags it may move under —
//! none in its own subtree, not its current parent (`tag_tree::move_candidates`) — plus
//! "↑ Top level" (disabled when it already is). A pick moves it and closes.

use super::state::{bind_dialog, CatalogGuard, TagsState};
use crate::shell::style::Colors;
use crate::storage::{ui, CloseDialog};
use chairphoto_core::catalog::TagWithCount;
use chairphoto_model::tag_tree::move_candidates;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::*;
use gpui_kit::{div, px, Context, Entity, EventEmitter, SharedString, Subscription, TestSupportExt as _, Window};

pub struct TagMove {
    tags: Entity<TagsState>,
    /// The tree this dialog opened over ([`bind_dialog`]): its jobs run under it.
    guard: CatalogGuard,
    pub moving: TagWithCount,
    pub filter: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<CloseDialog> for TagMove {}

impl TagMove {
    pub fn new(tags: Entity<TagsState>, moving: TagWithCount, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter parents…"));
        let changes = cx.subscribe(&filter, |_, _, _: &InputEvent, cx| cx.notify());
        let (guard, bound) = bind_dialog(&tags, cx);
        TagMove { tags, guard, moving, filter, _subscriptions: vec![changes, bound] }
    }

    /// The candidate parents' ids, in list order.
    pub fn candidates(&self, cx: &gpui_kit::App) -> Vec<TagWithCount> {
        let t = self.tags.read(cx);
        move_candidates(&t.tags, &t.index, &self.moving, &self.filter.read(cx).value()).into_iter().cloned().collect()
    }

    pub fn move_to(&mut self, parent: Option<i64>, cx: &mut Context<Self>) {
        if parent != self.moving.tag.parent_id {
            let (id, guard) = (self.moving.tag.id, self.guard);
            self.tags.update(cx, |t, cx| t.move_tag_as(guard, id, parent, cx));
        }
        cx.emit(CloseDialog);
    }
}

impl Render for TagMove {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let candidates = self.candidates(cx);
        let top_enabled = self.moving.tag.parent_id.is_some();
        let option = |id: SharedString, label: String| {
            div()
                .id(id)
                .px(px(8.))
                .py(px(4.))
                .rounded(px(4.))
                .text_size(px(12.))
                .text_color(colors.txt)
                .child(label)
                .hover(|s| s.bg(colors.elev))
                .cursor_pointer()
        };
        let mut list = div().id("tag-move-list").flex().flex_col().max_h(px(320.)).overflow_y_scroll().child(ui::clickable(
            option("tag-move-top".into(), "↑ Top level".into()).when(!top_enabled, |o| o.opacity(0.4)),
            top_enabled,
            cx.listener(|s, _, _, cx| s.move_to(None, cx)),
        ));
        for t in &candidates {
            let id = t.tag.id;
            list = list.child(ui::clickable(
                option(format!("tag-move-to-{id}").into(), t.tag.full_path.clone()),
                true,
                cx.listener(move |s, _, _, cx| s.move_to(Some(id), cx)),
            ));
        }
        if candidates.is_empty() {
            list = list.child(ui::empty("tag-move-none", "No matching parent", colors));
        }
        ui::body()
            .id("tag-move")
            .child(div().text_size(px(13.)).text_color(colors.txt).child(format!("Move “{}”", self.moving.tag.name)))
            .child(ui::sub(format!("Currently: {}", self.moving.tag.full_path), colors))
            .child(Input::new(&self.filter).id("tag-move-filter"))
            .child(list.test_support())
    }
}
