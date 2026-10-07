//! Preferences → Appearance (`AppearanceSection`): Follow Omarchy / ChairPhoto Standard, a
//! per-machine preference ([`crate::theme::set_mode`]), and under Follow the live line saying
//! which theme is followed. Opening the tab while following re-reads the system theme, as
//! React's mount did; the line follows every later `appearance:theme_changed`.

use super::{section, status};
use crate::shell::style::Colors;
use crate::storage::ui;
use crate::theme::{self, Appearance, AppearanceMode};
use gpui_kit::prelude::*;
use gpui_kit::{Context, Subscription, Window};

pub struct AppearanceSection {
    _observe: Subscription,
}

impl AppearanceSection {
    pub fn new(cx: &mut Context<Self>) -> Self {
        if theme::mode(cx) == AppearanceMode::FollowOmarchy {
            theme::reread_system_theme(cx);
        }
        AppearanceSection { _observe: cx.observe_global::<Appearance>(|_, cx| cx.notify()) }
    }

    pub fn choose(&mut self, mode: AppearanceMode, cx: &mut Context<Self>) {
        theme::set_mode(mode, cx);
        cx.notify();
    }
}

impl Render for AppearanceSection {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let mode = theme::mode(cx);
        let mut seg = ui::row();
        for (value, id, label) in [
            (AppearanceMode::FollowOmarchy, "appearance-follow", "Follow Omarchy"),
            (AppearanceMode::Standard, "appearance-standard", "ChairPhoto Standard"),
        ] {
            let on = mode == value;
            let chip = ui::chip(id, label, true, colors).when(on, |c| c.border_color(colors.accent).text_color(colors.accent));
            seg = seg.child(ui::clickable(chip, true, cx.listener(move |s, _, _, cx| s.choose(value, cx))));
        }
        let line = (mode == AppearanceMode::FollowOmarchy)
            .then(|| cx.try_global::<Appearance>().map(|a| theme::status_line(&a.system)))
            .flatten();
        section("prefs-appearance", "Appearance", colors)
            .child(ui::sub(
                "Choose the palette ChairPhoto renders with. This is a per-machine preference — it isn't saved to the \
                 catalog and doesn't travel with it between computers.",
                colors,
            ))
            .child(seg)
            .children(line.map(|l| status("appearance-status", l, colors)))
    }
}
