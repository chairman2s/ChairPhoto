//! The inspector column's chrome (`src/components/shell/Inspector.tsx`): a filename
//! header (the UI sans, not the display serif — App.css: filenames are identifiers, not
//! titles) with the photo's colour-label swatch and a hide button, the lowercase tab row
//! (details / tags / versions / publish), and the body the Photo inspector (#108) fills.
//! The tags tab shows the tagging block ([`crate::tags::photo_tags::PhotoTags`]: chips,
//! copy/paste, add-tag, nearby, quick-tag groups), which #108 keeps when it ports the tab
//! content; the other tabs wait for #108. The enabled
//! modules' inspector panels ([`crate::modules::PanelSlot::Inspector`]) render on the tags tab
//! under its built-in blocks, as `PhotoInspector.tsx` did, while a photo is active.

use crate::shell::state::{InspectorTab, ShellState, Side};
use crate::shell::style::{Colors, COLOR_LABELS};
use crate::view::RootView;
use gpui_kit::component::{Icon, IconName};
use gpui_kit::prelude::*;
use gpui_kit::{div, px, AnyElement, Context, FontWeight, SharedString, TestSupportExt as _};

impl RootView {
    pub(crate) fn render_inspector(
        &self,
        shell: &ShellState,
        colors: Colors,
        module_panels: Option<AnyElement>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let module_panels =
            module_panels.filter(|_| shell.inspector_tab == InspectorTab::Tags && shell.library.selection().active.is_some());
        let active = shell.library.selection().active;
        let filename = active.map(|p| p.path.rsplit('/').next().unwrap_or(&p.path).to_string()).unwrap_or_default();
        let swatch = active.and_then(|p| COLOR_LABELS.iter().find(|l| l.name.eq_ignore_ascii_case(&p.label)));

        let mut tabs = div().flex().flex_none().gap(px(16.)).px(px(14.)).pb(px(9.)).border_b_1().border_color(colors.line);
        for tab in InspectorTab::ALL {
            let on = shell.inspector_tab == tab;
            tabs = tabs.child(
                div()
                    .id(SharedString::from(format!("inspector-tab-{}", tab.label())))
                    .pb(px(3.))
                    .text_size(px(11.))
                    .cursor_pointer()
                    .child(tab.label())
                    .when(on, |t| t.text_color(colors.txt).border_b_2().border_color(colors.accent))
                    .when(!on, |t| t.text_color(colors.mute).hover(|s| s.text_color(colors.dim)))
                    .on_click(cx.listener(move |this, _, _, cx| this.shell.update(cx, |s, cx| s.set_inspector_tab(tab, cx))))
                    .test_support(),
            );
        }

        div()
            .id("inspector")
            .flex()
            .flex_col()
            .size_full()
            .bg(colors.panel)
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(9.))
                    .pt(px(13.))
                    .px(px(14.))
                    .pb(px(11.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_size(px(17.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors.txt)
                            .child(filename),
                    )
                    .when_some(swatch, |h, l| h.child(div().flex_none().size(px(10.)).rounded_full().bg(l.color())))
                    .child(
                        div()
                            .id("inspector-hide")
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(24.))
                            .rounded(px(6.))
                            .cursor_pointer()
                            .text_color(colors.mute)
                            .hover(|s| s.text_color(colors.txt).bg(colors.elev))
                            .child(Icon::new(IconName::ChevronRight).size(px(14.)))
                            .tooltip(crate::shell::title_bar::tooltip("Hide the inspector"))
                            .on_click(cx.listener(|this, _, _, cx| this.shell.update(cx, |s, cx| s.hide_panel(Side::Right, cx))))
                            .test_support(),
                    ),
            )
            .child(tabs)
            .child(match (active, shell.inspector_tab) {
                (Some(_), InspectorTab::Tags) => div()
                    .id("inspector-tags-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(self.photo_tags.clone())
                    .into_any_element(),
                _ => div()
                    .flex_1()
                    .p(px(16.))
                    .text_size(px(11.))
                    .text_color(colors.mute)
                    .child(match active {
                        Some(_) => "Photo inspector — not yet ported (#108)",
                        None => "Select a photo",
                    })
                    .into_any_element(),
            })
            .children(module_panels.map(|panels| div().id("module-slot-inspector").flex().flex_col().child(panels)))
            .into_any_element()
    }
}
