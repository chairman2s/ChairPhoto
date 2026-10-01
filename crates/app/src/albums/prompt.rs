//! The name prompt the albums sections ask with (`window.prompt` in `AlbumsPanel.tsx` and
//! `SmartAlbumsPanel.tsx`): one field, prefilled when renaming; Enter or OK submits a
//! non-empty, changed name; Cancel or Escape closes without asking anything.

use super::state::{bind_dialog, AlbumsState};
use crate::shell::style::Colors;
use crate::storage::{ui, CloseDialog};
use chairphoto_core::app::CatalogIdentity;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::*;
use gpui_kit::{Context, Entity, EventEmitter, Focusable as _, Subscription, Window};

/// What the prompt was opened for; the root view runs it on submit.
#[derive(Debug, Clone, PartialEq)]
pub enum PromptFor {
    NewAlbum,
    RenameAlbum(i64),
    RenameSmartAlbum(i64),
}

/// The name the user submitted.
#[derive(Debug, Clone, PartialEq)]
pub struct Submitted(pub PromptFor, pub String);

pub struct NamePrompt {
    pub what: PromptFor,
    pub input: Entity<InputState>,
    /// The name it opened with: submitting it unchanged does nothing (React).
    initial: String,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<CloseDialog> for NamePrompt {}
impl EventEmitter<Submitted> for NamePrompt {}

impl NamePrompt {
    pub fn new(
        albums: &Entity<AlbumsState>,
        from: Option<CatalogIdentity>,
        what: PromptFor,
        initial: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).default_value(initial.clone()));
        let enter = cx.subscribe(&input, |this: &mut Self, _, event: &InputEvent, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.submit(cx);
            }
        });
        let focus = input.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        let mut subs = bind_dialog(albums, from, |s| s.lists_from(), cx);
        subs.push(enter);
        NamePrompt { what, input, initial, _subscriptions: subs }
    }

    /// OK / Enter: a trimmed, non-empty name that differs from the one it opened with.
    pub fn submit(&mut self, cx: &mut Context<Self>) {
        let name = self.input.read(cx).value().trim().to_string();
        if !name.is_empty() && name != self.initial {
            cx.emit(Submitted(self.what.clone(), name));
        }
        cx.emit(CloseDialog);
    }
}

impl Render for NamePrompt {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        ui::body().id("album-prompt").child(Input::new(&self.input).id("album-prompt-name")).child(
            ui::row()
                .child(ui::clickable(
                    ui::chip("album-prompt-cancel", "Cancel", true, colors),
                    true,
                    cx.listener(|_, _, _, cx| cx.emit(CloseDialog)),
                ))
                .child(ui::clickable(ui::primary("album-prompt-ok", "OK", true, colors), true, cx.listener(|s, _, _, cx| s.submit(cx)))),
        )
    }
}
