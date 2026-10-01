//! The title bar (`src/components/shell/TitleBar.tsx`): a 46 px strip with the catalog pill
//! on the left, empty space in the middle (rich search lands there later), and on the right
//! the attention chips, the Import ▾ menu, Export, and the More ⋯ menu.
//!
//! **Decorations.** The window asks for server-side decorations (shell-apis.md § 7): on
//! Hyprland the compositor tiles and draws no title bar, and neither did the Tauri window, so
//! this strip is the app's own header, not a window frame. Only when the compositor refuses
//! and the window is client-decorated (no `xdg-decoration`, e.g. GNOME) does the strip become
//! gpui-component's `TitleBar`, which then adds a drag area and min/max/close. Under
//! server-side decorations it stays a plain strip on purpose: `TitleBar` starts a window
//! move on any press-and-drag inside it, which on a tiling compositor would turn a sloppy
//! click on the catalog pill into a tile drag.
//!
//! **Menus** are gpui-component `PopupMenu`s behind `Button` triggers. Items dispatch actions
//! (`shell::actions`) to the root view; the menu owns Escape, ↑/↓ and Enter in its
//! `PopupMenu` context, and the keymap stops `[`/`]` reaching the shell while it is focused
//! (React's "an open menu swallows the culling keys"). Two differences from `Menu.tsx`:
//! gpui-component runs the item's action before closing, and a check item closes the menu
//! (React kept `MenuCheckItem` open).

use crate::model::AppModel;
use crate::modules::ModuleRegistry;
use crate::shell::actions::*;
use crate::shell::state::{ShellState, Side};
use crate::shell::style::{grouped, Colors, RADIUS};
use crate::view::RootView;
use chairphoto_core::catalog::ImportBatch;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::{Icon, IconName, Sizable as _, TitleBar};
use gpui_kit::prelude::*;
use gpui_kit::{
    div, px, Action, AnyElement, Anchor, Decorations, Entity, FocusHandle, FontWeight, SharedString,
    TestSupportExt as _, Window,
};

/// The title bar's height (`.titlebar`).
pub const TITLE_BAR_H: f32 = 46.;

/// "Last path segment" label of an import batch, as everywhere React showed one.
pub fn batch_label(b: &ImportBatch) -> String {
    let trimmed = b.source_label.trim_end_matches('/');
    match trimmed.rsplit('/').next() {
        Some(last) if !last.is_empty() => last.to_string(),
        _ if !b.source_label.is_empty() => b.source_label.clone(),
        _ => "(ingest)".to_string(),
    }
}

/// A menu row with a right-aligned muted badge (`MenuItem`'s `badge`).
fn badge_item(label: &'static str, badge: String, action: Box<dyn Action>, colors: Colors) -> PopupMenuItem {
    PopupMenuItem::element(move |_, _| {
        div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .gap_2()
            .child(div().flex_1().child(label))
            .child(div().text_size(px(10.5)).text_color(colors.mute).child(badge.clone()))
    })
    .action(action)
}

impl RootView {
    pub(crate) fn render_title_bar(
        &self,
        shell: &ShellState,
        model: &AppModel,
        colors: Colors,
        window: &mut Window,
    ) -> AnyElement {
        let ready = model.catalog.is_some();
        let catalog_name = model.catalog.as_ref().map(|c| c.name.clone()).unwrap_or_default();
        let photo_count = shell
            .scope_info
            .total
            .or(model.catalog.as_ref().map(|c| c.photo_count))
            .unwrap_or(0);
        let can_export = !shell.library.selection().targets.is_empty();

        let left = div().flex().flex_none().items_center().child(
            div()
                .id("catalog-pill")
                .flex()
                .items_center()
                .gap(px(6.))
                .h(px(26.))
                .px(px(10.))
                .border_1()
                .border_color(colors.border)
                .rounded_full()
                .text_size(px(12.))
                .cursor_pointer()
                .hover(|s| s.border_color(colors.dim))
                .child(
                    div()
                        .font_weight(FontWeight::BOLD)
                        .text_color(colors.txt)
                        .child(if catalog_name.is_empty() { "Catalog".to_string() } else { catalog_name }),
                )
                .child(div().text_color(colors.dim).child(format!("· {}", grouped(photo_count))))
                .child(Icon::new(IconName::ChevronDown).size(px(10.)).text_color(colors.dim))
                .tooltip(tooltip("Open or create a catalog"))
                .on_click(|_, window, cx| window.dispatch_action(Box::new(OpenCatalogs), cx))
                .test_support(),
        );

        let mut right = div().flex().flex_none().items_center().gap(px(8.));
        if shell.counts.pending > 0 {
            right = right.child(attention(
                "attn-pending",
                format!("⤓ {} waiting for the NAS", shell.counts.pending),
                "Back up photos waiting for the NAS",
                Box::new(Reconcile),
                colors,
            ));
        }
        match shell.counts.identity_debt {
            Some(0) => {}
            debt => right = right.child(attention(
                "attn-identity",
                format!("{} identity debt", debt.map_or("?".to_string(), |n| n.to_string())),
                if debt.is_none() {
                    "Identity debt count could not be checked — open to see the current queue"
                } else {
                    "Photo copies whose sidecar doesn't carry their identity yet. Most of this is \
                     normally Unreachable (an offline volume), not a failure."
                },
                Box::new(OpenIdentityDebt),
                colors,
            )),
        }

        right = right
            .child(self.import_menu(shell, ready, colors))
            .child(
                ghost_button("export", "Export", can_export, colors)
                    .tooltip(tooltip("Export the selected photo(s) to files (RAW + XMP, or JPEG)"))
                    .when(can_export, |b| {
                        b.on_click(|_, window, cx| window.dispatch_action(Box::new(ExportSelection), cx))
                    })
                    .test_support(),
            )
            .child(self.more_menu(shell, ready, colors));

        let children = [left.into_any_element(), div().flex_1().into_any_element(), right.into_any_element()];

        if matches!(window.window_decorations(), Decorations::Client { .. }) {
            // Client-decorated fallback: gpui-component's bar adds the drag area and controls.
            TitleBar::new()
                .h(px(TITLE_BAR_H))
                .pl(px(16.))
                .bg(colors.panel)
                .border_color(colors.line)
                .child(div().flex().flex_1().items_center().gap(px(12.)).pr(px(8.)).children(children))
                .into_any_element()
        } else {
            div()
                .id("title-bar")
                .flex()
                .flex_none()
                .flex_row()
                .items_center()
                .gap(px(12.))
                .h(px(TITLE_BAR_H))
                .px(px(16.))
                .bg(colors.panel)
                .border_b_1()
                .border_color(colors.line)
                .children(children)
                .into_any_element()
        }
    }

    fn import_menu(&self, shell: &ShellState, ready: bool, colors: Colors) -> impl IntoElement {
        let cache_previews = shell.cache_previews;
        let batches = shell.lists.batches.clone();
        let root = self.focus.clone();
        let model = self.model.clone();
        menu_trigger("import-menu", "Import ▾", colors).dropdown_menu(move |menu, window, cx| {
            let batches = batches.clone();
            let model = model.clone();
            menu.action_context(root.clone())
                .min_w(px(200.))
                .menu_with_disabled("Import from card…", Box::new(ImportFromCard), !ready)
                .menu_with_disabled("Import a .chairphoto bundle…", Box::new(ImportBundle), !ready)
                .separator()
                .menu_with_disabled("Rescan library", Box::new(RescanLibrary), !ready)
                .menu_with_check("Cache previews on import", cache_previews, Box::new(ToggleCachePreviews))
                .separator()
                .submenu("Export a bundle", window, cx, move |sub, _, _| {
                    if batches.is_empty() {
                        return sub.item(PopupMenuItem::new("No import batches yet").disabled(true));
                    }
                    batches.iter().fold(sub, |sub, b| {
                        let model = model.clone();
                        sub.item(PopupMenuItem::new(batch_label(b)).on_click(move |_, _, cx| {
                            model.update(cx, |m, cx| m.not_yet_ported("Export a bundle", 115, cx));
                        }))
                    })
                })
        })
    }

    fn more_menu(&self, shell: &ShellState, ready: bool, colors: Colors) -> impl IntoElement {
        let root = self.focus.clone();
        let selection = shell.library.selection();
        // React: `loupeEnabled = !!selected && activeView === null`, with an "On" badge while
        // the inline loupe shows.
        let loupe_enabled = selection.active.is_some();
        let loupe_on = shell.stage_view() == crate::shell::state::StageView::Loupe;
        let debt = shell.counts.identity_debt.map_or("?".to_string(), |n| n.to_string());
        let pending = shell.counts.pending.to_string();
        let left_on = shell.panel_visible(Side::Left);
        let right_on = shell.panel_visible(Side::Right);
        let modules = self.modules.clone();
        Button::new("more-menu")
            .ghost()
            .small()
            .icon(IconName::Ellipsis)
            .text_color(colors.dim)
            .tooltip("More")
            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu: PopupMenu, window, cx| {
                let menu = more_menu_head(menu, root.clone(), ready, loupe_enabled, loupe_on);
                let menu = module_actions_submenu(menu, &modules, window, cx);
                more_menu_tail(menu, &debt, &pending, left_on, right_on, colors)
            })
    }
}

/// More ⋯, in React's order: [`more_menu_head`], the "Modules" submenu
/// ([`module_actions_submenu`]), [`more_menu_tail`].
fn more_menu_head(menu: PopupMenu, root: FocusHandle, ready: bool, loupe_enabled: bool, loupe_on: bool) -> PopupMenu {
    menu.action_context(root)
        .min_w(px(200.))
        .menu("Open loupe in a new window", Box::new(PopOutLoupe))
        .menu_with_check_and_disabled("Loupe", loupe_on, Box::new(ToggleLoupe), !loupe_enabled)
        .separator()
        .label("WHOLE VIEW")
        .menu_with_disabled("Analyse burst sharpness", Box::new(AnalyseBurst), !ready)
        .menu_with_disabled("Propose stacks…", Box::new(ProposeStacks), !ready)
        .menu_with_disabled("Start cull session", Box::new(StartCullSession), !ready)
        .separator()
}

/// More ⋯ → Modules: each enabled module's actions under its name, in registration order
/// (host.ts `toolbarActionGroups`), then a separator. With no module actions there is no
/// submenu, as in React.
fn module_actions_submenu(
    menu: PopupMenu,
    modules: &Entity<ModuleRegistry>,
    window: &mut Window,
    cx: &mut gpui_kit::Context<PopupMenu>,
) -> PopupMenu {
    let groups = modules.read(cx).action_groups();
    if groups.is_empty() {
        return menu;
    }
    let modules = modules.clone();
    menu.submenu("Modules", window, cx, move |sub, _, _| {
        groups.iter().fold(sub, |sub, group| {
            let sub = sub.label(group.module_name.clone());
            group.actions.iter().fold(sub, |sub, action| {
                let (modules, module_id, action_id) = (modules.clone(), group.module_id.clone(), action.id.clone());
                sub.item(PopupMenuItem::new(action.label.clone()).on_click(move |_, window, cx| {
                    ModuleRegistry::activate(&modules, &module_id, &action_id, window, cx)
                }))
            })
        })
    })
    .separator()
}

fn more_menu_tail(menu: PopupMenu, debt: &str, pending: &str, left_on: bool, right_on: bool, colors: Colors) -> PopupMenu {
    menu.item(badge_item("Identity debt", debt.to_string(), Box::new(OpenIdentityDebt), colors))
        .item(badge_item("Back-up queue", pending.to_string(), Box::new(Reconcile), colors))
        .separator()
        .label("VIEW")
        .menu_with_check("Tags & collections panel", left_on, Box::new(ToggleLeftPanel))
        .menu_with_check("Inspector", right_on, Box::new(ToggleRightPanel))
        .separator()
        .menu("Preferences…", Box::new(OpenPreferences))
}

/// A tooltip builder for `.tooltip(..)`.
pub(crate) fn tooltip(
    text: &'static str,
) -> impl Fn(&mut Window, &mut gpui_kit::App) -> gpui_kit::AnyView + 'static {
    move |window, cx| gpui_kit::component::tooltip::Tooltip::new(text).build(window, cx)
}

/// A menu trigger styled like the title bar's ghost buttons.
pub(crate) fn menu_trigger(id: &'static str, label: &'static str, colors: Colors) -> Button {
    Button::new(id)
        .ghost()
        .small()
        .label(label)
        .text_color(colors.dim)
        .text_size(px(12.))
}

/// `.btn-ghost`: transparent, 1 px border, 8 px radius, dim 12 px medium text.
pub(crate) fn ghost_button(
    id: &'static str,
    label: impl Into<SharedString>,
    enabled: bool,
    colors: Colors,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .px(px(12.))
        .py(px(5.))
        .border_1()
        .border_color(colors.border)
        .rounded(RADIUS)
        .text_size(px(12.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(colors.dim)
        .child(label.into())
        .when(enabled, |b| b.cursor_pointer().hover(|s| s.border_color(colors.dim).text_color(colors.txt)))
        .when(!enabled, |b| b.opacity(0.4))
}

/// `.attn`: the accent-tinted attention chip.
fn attention(
    id: &'static str,
    text: String,
    tip: &'static str,
    action: Box<dyn Action>,
    colors: Colors,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .h(px(24.))
        .px(px(10.))
        .bg(colors.sel)
        .border_1()
        .border_color(colors.accent_border())
        .rounded_full()
        .text_color(colors.accent)
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .cursor_pointer()
        .child(text)
        .tooltip(tooltip(tip))
        .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
        .test_support()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(label: &str) -> ImportBatch {
        ImportBatch {
            id: 1,
            uuid: "u".into(),
            source_label: label.into(),
            note: String::new(),
            created_at: 0,
            photo_count: 0,
        }
    }

    #[test]
    fn batch_labels_are_the_last_path_segment() {
        assert_eq!(batch_label(&batch("/run/media/card/DCIM/")), "DCIM");
        assert_eq!(batch_label(&batch("card")), "card");
        assert_eq!(batch_label(&batch("/")), "/");
        assert_eq!(batch_label(&batch("")), "(ingest)");
    }
}
