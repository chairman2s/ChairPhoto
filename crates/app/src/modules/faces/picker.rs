//! [`PersonPicker`]: the searchable person list (`PersonPicker` in faces.tsx), shared by the
//! loupe overlay's reassign dropdown and the inspector's reassign row. A filter field on top
//! (focused when it opens), the matching tags below, and "＋ Create “name”" when the typed name
//! matches no tag. ↑/↓ move the highlight, Enter picks it (or creates), Esc cancels.
//!
//! The keys are taken by a keystroke **interceptor** while the field has focus, before any
//! binding: otherwise Esc would also close the loupe (its `CloseLoupe`) and Enter would toggle
//! it, since the field sits inside the loupe's key context.

use super::logic::{picker_rows, step_highlight, PickerRow};
use crate::shell::style::Colors;
use chairphoto_core::catalog::Tag;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::*;
use gpui_kit::{div, px, Context, EventEmitter, Focusable as _, SharedString, Subscription, TestSupportExt as _, Window};

/// What the picker asks its owner to do.
#[derive(Debug, Clone, PartialEq)]
pub enum PickerEvent {
    Pick { tag_id: i64, name: String },
    Create(String),
    Cancel,
}

pub struct PersonPicker {
    pub input: gpui_kit::Entity<InputState>,
    tags: Vec<Tag>,
    current: Option<i64>,
    pub highlight: usize,
    /// Element-id prefix (several pickers can be on screen).
    id: SharedString,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PickerEvent> for PersonPicker {}

impl PersonPicker {
    pub fn new(
        id: impl Into<SharedString>,
        tags: Vec<Tag>,
        current: Option<i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search or create person…"));
        input.update(cx, |i, cx| i.focus(window, cx));
        let change = cx.subscribe(&input, |this: &mut Self, _, e: &InputEvent, cx| {
            if matches!(e, InputEvent::Change) {
                this.highlight = 0;
                cx.notify();
            }
        });
        let window_handle = window.window_handle();
        let this = cx.entity().downgrade();
        let keys = cx.intercept_keystrokes({
            move |event, window, cx| {
                if window.window_handle() != window_handle {
                    return;
                }
                let Some(this) = this.upgrade() else { return };
                let focused = this.read(cx).input.read(cx).focus_handle(cx).is_focused(window);
                let k = &event.keystroke;
                if !focused || k.modifiers.control || k.modifiers.alt || k.modifiers.platform {
                    return;
                }
                let handled = match k.key.as_str() {
                    "up" => this.update(cx, |p, cx| p.step(-1, cx)),
                    "down" => this.update(cx, |p, cx| p.step(1, cx)),
                    "enter" => this.update(cx, |p, cx| p.choose(None, cx)),
                    "escape" => {
                        this.update(cx, |_, cx| cx.emit(PickerEvent::Cancel));
                        true
                    }
                    _ => false,
                };
                if handled {
                    cx.stop_propagation();
                }
            }
        });
        PersonPicker { input, tags, current, highlight: 0, id: id.into(), _subscriptions: vec![change, keys] }
    }

    pub fn rows(&self, cx: &gpui_kit::App) -> Vec<PickerRow> {
        picker_rows(&self.tags, &self.input.read(cx).value(), true)
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        let len = self.rows(cx).len();
        self.highlight = step_highlight(self.highlight, delta, len);
        cx.notify();
        true
    }

    /// Pick row `index` (the highlighted one when `None`). Returns whether a row was there.
    pub fn choose(&mut self, index: Option<usize>, cx: &mut Context<Self>) -> bool {
        let rows = self.rows(cx);
        let i = index.unwrap_or(self.highlight.min(rows.len().saturating_sub(1)));
        match rows.into_iter().nth(i) {
            Some(PickerRow::Tag { id, name, .. }) => cx.emit(PickerEvent::Pick { tag_id: id, name }),
            Some(PickerRow::Create(name)) => cx.emit(PickerEvent::Create(name)),
            None => return false,
        }
        true
    }
}

impl Render for PersonPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let rows = self.rows(cx);
        let highlight = self.highlight.min(rows.len().saturating_sub(1));
        let empty = rows.is_empty().then(|| {
            div()
                .px(px(10.))
                .py(px(4.))
                .text_size(px(11.))
                .text_color(colors.mute)
                .child(if self.tags.is_empty() { "No tags found" } else { "No match" })
        });
        let mut list = div().id(SharedString::from(format!("{}-list", self.id))).flex().flex_col().max_h(px(160.)).overflow_y_scroll();
        for (i, row) in rows.into_iter().enumerate() {
            let (label, color) = match &row {
                PickerRow::Tag { id, full_path, .. } => {
                    (full_path.clone(), if Some(*id) == self.current { colors.ok } else { colors.txt })
                }
                PickerRow::Create(name) => (format!("＋ Create “{name}”"), colors.accent),
            };
            list = list.child(
                div()
                    .id(SharedString::from(format!("{}-row-{i}", self.id)))
                    .px(px(10.))
                    .py(px(4.))
                    .text_size(px(12.))
                    .text_color(color)
                    .cursor_pointer()
                    .when(i == highlight, |d| d.bg(colors.sel))
                    .child(label.clone())
                    .aria_label(label)
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if *hovered {
                            this.highlight = i;
                            cx.notify();
                        }
                    }))
                    .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.choose(Some(i), cx);
                    }))
                    .test_support(),
            );
        }
        div()
            .id(self.id.clone())
            .flex()
            .flex_col()
            .gap(px(4.))
            .min_w(px(200.))
            .child(Input::new(&self.input))
            .child(list)
            .children(empty)
            .test_support()
    }
}
