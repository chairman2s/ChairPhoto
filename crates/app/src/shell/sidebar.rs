//! The left side of the shell: the icon rail (`IconRail.tsx`, `railOrder.ts`) and the
//! collection browser column (`CollectionBrowser.tsx`).
//!
//! **Rail.** Library; Develop when an editor exists (the `edit` feature) — disabled with
//! nothing selected, and not ported yet (#111); one button per enabled module's main view, in
//! [`rail_order`] (`rail-view-<id>`, [`crate::modules::MainView`]); the Preferences gear at the
//! foot.
//!
//! **Collection browser.** The fixed "library" header with All photos and Trash, then the
//! collapsible sections — tags ([`crate::tags::panel::TagPanel`]), smart albums, albums,
//! import batches ([`crate::storage::batches`]); a section whose panel is a later ticket
//! (Albums and export #115) says so. The browser scrolls as one column; the tag panel holds
//! its scroll handle, so a tag search can scroll its row into view. Below them, `module-slot-sidebar` holds the enabled
//! modules' sidebar panels ([`crate::modules::PanelSlot::Sidebar`]); it is empty while no
//! module contributes one.

use crate::shell::actions::*;
use crate::shell::state::{Section, ShellState, Surface};
use crate::modules::MainView;
use crate::shell::style::{grouped, Colors};
use crate::view::RootView;
use gpui_kit::component::{Icon, IconName};
use gpui_kit::prelude::*;
use gpui_kit::{div, px, Action, AnyElement, Context, SharedString, TestSupportExt as _};

/// The rail's width (`.body`'s first track).
pub const RAIL_W: f32 = 52.;

/// Ids that get a fixed spot at the front of the rail, in this order (`railOrder.ts`).
const PREFERRED_ORDER: [&str; 4] = ["map", "people", "statistics", "tag-graph"];

/// Order module main views for the rail: the preferred ids first, in their fixed order, then
/// every other view in input order. Generic over the view type so the Module registry's
/// view descriptor can use it unchanged.
pub fn rail_order<'a, V>(views: &'a [V], id: impl Fn(&V) -> &str) -> Vec<&'a V> {
    let mut out: Vec<&V> = PREFERRED_ORDER
        .iter()
        .filter_map(|pref| views.iter().find(|v| id(v) == *pref))
        .collect();
    out.extend(views.iter().filter(|v| !PREFERRED_ORDER.contains(&id(v))));
    out
}

/// What each section's panel is waiting for.
fn section_placeholder(section: Section) -> &'static str {
    match section {
        Section::Tags => "", // `crate::tags::panel`
        Section::SmartAlbums => "Smart albums — not yet ported (#115)",
        Section::Albums => "Albums — not yet ported (#115)",
        Section::Batches => "", // `crate::storage::batches`
    }
}

impl RootView {
    pub(crate) fn render_rail(
        &self,
        shell: &ShellState,
        colors: Colors,
        module_views: Vec<(SharedString, MainView)>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let can_develop = cfg!(feature = "edit");
        let develop_enabled = shell.library.selection().active.is_some();
        let item = |id: &'static str, icon: Icon, on: bool, enabled: bool, action: Box<dyn Action>| {
            div()
                .id(id)
                .relative()
                .flex()
                .items_center()
                .justify_center()
                .size(px(34.))
                .rounded(px(9.))
                .child(icon.size(px(17.)))
                .when(on, |b| {
                    b.bg(colors.sel).text_color(colors.accent).child(
                        // The active item's dot (`.rail-item.on::after`).
                        div().absolute().bottom(px(3.)).size(px(4.)).rounded_full().bg(colors.accent),
                    )
                })
                .when(!on, |b| b.text_color(colors.mute).hover(|s| s.text_color(colors.dim).bg(colors.panel)))
                .when(enabled, move |b| {
                    b.cursor_pointer()
                        .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
                })
                .when(!enabled, |b| b.opacity(0.4))
                .test_support()
        };
        let ordered = rail_order(&module_views, |(_, v)| v.id.as_ref());
        let module_items: Vec<AnyElement> = ordered
            .into_iter()
            .map(|(_, v)| {
                let on = matches!(&shell.surface, Surface::Module(id) if id == v.id.as_ref());
                let view_id = v.id.clone();
                let label = v.label.clone();
                let icon = v.icon.clone().unwrap_or_else(|| Icon::new(IconName::LayoutDashboard));
                div()
                    .id(SharedString::from(format!("rail-view-{}", v.id)))
                    .relative()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(34.))
                    .rounded(px(9.))
                    .cursor_pointer()
                    .child(icon.size(px(17.)))
                    .when(on, |b| {
                        b.bg(colors.sel).text_color(colors.accent).child(
                            div().absolute().bottom(px(3.)).size(px(4.)).rounded_full().bg(colors.accent),
                        )
                    })
                    .when(!on, |b| b.text_color(colors.mute).hover(|s| s.text_color(colors.dim).bg(colors.panel)))
                    .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(label.clone()).build(window, cx))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.shell.update(cx, |s, cx| s.show_module_view(&view_id, cx))
                    }))
                    .test_support()
                    .into_any_element()
            })
            .collect();
        div()
            .id("rail")
            .flex()
            .flex_col()
            .flex_none()
            .items_center()
            .gap(px(4.))
            .py(px(10.))
            .w(px(RAIL_W))
            .h_full()
            .bg(colors.canvas)
            .border_r_1()
            .border_color(colors.line)
            .child(
                item("rail-library", Icon::new(RailIcon::LayoutGrid), shell.surface == Surface::Library, true, Box::new(ShowLibrary))
                    .tooltip(crate::shell::title_bar::tooltip("Library")),
            )
            .when(can_develop, |r| {
                r.child(
                    item(
                        "rail-develop",
                        Icon::new(RailIcon::SlidersHorizontal),
                        shell.surface == Surface::Develop,
                        develop_enabled,
                        Box::new(OpenDevelop),
                    )
                    .tooltip(crate::shell::title_bar::tooltip("Develop the selected photo (crop & tone)")),
                )
            })
            .children(module_items)
            .child(div().flex_1())
            .child(
                item("rail-preferences", Icon::new(IconName::Settings), false, true, Box::new(OpenPreferences))
                    .tooltip(crate::shell::title_bar::tooltip("Preferences")),
            )
            .into_any_element()
    }

    pub(crate) fn render_collection_browser(
        &self,
        shell: &ShellState,
        colors: Colors,
        module_panels: Option<AnyElement>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let all_on = shell.is_all_scope();
        let row = |id: &'static str| {
            div()
                .id(id)
                .flex()
                .items_center()
                .w_full()
                .h(px(25.))
                .px(px(14.))
                .cursor_pointer()
                .text_color(colors.dim)
                .hover(|s| s.text_color(colors.txt).bg(colors.elev))
        };
        let header = |label: &'static str| {
            div()
                .flex()
                .items_center()
                .gap(px(7.))
                .w_full()
                .h(px(28.))
                .px(px(14.))
                .mt(px(4.))
                .child(div().flex_none().text_size(px(10.)).text_color(colors.mute).child(label))
                .child(div().flex_1().h(px(1.)).bg(colors.line))
        };

        let mut browser = div()
            .id("collection-browser")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.tag_panel.read(cx).scroll)
            .bg(colors.panel)
            .text_size(px(12.5))
            .child(header("library"))
            .child(
                row("browser-all")
                    .when(all_on, |r| {
                        r.text_color(colors.txt).child(
                            // `.brow-li.on::before`: the accent tick.
                            div().w(px(2.)).h(px(13.)).ml(px(-10.)).mr(px(8.)).rounded(px(1.)).bg(colors.accent),
                        )
                    })
                    .child("All photos")
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(ShowAllPhotos), cx))
                    .test_support(),
            )
            .child(
                row("browser-trash")
                    .child("Trash")
                    .when_some(shell.counts.trash, |r, n| {
                        r.child(div().ml_auto().text_size(px(10.5)).text_color(colors.mute).child(grouped(n)))
                    })
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(OpenTrash), cx))
                    .test_support(),
            );

        for section in Section::ALL {
            let open = shell.section_open(section);
            browser = browser.child(
                div()
                    .id(SharedString::from(format!("section-{}", section.label().replace(' ', "-"))))
                    .flex()
                    .items_center()
                    .gap(px(7.))
                    .w_full()
                    .h(px(28.))
                    .px(px(14.))
                    .mt(px(4.))
                    .cursor_pointer()
                    .child(div().flex_none().w(px(9.)).text_size(px(9.)).text_color(colors.mute).child(if open { "▾" } else { "▸" }))
                    .child(div().flex_none().text_size(px(10.)).text_color(colors.mute).child(section.label()))
                    .child(div().flex_1().h(px(1.)).bg(colors.line))
                    .on_click(cx.listener(move |this, _, _, cx| this.shell.update(cx, |s, cx| s.toggle_section(section, cx))))
                    .test_support(),
            );
            if open && section == Section::Batches {
                browser = browser.child(self.render_batches(shell, colors, cx));
            } else if open && section == Section::Tags {
                browser = browser.child(self.tag_panel.clone());
            } else if open {
                browser = browser.child(
                    div()
                        .px(px(14.))
                        .py(px(4.))
                        .text_size(px(11.))
                        .text_color(colors.mute)
                        .child(section_placeholder(section)),
                );
            }
        }
        // The enabled modules' sidebar panels.
        browser = browser.child(div().id("module-slot-sidebar").flex().flex_col().children(module_panels));
        browser.into_any_element()
    }
}

gpui_kit::assets::icon_assets!(pub RailIcons, [LayoutGrid, SlidersHorizontal]);

/// The rail's two glyphs that gpui-kit's default icon bundle lacks: the React rail's grid
/// and sliders. Served by [`RailIcons`] through `crate::assets::Assets`.
#[derive(Clone, Copy)]
pub enum RailIcon {
    LayoutGrid,
    SlidersHorizontal,
}

impl gpui_kit::component::IconNamed for RailIcon {
    fn path(self) -> SharedString {
        match self {
            RailIcon::LayoutGrid => gpui_kit::assets::IconName::LayoutGrid.path(),
            RailIcon::SlidersHorizontal => gpui_kit::assets::IconName::SlidersHorizontal.path(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::AssetSource as _;

    #[test]
    fn rail_order_puts_the_preferred_views_first_then_the_rest_in_input_order() {
        let views = ["collage", "tag-graph", "map", "slideshow", "people"];
        let ordered: Vec<&&str> = rail_order(&views, |v| v);
        assert_eq!(ordered, [&"map", &"people", &"tag-graph", &"collage", &"slideshow"]);
        let none: [&str; 0] = [];
        assert!(rail_order(&none, |v| v).is_empty());
    }

    #[test]
    fn the_rail_icons_are_served() {
        for icon in [RailIcon::LayoutGrid, RailIcon::SlidersHorizontal] {
            let path = gpui_kit::component::IconNamed::path(icon);
            assert!(crate::assets::Assets.load(&path).unwrap().is_some(), "{path}");
        }
    }
}
