//! The command pill (`src/components/shell/CommandPill.tsx`): the filter toolbar docked
//! above the stage — the culling segment, colour-label dots, removable scope chips, the
//! ＋ Filter menu and the thumbnail-size slider.
//!
//! It edits the Library session's scope directly ([`ShellState::update_scope`]); the match
//! count and chip names follow through `ShellState`'s scope read, and the Library view
//! (#106) re-runs its query on `ScopeChanged`. Shown only on the Library surface, the only
//! one with a grid or loupe to filter, and hidden in Compare, as React hid it.

use crate::shell::state::{snap_thumb, ShellState, THUMB_MAX, THUMB_MIN, THUMB_STEP};
use crate::shell::style::{dot_ring, Colors, COLOR_LABELS};
use crate::view::RootView;
use chairphoto_core::catalog::{CullingFilter, PhotoSort, StorageTier};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::{Icon, IconName, Sizable as _};
use gpui_kit::prelude::*;
use gpui_kit::{div, px, AnyElement, App, Context, Entity, FontWeight, Hsla, SharedString, TestSupportExt as _, Window};

/// The culling segment, in order, with the mockup's plural labels.
pub const FILTERS: [(CullingFilter, &str); 5] = [
    (CullingFilter::All, "All"),
    (CullingFilter::Unrated, "Unrated"),
    (CullingFilter::Pick, "Picks"),
    (CullingFilter::Reject, "Rejects"),
    (CullingFilter::Edited, "Edited"),
];

/// A non-default storage tier's chip text. The safety tiers are set from the Safety panel,
/// but an active one still shows as a chip: a filter you cannot see cannot be turned off.
pub fn storage_chip_label(tier: StorageTier) -> Option<&'static str> {
    match tier {
        StorageTier::All => None,
        StorageTier::Local => Some("On disk"),
        StorageTier::Nas => Some("NAS only"),
        StorageTier::AtRisk => Some("At risk"),
        StorageTier::Stale => Some("Edits not carried home"),
    }
}

/// The tiers ＋ Filter offers directly.
const STORAGE_MENU: [(StorageTier, &str); 3] =
    [(StorageTier::All, "All"), (StorageTier::Local, "On disk"), (StorageTier::Nas, "NAS only")];

const SORTS: [(PhotoSort, &str); 3] = [
    (PhotoSort::Date, "Date"),
    (PhotoSort::SharpnessAsc, "Least sharp first"),
    (PhotoSort::SharpnessDesc, "Sharpest first"),
];

/// The thumbnail-size slider's state, owned by the root view and mirrored into
/// `ShellState::layout.thumb_size`.
pub fn thumb_slider(shell: &Entity<ShellState>, window: &mut Window, cx: &mut Context<RootView>) -> Entity<SliderState> {
    let initial = shell.read(cx).layout.thumb_size;
    let slider = cx.new(|_| {
        // `max` before `min`: `min` clamps against the default max (100).
        SliderState::new().max(THUMB_MAX).min(THUMB_MIN).step(THUMB_STEP).default_value(initial)
    });
    let shell = shell.clone();
    cx.subscribe_in(&slider, window, move |_, _, event: &SliderEvent, _, cx| {
        // `Change` fires while dragging, as React's `onChange` did; `Release` adds nothing.
        if let SliderEvent::Change(value) = event {
            let v = snap_thumb(value.start());
            shell.update(cx, |s, cx| s.set_thumb_size(v, cx));
        }
    })
    .detach();
    slider
}

impl RootView {
    pub(crate) fn render_command_pill(&self, shell: &ShellState, colors: Colors, cx: &Context<Self>) -> AnyElement {
        let scope = shell.library.scope().clone();
        let mut bar = div()
            .id("command-pill")
            .flex()
            .flex_none()
            .flex_row()
            .items_center()
            .gap(px(4.))
            .h(px(44.))
            .px(px(12.))
            .bg(colors.canvas)
            .border_b_1()
            .border_color(colors.line)
            .overflow_x_scroll();

        for (filter, label) in FILTERS {
            let on = scope.filter == filter;
            bar = bar.child(
                seg_button(SharedString::from(format!("filter-{label}")), label, on, colors)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.shell.update(cx, |s, cx| s.update_scope(cx, |l| l.set_filter(filter)))
                    }))
                    .test_support(),
            );
        }
        bar = bar.child(separator(colors));

        // Colour-label dots: multi-select (OR), plus "No label". The dots show the state.
        let mut dots = div().flex().items_center().gap(px(5.)).px(px(7.));
        for label in COLOR_LABELS {
            let on = scope.labels.iter().any(|l| l == label.name);
            dots = dots.child(
                label_dot(SharedString::from(format!("label-{}", label.name)), Some(label.color()), on, colors)
                    .tooltip(crate::shell::title_bar::tooltip(match label.name {
                        "Red" => "Red label",
                        "Yellow" => "Yellow label",
                        "Green" => "Green label",
                        "Blue" => "Blue label",
                        _ => "Purple label",
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.shell.update(cx, |s, cx| s.update_scope(cx, |l| l.toggle_label(label.name)))
                    }))
                    .test_support(),
            );
        }
        let none_on = scope.labels.iter().any(|l| l.is_empty());
        dots = dots.child(
            label_dot("label-none".into(), None, none_on, colors)
                .tooltip(crate::shell::title_bar::tooltip("No label"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.shell.update(cx, |s, cx| s.update_scope(cx, |l| l.toggle_label("")))
                }))
                .test_support(),
        );
        bar = bar.child(dots).child(separator(colors));

        // Removable scope chips.
        let info = &shell.scope_info;
        let name = |n: &Option<String>| n.clone().unwrap_or_else(|| "…".into());
        if scope.tag_id.is_some() {
            bar = bar.child(self.chip("chip-tag", format!("Tag: {}", name(&info.tag_name)), colors, cx, |l| l.select_tag(None)));
        }
        if scope.album_id.is_some() {
            bar = bar.child(self.chip("chip-album", format!("Album: {}", name(&info.album_name)), colors, cx, |l| l.select_album(None)));
        }
        if scope.smart_album_id.is_some() {
            bar = bar.child(self.chip(
                "chip-smart-album",
                format!("Smart album: {}", name(&info.smart_album_name)),
                colors,
                cx,
                |l| l.select_smart_album(None),
            ));
        }
        if let Some(batch) = &scope.batch {
            bar = bar.child(self.chip(
                "chip-batch",
                format!("Batch: {}", crate::shell::title_bar::batch_label(batch)),
                colors,
                cx,
                |l| l.select_batch(None),
            ));
        }
        for key in &scope.facets {
            let label = shell.lists.facets.iter().find(|f| &f.key == key).map_or(key.clone(), |f| f.label.clone());
            let key2 = key.clone();
            bar = bar.child(self.chip(format!("chip-facet-{key}"), label, colors, cx, move |l| l.toggle_facet(&key2)));
        }
        if let Some(camera) = &scope.camera {
            bar = bar.child(self.chip("chip-camera", format!("Camera: {camera}"), colors, cx, |l| l.set_camera(None)));
        }
        if let Some(lens) = &scope.lens {
            bar = bar.child(self.chip("chip-lens", format!("Lens: {lens}"), colors, cx, |l| l.set_lens(None)));
        }
        if let Some(label) = storage_chip_label(scope.storage_tier) {
            bar = bar.child(self.chip("chip-storage", label.to_string(), colors, cx, |l| l.set_storage_tier(StorageTier::All)));
        }

        bar = bar.child(self.filter_menu(shell, colors)).child(separator(colors)).child(
            div()
                .id("thumb-size")
                .flex()
                .items_center()
                .gap(px(7.))
                .pl(px(7.))
                .pr(px(10.))
                .text_color(colors.mute)
                .tooltip(crate::shell::title_bar::tooltip("Thumbnail size"))
                .child(Icon::new(IconName::LayoutDashboard).size(px(12.)))
                .child(div().w(px(70.)).child(Slider::new(&self.thumb_slider))),
        );
        bar.into_any_element()
    }

    /// `.ftchip`: one click clears that part of the scope.
    fn chip(
        &self,
        id: impl Into<SharedString>,
        text: String,
        colors: Colors,
        cx: &Context<Self>,
        clear: impl Fn(&mut chairphoto_model::library::session::LibrarySession) + 'static,
    ) -> impl IntoElement {
        div()
            .id(id.into())
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.))
            .h(px(26.))
            .px(px(10.))
            .bg(colors.sel)
            .border_1()
            .border_color(colors.accent_border())
            .rounded_full()
            .text_color(colors.accent)
            .text_size(px(11.5))
            .whitespace_nowrap()
            .cursor_pointer()
            .hover(|s| s.bg(colors.accent.opacity(0.22)))
            .child(text)
            .child(div().text_color(colors.dim).child("✕"))
            .on_click(cx.listener(move |this, _, _, cx| this.shell.update(cx, |s, cx| s.update_scope(cx, |l| clear(l)))))
            .test_support()
    }

    fn filter_menu(&self, shell: &ShellState, colors: Colors) -> impl IntoElement {
        let scope = shell.library.scope().clone();
        let lists = shell.lists.clone();
        let handle = self.shell.clone();
        Button::new("filter-menu")
            .ghost()
            .small()
            .rounded_full()
            .label("＋ Filter")
            .text_color(colors.dim)
            .text_size(px(11.5))
            .tooltip("Add a filter")
            .dropdown_menu(move |menu, window, cx| {
                let set = |f: Box<dyn Fn(&mut chairphoto_model::library::session::LibrarySession)>| {
                    let handle = handle.clone();
                    move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut App| {
                        handle.update(cx, |s, cx| s.update_scope(cx, |l| f(l)))
                    }
                };
                let mut menu = menu.min_w(px(200.)).label("FACETS");
                for facet in &lists.facets {
                    let key = facet.key.clone();
                    menu = menu.item(
                        PopupMenuItem::new(facet.label.clone())
                            .checked(scope.facets.contains(&facet.key))
                            .on_click(set(Box::new(move |l| l.toggle_facet(&key)))),
                    );
                }
                let cameras = lists.cameras.clone();
                let lenses = lists.lenses.clone();
                let (h1, h2) = (handle.clone(), handle.clone());
                menu = menu
                    .separator()
                    .submenu("Camera", window, cx, move |sub, _, _| {
                        value_submenu(sub, "Any camera", &cameras, h1.clone(), |l, v| l.set_camera(v))
                    })
                    .submenu("Lens", window, cx, move |sub, _, _| {
                        value_submenu(sub, "Any lens", &lenses, h2.clone(), |l, v| l.set_lens(v))
                    })
                    .separator()
                    .label("STORAGE");
                for (tier, label) in STORAGE_MENU {
                    menu = menu.item(
                        PopupMenuItem::new(label)
                            .checked(scope.storage_tier == tier)
                            .on_click(set(Box::new(move |l| l.set_storage_tier(tier)))),
                    );
                }
                menu = menu.separator().label("SORT");
                for (sort, label) in SORTS {
                    menu = menu.item(
                        PopupMenuItem::new(label)
                            .checked(scope.sort == sort)
                            .on_click(set(Box::new(move |l| l.set_sort(sort)))),
                    );
                }
                menu
            })
    }
}

/// The Camera / Lens submenus: "Any …", then every distinct value in the catalog.
fn value_submenu(
    sub: gpui_kit::component::menu::PopupMenu,
    any: &'static str,
    values: &[String],
    shell: Entity<ShellState>,
    set: fn(&mut chairphoto_model::library::session::LibrarySession, Option<String>),
) -> gpui_kit::component::menu::PopupMenu {
    let pick = |value: Option<String>| {
        let shell = shell.clone();
        move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut App| {
            let value = value.clone();
            shell.update(cx, |s, cx| s.update_scope(cx, |l| set(l, value)))
        }
    };
    let sub = sub.item(PopupMenuItem::new(any).on_click(pick(None)));
    values
        .iter()
        .fold(sub, |sub, v| sub.item(PopupMenuItem::new(v.clone()).on_click(pick(Some(v.clone())))))
}

/// `.ft`: a flat pill in the culling segment; `.on` gets the elevated fill.
fn seg_button(id: SharedString, label: &'static str, on: bool, colors: Colors) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .h(px(26.))
        .px(px(12.))
        .rounded_full()
        .text_size(px(11.5))
        .whitespace_nowrap()
        .cursor_pointer()
        .child(label)
        .when(on, |b| b.bg(colors.elev).text_color(colors.txt).font_weight(FontWeight::SEMIBOLD))
        .when(!on, |b| b.text_color(colors.dim).hover(|s| s.text_color(colors.txt)))
}

/// `.fd` / `.bench-dot`: a 10 px colour dot, or the crossed "no label" dot when `color` is
/// `None`. Active: full opacity and a ring in its own colour.
pub(crate) fn label_dot(id: SharedString, color: Option<Hsla>, on: bool, colors: Colors) -> gpui_kit::Stateful<gpui_kit::Div> {
    let dot = div().id(id).flex_none().size(px(10.)).rounded_full().cursor_pointer();
    match color {
        Some(c) => dot
            .bg(c)
            .opacity(if on { 1. } else { 0.42 })
            .when(on, |d| d.shadow(dot_ring(c, colors.panel))),
        None => dot
            .border(px(1.4))
            .border_color(colors.mute)
            .opacity(if on { 1. } else { 0.6 })
            .when(on, |d| d.shadow(dot_ring(colors.mute, colors.panel))),
    }
}

/// `.ftsep`: a 1 × 16 px divider.
fn separator(colors: Colors) -> impl IntoElement {
    div().flex_none().w(px(1.)).h(px(16.)).mx(px(5.)).bg(colors.border)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_non_default_storage_tiers_show_a_chip() {
        assert_eq!(storage_chip_label(StorageTier::All), None);
        assert_eq!(storage_chip_label(StorageTier::AtRisk), Some("At risk"));
        assert_eq!(storage_chip_label(StorageTier::Stale), Some("Edits not carried home"));
    }
}
