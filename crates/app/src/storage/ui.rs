//! The small pieces the storage dialogs share, after App.css's modal classes: `.chip`,
//! `.chip-danger`, `.scan-btn`, `.modal-sub`, `.modal-error`, `.term-note`, `.panel-empty`.

use crate::shell::style::{Colors, RADIUS};
use gpui_kit::prelude::*;
use gpui_kit::{div, px, AnyElement, Div, FontWeight, SharedString, Stateful, TestSupportExt as _};

/// `.chip`: a small bordered button. Disabled chips are dimmed and take no clicks (the
/// caller adds `on_click` only when enabled — see [`clickable`]).
pub fn chip(id: impl Into<SharedString>, label: impl Into<SharedString>, enabled: bool, colors: Colors) -> Stateful<Div> {
    div()
        .id(id.into())
        .flex()
        .flex_none()
        .items_center()
        .h(px(24.))
        .px(px(10.))
        .border_1()
        .border_color(colors.border)
        .rounded_full()
        .text_size(px(11.5))
        .text_color(colors.dim)
        .child(label.into())
        .when(enabled, |b| b.cursor_pointer().hover(|s| s.border_color(colors.dim).text_color(colors.txt)))
        .when(!enabled, |b| b.opacity(0.4))
}

/// `.chip-danger` / `.trash-danger`.
pub fn danger_chip(id: impl Into<SharedString>, label: impl Into<SharedString>, enabled: bool, colors: Colors) -> Stateful<Div> {
    chip(id, label, enabled, colors).text_color(colors.danger).border_color(colors.danger)
}

/// `.scan-btn`: the dialog's primary action.
pub fn primary(id: impl Into<SharedString>, label: impl Into<SharedString>, enabled: bool, colors: Colors) -> Stateful<Div> {
    div()
        .id(id.into())
        .flex()
        .flex_none()
        .items_center()
        .h(px(28.))
        .px(px(14.))
        .rounded(RADIUS)
        .bg(colors.accent)
        .text_color(colors.onaccent)
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .child(label.into())
        .when(enabled, |b| b.cursor_pointer())
        .when(!enabled, |b| b.opacity(0.4))
}

/// Attach `on_click` only when `enabled`, and make the element findable in tests.
pub fn clickable(
    el: Stateful<Div>,
    enabled: bool,
    f: impl Fn(&gpui_kit::ClickEvent, &mut gpui_kit::Window, &mut gpui_kit::App) + 'static,
) -> AnyElement {
    let el = if enabled { el.on_click(f) } else { el };
    el.test_support().into_any_element()
}

/// `.modal-sub`.
pub fn sub(text: impl Into<SharedString>, colors: Colors) -> Div {
    div().text_size(px(11.5)).text_color(colors.dim).child(text.into())
}

/// `.modal-error`.
pub fn error(id: &'static str, text: impl Into<SharedString>, colors: Colors) -> AnyElement {
    div().id(id).text_size(px(11.5)).text_color(colors.danger).child(text.into()).test_support().into_any_element()
}

/// `.field > label`.
pub fn label(text: impl Into<SharedString>, colors: Colors) -> Div {
    div().text_size(px(11.)).text_color(colors.mute).child(text.into())
}

/// `.panel-empty`.
pub fn empty(id: &'static str, text: impl Into<SharedString>, colors: Colors) -> AnyElement {
    div().id(id).py(px(12.)).text_size(px(12.)).text_color(colors.mute).child(text.into()).test_support().into_any_element()
}

/// A row of controls.
pub fn row() -> Div {
    div().flex().flex_row().flex_wrap().items_center().gap(px(8.))
}

/// The body column every dialog renders into.
pub fn body() -> Div {
    div().flex().flex_col().gap(px(10.)).w_full()
}

/// "1 photo" / "3 photos".
pub fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// A destructive confirm: gpui-component's `AlertDialog` through a oneshot (shell-apis.md
/// § 5). `true` only for the OK button; Cancel, Escape or a dismissal is `false` (a dropped
/// sender reads as `Err`, which callers treat as `false`).
pub fn confirm(
    window: &mut gpui_kit::Window,
    cx: &mut gpui_kit::App,
    title: SharedString,
    body: SharedString,
    ok_text: &'static str,
) -> futures::channel::oneshot::Receiver<bool> {
    use gpui_kit::component::button::ButtonVariant;
    use gpui_kit::component::WindowExt as _;
    use std::cell::RefCell;
    use std::rc::Rc;
    let (tx, rx) = futures::channel::oneshot::channel();
    let tx = Rc::new(RefCell::new(Some(tx)));
    window.open_alert_dialog(cx, move |alert, _, _| {
        let (ok, cancel) = (tx.clone(), tx.clone());
        alert
            .title(title.clone())
            .description(body.clone())
            .confirm()
            .ok_text(ok_text)
            .ok_variant(ButtonVariant::Danger)
            .on_ok(move |_, _, _| {
                if let Some(t) = ok.borrow_mut().take() {
                    let _ = t.send(true);
                }
                true
            })
            .on_cancel(move |_, _, _| {
                if let Some(t) = cancel.borrow_mut().take() {
                    let _ = t.send(false);
                }
                true
            })
    });
    rx
}
