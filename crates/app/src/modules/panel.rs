//! The module UI the shell opens as dialogs: the Modules panel (`ModulesPanel.tsx`) and the
//! Publish dialog (`PublishDialog.tsx`), plus the slot renderers the shell's columns share.
//!
//! **Modules panel.** One row per registered module, in registration order: name,
//! description, "Requires: …" (an unavailable requirement marked), "backend … not included
//! in this build", any other reason it cannot be enabled, and the enabled checkbox (disabled
//! while blocked). An enabled module's settings panels render under its row until
//! Preferences (#113) gives each module its own tab and takes this panel in as its Modules
//! tab. Dropped from React (parity.md, #104): the external-modules section, the install hint,
//! versions, and the permission and network-access review.

use super::registry::{ModuleRegistry, SlotView};
use super::PanelSlot;
use crate::shell::style::Colors;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::{Disableable as _, WindowExt as _};
use gpui_kit::prelude::*;
use gpui_kit::{div, px, AnyElement, App, Context, Entity, FontWeight, SharedString, Subscription, TestSupportExt as _, Window};

/// The Modules panel's width in its dialog.
const PANEL_W: f32 = 560.;

/// Open the Modules panel in a dialog.
pub fn open_modules_panel(registry: &Entity<ModuleRegistry>, window: &mut Window, cx: &mut App) {
    let panel = cx.new(|cx| ModulesPanel::new(registry.clone(), cx));
    window.open_dialog(cx, move |dialog, _, _| dialog.title("Modules").w(px(PANEL_W)).child(panel.clone()));
}

/// Open the Publish dialog over the enabled modules' publish targets.
pub fn open_publish_dialog(registry: &Entity<ModuleRegistry>, window: &mut Window, cx: &mut App) {
    let targets = ModuleRegistry::publish_target_views(registry, window, cx);
    let dialog_view = cx.new(|_| PublishDialog { targets, selected: 0 });
    window.open_dialog(cx, move |dialog, _, _| dialog.title("Publish").w(px(PANEL_W)).child(dialog_view.clone()));
}

/// The Modules panel (`ModulesSection` in ModulesPanel.tsx).
pub struct ModulesPanel {
    registry: Entity<ModuleRegistry>,
    _observe: Subscription,
}

impl ModulesPanel {
    pub fn new(registry: Entity<ModuleRegistry>, cx: &mut Context<Self>) -> Self {
        let _observe = cx.observe(&registry, |_, _, cx| cx.notify());
        ModulesPanel { registry, _observe }
    }
}

impl Render for ModulesPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let rows = self.registry.read(cx).list();
        let mut list = div().id("modules-panel").flex().flex_col().gap(px(10.)).text_size(px(12.));
        if rows.is_empty() {
            return list.child(div().text_color(colors.mute).child("No modules in this build.")).into_any_element();
        }
        for m in rows {
            let settings = if m.enabled { ModuleRegistry::settings_views(&self.registry, &m.id, window, cx) } else { Vec::new() };
            let blocked = !m.enabled && m.blocked_reason.is_some();
            let registry = self.registry.clone();
            let id = m.id.clone();
            let mut info = div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap(px(2.))
                .child(div().font_weight(FontWeight::SEMIBOLD).text_color(colors.txt).child(m.name.clone()));
            if !m.description.is_empty() {
                info = info.child(div().text_color(colors.dim).child(m.description.clone()));
            }
            if !m.requires.is_empty() {
                let mut line = div().flex().flex_row().flex_wrap().text_color(colors.dim).child("Requires: ");
                for (i, req) in m.requires.iter().enumerate() {
                    let text = format!("{}{}{}", if i > 0 { ", " } else { "" }, req.name, if req.met { "" } else { " (unavailable)" });
                    line = line.child(div().when(!req.met, |d| d.text_color(colors.danger)).child(text));
                }
                info = info.child(line);
            }
            if !m.backend_available {
                let feature = m.backend_feature.clone().unwrap_or_default();
                info = info.child(
                    div().text_color(colors.danger).child(format!("backend \u{201c}{feature}\u{201d} not included in this build")),
                );
            }
            if let (true, Some(reason), true) = (!m.enabled, m.blocked_reason.clone(), m.backend_available) {
                info = info.child(div().text_color(colors.danger).child(reason));
            }
            let toggle = Checkbox::new(SharedString::from(format!("module-toggle-{}", m.id)))
                .label("enabled")
                .checked(m.enabled)
                .disabled(blocked)
                .on_change(move |checked, _, cx| {
                    if *checked {
                        ModuleRegistry::enable(&registry, &id, cx);
                    } else {
                        ModuleRegistry::disable(&registry, &id, cx);
                    }
                });
            let row = div()
                .id(SharedString::from(format!("module-row-{}", m.id)))
                .flex()
                .flex_col()
                .gap(px(6.))
                .pb(px(10.))
                .border_b_1()
                .border_color(colors.line)
                .child(div().flex().flex_row().items_start().gap(px(12.)).child(info).child(div().flex_none().child(toggle)))
                .children(settings.into_iter().map(|view| div().pl(px(12.)).child(view)))
                .test_support();
            list = list.child(row);
        }
        list.into_any_element()
    }
}

/// The Publish dialog: a chip per publish target, and the chosen target's form.
pub struct PublishDialog {
    targets: Vec<SlotView>,
    selected: usize,
}

impl Render for PublishDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let body = div().id("publish-dialog").flex().flex_col().gap(px(10.)).text_size(px(12.));
        if self.targets.is_empty() {
            return body
                .child(
                    div()
                        .text_color(colors.mute)
                        .child("Enable a publishing module (Instagram, Flickr, SmugMug) in Modules first."),
                )
                .into_any_element();
        }
        let selected = self.selected.min(self.targets.len() - 1);
        let mut chips = div().flex().flex_row().flex_wrap().gap(px(6.));
        for (i, t) in self.targets.iter().enumerate() {
            let on = i == selected;
            chips = chips.child(
                div()
                    .id(SharedString::from(format!("publish-target-{}", t.id)))
                    .px(px(10.))
                    .py(px(3.))
                    .rounded_full()
                    .border_1()
                    .border_color(if on { colors.accent } else { colors.border })
                    .text_color(if on { colors.accent } else { colors.dim })
                    .cursor_pointer()
                    .child(t.label.clone())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected = i;
                        cx.notify();
                    }))
                    .test_support(),
            );
        }
        body.child(chips).child(self.targets[selected].view.clone()).into_any_element()
    }
}

/// Render the enabled modules' panels at `slot` as labelled blocks (`.ins-block` +
/// `.ins-label`), one per panel; `None` when no enabled module contributes there. The
/// inspector and the collection browser use it; the loupe (#109) and the tag editor (#107)
/// mount [`ModuleRegistry::panel_views`] directly.
pub fn render_panel_blocks(
    registry: &Entity<ModuleRegistry>,
    slot: PanelSlot,
    colors: Colors,
    window: &mut Window,
    cx: &mut App,
) -> Option<AnyElement> {
    let views = ModuleRegistry::panel_views(registry, slot, window, cx);
    if views.is_empty() {
        return None;
    }
    let blocks = views.into_iter().map(|v| {
        div()
            .id(SharedString::from(format!("module-panel-{}-{}", slot.name(), v.id)))
            .flex()
            .flex_col()
            .gap(px(6.))
            .px(px(14.))
            .py(px(8.))
            .child(
                div()
                    .text_size(px(10.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(colors.mute)
                    .child(v.label.to_uppercase()),
            )
            .child(v.view)
            .test_support()
    });
    Some(div().flex().flex_col().children(blocks).into_any_element())
}
