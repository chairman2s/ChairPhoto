//! The Smart Tagging views: the inspector's "Similar tags" panel (`SimilarTagsPanel`) and the
//! settings panel (`SmarttagsSettings`), both over [`SmarttagsState`].
//!
//! - Panel: "Download model (~350 MB)" with its progress when the default model is missing;
//!   a broken custom path's error; Index (N %) with Cancel — the followed run, re-attached
//!   after a reopen; the result and error lines; Suggest; suggestions (path, confidence,
//!   "from N similar photos", ✓ add, ✗ reject).
//! - Settings: the model line; Download; the model path (blank = default); the privacy note;
//!   Save; Train classifiers with its result; Delete index (no confirm, as React).

use super::logic::{download_label, index_label, provenance};
use super::state::{IndexPhase, PhotoView, SmarttagsState};
use crate::shell::style::Colors;
use crate::storage::ui;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::*;
use gpui_kit::{div, px, Context, Entity, FontWeight, SharedString, Subscription, TestSupportExt as _, Window};

/// "Download model" with its live label, shared by both views.
fn download_button(state: &Entity<SmarttagsState>, id: &'static str, colors: Colors, cx: &gpui_kit::App) -> gpui_kit::Div {
    let s = state.read(cx);
    let label = if s.downloading { download_label(s.download_progress) } else { "Download model (~350 MB)".into() };
    let enabled = !s.downloading;
    let st = state.clone();
    ui::row()
        .gap(px(6.))
        .child(ui::clickable(ui::primary(id, label, enabled, colors), enabled, move |_, _, cx| st.update(cx, |s, cx| s.download(cx))))
        .when_some(s.download_error.clone(), |d, e| d.child(ui::error("smarttags-download-error", e, colors)))
}

pub struct SimilarTagsPanel {
    pub state: Entity<SmarttagsState>,
    _subscriptions: Vec<Subscription>,
}

impl SimilarTagsPanel {
    pub fn new(state: Entity<SmarttagsState>, cx: &mut Context<Self>) -> Self {
        let shell = state.read(cx).shell().clone();
        let _subscriptions = vec![cx.observe(&state, |_, _, cx| cx.notify()), cx.observe(&shell, |_, _, cx| cx.notify())];
        SimilarTagsPanel { state, _subscriptions }
    }
}

impl Render for SimilarTagsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let s = self.state.read(cx);
        let active = s.shell().read(cx).library.selection().active_id;
        if active.is_none() {
            return ui::empty("smarttags-none", "Select a photo", colors);
        }
        if let Some(m) = s.model_status.as_ref().filter(|m| !m.ready) {
            if !m.model.custom {
                return div()
                    .id("smarttags-panel")
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .text_size(px(12.))
                    .child(ui::sub("The Smart Tagging model (~350 MB) is not downloaded yet.", colors))
                    .child(download_button(&self.state, "smarttags-download", colors, cx))
                    .test_support()
                    .into_any_element();
            }
            let e = m.model.detail.clone().unwrap_or_else(|| {
                "Custom model path is missing or unreadable. Check Settings → Smart Tagging.".into()
            });
            return ui::error("smarttags-model-error", e, colors);
        }
        let run = s.index.clone();
        let progress = match run.phase {
            IndexPhase::Running { done, total, progress: true, .. } => Some((done, total)),
            _ => None,
        };
        let can = s.can_index();
        let busy = s.suggesting;
        let list = match &s.photo {
            PhotoView::Ready(p) if Some(p.photo_id) == active => Some(p.list.clone()),
            PhotoView::Failed(_, e) => return ui::error("smarttags-load-error", format!("Suggestions unavailable: {e}"), colors),
            _ => None,
        };
        let error = s.error.clone();
        let st = self.state.clone();
        let mut body = div()
            .id("smarttags-panel")
            .flex()
            .flex_col()
            .gap(px(6.))
            .text_size(px(12.))
            .child(
                ui::row()
                    .gap(px(6.))
                    .child({
                        let label = index_label(run.busy(), progress);
                        let st = st.clone();
                        ui::clickable(ui::primary("smarttags-index", label.clone(), can, colors).aria_label(label), can, move |_, _, cx| {
                            st.update(cx, |s, cx| s.index_photos(cx))
                        })
                    })
                    .when(run.job().is_some(), |d| {
                        let st = st.clone();
                        d.child(ui::clickable(ui::danger_chip("smarttags-cancel", "Cancel", true, colors), true, move |_, _, cx| {
                            st.update(cx, |s, cx| s.cancel_index(cx))
                        }))
                    }),
            )
            .when_some(run.last_result.clone(), |d, r| {
                d.child(div().id("smarttags-result").text_color(colors.ok).child(r.clone()).aria_label(r).test_support())
            })
            .when_some(run.error.clone(), |d, e| d.child(ui::error("smarttags-index-error", e, colors)))
            .when_some(error, |d, e| d.child(ui::error("smarttags-error", e, colors)))
            .child(ui::row().child({
                let st = st.clone();
                let enabled = !busy && list.is_some();
                ui::clickable(ui::primary("smarttags-suggest", if busy { "Suggesting…" } else { "Suggest" }, enabled, colors), enabled, move |_, _, cx| {
                    st.update(cx, |s, cx| s.suggest(cx))
                })
            }));
        for sug in list.unwrap_or_default() {
            let key = sug.path.replace('/', "_");
            let (a, r, pa, pr) = (st.clone(), st.clone(), sug.path.clone(), sug.path.clone());
            body = body.child(
                div()
                    .id(SharedString::from(format!("smarttags-sug-{key}")))
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .py(px(4.))
                    .child(
                        ui::row()
                            .gap(px(6.))
                            .child(div().text_color(colors.txt).child(sug.path.clone()))
                            .child(div().text_size(px(11.)).text_color(colors.dim).child(format!("{}%", (sug.confidence * 100.).round() as i32))),
                    )
                    .when(!sug.source_photo_ids.is_empty(), |d| d.child(ui::sub(provenance(sug.source_photo_ids.len()), colors)))
                    .child(
                        ui::row()
                            .gap(px(4.))
                            .child(ui::clickable(ui::chip(SharedString::from(format!("smarttags-add-{key}")), "✓ add", true, colors), true, move |_, _, cx| {
                                a.update(cx, |s, cx| s.accept(pa.clone(), cx))
                            }))
                            .child(ui::clickable(ui::chip(SharedString::from(format!("smarttags-reject-{key}")), "✗ reject", true, colors), true, move |_, _, cx| {
                                r.update(cx, |s, cx| s.reject(pr.clone(), cx))
                            })),
                    )
                    .aria_label(sug.path.clone())
                    .test_support(),
            );
        }
        body.test_support().into_any_element()
    }
}

pub struct SmarttagsSettings {
    pub state: Entity<SmarttagsState>,
    pub model_path: Entity<InputState>,
    shown: Option<String>,
    edited_at: Option<u64>,
    _subscriptions: Vec<Subscription>,
}

impl SmarttagsSettings {
    pub fn new(state: Entity<SmarttagsState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let model_path = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Path to a CLIP vision ONNX model (blank = pinned default, downloadable above)")
        });
        let _subscriptions = vec![
            cx.observe(&state, |_, _, cx| cx.notify()),
            cx.subscribe(&model_path, |this, _, e: &InputEvent, cx| {
                if matches!(e, InputEvent::Change) {
                    this.edited_at = None;
                    cx.notify();
                }
            }),
        ];
        SmarttagsSettings { state, model_path, shown: None, edited_at: None, _subscriptions }
    }

    pub fn save(&mut self, cx: &mut Context<Self>) {
        let path = self.model_path.read(cx).value().to_string();
        self.edited_at = Some(self.state.read(cx).saves + 1);
        self.state.update(cx, |s, cx| s.save_model_path(path, cx));
    }
}

impl Render for SmarttagsSettings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let stored = self.state.read(cx).model_path.clone();
        if let Some(p) = &stored {
            if self.shown.as_ref() != Some(p) {
                self.shown = Some(p.clone());
                self.model_path.update(cx, |i, cx| i.set_value(p.clone(), window, cx));
            }
        }
        let s = self.state.read(cx);
        let model_line = match &s.model_status {
            None => "Checking…".to_string(),
            Some(m) if m.ready => format!("Ready ({}): {}", if m.model.custom { "custom" } else { "default" }, m.model.path),
            Some(m) => format!("Not available — {}", m.model.detail.as_deref().unwrap_or("missing")),
        };
        let offer_download = s.model_status.as_ref().is_some_and(|m| !m.ready && !m.model.custom);
        let saved = self.edited_at.is_some_and(|at| s.saves >= at);
        let ready = stored.is_some();
        let (training, train_status, deleting, delete_error) = (s.training, s.train_status.clone(), s.deleting, s.delete_error.clone());
        let (t, d) = (self.state.clone(), self.state.clone());
        ui::body()
            .id("smarttags-settings")
            .text_size(px(12.))
            .child(div().font_weight(FontWeight::SEMIBOLD).text_color(colors.txt).child("Smart Tagging"))
            .child(ui::label("Model", colors))
            .child(div().id("smarttags-model").text_color(colors.dim).child(model_line.clone()).aria_label(model_line).test_support())
            .when(offer_download, |b| b.child(download_button(&self.state, "smarttags-settings-download", colors, cx)))
            .child(ui::label("Model path", colors))
            .child(Input::new(&self.model_path))
            .child(ui::sub(
                "CLIP embeddings are computed from cached preview images and stored locally. The index is not shared \
                 and never leaves this machine.",
                colors,
            ))
            .child(ui::row().child(ui::clickable(
                ui::primary("smarttags-save", if saved { "Saved" } else { "Save Smart Tagging settings" }, ready, colors),
                ready,
                cx.listener(|this, _, _, cx| this.save(cx)),
            )))
            .child(ui::label("Classifiers", colors))
            .child(ui::sub(
                "Per-tag classifiers refine the similarity suggestions for tags with many confirmed photos. Retrain \
                 after a batch of accepts/rejects.",
                colors,
            ))
            .child(
                ui::row()
                    .gap(px(6.))
                    .child(ui::clickable(
                        ui::primary("smarttags-train", if training { "Training…" } else { "Train classifiers" }, ready && !training, colors),
                        ready && !training,
                        move |_, _, cx| t.update(cx, |s, cx| s.train(cx)),
                    ))
                    .when_some(train_status, |r, st| {
                        r.child(div().id("smarttags-train-status").text_color(colors.dim).child(st.clone()).aria_label(st).test_support())
                    }),
            )
            .child(ui::label("Index management", colors))
            .child(
                ui::row()
                    .gap(px(6.))
                    .child(ui::clickable(
                        ui::danger_chip("smarttags-delete", if deleting { "Deleting…" } else { "Delete index" }, ready && !deleting, colors),
                        ready && !deleting,
                        move |_, _, cx| d.update(cx, |s, cx| s.delete_index(cx)),
                    ))
                    .when_some(delete_error, |r, e| r.child(ui::error("smarttags-delete-error", e, colors))),
            )
            .test_support()
            .into_any_element()
    }
}
