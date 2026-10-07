//! The Faces settings panel (`FacesSettings` in faces.tsx), on the module's Preferences tab:
//! model status (YuNet + AuraFace) with Download / Re-download; the inference line (GPU, CPU,
//! CPU fallback of a CUDA build, or idle until the first run); indexing speed Background/Full
//! (from the next run); the people root with a type-ahead over the tag paths; the match
//! threshold; Save; and "Index faces" with Cancel, progress, the last run's result and its
//! error. The panel follows whatever run [`FacesState`] follows, so closing and reopening it
//! — or a run started before the module loaded — shows the run, not idle.
//!
//! "Run matching" (#130) has its own section — the button, Cancel, the step and progress, the
//! last run's result and its error — and, like React's shared job phase, neither job can be
//! started while the other runs.

use super::logic::{index_progress_line, match_progress_line, root_suggestions, step_highlight};
use super::state::{FacesState, IndexPhase, MatchPhase, DEFAULT_THRESHOLD};
use crate::shell::style::Colors;
use crate::storage::ui;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::*;
use gpui_kit::{
    div, px, relative, Context, Entity, Focusable as _, FontWeight, SharedString, Subscription, TestSupportExt as _, Window,
};

pub struct FacesSettings {
    pub state: Entity<FacesState>,
    pub people_root: Entity<InputState>,
    pub threshold: Entity<InputState>,
    /// The stored values the inputs were last filled from.
    shown: Option<(String, String)>,
    /// The save count when the inputs were last edited: "Saved" shows only after a save.
    edited_at: Option<u64>,
    pub root_highlight: usize,
    /// The type-ahead was dismissed (Esc, or a pick) until the next edit.
    root_dismissed: bool,
    _subscriptions: Vec<Subscription>,
}

impl FacesSettings {
    pub fn new(state: Entity<FacesState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let people_root = cx.new(|cx| InputState::new(window, cx).placeholder("e.g. People"));
        let threshold = cx.new(|cx| InputState::new(window, cx).placeholder(DEFAULT_THRESHOLD));
        let edited = |this: &mut Self, e: &InputEvent, cx: &mut Context<Self>| {
            if matches!(e, InputEvent::Change) {
                this.edited_at = None;
                this.root_highlight = 0;
                this.root_dismissed = false;
                cx.notify();
            }
        };
        let window_handle = window.window_handle();
        let this = cx.entity().downgrade();
        let subscriptions = vec![
            cx.subscribe(&people_root, move |this, _, e: &InputEvent, cx| edited(this, e, cx)),
            cx.subscribe(&threshold, move |this, _, e: &InputEvent, cx| edited(this, e, cx)),
            cx.observe(&state, |_, _, cx| cx.notify()),
            // The type-ahead's keys, before any binding (Enter must not save, Esc must not
            // close Preferences) — only while the root field has focus and offers something.
            cx.intercept_keystrokes({
                move |event, window, cx| {
                    if window.window_handle() != window_handle {
                        return;
                    }
                    let Some(this) = this.upgrade() else { return };
                    let k = &event.keystroke;
                    if k.modifiers.control || k.modifiers.alt || k.modifiers.platform {
                        return;
                    }
                    let open = this.read(cx).suggestions(window, cx).len();
                    if open == 0 {
                        return;
                    }
                    let handled = match k.key.as_str() {
                        "up" | "down" => {
                            let delta = if k.key == "up" { -1 } else { 1 };
                            this.update(cx, |s, cx| {
                                s.root_highlight = step_highlight(s.root_highlight, delta, open);
                                cx.notify();
                            });
                            true
                        }
                        "enter" => {
                            let pick = this.read(cx).suggestions(window, cx).get(this.read(cx).root_highlight.min(open - 1)).cloned();
                            if let Some(path) = pick {
                                this.update(cx, |s, cx| s.pick_root(path, window, cx));
                            }
                            true
                        }
                        "escape" => {
                            this.update(cx, |s, cx| {
                                s.root_dismissed = true;
                                cx.notify();
                            });
                            true
                        }
                        _ => false,
                    };
                    if handled {
                        cx.stop_propagation();
                    }
                }
            }),
        ];
        FacesSettings {
            state,
            people_root,
            threshold,
            shown: None,
            edited_at: None,
            root_highlight: 0,
            root_dismissed: false,
            _subscriptions: subscriptions,
        }
    }

    /// The type-ahead's paths: while the root field has focus and it was not dismissed.
    pub fn suggestions(&self, window: &Window, cx: &gpui_kit::App) -> Vec<String> {
        if self.root_dismissed || !self.people_root.read(cx).focus_handle(cx).is_focused(window) {
            return Vec::new();
        }
        root_suggestions(&self.state.read(cx).all_tags, &self.people_root.read(cx).value())
    }

    pub fn pick_root(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        self.people_root.update(cx, |i, cx| i.set_value(path, window, cx));
        self.root_dismissed = true;
        self.edited_at = None;
        cx.notify();
    }

    pub fn save(&mut self, cx: &mut Context<Self>) {
        let root = self.people_root.read(cx).value().to_string();
        let threshold = self.threshold.read(cx).value().to_string();
        let next = self.state.read(cx).saves + 1;
        self.edited_at = Some(next);
        self.state.update(cx, |s, cx| s.save_settings(root, threshold, cx));
    }

    fn render_models(&self, colors: Colors, cx: &mut Context<Self>) -> gpui_kit::Div {
        let s = self.state.read(cx);
        let ready = s.models_ready();
        let line: (String, gpui_kit::Hsla) = match (&s.models, s.models_busy) {
            (_, true) => ("Downloading…".into(), colors.dim),
            (None, _) => ("Checking…".into(), colors.dim),
            (Some(m), _) if m.ready => ("YuNet + AuraFace ready".into(), colors.ok),
            (Some(m), _) => (
                m.models
                    .iter()
                    .map(|r| {
                        format!(
                            "{}: {}{}",
                            r.key,
                            if r.present { "OK" } else { "missing" },
                            r.detail.as_deref().map(|d| format!(" ({d})")).unwrap_or_default()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("  "),
                colors.dim,
            ),
        };
        let enabled = !s.models_busy && !(ready && s.models_error.is_none());
        let label = if s.models_busy { "Downloading…" } else if ready { "Re-download models" } else { "Download models" };
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(ui::label("Models", colors))
            .child(div().id("faces-models").text_color(line.1).child(line.0.clone()).aria_label(line.0).test_support())
            .when(!ready, |d| {
                d.child(ui::sub(
                    "YuNet (Apache-2.0) and AuraFace-v1 (Apache-2.0) will be downloaded from their official sources \
                     into the app data directory.",
                    colors,
                ))
            })
            .child(
                ui::row()
                    .child(ui::clickable(ui::chip("faces-download", label, enabled, colors), enabled, {
                        let state = self.state.clone();
                        move |_, _, cx| state.update(cx, |s, cx| s.download_models(cx))
                    }))
                    .when_some(s.models_error.clone(), |d, e| d.child(ui::error("faces-models-error", e, colors))),
            )
    }

    fn render_inference(&self, colors: Colors, cx: &mut Context<Self>) -> gpui_kit::Div {
        let s = self.state.read(cx);
        let (line, color) = match &s.inference {
            None => ("Checking…".to_string(), colors.dim),
            Some(i) if i.ep == "cuda" => ("GPU (CUDA)".into(), colors.ok),
            Some(i) if i.ep == "cpu" => (
                format!("CPU{}", if i.cuda_built { " — CUDA build fell back (see terminal log)" } else { "" }),
                colors.dim,
            ),
            Some(i) => (
                format!("Idle — determined on first run ({})", if i.cuda_built { "CUDA-capable build" } else { "CPU build" }),
                colors.dim,
            ),
        };
        let speed = s.inference.as_ref().map(|i| i.speed.clone());
        let known = speed.is_some();
        let speed_chip = |id: &'static str, value: &'static str, label: &'static str| {
            let on = speed.as_deref() == Some(value);
            let state = self.state.clone();
            ui::clickable(
                ui::chip(id, label, known, colors).when(on, |c| c.border_color(colors.accent).text_color(colors.txt)),
                known && !on,
                move |_, _, cx| state.update(cx, |s, cx| s.set_speed(value, cx)),
            )
        };
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(ui::label("Inference", colors))
            .child(div().id("faces-inference").text_color(color).child(line.clone()).aria_label(line).test_support())
            .child(ui::label("Indexing speed", colors))
            .child(
                ui::row()
                    .flex_wrap()
                    .child(speed_chip("faces-speed-background", "background", "Background (keep desktop responsive)"))
                    .child(speed_chip("faces-speed-full", "full", "Full (use all cores / GPU)")),
            )
            .when_some(s.speed_note.clone(), |d, n| {
                d.child(div().id("faces-speed-note").text_color(colors.dim).child(n.clone()).aria_label(n).test_support())
            })
    }

    fn render_index(&self, colors: Colors, cx: &mut Context<Self>) -> gpui_kit::Div {
        let s = self.state.read(cx);
        let can = s.can_index();
        let run = s.index.clone();
        let label = if run.busy() { "Indexing…" } else { "Index faces" };
        let progress = match run.phase {
            IndexPhase::Idle => None,
            IndexPhase::Starting | IndexPhase::Running { progress: false, .. } => Some(("Starting…".to_string(), None)),
            IndexPhase::Running { done, total, progress: true, stage, .. } => Some(index_progress_line(stage, done, total)),
        };
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .pt(px(12.))
            .border_t_1()
            .border_color(colors.border)
            .child(div().font_weight(FontWeight::SEMIBOLD).text_color(colors.txt).child("Index"))
            .child(ui::sub(
                "\u{201c}Index faces\u{201d} detects and embeds all unindexed photos, in the background; you can cancel \
                 at any time.",
                colors,
            ))
            .child(
                ui::row()
                    .child(ui::clickable(ui::primary("faces-index", label, can, colors), can, {
                        let state = self.state.clone();
                        move |_, _, cx| state.update(cx, |s, cx| s.index_faces(cx))
                    }))
                    .when(run.job().is_some(), |d| {
                        let state = self.state.clone();
                        d.child(ui::clickable(ui::chip("faces-index-cancel", "Cancel", true, colors), true, move |_, _, cx| {
                            state.update(cx, |s, cx| s.cancel_index(cx))
                        }))
                    })
                    .when(!s.models_ready(), |d| d.child(ui::sub("Download models first", colors)))
                    .when(s.matching.busy() && !run.busy(), |d| d.child(ui::sub("Face matching is running…", colors))),
            )
            .when_some(progress, |d, (line, pct)| {
                d.child(div().id("faces-progress").text_color(colors.dim).child(line.clone()).aria_label(line).test_support())
                    .when_some(pct, |d, p| {
                        d.child(
                            div()
                                .h(px(4.))
                                .w_full()
                                .rounded(px(2.))
                                .bg(colors.txt.opacity(0.12))
                                .child(div().h_full().rounded(px(2.)).bg(colors.accent).w(relative(p as f32 / 100.))),
                        )
                    })
            })
            .when_some(run.last_result.clone(), |d, r| {
                let color = if run.busy() { colors.dim } else { colors.ok };
                d.child(div().id("faces-last-result").text_color(color).child(r.clone()).aria_label(r).test_support())
            })
            .when_some(run.error.clone(), |d, e| d.child(ui::error("faces-index-error", e, colors)))
    }
}

impl FacesSettings {
    fn render_match(&self, colors: Colors, cx: &mut Context<Self>) -> gpui_kit::Div {
        let s = self.state.read(cx);
        let can = s.can_match();
        let run = s.matching.clone();
        let label = if run.busy() { "Matching…" } else { "Run matching" };
        let progress = match run.phase {
            MatchPhase::Idle => None,
            MatchPhase::Starting | MatchPhase::Running { progress: false, .. } => Some(("Starting…".to_string(), None)),
            MatchPhase::Running { done, total, step, progress: true, .. } => Some(match_progress_line(step, done, total)),
        };
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .pt(px(12.))
            .border_t_1()
            .border_color(colors.border)
            .child(div().font_weight(FontWeight::SEMIBOLD).text_color(colors.txt).child("Match"))
            .child(ui::sub(
                "\u{201c}Run matching\u{201d} seeds known people from your existing person tags, then suggests matches \
                 and groups unknown faces into clusters. Nothing is confirmed without you, except a photo with one face \
                 and one person tag.",
                colors,
            ))
            .child(
                ui::row()
                    .child(ui::clickable(ui::primary("faces-match", label, can, colors), can, {
                        let state = self.state.clone();
                        move |_, _, cx| state.update(cx, |s, cx| s.run_matching(cx))
                    }))
                    .when(run.job().is_some(), |d| {
                        let state = self.state.clone();
                        d.child(ui::clickable(ui::chip("faces-match-cancel", "Cancel", true, colors), true, move |_, _, cx| {
                            state.update(cx, |s, cx| s.cancel_match(cx))
                        }))
                    })
                    .when(s.index.busy() && !run.busy(), |d| d.child(ui::sub("Face indexing is running…", colors))),
            )
            .when_some(progress, |d, (line, pct)| {
                d.child(div().id("faces-match-progress").text_color(colors.dim).child(line.clone()).aria_label(line).test_support())
                    .when_some(pct, |d, p| {
                        d.child(
                            div()
                                .h(px(4.))
                                .w_full()
                                .rounded(px(2.))
                                .bg(colors.txt.opacity(0.12))
                                .child(div().h_full().rounded(px(2.)).bg(colors.accent).w(relative(p as f32 / 100.))),
                        )
                    })
            })
            .when_some(run.last_result.clone(), |d, r| {
                let color = if run.busy() { colors.dim } else { colors.ok };
                d.child(div().id("faces-match-result").text_color(color).child(r.clone()).aria_label(r).test_support())
            })
            .when_some(run.error.clone(), |d, e| d.child(ui::error("faces-match-error", e, colors)))
    }
}

impl Render for FacesSettings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let stored = self.state.read(cx).stored.clone();
        let saves = self.state.read(cx).saves;
        if let Some(st) = &stored {
            let pair = (st.people_root.clone(), st.threshold.clone());
            if self.shown.as_ref() != Some(&pair) {
                self.shown = Some(pair.clone());
                self.people_root.update(cx, |i, cx| i.set_value(pair.0, window, cx));
                self.threshold.update(cx, |i, cx| i.set_value(pair.1, window, cx));
                self.root_dismissed = true;
            }
        }
        let suggestions = self.suggestions(window, cx);
        let highlight = self.root_highlight.min(suggestions.len().saturating_sub(1));
        let typeahead = (!suggestions.is_empty()).then(|| {
            let mut list = div()
                .id("faces-root-suggestions")
                .flex()
                .flex_col()
                .border_1()
                .border_color(colors.border)
                .rounded(px(6.))
                .bg(colors.panel);
            for (i, path) in suggestions.into_iter().enumerate() {
                let p = path.clone();
                list = list.child(
                    div()
                        .id(SharedString::from(format!("faces-root-suggestion-{i}")))
                        .px(px(10.))
                        .py(px(5.))
                        .text_color(colors.txt)
                        .cursor_pointer()
                        .when(i == highlight, |d| d.bg(colors.sel))
                        .child(path.clone())
                        .aria_label(path)
                        .on_mouse_down(
                            gpui_kit::MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.pick_root(p.clone(), window, cx);
                            }),
                        )
                        .test_support(),
                );
            }
            list.test_support()
        });
        let saved = self.edited_at.is_some_and(|at| saves >= at);
        ui::body()
            .id("faces-settings")
            .text_size(px(12.))
            .child(div().font_weight(FontWeight::SEMIBOLD).text_color(colors.txt).child("Faces"))
            .child(self.render_models(colors, cx))
            .child(self.render_inference(colors, cx))
            .child(ui::label("People root tag", colors))
            .child(Input::new(&self.people_root))
            .children(typeahead)
            .child(ui::sub(
                "Person tags must be descendants of this root (e.g. People/Friends/Jane). Used to seed recognition \
                 from existing photo-level tags.",
                colors,
            ))
            .child(ui::label("Match threshold (0–1)", colors))
            .child(Input::new(&self.threshold))
            .child(ui::sub(
                "Cosine similarity threshold for face matching (default 0.45). Lower = more proposals but more false \
                 positives; higher = fewer but more precise.",
                colors,
            ))
            .child(ui::row().child(ui::clickable(
                ui::primary("faces-save", if saved { "Saved" } else { "Save settings" }, stored.is_some(), colors),
                stored.is_some(),
                cx.listener(|this, _, _, cx| this.save(cx)),
            )))
            .child(self.render_index(colors, cx))
            .child(self.render_match(colors, cx))
            .test_support()
    }
}
