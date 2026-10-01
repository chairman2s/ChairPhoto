//! The tag panel (`TagPanel.tsx`): the collection browser's tags section.
//!
//! - **Header:** ＋ New tags…, and Expand/Collapse all while any tag has children.
//! - **Search tags…:** the top eight matches with their counts, deepest first
//!   (`tag_tree::search`); ↑/↓ move the highlight, Enter picks it, Escape clears. A pick
//!   filters the Library to the tag, expands its collapsed ancestors and scrolls its row
//!   into view.
//! - **All photos** clears the tag filter. **Rows:** twisty, name, 🔒 private, `auto`,
//!   count, ⚙ edit; a click filters the Library to the tag and its descendants
//!   (`LibrarySession::select_tag`).
//! - **Drag-and-drop** reparents a tag: GPUI `on_drag` on each row, `on_drop` on each row and
//!   on All photos (= top level). Only a valid target lights up (`TagIndex::can_drop`); a
//!   drop onto the tag's own subtree is refused with the backend's reason, as React's failed
//!   `move_tag` reported it.
//! - **Context menu** (right-click): Move to…, Move to top level, Make private/public (and
//!   incl. sub-tags), Merge into…, Split off N selected…, Edit…, New child tags…. Escape or
//!   a click outside closes it (gpui-component's `ContextMenu`). Outcomes go to the status
//!   line.

use super::state::{TagDialog, TagsState};
use super::{create, editor, merge, move_tag, split};
use crate::modules::ModuleRegistry;
use crate::shell::style::Colors;
use crate::shell::ShellState;
use chairphoto_core::catalog::TagWithCount;
use chairphoto_model::tag_tree::{self, ancestor_prefix};
use gpui_kit::component::input::{Escape, Input, InputEvent, InputState, MoveDown, MoveUp};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::Sizable as _;
use gpui_kit::prelude::*;
use gpui_kit::{
    div, px, AnyElement, App, Context, Entity, ScrollAnchor, ScrollHandle, SharedString, Subscription,
    TestSupportExt as _, Window,
};
use std::collections::HashSet;

/// What a row carries while it is dragged.
#[derive(Debug, Clone, PartialEq)]
pub struct DraggedTag {
    pub id: i64,
    pub name: SharedString,
}

/// The drag preview: the tag's name in a chip.
struct DragChip(SharedString);

impl Render for DragChip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        div()
            .px(px(8.))
            .py(px(3.))
            .rounded(px(6.))
            .bg(colors.elev)
            .border_1()
            .border_color(colors.accent)
            .text_size(px(12.))
            .text_color(colors.txt)
            .child(self.0.clone())
    }
}

pub struct TagPanel {
    tags: Entity<TagsState>,
    shell: Entity<ShellState>,
    modules: Entity<ModuleRegistry>,
    pub search: Entity<InputState>,
    /// The highlighted search match.
    pub highlight: usize,
    /// Collapsed parents: their descendants are hidden.
    pub collapsed: HashSet<i64>,
    /// The collection browser's scroll handle (the browser tracks it), so a search pick can
    /// scroll its row into view.
    pub scroll: ScrollHandle,
    /// A row to scroll to on the next render (after its ancestors expanded).
    scroll_to: Option<i64>,
    /// A pick clears the search box on the next render (where a `Window` is at hand).
    clear_search_on_next_render: bool,
    dialog_close: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl TagPanel {
    pub fn new(
        tags: Entity<TagsState>,
        shell: Entity<ShellState>,
        modules: Entity<ModuleRegistry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search tags…"));
        let input_events = cx.subscribe(&search, |this: &mut Self, _, event: &InputEvent, cx| match event {
            InputEvent::Change => {
                this.highlight = 0;
                cx.notify();
            }
            InputEvent::PressEnter { .. } => this.pick_highlighted(cx),
            _ => {}
        });
        let observe_tags = cx.observe(&tags, |_, _, cx| cx.notify());
        let observe_shell = cx.observe(&shell, |_, _, cx| cx.notify());
        TagPanel {
            tags,
            shell,
            modules,
            search,
            highlight: 0,
            collapsed: HashSet::new(),
            scroll: ScrollHandle::new(),
            scroll_to: None,
            clear_search_on_next_render: false,
            dialog_close: None,
            _subscriptions: vec![input_events, observe_tags, observe_shell],
        }
    }

    /// The current search matches.
    pub fn matches(&self, cx: &App) -> Vec<TagWithCount> {
        let query = self.search.read(cx).value();
        tag_tree::search(&self.tags.read(cx).tags, &query, &HashSet::new()).into_iter().cloned().collect()
    }

    pub fn move_highlight(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        let n = self.matches(cx).len();
        if n == 0 {
            return false;
        }
        self.highlight = (self.highlight.min(n - 1) as isize + delta).clamp(0, n as isize - 1) as usize;
        cx.notify();
        true
    }

    fn pick_highlighted(&mut self, cx: &mut Context<Self>) {
        let matches = self.matches(cx);
        if let Some(tag) = matches.get(self.highlight.min(matches.len().saturating_sub(1))) {
            self.pick(tag.tag.id, cx);
        }
    }

    /// A search pick: filter to the tag, expand its ancestors, scroll its row into view and
    /// clear the search.
    pub fn pick(&mut self, tag_id: i64, cx: &mut Context<Self>) {
        let ancestors = self.tags.read(cx).index.ancestors(tag_id);
        for a in ancestors {
            self.collapsed.remove(&a);
        }
        self.select(Some(tag_id), cx);
        self.scroll_to = Some(tag_id);
        self.highlight = 0;
        self.clear_search_on_next_render = true;
        cx.notify();
    }

    /// Filter the Library to a tag (and its descendants), or clear the tag filter.
    pub fn select(&mut self, tag_id: Option<i64>, cx: &mut Context<Self>) {
        self.shell.update(cx, |s, cx| s.update_scope(cx, |l| l.select_tag(tag_id)));
    }

    pub fn toggle_collapse(&mut self, id: i64, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&id) {
            self.collapsed.insert(id);
        }
        cx.notify();
    }

    /// Expand/Collapse all: collapse every parent while none is collapsed, else expand all.
    pub fn toggle_all(&mut self, cx: &mut Context<Self>) {
        if self.collapsed.is_empty() {
            self.collapsed = self.tags.read(cx).index.parent_ids();
        } else {
            self.collapsed.clear();
        }
        cx.notify();
    }

    /// A drop of `dragged` onto `target` (`None` = All photos, the top level).
    pub fn drop_tag(&mut self, dragged: i64, target: Option<i64>, cx: &mut Context<Self>) {
        let index = self.tags.read(cx).index.clone();
        if Some(dragged) == target || !index.can_drop(dragged, target) {
            if target.is_some_and(|t| index.subtree(dragged).contains(&t)) && Some(dragged) != target {
                self.tags.update(cx, |t, cx| {
                    t.set_status("Move failed: cannot move a tag into itself or one of its descendants", cx)
                });
            }
            return;
        }
        self.tags.update(cx, |t, cx| t.move_tag(dragged, target, cx));
    }

    // --- dialogs -----------------------------------------------------------------------------

    fn remember(&mut self, dialog: TagDialog, cx: &mut Context<Self>) {
        self.tags.update(cx, |t, _| t.last_dialog = Some(dialog));
    }

    pub fn open_editor(&mut self, tag: TagWithCount, window: &mut Window, cx: &mut Context<Self>) {
        let (tags, shell, modules) = (self.tags.clone(), self.shell.clone(), self.modules.clone());
        let view = cx.new(|cx| editor::TagEditor::new(tags, shell, Some(modules), tag, window, cx));
        self.remember(TagDialog::Editor(view.downgrade()), cx);
        self.dialog_close = Some(super::open_dialog("Edit tag", 620., view, window, cx));
    }

    pub fn open_create(&mut self, parent_path: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let tags = self.tags.clone();
        let view = cx.new(|cx| create::TagCreate::new(tags, parent_path, window, cx));
        self.remember(TagDialog::Create(view.downgrade()), cx);
        self.dialog_close = Some(super::open_dialog("New tags", 560., view, window, cx));
    }

    pub fn open_move(&mut self, tag: TagWithCount, window: &mut Window, cx: &mut Context<Self>) {
        let tags = self.tags.clone();
        let view = cx.new(|cx| move_tag::TagMove::new(tags, tag, window, cx));
        self.remember(TagDialog::Move(view.downgrade()), cx);
        self.dialog_close = Some(super::open_dialog("Move tag", 520., view, window, cx));
    }

    pub fn open_merge(&mut self, tag: TagWithCount, window: &mut Window, cx: &mut Context<Self>) {
        let tags = self.tags.clone();
        let view = cx.new(|cx| merge::TagMerge::new(tags, tag, window, cx));
        self.remember(TagDialog::Merge(view.downgrade()), cx);
        self.dialog_close = Some(super::open_dialog("Merge tag", 620., view, window, cx));
    }

    pub fn open_split(&mut self, tag: TagWithCount, window: &mut Window, cx: &mut Context<Self>) {
        let photo_ids = self.shell.read(cx).library.selection().ids.to_vec();
        let tags = self.tags.clone();
        let view = cx.new(|cx| split::TagSplit::new(tags, tag, photo_ids, window, cx));
        self.remember(TagDialog::Split(view.downgrade()), cx);
        self.dialog_close = Some(super::open_dialog("Split tag", 620., view, window, cx));
    }

    /// The row's context menu.
    fn context_menu(
        this: &gpui_kit::WeakEntity<Self>,
        tag: &TagWithCount,
        menu: PopupMenu,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        let Some(panel) = this.upgrade() else { return menu };
        let has_children = panel.read(cx).tags.read(cx).index.has_children(tag.tag.id);
        let selected = panel.read(cx).shell.read(cx).library.selection().ids.len();
        let id = tag.tag.id;
        let private = tag.tag.private;
        let item = |label: String, f: Box<dyn Fn(&mut TagPanel, &mut Window, &mut Context<TagPanel>)>| {
            let panel = panel.clone();
            PopupMenuItem::new(label).on_click(move |_, window, cx| panel.update(cx, |p, cx| f(p, window, cx)))
        };
        let t = tag.clone();
        let mut menu = menu
            .label(tag.tag.name.clone())
            .item(item("Move to…".into(), Box::new(move |p, w, cx| p.open_move(t.clone(), w, cx))))
            .item(
                item("Move to top level".into(), Box::new(move |p, _, cx| p.tags.update(cx, |t, cx| t.move_tag(id, None, cx))))
                    .disabled(tag.tag.parent_id.is_none()),
            )
            .separator()
            .item(item(
                if private { "Make public (cloud AI)" } else { "Make private (hide from cloud AI)" }.into(),
                Box::new(move |p, _, cx| p.tags.update(cx, |t, cx| t.set_private(id, !private, false, cx))),
            ));
        if has_children {
            menu = menu
                .item(item(
                    "Make private incl. sub-tags".into(),
                    Box::new(move |p, _, cx| p.tags.update(cx, |t, cx| t.set_private(id, true, true, cx))),
                ))
                .item(item(
                    "Make public incl. sub-tags".into(),
                    Box::new(move |p, _, cx| p.tags.update(cx, |t, cx| t.set_private(id, false, true, cx))),
                ));
        }
        let t = tag.clone();
        menu = menu.separator().item(item("Merge into…".into(), Box::new(move |p, w, cx| p.open_merge(t.clone(), w, cx))));
        if selected > 0 {
            let t = tag.clone();
            menu = menu.item(item(
                format!("Split off {selected} selected…"),
                Box::new(move |p, w, cx| p.open_split(t.clone(), w, cx)),
            ));
        }
        let (t, path) = (tag.clone(), tag.tag.full_path.clone());
        menu.separator()
            .item(item("Edit…".into(), Box::new(move |p, w, cx| p.open_editor(t.clone(), w, cx))))
            .item(item("New child tags…".into(), Box::new(move |p, w, cx| p.open_create(Some(path.clone()), w, cx))))
    }

    fn row(&self, tag: &TagWithCount, active: Option<i64>, colors: Colors, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let index = self.tags.read(cx).index.clone();
        let id = tag.tag.id;
        let depth = tag_tree::depth(&tag.tag.full_path) - 1;
        let has_children = index.has_children(id);
        let collapsed = self.collapsed.contains(&id);
        let on = active == Some(id);
        let weak = cx.entity().downgrade();
        let menu_tag = tag.clone();
        let edit_tag = tag.clone();
        let anchor = (self.scroll_to == Some(id)).then(|| ScrollAnchor::for_handle(self.scroll.clone()));
        if let Some(anchor) = &anchor {
            anchor.scroll_to(window, cx);
        }
        let drop_index = index.clone();
        div()
            .id(SharedString::from(format!("tag-row-{id}")))
            .flex()
            .items_center()
            .gap(px(4.))
            .w_full()
            .h(px(25.))
            .pl(px(12. + depth as f32 * 14.))
            .pr(px(10.))
            .text_color(if on { colors.txt } else { colors.dim })
            .when(on, |r| r.bg(colors.sel))
            .hover(|s| s.bg(colors.elev))
            .anchor_scroll(anchor)
            .child(if has_children {
                div()
                    .id(SharedString::from(format!("tag-twisty-{id}")))
                    .flex_none()
                    .w(px(12.))
                    .text_size(px(9.))
                    .text_color(colors.mute)
                    .cursor_pointer()
                    .child(if collapsed { "▸" } else { "▾" })
                    .on_click(cx.listener(move |p, _, _, cx| {
                        cx.stop_propagation();
                        p.toggle_collapse(id, cx)
                    }))
                    .test_support()
                    .into_any_element()
            } else {
                div().flex_none().w(px(12.)).into_any_element()
            })
            .child(
                div()
                    .id(SharedString::from(format!("tag-filter-{id}")))
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap(px(5.))
                    .cursor_pointer()
                    .child(div().truncate().child(tag.tag.name.clone()))
                    .when(tag.tag.private, |r| {
                        r.child(div().flex_none().text_size(px(10.)).child("🔒")).tooltip(super::tip(
                            "Private — hidden from external/cloud AI (local AI still uses it)",
                        ))
                    })
                    .when_some(tag.tag.auto_rule.clone(), |r, rule| {
                        r.child(
                            div()
                                .id(SharedString::from(format!("tag-auto-{id}")))
                                .flex_none()
                                .px(px(4.))
                                .rounded(px(4.))
                                .border_1()
                                .border_color(colors.border)
                                .text_size(px(9.5))
                                .text_color(colors.mute)
                                .child("auto")
                                .tooltip(super::tip(format!("auto-tag ({rule})"))),
                        )
                    })
                    .on_click(cx.listener(move |p, _, _, cx| p.select(Some(id), cx)))
                    .test_support(),
            )
            .child(div().flex_none().text_size(px(10.5)).text_color(colors.mute).child(tag_tree::grouped(tag.photo_count.max(0) as usize)))
            .child(
                div()
                    .id(SharedString::from(format!("tag-edit-{id}")))
                    .flex_none()
                    .px(px(3.))
                    .text_size(px(11.))
                    .text_color(colors.mute)
                    .cursor_pointer()
                    .hover(|s| s.text_color(colors.txt))
                    .child("⚙")
                    .tooltip(super::tip("Edit translations & synonyms"))
                    .on_click(cx.listener(move |p, _, window, cx| p.open_editor(edit_tag.clone(), window, cx)))
                    .test_support(),
            )
            .on_drag(DraggedTag { id, name: tag.tag.name.clone().into() }, |d, _, _, cx| cx.new(|_| DragChip(d.name.clone())))
            .drag_over::<DraggedTag>(move |s, d, _, _| {
                if drop_index.can_drop(d.id, Some(id)) {
                    s.bg(colors.sel).border_1().border_color(colors.accent)
                } else {
                    s
                }
            })
            .on_drop(cx.listener(move |p, d: &DraggedTag, _, cx| p.drop_tag(d.id, Some(id), cx)))
            .test_support()
            .context_menu(move |menu, _, cx| Self::context_menu(&weak, &menu_tag, menu, cx))
            .into_any_element()
    }
}

impl Render for TagPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.clear_search_on_next_render) {
            self.search.update(cx, |i, cx| i.set_value("", window, cx));
        }
        let colors = Colors::get(cx);
        let active = self.shell.read(cx).library.scope().tag_id;
        let all_on = active.is_none();
        let (tags, index, loaded) = {
            let t = self.tags.read(cx);
            (t.tags.clone(), t.index.clone(), t.loaded)
        };
        let query = self.search.read(cx).value();
        let matches = self.matches(cx);
        let highlight = self.highlight.min(matches.len().saturating_sub(1));

        let header = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .px(px(14.))
            .pb(px(4.))
            .child(div().flex_1())
            .child(
                div()
                    .id("tag-new")
                    .px(px(4.))
                    .cursor_pointer()
                    .text_color(colors.mute)
                    .hover(|s| s.text_color(colors.txt))
                    .child("＋")
                    .tooltip(super::tip("New tags…"))
                    .on_click(cx.listener(|p, _, window, cx| p.open_create(None, window, cx)))
                    .test_support(),
            )
            .when(!index.parent_ids().is_empty(), |h| {
                let all_collapsed = !self.collapsed.is_empty();
                h.child(
                    div()
                        .id("tag-collapse-all")
                        .px(px(4.))
                        .cursor_pointer()
                        .text_color(colors.mute)
                        .hover(|s| s.text_color(colors.txt))
                        .child(if all_collapsed { "⊕" } else { "⊖" })
                        .tooltip(super::tip(if all_collapsed { "Expand all" } else { "Collapse all" }))
                        .on_click(cx.listener(|p, _, _, cx| p.toggle_all(cx)))
                        .test_support(),
                )
            });

        let search = div()
            .id("tag-search-box")
            .px(px(12.))
            .pb(px(4.))
            // The Input binds ↑/↓/Escape itself; while there are matches, the panel takes them
            // first (capture phase), as React's keydown handler did.
            .capture_action(cx.listener(|p, _: &MoveDown, _, cx| {
                if p.move_highlight(1, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|p, _: &MoveUp, _, cx| {
                if p.move_highlight(-1, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|p, _: &Escape, window, cx| {
                if !p.search.read(cx).value().is_empty() {
                    p.search.update(cx, |i, cx| i.set_value("", window, cx));
                    p.highlight = 0;
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .child(Input::new(&self.search).id("tag-search").small())
            .when(!matches.is_empty(), |s| {
                s.child(div().id("tag-search-hits").flex().flex_col().mt(px(3.)).children(matches.iter().enumerate().map(|(i, t)| {
                    let id = t.tag.id;
                    div()
                        .id(SharedString::from(format!("tag-search-hit-{i}")))
                        .flex()
                        .items_center()
                        .gap(px(2.))
                        .px(px(6.))
                        .h(px(22.))
                        .rounded(px(4.))
                        .cursor_pointer()
                        .text_size(px(11.5))
                        .when(i == highlight, |r| r.bg(colors.sel))
                        .child(div().text_color(colors.mute).truncate().child(ancestor_prefix(&t.tag.full_path, &t.tag.name)))
                        .child(div().text_color(colors.txt).child(t.tag.name.clone()))
                        .child(div().ml_auto().text_size(px(10.)).text_color(colors.mute).child(t.photo_count.to_string()))
                        .on_hover(cx.listener(move |p, hovered: &bool, _, cx| {
                            if *hovered {
                                p.highlight = i;
                                cx.notify();
                            }
                        }))
                        .on_click(cx.listener(move |p, _, _, cx| p.pick(id, cx)))
                        .test_support()
                })))
            })
            .when(!query.trim().is_empty() && matches.is_empty(), |s| {
                s.child(div().id("tag-search-none").py(px(4.)).text_size(px(11.)).text_color(colors.mute).child("No matching tag").test_support())
            });

        let all = div()
            .id("tag-all")
            .flex()
            .items_center()
            .w_full()
            .h(px(25.))
            .px(px(14.))
            .cursor_pointer()
            .text_color(if all_on { colors.txt } else { colors.dim })
            .when(all_on, |r| r.bg(colors.sel))
            .hover(|s| s.bg(colors.elev))
            .child("All photos")
            .drag_over::<DraggedTag>({
                let index = index.clone();
                move |s, d, _, _| if index.can_drop(d.id, None) { s.border_1().border_color(colors.accent) } else { s }
            })
            .on_drop(cx.listener(|p, d: &DraggedTag, _, cx| p.drop_tag(d.id, None, cx)))
            .on_click(cx.listener(|p, _, _, cx| p.select(None, cx)))
            .test_support();

        let mut rows: Vec<AnyElement> = Vec::new();
        for tag in &tags {
            if index.is_hidden(tag.tag.id, &self.collapsed) {
                continue;
            }
            rows.push(self.row(tag, active, colors, window, cx));
        }
        self.scroll_to = None;

        div()
            .id("tag-panel")
            .flex()
            .flex_col()
            .w_full()
            .text_size(px(12.5))
            .child(header)
            .child(search)
            .child(all)
            .children(rows)
            .when(loaded && tags.is_empty(), |p| {
                p.child(div().id("tag-empty").px(px(14.)).py(px(4.)).text_size(px(11.)).text_color(colors.mute).child("No tags yet").test_support())
            })
    }
}
