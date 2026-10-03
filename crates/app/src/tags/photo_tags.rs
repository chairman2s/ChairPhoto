//! The inspector tags tab's tagging block — the tags parts of `PhotoInspector.tsx` and
//! `Inspector.tsx`'s `QuickTagGroups` — as one self-contained view for the photos it edits
//! ([`TagTarget`]). The Photo inspector (#108) owns the tab and mounts this view; until it
//! lands the inspector chrome shows it on the tags tab.
//!
//! - **Chips:** the active photo's tags; × removes the tag from every target (the whole
//!   selection, not only the photo shown — App.tsx's `removeFromSelection`). "none" when it
//!   has none.
//! - **Copy tags** (this photo's) / **Paste N → M** (onto every target): the in-app clipboard
//!   in [`TagsState`], cleared by a catalog switch.
//! - **Add tag…** with autocomplete: the top eight tags, deepest match first, minus those
//!   already on the photo (`tag_tree::search`); ↑/↓ move the highlight, Enter (or +) assigns
//!   it, or creates the typed path when nothing matches; Escape clears.
//! - **From nearby photos:** tags on photos taken within the chosen window (±30 s – 10 min,
//!   the catalog setting `nearby_window_seconds`), one click assigns.
//! - **Quick tags:** the virtual "Recently used" group (ten) and the user's groups; a tag
//!   button assigns it to every target; "⚙ groups" opens [`TagGroupsManager`].
//! - **Auto-tags** (#181) are the catalog's to assign: never offered by the add box, nearby
//!   or quick tags, and their chips have no ×. A typed path naming one is refused by the core
//!   and its message shown; paste skips them and says so.
//!
//! Every read is off the UI thread and re-runs when the target changes or a tag write lands
//! ([`TagsState::revision`]); a read for a superseded target is dropped.

use super::groups::TagGroupsManager;
use super::state::{run, run_as, TagDialog, TagsState};
use crate::shell::style::Colors;
use crate::storage::ui;
use chairphoto_core::catalog::{Tag, TagGroup, TagWithCount};
use chairphoto_model::tag_tree::{ancestor_prefix, search};
use gpui_kit::component::input::{Escape, Input, InputEvent, InputState, MoveDown, MoveUp};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::Sizable as _;
use gpui_kit::prelude::*;
use gpui_kit::{div, px, App, Context, Entity, FontWeight, SharedString, Subscription, TestSupportExt as _, Window};
use std::collections::HashSet;

/// The "From nearby photos" windows (`WINDOW_OPTIONS`), label and seconds.
pub const WINDOW_OPTIONS: [(&str, i64); 5] = [("30s", 30), ("1m", 60), ("2m", 120), ("5m", 300), ("10m", 600)];
/// The catalog setting that remembers the window.
pub const WINDOW_SETTING: &str = "nearby_window_seconds";
/// The virtual "Recently used" group's id, and how many tags it shows.
pub const RECENT_GROUP: i64 = -1;
pub const RECENT_LIMIT: usize = 10;

/// What the block edits: the photo it shows, and what a bulk action applies to (the
/// selection, else that photo — `LibrarySelection::targets`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagTarget {
    pub active: Option<i64>,
    pub targets: Vec<i64>,
}

pub struct PhotoTags {
    tags: Entity<TagsState>,
    pub target: TagTarget,
    /// The active photo's tags.
    pub assigned: Vec<Tag>,
    pub nearby: Vec<TagWithCount>,
    pub window_secs: i64,
    pub input: Entity<InputState>,
    pub highlight: usize,
    pub groups: Vec<TagGroup>,
    /// The active quick-tag group ([`RECENT_GROUP`] = Recently used).
    pub group: i64,
    pub members: Vec<Tag>,
    /// The tag state's revision the reads were last made for.
    seen_revision: u64,
    /// Bumped by every read of the photo's tags; a superseded read is dropped.
    generation: u64,
    groups_generation: u64,
    /// An add clears the box on the next render (where a `Window` is at hand).
    clear_input: bool,
    dialog_close: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl PhotoTags {
    pub fn new(tags: Entity<TagsState>, target: TagTarget, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Add tag…"));
        let subscriptions = vec![
            cx.subscribe(&input, |s: &mut Self, _, e: &InputEvent, cx| match e {
                InputEvent::Change => {
                    s.highlight = 0;
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => s.add(cx),
                _ => {}
            }),
            cx.observe(&tags, |s: &mut Self, tags, cx| {
                let revision = tags.read(cx).revision;
                if revision != s.seen_revision {
                    s.seen_revision = revision;
                    s.reload(cx);
                }
                cx.notify();
            }),
        ];
        let seen_revision = tags.read(cx).revision;
        let mut this = PhotoTags {
            tags,
            target,
            assigned: Vec::new(),
            nearby: Vec::new(),
            window_secs: 120,
            input,
            highlight: 0,
            groups: Vec::new(),
            group: RECENT_GROUP,
            members: Vec::new(),
            seen_revision,
            generation: 0,
            groups_generation: 0,
            clear_input: false,
            dialog_close: None,
            _subscriptions: subscriptions,
        };
        // The remembered window, then the reads that depend on it.
        run(&this.tags, cx, false, |c| c.get_setting(WINDOW_SETTING), |s: &mut Self, r, cx| {
            if let Some(n) = r.ok().flatten().and_then(|v| v.trim().parse::<i64>().ok()) {
                s.window_secs = n;
            }
            s.reload(cx);
        });
        this.reload(cx);
        this
    }

    /// Point the block at other photos (the selection changed).
    pub fn set_target(&mut self, target: TagTarget, cx: &mut Context<Self>) {
        if target != self.target {
            let photo_changed = target.active != self.target.active;
            self.target = target;
            if photo_changed {
                self.assigned.clear();
                self.nearby.clear();
                self.reload(cx);
            }
            cx.notify();
        }
    }

    /// Re-read the photo's tags, the nearby suggestions and the quick-tag groups.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        if let Some(photo) = self.target.active {
            let window = self.window_secs;
            run(
                &self.tags,
                cx,
                false,
                move |c| Ok((c.get_photo_tags(photo)?, c.suggest_tags_by_time(photo, window).unwrap_or_default())),
                move |s: &mut Self, r, cx| {
                    if s.generation != generation {
                        return;
                    }
                    if let Ok((assigned, nearby)) = r {
                        s.assigned = assigned;
                        s.nearby = nearby;
                    }
                    cx.notify();
                },
            );
        } else {
            self.assigned.clear();
            self.nearby.clear();
        }
        self.reload_groups(cx);
    }

    fn reload_groups(&mut self, cx: &mut Context<Self>) {
        self.groups_generation += 1;
        let generation = self.groups_generation;
        let group = self.group;
        run(
            &self.tags,
            cx,
            false,
            move |c| {
                let groups = c.list_tag_groups()?;
                let keep = group == RECENT_GROUP || groups.iter().any(|g| g.id == group);
                let group = if keep { group } else { RECENT_GROUP };
                let members =
                    if group == RECENT_GROUP { c.recently_used_tags(RECENT_LIMIT)? } else { c.group_members(group)? };
                Ok((groups, group, members))
            },
            move |s: &mut Self, r, cx| {
                if s.groups_generation != generation {
                    return;
                }
                match r {
                    Ok((groups, group, members)) => {
                        s.groups = groups;
                        s.group = group;
                        // A group can hold an auto-tag (the manager adds any tag); its
                        // button would only be refused (#181).
                        s.members = members.into_iter().filter(|t| t.auto_rule.is_none()).collect();
                    }
                    Err(_) => {
                        s.groups.clear();
                        s.members.clear();
                    }
                }
                cx.notify();
            },
        );
    }

    pub fn select_group(&mut self, group: i64, cx: &mut Context<Self>) {
        self.group = group;
        self.members.clear();
        self.reload_groups(cx);
        cx.notify();
    }

    /// The add-tag box's suggestions: never a tag already on the photo, nor an auto-tag (the
    /// catalog assigns those from the photo; a hand assignment is refused, #181).
    pub fn suggestions(&self, cx: &App) -> Vec<TagWithCount> {
        let tags = &self.tags.read(cx).tags;
        let auto = tags.iter().filter(|t| t.tag.auto_rule.is_some()).map(|t| t.tag.id);
        let exclude: HashSet<i64> = self.assigned.iter().map(|t| t.id).chain(auto).collect();
        search(tags, &self.input.read(cx).value(), &exclude).into_iter().cloned().collect()
    }

    pub fn move_highlight(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        let n = self.suggestions(cx).len();
        if n == 0 {
            return false;
        }
        self.highlight = (self.highlight.min(n - 1) as isize + delta).clamp(0, n as isize - 1) as usize;
        cx.notify();
        true
    }

    /// Enter / +: assign the highlighted suggestion, or create the typed path when nothing
    /// matches. The box clears on the next render.
    pub fn add(&mut self, cx: &mut Context<Self>) {
        let typed = self.input.read(cx).value().trim().to_string();
        if typed.is_empty() {
            return;
        }
        let suggestions = self.suggestions(cx);
        let targets = self.target.targets.clone();
        match suggestions.get(self.highlight.min(suggestions.len().saturating_sub(1))) {
            Some(t) if !suggestions.is_empty() => {
                let id = t.tag.id;
                self.tags.update(cx, |s, cx| s.assign(targets, id, cx));
            }
            _ => self.tags.update(cx, |s, cx| s.create_and_assign(targets, typed, cx)),
        }
        self.clear_input = true;
        self.highlight = 0;
        cx.notify();
    }

    pub fn assign(&mut self, tag_id: i64, cx: &mut Context<Self>) {
        let targets = self.target.targets.clone();
        self.tags.update(cx, |s, cx| s.assign(targets, tag_id, cx));
    }

    /// A nearby chip: assign it, and drop it from the list at once.
    pub fn assign_nearby(&mut self, tag_id: i64, cx: &mut Context<Self>) {
        self.nearby.retain(|t| t.tag.id != tag_id);
        self.assign(tag_id, cx);
        cx.notify();
    }

    pub fn remove(&mut self, tag_id: i64, cx: &mut Context<Self>) {
        let targets = self.target.targets.clone();
        self.tags.update(cx, |s, cx| s.remove(targets, tag_id, cx));
    }

    pub fn copy(&mut self, cx: &mut Context<Self>) {
        let ids = self.assigned.iter().map(|t| t.id).collect();
        self.tags.update(cx, |s, cx| s.copy(ids, cx));
    }

    pub fn paste(&mut self, cx: &mut Context<Self>) {
        let targets = self.target.targets.clone();
        self.tags.update(cx, |s, cx| s.paste(targets, cx));
    }

    /// Change the nearby window, remember it, and re-read. Remembered in the catalog the tag
    /// tree was read from (`run_as` under its guard: refused once another catalog is open);
    /// with no tree read yet — between a switch and its re-read — it is not remembered.
    pub fn set_window(&mut self, secs: i64, cx: &mut Context<Self>) {
        self.window_secs = secs;
        let guard = self.tags.read(cx).guard();
        if guard.identity.is_some() {
            run_as(&self.tags, &guard, cx, false, move |c| c.set_setting(WINDOW_SETTING, &secs.to_string()), |_: &mut Self, _, _| {});
        }
        self.reload(cx);
        cx.notify();
    }

    pub fn open_groups(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let tags = self.tags.clone();
        let view = cx.new(|cx| TagGroupsManager::new(tags, window, cx));
        self.tags.update(cx, |t, _| t.last_dialog = Some(TagDialog::Groups(view.downgrade())));
        self.dialog_close = Some(super::open_dialog("Tag groups", 560., view, window, cx));
    }
}

impl Render for PhotoTags {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.clear_input) {
            self.input.update(cx, |i, cx| i.set_value("", window, cx));
        }
        let colors = Colors::get(cx);
        let label = |t: &'static str| div().text_size(px(10.)).font_weight(FontWeight::BOLD).text_color(colors.mute).child(t);
        let Some(_) = self.target.active else {
            return div().id("photo-tags").child(ui::empty("photo-tags-none", "Select a photo", colors)).into_any_element();
        };
        let n_targets = self.target.targets.len();
        let clipboard = self.tags.read(cx).clipboard.len();

        let mut chips = ui::row().id("photo-tags-chips").gap(px(5.));
        for t in &self.assigned {
            let id = t.id;
            chips = chips.child(
                div()
                    .id(SharedString::from(format!("photo-tag-{id}")))
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .px(px(7.))
                    .py(px(2.))
                    .rounded(px(6.))
                    .bg(colors.elev)
                    .text_size(px(11.5))
                    .text_color(colors.txt)
                    .child(t.name.clone())
                    .tooltip(super::tip(match &t.auto_rule {
                        Some(rule) => format!("{} · auto-tag ({rule})", t.full_path),
                        None => t.full_path.clone(),
                    }))
                    // An auto-tag has no ×: the catalog assigns it, and a hand removal would be
                    // refused (#181).
                    .when(t.auto_rule.is_none(), |chip| {
                        chip.child(
                            div()
                                .id(SharedString::from(format!("photo-tag-remove-{id}")))
                                .cursor_pointer()
                                .text_color(colors.mute)
                                .hover(|s| s.text_color(colors.danger))
                                .child("×")
                                .on_click(cx.listener(move |s, _, _, cx| s.remove(id, cx)))
                                .test_support(),
                        )
                    }),
            );
        }
        if self.assigned.is_empty() {
            chips = chips.child(ui::sub("none", colors));
        }

        let paste_label = format!(
            "Paste{}{}",
            if clipboard > 0 { format!(" {clipboard}") } else { String::new() },
            if n_targets > 1 { format!(" → {n_targets}") } else { String::new() }
        );
        let copy_paste = ui::row()
            .child(ui::clickable(
                ui::chip("photo-tags-copy", "Copy tags", !self.assigned.is_empty(), colors),
                !self.assigned.is_empty(),
                cx.listener(|s, _, _, cx| s.copy(cx)),
            ))
            .child(ui::clickable(
                ui::chip("photo-tags-paste", paste_label, clipboard > 0, colors),
                clipboard > 0,
                cx.listener(|s, _, _, cx| s.paste(cx)),
            ));

        let suggestions = self.suggestions(cx);
        let highlight = self.highlight.min(suggestions.len().saturating_sub(1));
        let add = div()
            .id("photo-tags-add")
            .flex()
            .flex_col()
            .gap(px(3.))
            .capture_action(cx.listener(|s, _: &MoveDown, _, cx| {
                if s.move_highlight(1, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|s, _: &MoveUp, _, cx| {
                if s.move_highlight(-1, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|s, _: &Escape, window, cx| {
                if !s.input.read(cx).value().is_empty() {
                    s.input.update(cx, |i, cx| i.set_value("", window, cx));
                    cx.stop_propagation();
                }
            }))
            .child(
                ui::row()
                    .child(div().flex_1().child(Input::new(&self.input).id("photo-tags-input").small()))
                    .child(ui::clickable(ui::chip("photo-tags-add-button", "+", true, colors), true, cx.listener(|s, _, _, cx| s.add(cx)))),
            )
            .children(suggestions.iter().enumerate().map(|(i, t)| {
                let id = t.tag.id;
                div()
                    .id(SharedString::from(format!("photo-tags-hit-{i}")))
                    .flex()
                    .px(px(6.))
                    .h(px(22.))
                    .items_center()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .text_size(px(11.5))
                    .when(i == highlight, |r| r.bg(colors.sel))
                    .child(div().text_color(colors.mute).child(ancestor_prefix(&t.tag.full_path, &t.tag.name)))
                    .child(div().text_color(colors.txt).child(t.tag.name.clone()))
                    .on_hover(cx.listener(move |s, hovered: &bool, _, cx| {
                        if *hovered {
                            s.highlight = i;
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |s, _, _, cx| {
                        s.assign(id, cx);
                        s.clear_input = true;
                        s.highlight = 0;
                        cx.notify();
                    }))
                    .test_support()
            }));

        let current = WINDOW_OPTIONS.iter().find(|(_, s)| *s == self.window_secs).map(|(l, _)| *l).unwrap_or("2m");
        let this = cx.entity().downgrade();
        let window_menu = Button::new("photo-tags-window")
            .ghost()
            .xsmall()
            .label(format!("±{current} ▾"))
            .text_color(colors.dim)
            .tooltip("Time window for nearby suggestions")
            .dropdown_menu(move |mut menu, _, _| {
                for (label, secs) in WINDOW_OPTIONS {
                    let this = this.clone();
                    menu = menu.item(PopupMenuItem::new(format!("±{label}")).on_click(move |_, _, cx| {
                        this.update(cx, |s, cx| s.set_window(secs, cx)).ok();
                    }));
                }
                menu
            });
        let mut nearby = ui::row().id("photo-tags-nearby").gap(px(5.));
        for t in &self.nearby {
            let id = t.tag.id;
            nearby = nearby.child(ui::clickable(
                ui::chip(format!("photo-tags-nearby-{id}"), format!("+ {}  {}", t.tag.name, t.photo_count), true, colors),
                true,
                cx.listener(move |s, _, _, cx| s.assign_nearby(id, cx)),
            ));
        }
        if self.nearby.is_empty() {
            nearby = nearby.child(ui::sub("none in window", colors));
        }

        // Quick tags.
        let mut group_chips = ui::row().id("quick-tag-groups").gap(px(5.));
        let recent = std::iter::once((RECENT_GROUP, "Recently used".to_string()));
        for (id, name) in recent.chain(self.groups.iter().map(|g| (g.id, g.name.clone()))) {
            let chip = ui::chip(format!("quick-group-{id}"), name, true, colors).when(self.group == id, |c| c.bg(colors.sel).text_color(colors.txt));
            group_chips = group_chips.child(ui::clickable(chip, true, cx.listener(move |s, _, _, cx| s.select_group(id, cx))));
        }
        group_chips = group_chips.child(ui::clickable(
            ui::chip("quick-groups-manage", "⚙ groups", true, colors),
            true,
            cx.listener(|s, _, window, cx| s.open_groups(window, cx)),
        ));
        let target_label = if n_targets > 1 { format!(" → {n_targets} selected") } else { String::new() };
        let mut quick = ui::row().id("quick-tags").gap(px(5.));
        for t in &self.members {
            let id = t.id;
            quick = quick.child(
                ui::clickable(
                    ui::chip(format!("quick-tag-{id}"), t.name.clone(), true, colors)
                        .tooltip(super::tip(format!("{}{target_label}", t.full_path))),
                    true,
                    cx.listener(move |s, _, _, cx| s.assign(id, cx)),
                ),
            );
        }
        if self.members.is_empty() {
            // The whole row's width, and no wider: a text child of a flex row lays out at its
            // one-line width and runs past the column (#176); React's `.panel-empty` wraps.
            quick = quick.child(
                ui::sub(
                    if self.group == RECENT_GROUP {
                        "No recently used tags yet — tag a photo and they’ll show here."
                    } else {
                        "Empty group — add tags via “⚙ groups”."
                    },
                    colors,
                )
                .id("quick-tags-empty")
                .w_full()
                .min_w_0()
                .test_support(),
            );
        }

        div()
            .id("photo-tags")
            .flex()
            .flex_col()
            .gap(px(8.))
            .px(px(14.))
            .py(px(10.))
            .child(label("TAGS"))
            .child(chips.test_support())
            .child(copy_paste)
            .child(add)
            .child(ui::row().child(ui::sub("From nearby photos", colors)).child(window_menu))
            .child(nearby.test_support())
            .child(label("QUICK TAGS"))
            .child(group_chips.test_support())
            .child(quick.test_support())
            .into_any_element()
    }
}
