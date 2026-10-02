//! The Obsidian views over [`ObsidianState`]: the inspector's "Note" panel ([`NotePanel`]),
//! the tag editor's "Obsidian note" section ([`TagNotePanel`]) and the settings panel
//! ([`ObsidianSettings`]).
//!
//! - Note / Obsidian note: "Create note in Obsidian" ("Creating…" while it runs); once the
//!   subject has a note, its name (the full vault path as tooltip), "Open note" and "Forget"
//!   (tooltip: the note stays in the vault). The tag section adds React's explanation line.
//! - Settings: "Vault name" (placeholder "My vault"), "Notes folder (inside the vault)"
//!   (placeholder "ChairPhoto"), Save — refused with a reason when a value is not valid;
//!   a save says "Obsidian settings saved" on the status line (React's toast).

use super::state::{Kind, NoteView, ObsidianState};
use crate::shell::style::Colors;
use crate::storage::ui;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::*;
use gpui_kit::{div, px, AnyElement, App, Context, Entity, FontWeight, SharedString, Subscription, TestSupportExt as _, Window};

/// Element ids of one kind's block: the photo's are `obsidian-*`, the tag's `obsidian-tag-*`.
pub fn element_id(kind: Kind, what: &str) -> SharedString {
    match kind {
        Kind::Photo => format!("obsidian-{what}").into(),
        Kind::Tag => format!("obsidian-tag-{what}").into(),
    }
}

/// The block both panels render for their subject.
fn note_block(state: &Entity<ObsidianState>, kind: Kind, colors: Colors, cx: &App) -> AnyElement {
    let s = state.read(cx);
    let slot = s.slot(kind);
    let body = div().id(element_id(kind, "panel")).flex().flex_col().gap(px(6.)).text_size(px(12.));
    let linked = match &slot.view {
        NoteView::Ready(l) => l,
        NoteView::Failed(_, e) => {
            let error = match kind {
                Kind::Photo => "obsidian-error",
                Kind::Tag => "obsidian-tag-error",
            };
            return body.child(ui::error(error, format!("Note unavailable: {e}"), colors)).test_support().into_any_element();
        }
        NoteView::Loading(_) => return body.test_support().into_any_element(),
        NoteView::None => {
            return match kind {
                Kind::Photo => ui::empty("obsidian-none", "Select a photo.", colors),
                Kind::Tag => body.test_support().into_any_element(),
            }
        }
    };
    let id = |what: &str| element_id(kind, what);
    if let Some(record) = &linked.record {
        let (file, name) = (SharedString::from(record.file.clone()), SharedString::from(record.name().to_string()));
        let (open, forget) = (state.clone(), state.clone());
        return body
            .child(
                div()
                    .id(id("file"))
                    .text_color(colors.dim)
                    .child(name.clone())
                    .tooltip(move |window, cx| Tooltip::new(file.clone()).build(window, cx))
                    .aria_label(name)
                    .test_support(),
            )
            .child(
                ui::row()
                    .gap(px(6.))
                    .child(ui::clickable(ui::primary(id("open"), "Open note", true, colors), true, move |_, _, cx| {
                        open.update(cx, |s, cx| s.open(kind, cx))
                    }))
                    .child(ui::clickable(
                        ui::chip(id("forget"), "Forget", true, colors).tooltip(|window, cx| {
                            Tooltip::new("Forget the link only — the note stays in your vault").build(window, cx)
                        }),
                        true,
                        move |_, _, cx| forget.update(cx, |s, cx| s.forget(kind, cx)),
                    )),
            )
            .test_support()
            .into_any_element();
    }
    let busy = slot.creating;
    let create = state.clone();
    body.when(kind == Kind::Tag, |b| {
        b.child(ui::sub(
            "A companion note for this tag — a place for a detailed explanation of what it means, linked both ways.",
            colors,
        ))
    })
    .child(ui::row().child(ui::clickable(
        ui::primary(id("create"), if busy { "Creating…" } else { "Create note in Obsidian" }, !busy, colors),
        !busy,
        move |_, _, cx| create.update(cx, |s, cx| s.create(kind, cx)),
    )))
    .test_support()
    .into_any_element()
}

/// The inspector's "Note" panel: the active photo's note.
pub struct NotePanel {
    pub state: Entity<ObsidianState>,
    _subscriptions: Vec<Subscription>,
}

impl NotePanel {
    pub fn new(state: Entity<ObsidianState>, cx: &mut Context<Self>) -> Self {
        let _subscriptions = vec![cx.observe(&state, |_, _, cx| cx.notify())];
        NotePanel { state, _subscriptions }
    }
}

impl Render for NotePanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        note_block(&self.state, Kind::Photo, Colors::get(cx), cx)
    }
}

/// The tag editor's "Obsidian note" section: the edited tag's note.
pub struct TagNotePanel {
    pub state: Entity<ObsidianState>,
    _subscriptions: Vec<Subscription>,
}

impl TagNotePanel {
    pub fn new(state: Entity<ObsidianState>, cx: &mut Context<Self>) -> Self {
        let _subscriptions = vec![cx.observe(&state, |_, _, cx| cx.notify())];
        TagNotePanel { state, _subscriptions }
    }
}

impl Render for TagNotePanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        note_block(&self.state, Kind::Tag, Colors::get(cx), cx)
    }
}

/// The settings panel: vault name and notes folder.
pub struct ObsidianSettings {
    pub state: Entity<ObsidianState>,
    pub vault: Entity<InputState>,
    pub folder: Entity<InputState>,
    /// The stored values last put into the fields.
    shown: Option<(String, String)>,
    /// Why the last Save was refused.
    pub error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl ObsidianSettings {
    pub fn new(state: Entity<ObsidianState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let vault = cx.new(|cx| InputState::new(window, cx).placeholder("My vault"));
        let folder = cx.new(|cx| InputState::new(window, cx).placeholder("ChairPhoto"));
        let _subscriptions = vec![cx.observe(&state, |_, _, cx| cx.notify())];
        ObsidianSettings { state, vault, folder, shown: None, error: None, _subscriptions }
    }

    pub fn save(&mut self, cx: &mut Context<Self>) {
        let vault = self.vault.read(cx).value().to_string();
        let folder = self.folder.read(cx).value().to_string();
        self.error = self.state.update(cx, |s, cx| s.save_settings(&vault, &folder, cx)).err();
        cx.notify();
    }
}

impl Render for ObsidianSettings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let s = self.state.read(cx);
        let stored = s.vault.clone().zip(s.folder.clone());
        let ready = s.settings_ready();
        if let Some((vault, folder)) = stored {
            if self.shown.as_ref() != Some(&(vault.clone(), folder.clone())) {
                self.shown = Some((vault.clone(), folder.clone()));
                self.vault.update(cx, |i, cx| i.set_value(vault, window, cx));
                self.folder.update(cx, |i, cx| i.set_value(folder, window, cx));
            }
        }
        ui::body()
            .id("obsidian-settings")
            .text_size(px(12.))
            .child(div().font_weight(FontWeight::SEMIBOLD).text_color(colors.txt).child("Obsidian"))
            .child(ui::label("Vault name", colors))
            .child(Input::new(&self.vault))
            .child(ui::label("Notes folder (inside the vault)", colors))
            .child(Input::new(&self.folder))
            .when_some(self.error.clone(), |b, e| b.child(ui::error("obsidian-settings-error", e, colors)))
            .child(ui::row().child(ui::clickable(
                ui::primary("obsidian-save", "Save", ready, colors),
                ready,
                cx.listener(|this, _, _, cx| this.save(cx)),
            )))
            .test_support()
            .into_any_element()
    }
}
