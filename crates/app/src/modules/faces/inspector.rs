//! The inspector's "Faces" block (`FacesInspectorPanel`): the faces of the photo the inspector
//! shows ([`FacesState::shown_photo`]: Compare's focused pane while Compare is open), each with
//! its number, name, confidence and state, and ✓ confirm, "✓✓ confirm on N" (the whole
//! selection, reported on the status line), ✕ reject, ⇄ reassign (the person picker inline),
//! – ignore and 🗑 delete (drawn boxes only). Every write is [`FacesState`]'s, bound to the
//! catalog the faces were read from.

use super::logic::state_color;
use super::picker::{PersonPicker, PickerEvent};
use super::state::{FacesState, PhotoView};
use crate::shell::style::Colors;
use crate::storage::ui;
use chairphoto_core::app::faces::FaceForPhoto;
use gpui_kit::prelude::*;
use gpui_kit::{div, px, AnyElement, Context, Entity, FontWeight, SharedString, Subscription, TestSupportExt as _, Window};

pub struct FacesInspector {
    state: Entity<FacesState>,
    /// The face being reassigned and its picker.
    picker: Option<(i64, Entity<PersonPicker>, Subscription)>,
    _observers: Vec<Subscription>,
}

impl FacesInspector {
    pub fn new(state: Entity<FacesState>, cx: &mut Context<Self>) -> Self {
        let shell = state.read(cx).shell().clone();
        let _observers = vec![
            cx.observe(&state, |this, state, cx| {
                // The picker goes with the face it was opened for.
                if let Some((face, ..)) = &this.picker {
                    let still = state.read(cx).faces().is_some_and(|p| p.faces.iter().any(|f| f.id == *face));
                    if !still {
                        this.picker = None;
                    }
                }
                cx.notify();
            }),
            cx.observe(&shell, |_, _, cx| cx.notify()),
        ];
        FacesInspector { state, picker: None, _observers }
    }

    pub fn picker(&self) -> Option<&Entity<PersonPicker>> {
        self.picker.as_ref().map(|(_, p, _)| p)
    }

    pub fn open_picker(&mut self, face: &FaceForPhoto, window: &mut Window, cx: &mut Context<Self>) {
        let tags = self.state.read(cx).faces().map(|p| p.people.tags.clone()).unwrap_or_default();
        let (face_id, current) = (face.id, face.person_tag_id);
        let picker = cx.new(|cx| PersonPicker::new(format!("faces-insp-picker-{face_id}"), tags, current, window, cx));
        let sub = cx.subscribe(&picker, move |this, _, event: &PickerEvent, cx| {
            match event {
                PickerEvent::Pick { tag_id, .. } => this.state.update(cx, |s, cx| s.assign(face_id, *tag_id, cx)),
                PickerEvent::Create(name) => this.state.update(cx, |s, cx| s.create_person(face_id, name.clone(), cx)),
                PickerEvent::Cancel => {}
            }
            this.picker = None;
            cx.notify();
        });
        self.picker = Some((face_id, picker, sub));
        cx.notify();
    }

    fn row(&self, index: usize, face: &FaceForPhoto, selected: usize, colors: Colors, cx: &mut Context<Self>) -> AnyElement {
        let id = face.id;
        let state = face.state.as_str();
        let suggested = state == "suggested";
        // The person drawn as suggested: ✓ and ✕ carry it, so a verdict applies only to the
        // person the user saw (#208).
        let shown = face.person_tag_id.filter(|_| suggested);
        let batch_busy = self.state.read(cx).batch_busy;
        let reassigning = self.picker.as_ref().filter(|(f, ..)| *f == id).map(|(_, p, _)| p.clone());
        let chip = |name: String, label: String| ui::chip(SharedString::from(format!("faces-insp-{name}-{id}")), label, true, colors);
        let mut actions = ui::row().flex_wrap().gap(px(4.));
        if suggested {
            actions = actions.child(ui::clickable(chip("confirm".into(), "✓ confirm".into()), true, {
                let s = self.state.clone();
                move |_, _, cx| {
                    if let Some(tag) = shown {
                        s.update(cx, |s, cx| s.accept(id, tag, cx))
                    }
                }
            }));
            if face.person_tag_id.is_some() && selected > 1 {
                let label = if batch_busy == Some(id) { "confirming…".to_string() } else { format!("✓✓ confirm on {selected}") };
                let enabled = batch_busy.is_none();
                let face = face.clone();
                let s = self.state.clone();
                actions = actions.child(ui::clickable(
                    ui::chip(SharedString::from(format!("faces-insp-confirm-all-{id}")), label, enabled, colors),
                    enabled,
                    move |_, _, cx| s.update(cx, |s, cx| s.accept_on_selection(&face, cx)),
                ));
            }
        }
        if suggested || state == "unassigned" {
            let s = self.state.clone();
            actions = actions.child(ui::clickable(chip("reject".into(), "✕ reject".into()), true, move |_, _, cx| {
                s.update(cx, |s, cx| s.reject(id, shown, cx))
            }));
        }
        if reassigning.is_none() {
            let face = face.clone();
            actions = actions.child(ui::clickable(
                chip("reassign".into(), "⇄ reassign".into()),
                true,
                cx.listener(move |this, _, window, cx| this.open_picker(&face, window, cx)),
            ));
        }
        if state != "ignored" {
            let s = self.state.clone();
            actions = actions.child(ui::clickable(chip("ignore".into(), "– ignore".into()), true, move |_, _, cx| {
                s.update(cx, |s, cx| s.ignore(id, cx))
            }));
        }
        if face.source == "drawn" {
            let s = self.state.clone();
            actions = actions.child(ui::clickable(chip("delete".into(), "🗑 delete".into()), true, move |_, _, cx| {
                s.update(cx, |s, cx| s.delete_drawn(id, cx))
            }));
        }
        if state == "confirmed" {
            actions = actions.child(div().text_size(px(11.)).text_color(colors.ok).child("confirmed"));
        }
        let name = face.person_name.clone().unwrap_or_else(|| "Unknown".into());
        let conf = suggested.then_some(face.match_confidence).flatten().map(|c| format!("{}%", (c * 100.).round() as i32));
        div()
            .id(SharedString::from(format!("faces-insp-face-{id}")))
            .flex()
            .gap(px(8.))
            .py(px(4.))
            .child(div().w(px(22.)).text_size(px(11.)).text_color(colors.mute).child(format!("#{}", index + 1)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .gap(px(4.))
                    .child(
                        ui::row()
                            .gap(px(6.))
                            .child(
                                div()
                                    .id(SharedString::from(format!("faces-insp-name-{id}")))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(state_color(state, colors))
                                    .child(name.clone())
                                    .aria_label(name)
                                    .test_support(),
                            )
                            .children(conf.map(|c| div().text_size(px(11.)).text_color(colors.dim).child(c)))
                            .child(div().text_size(px(11.)).text_color(colors.mute).child(face.state.clone())),
                    )
                    .child(actions)
                    .children(reassigning.map(|p| {
                        div().flex().flex_col().gap(px(4.)).child(p).child(ui::clickable(
                            ui::chip(SharedString::from(format!("faces-insp-cancel-{id}")), "Cancel", true, colors),
                            true,
                            cx.listener(|this, _, _, cx| {
                                this.picker = None;
                                cx.notify();
                            }),
                        ))
                    })),
            )
            .test_support()
            .into_any_element()
    }
}

impl Render for FacesInspector {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let state = self.state.read(cx);
        // The inspector's photo (Compare's focused pane while Compare is open), not the
        // Library's active one: the block acts on the photo the inspector shows.
        let active = state.shown_photo(cx);
        if active.is_none() {
            return ui::empty("faces-insp-none", "No photo selected", colors);
        }
        let faces = match &state.photo {
            PhotoView::Ready(p) if Some(p.photo_id) == active => p.faces.clone(),
            PhotoView::Failed(_, e) => return ui::error("faces-insp-error", format!("Faces unavailable: {e}"), colors),
            _ => return ui::empty("faces-insp-loading", "Loading…", colors),
        };
        if faces.is_empty() {
            return div()
                .id("faces-insp-empty")
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(ui::sub("No faces indexed for this photo.", colors))
                .child(ui::sub("Use the Faces settings panel to index faces.", colors))
                .test_support()
                .into_any_element();
        }
        let selected = state.selection_targets(cx).len();
        let rows: Vec<AnyElement> = faces.iter().enumerate().map(|(i, f)| self.row(i, f, selected, colors, cx)).collect();
        div().id("faces-inspector").flex().flex_col().text_size(px(12.)).children(rows).test_support().into_any_element()
    }
}
