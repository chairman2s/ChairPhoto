//! "Tag groups" (`TagGroupsManager.tsx`): the quick-tag groups' manager. Group chips (pick
//! one), a new-group field (Enter adds and selects it); for the active group its member chips
//! (× removes), "delete group" (no confirmation, as React), an add-member field taking a path
//! that is created if new (Enter adds, `app::tags::add_tag_to_group`), and a rename field saved
//! when it loses focus. Every write bumps the tag state's revision, so the quick-tag block
//! re-reads (React refetched on close).

use super::state::{run, TagsState};
use crate::shell::style::Colors;
use crate::storage::{ui, CloseDialog};
use chairphoto_core::catalog::{Tag, TagGroup};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::*;
use gpui_kit::{div, px, Context, Entity, EventEmitter, FontWeight, Subscription, TestSupportExt as _, Window};

pub struct TagGroupsManager {
    tags: Entity<TagsState>,
    pub groups: Vec<TagGroup>,
    pub active: Option<i64>,
    pub members: Vec<Tag>,
    pub new_group: Entity<InputState>,
    pub new_member: Entity<InputState>,
    pub rename: Entity<InputState>,
    /// The group the rename field was last filled for.
    rename_for: Option<i64>,
    pub error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<CloseDialog> for TagGroupsManager {}

impl TagGroupsManager {
    pub fn new(tags: Entity<TagsState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let new_group = cx.new(|cx| InputState::new(window, cx).placeholder("New group name (e.g. Street photo)"));
        let new_member = cx.new(|cx| InputState::new(window, cx).placeholder("Add tag (path, created if new — e.g. Street/Candid)"));
        let rename = cx.new(|cx| InputState::new(window, cx));
        let subscriptions = vec![
            cx.subscribe_in(&new_group, window, |s: &mut Self, _, e: &InputEvent, window, cx| {
                if matches!(e, InputEvent::PressEnter { .. }) {
                    s.add_group(window, cx);
                }
            }),
            cx.subscribe_in(&new_member, window, |s: &mut Self, _, e: &InputEvent, window, cx| {
                if matches!(e, InputEvent::PressEnter { .. }) {
                    s.add_member(window, cx);
                }
            }),
            cx.subscribe(&rename, |s: &mut Self, _, e: &InputEvent, cx| {
                if matches!(e, InputEvent::Blur) {
                    s.rename_active(cx);
                }
            }),
        ];
        let mut this = TagGroupsManager {
            tags,
            groups: Vec::new(),
            active: None,
            members: Vec::new(),
            new_group,
            new_member,
            rename,
            rename_for: None,
            error: None,
            _subscriptions: subscriptions,
        };
        this.reload_groups(None, cx);
        this
    }

    /// Re-read the groups; keep `keep` (or the current group) active if it still exists, else
    /// the first.
    pub fn reload_groups(&mut self, keep: Option<Option<i64>>, cx: &mut Context<Self>) {
        run(&self.tags, cx, false, |c| c.list_tag_groups(), move |s: &mut Self, r, cx| {
            match r {
                Ok(groups) => {
                    let want = keep.unwrap_or(s.active);
                    s.active = want.filter(|w| groups.iter().any(|g| g.id == *w)).or(groups.first().map(|g| g.id));
                    s.groups = groups;
                    s.reload_members(cx);
                }
                Err(e) => s.error = Some(e),
            }
            cx.notify();
        });
    }

    pub fn reload_members(&mut self, cx: &mut Context<Self>) {
        let Some(group) = self.active else {
            self.members.clear();
            return;
        };
        run(&self.tags, cx, false, move |c| c.group_members(group), move |s: &mut Self, r, cx| {
            if s.active == Some(group) {
                s.members = r.unwrap_or_default();
                cx.notify();
            }
        });
    }

    pub fn select(&mut self, group: i64, cx: &mut Context<Self>) {
        self.active = Some(group);
        self.members.clear();
        self.reload_members(cx);
        cx.notify();
    }

    pub fn add_group(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.new_group.read(cx).value().trim().to_string();
        if name.is_empty() {
            return;
        }
        self.new_group.update(cx, |i, cx| i.set_value("", window, cx));
        run(&self.tags, cx, true, move |c| c.create_tag_group(&name), |s: &mut Self, r, cx| match r {
            Ok(id) => s.reload_groups(Some(Some(id)), cx),
            Err(e) => {
                s.error = Some(e);
                cx.notify();
            }
        });
    }

    pub fn add_member(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.new_member.read(cx).value().trim().to_string();
        let Some(group) = self.active else { return };
        if path.is_empty() {
            return;
        }
        self.new_member.update(cx, |i, cx| i.set_value("", window, cx));
        run(&self.tags, cx, true, move |c| chairphoto_core::app::tags::add_tag_to_group(c, group, &path), |s: &mut Self, r, cx| {
            if let Err(e) = r {
                s.error = Some(e);
            }
            s.reload_members(cx);
            cx.notify();
        });
    }

    pub fn remove_member(&mut self, tag_id: i64, cx: &mut Context<Self>) {
        let Some(group) = self.active else { return };
        run(&self.tags, cx, true, move |c| c.remove_tag_from_group(group, tag_id), |s: &mut Self, r, cx| {
            if let Err(e) = r {
                s.error = Some(e);
            }
            s.reload_members(cx);
        });
    }

    pub fn delete_active(&mut self, cx: &mut Context<Self>) {
        let Some(group) = self.active else { return };
        run(&self.tags, cx, true, move |c| c.delete_tag_group(group), |s: &mut Self, r, cx| {
            if let Err(e) = r {
                s.error = Some(e);
            }
            s.reload_groups(Some(None), cx);
        });
    }

    pub fn rename_active(&mut self, cx: &mut Context<Self>) {
        let Some(group) = self.active else { return };
        let name = self.rename.read(cx).value().trim().to_string();
        let current = self.groups.iter().find(|g| g.id == group).map(|g| g.name.clone());
        if name.is_empty() || current.as_deref() == Some(name.as_str()) {
            return;
        }
        run(&self.tags, cx, true, move |c| c.rename_tag_group(group, &name), |s: &mut Self, r, cx| {
            if let Err(e) = r {
                s.error = Some(e);
            }
            s.reload_groups(None, cx);
        });
    }
}

impl Render for TagGroupsManager {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        // The rename field shows the active group's name (React's `defaultValue`).
        if self.rename_for != self.active {
            self.rename_for = self.active;
            let name = self.active.and_then(|a| self.groups.iter().find(|g| g.id == a)).map(|g| g.name.clone()).unwrap_or_default();
            self.rename.update(cx, |i, cx| i.set_value(name, window, cx));
        }
        let heading = |t: String| div().text_size(px(10.)).font_weight(FontWeight::BOLD).text_color(colors.mute).child(t.to_uppercase());
        let mut chips = ui::row().id("tag-groups-list");
        for g in &self.groups {
            let id = g.id;
            let chip = ui::chip(format!("tag-group-{id}"), g.name.clone(), true, colors).when(self.active == Some(id), |c| c.bg(colors.sel).text_color(colors.txt));
            chips = chips.child(ui::clickable(chip, true, cx.listener(move |s, _, _, cx| s.select(id, cx))));
        }
        if self.groups.is_empty() {
            chips = chips.child(ui::sub("No groups yet", colors));
        }
        let mut body = ui::body()
            .id("tag-groups")
            .children(self.error.clone().map(|e| ui::error("tag-groups-error", e, colors)))
            .child(heading("Groups".into()))
            .child(chips.test_support())
            .child(
                ui::row()
                    .child(div().flex_1().child(Input::new(&self.new_group).id("tag-groups-new")))
                    .child(ui::clickable(ui::chip("tag-groups-add", "Add group", true, colors), true, cx.listener(|s, _, w, cx| s.add_group(w, cx)))),
            );
        if let Some(active) = self.active {
            let name = self.groups.iter().find(|g| g.id == active).map(|g| g.name.clone()).unwrap_or_default();
            let mut members = ui::row().id("tag-groups-members");
            for t in &self.members {
                let id = t.id;
                members = members.child(
                    ui::row()
                        .gap(px(3.))
                        .px(px(7.))
                        .py(px(2.))
                        .rounded(px(6.))
                        .bg(colors.elev)
                        .text_size(px(11.5))
                        .child(t.full_path.clone())
                        .child(ui::clickable(ui::chip(format!("tag-groups-remove-{id}"), "×", true, colors), true, cx.listener(move |s, _, _, cx| s.remove_member(id, cx)))),
                );
            }
            if self.members.is_empty() {
                members = members.child(ui::sub("No tags in this group", colors));
            }
            body = body
                .child(
                    ui::row()
                        .child(heading(format!("Tags in “{name}”")))
                        .child(ui::clickable(ui::danger_chip("tag-groups-delete", "delete group", true, colors), true, cx.listener(|s, _, _, cx| s.delete_active(cx)))),
                )
                .child(members.test_support())
                .child(
                    ui::row()
                        .child(div().flex_1().child(Input::new(&self.new_member).id("tag-groups-member")))
                        .child(ui::clickable(ui::chip("tag-groups-member-add", "+", true, colors), true, cx.listener(|s, _, w, cx| s.add_member(w, cx)))),
                )
                .child(ui::row().child(ui::sub("Rename:", colors)).child(div().w(px(220.)).child(Input::new(&self.rename).id("tag-groups-rename"))));
        }
        body.child(ui::row().child(ui::clickable(ui::chip("tag-groups-close", "Close", true, colors), true, cx.listener(|_, _, _, cx| cx.emit(CloseDialog)))))
    }
}
