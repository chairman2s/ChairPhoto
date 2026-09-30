//! The root view: for now the open catalog's name and photo count, what the app is doing, and
//! a live line fed by core events — the proof that the window, the core and the event bridge
//! are wired. The shell chrome (#105) replaces the body; the root context and focus stay.

use crate::keymap::{contexts, ReloadTheme};
use crate::model::AppModel;
use crate::theme::{Palette, FONT_DISPLAY};
use gpui_kit::component::{ActiveTheme as _, Colorize as _};
use gpui_kit::prelude::*;
use gpui_kit::{div, px, Context, Entity, FocusHandle, Hsla, Subscription, Window};

pub struct RootView {
    model: Entity<AppModel>,
    focus: FocusHandle,
    _model_changed: Subscription,
}

impl RootView {
    /// The root owns focus from the start, so the app-wide bindings in [`contexts::ROOT`]
    /// work before anything else takes it.
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let _model_changed = cx.observe(&model, |_, _, cx| cx.notify());
        Self { model, focus, _model_changed }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// Re-read the system theme off the UI thread and apply it.
    fn reload_theme(&mut self, cx: &mut Context<Self>) {
        let read = cx
            .background_executor()
            .spawn(async { chairphoto_core::appearance::read_current_theme() });
        cx.spawn(async move |_, cx| {
            let result = read.await;
            cx.update(|cx| crate::theme::apply_system_theme(&result, cx));
        })
        .detach();
    }
}

impl Render for RootView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (fg, bg, dim) = (theme.foreground, theme.background, theme.muted_foreground);
        let (panel, border, accent) = (theme.sidebar, theme.border, theme.primary);
        // The third text tier has no gpui field; it lives in the Palette global.
        let mute: Hsla = cx
            .try_global::<Palette>()
            .and_then(|p| Hsla::parse_hex(&p.tokens.mute).ok())
            .unwrap_or(dim);
        let model = self.model.read(cx);

        let catalog_line = match &model.catalog {
            Some(c) => format!("{} · {} photos", c.name, c.photo_count),
            None => "No catalog open".to_string(),
        };
        let last_event = model
            .last_event
            .clone()
            .unwrap_or_else(|| "Waiting for core events…".into());

        div()
            .id("root")
            .key_context(contexts::ROOT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &ReloadTheme, _, cx| this.reload_theme(cx)))
            .size_full()
            .flex()
            .flex_col()
            .bg(bg)
            .text_color(fg)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .h(px(46.))
                    .px_4()
                    .bg(panel)
                    .border_b_1()
                    .border_color(border)
                    .child(div().font_family(FONT_DISPLAY).text_xl().child("ChairPhoto"))
                    .child(
                        div()
                            .id("catalog")
                            .px_3()
                            .py_1()
                            .rounded_full()
                            .border_1()
                            .border_color(border)
                            .child(catalog_line),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .items_center()
                    .gap_2()
                    .child(div().text_color(dim).child(model.status.clone()))
                    .child(
                        div()
                            .id("last-event")
                            .text_sm()
                            .text_color(accent)
                            .child(last_event),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(mute)
                            .child(format!("{} core events · Ctrl+Q quits", model.events_seen)),
                    ),
            )
    }
}
