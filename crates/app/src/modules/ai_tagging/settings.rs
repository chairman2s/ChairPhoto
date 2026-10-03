//! The AI Tagging settings panel (`AiSettings` in aiTagging.tsx), on the module's Preferences
//! tab: the engine (with the cloud upload note), the selected provider's URL / model / API key
//! (masked) — with "Send photos to this Ollama server" when the Ollama URL is not on this machine —
//! "Suggest only existing tags", the confidence floor, the Advanced prompt editor
//! (Load default / Reset to default) and "Save AI settings". Nothing is stored until Save; the
//! engine chosen here is the per-provider opt-in [`AiState::may_send`] checks.

use super::logic::{self, api_key_key, curated_models, is_cloud, model_options, provider_fields, provider_short, DEFAULTS};
use super::state::AiState;
use crate::shell::style::Colors;
use crate::storage::ui;
use gpui_kit::component::button::Button;
use gpui_kit::component::{Disableable as _, Sizable as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::prelude::*;
use gpui_kit::{div, px, Context, Entity, FontWeight, SharedString, Subscription, TestSupportExt as _, Window};
use std::collections::BTreeMap;

/// The text fields, one input each (`ai.<key>`).
pub const TEXT_KEYS: [&str; 9] = [
    "ollama_url",
    "ollama_model",
    "cloud_model",
    "cloud_api_key",
    "openai_model",
    "openai_api_key",
    "gemini_model",
    "gemini_api_key",
    "min_confidence",
];

pub struct AiSettings {
    pub state: Entity<AiState>,
    pub inputs: BTreeMap<&'static str, Entity<InputState>>,
    pub prompt: Entity<TextareaState>,
    /// The engine and "existing only" as edited (stored on Save).
    pub provider: String,
    pub existing_only: bool,
    /// "Send photos to this Ollama server" — the opt-in for an Ollama URL that is not on this
    /// machine; saved as `ai.ollama_remote_url` = that URL (blank when unchecked or local).
    pub ollama_remote: bool,
    pub advanced: bool,
    shown: Option<super::state::Stored>,
    /// The save count when last edited: "Saved" shows only after a save.
    edited_at: Option<u64>,
    _subscriptions: Vec<Subscription>,
}

impl AiSettings {
    pub fn new(state: Entity<AiState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut inputs = BTreeMap::new();
        let mut subscriptions = vec![cx.observe(&state, |_, _, cx| cx.notify())];
        for key in TEXT_KEYS {
            let secret = key.ends_with("_api_key");
            let input = cx.new(|cx| InputState::new(window, cx).masked(secret).placeholder(logic::default_of(key)));
            subscriptions.push(cx.subscribe(&input, move |this: &mut Self, _, e: &InputEvent, cx| {
                if matches!(e, InputEvent::Change) {
                    this.edited_at = None;
                    if key == "ollama_url" {
                        // The model list follows the URL (React's effect on values.ollama_url).
                        let url = this.inputs[key].read(cx).value().to_string();
                        this.state.update(cx, |s, cx| s.fetch_ollama(url, cx));
                    }
                    cx.notify();
                }
            }));
            inputs.insert(key, input);
        }
        let prompt = cx.new(|cx| TextareaState::new(window, cx).rows(10));
        AiSettings {
            state,
            inputs,
            prompt,
            provider: "ollama".into(),
            existing_only: false,
            ollama_remote: false,
            advanced: false,
            shown: None,
            edited_at: None,
            _subscriptions: subscriptions,
        }
    }

    fn value(&self, key: &str, cx: &gpui_kit::App) -> String {
        self.inputs.get(key).map(|i| i.read(cx).value().to_string()).unwrap_or_default()
    }

    pub fn set_input(&mut self, key: &str, value: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(i) = self.inputs.get(key) {
            i.update(cx, |i, cx| i.set_value(value.to_string(), window, cx));
        }
        self.edited_at = None;
        cx.notify();
    }

    pub fn set_provider(&mut self, provider: &str, cx: &mut Context<Self>) {
        self.provider = provider.into();
        self.edited_at = None;
        if !is_cloud(provider) {
            let url = self.value("ollama_url", cx);
            let url = if url.trim().is_empty() { logic::default_of("ollama_url").to_string() } else { url };
            self.state.update(cx, |s, cx| s.fetch_ollama(url, cx));
        }
        cx.notify();
    }

    /// "Save AI settings": every key, blank meaning the default (as React stored them).
    pub fn save(&mut self, cx: &mut Context<Self>) {
        let mut values: Vec<(&'static str, String)> = Vec::new();
        for (key, _) in DEFAULTS {
            let v = match key {
                "provider" => self.provider.clone(),
                "existing_only" => if self.existing_only { "true" } else { "false" }.to_string(),
                "ollama_remote_url" => {
                    let url = self.value("ollama_url", cx).trim().to_string();
                    if self.ollama_remote && !url.is_empty() && !logic::is_local_url(&url) { url } else { String::new() }
                }
                "prompt_template" => self.prompt.read(cx).value().to_string(),
                k => self.value(k, cx).trim().to_string(),
            };
            values.push((key, v));
        }
        self.edited_at = Some(self.state.read(cx).saves + 1);
        self.state.update(cx, |s, cx| s.save(values, cx));
    }

    fn model_field(&self, key: &'static str, colors: Colors, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let current = self.value(key, cx);
        let choices: Vec<String> = if key == "ollama_model" {
            self.state.read(cx).ollama_models()
        } else {
            curated_models(&self.provider).iter().map(|m| m.to_string()).collect()
        };
        let options = model_options(&current, &choices);
        let this = cx.entity().downgrade();
        let pick = Button::new(SharedString::from(format!("ai-set-pick-{key}")))
            .outline()
            .small()
            .label(if options.is_empty() { "No models found" } else { "Pick…" })
            .disabled(options.is_empty())
            .dropdown_menu(move |menu: PopupMenu, _, _| {
                options.iter().fold(menu, |menu, m| {
                    let (this, m) = (this.clone(), m.clone());
                    menu.item(PopupMenuItem::new(m.clone()).on_click(move |_, window, cx| {
                        this.update(cx, |s, cx| s.set_input(key, &m, window, cx)).ok();
                    }))
                })
            });
        let _ = colors;
        ui::row().gap(px(6.)).child(div().flex_1().child(Input::new(&self.inputs[key]).small())).child(pick).into_any_element()
    }
}

impl Render for AiSettings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let stored = self.state.read(cx).stored.clone();
        if let Some(st) = &stored {
            if self.shown.as_ref() != Some(st) {
                self.shown = Some(st.clone());
                for key in TEXT_KEYS {
                    // Blank shows the default, as React's settings panel did.
                    let v = st.value(key);
                    self.inputs[key].update(cx, |i, cx| i.set_value(v, window, cx));
                }
                self.prompt.update(cx, |i, cx| i.set_value(st.raw("prompt_template").to_string(), window, cx));
                self.provider = st.provider();
                self.existing_only = st.value("existing_only") == "true";
                let url = st.value("ollama_url");
                self.ollama_remote = !st.raw("ollama_remote_url").is_empty() && st.raw("ollama_remote_url") == url.trim();
            }
        }
        let saves = self.state.read(cx).saves;
        let saved = self.edited_at.is_some_and(|at| saves >= at);
        let ready = stored.is_some();
        let this = cx.entity().downgrade();
        let engine = Button::new("ai-set-engine")
            .outline()
            .small()
            .label(logic::PROVIDERS.iter().find(|(p, _)| *p == self.provider).map_or("?", |(_, l)| l))
            .dropdown_menu(move |menu: PopupMenu, _, _| {
                logic::PROVIDERS.iter().fold(menu, |menu, (id, label)| {
                    let this = this.clone();
                    menu.item(PopupMenuItem::new(*label).on_click(move |_, _, cx| {
                        this.update(cx, |s, cx| s.set_provider(id, cx)).ok();
                    }))
                })
            });
        let mut body = ui::body()
            .id("ai-settings")
            .text_size(px(12.))
            .child(div().font_weight(FontWeight::SEMIBOLD).text_color(colors.txt).child("AI Tagging"))
            .child(ui::label("Engine", colors))
            .child(ui::row().child(engine));
        if is_cloud(&self.provider) {
            let note = format!(
                "Cloud mode uploads each photo to {} when you click Suggest tags. Photos are sent only once its API key is saved.",
                provider_short(&self.provider)
            );
            body = body.child(div().id("ai-cloud-note").text_color(colors.dim).child(note.clone()).aria_label(note).test_support());
        }
        for (key, label) in provider_fields(&self.provider) {
            body = body.child(ui::label(label, colors));
            body = if key.ends_with("_model") {
                body.child(self.model_field(key, colors, cx))
            } else {
                body.child(div().id(SharedString::from(format!("ai-set-{key}"))).child(Input::new(&self.inputs[key]).small()))
            };
            if api_key_key(&self.provider) == Some(key) {
                body = body.child(ui::sub("Stored in this catalog's settings; shown masked, never logged.", colors));
            }
        }
        let url = self.value("ollama_url", cx);
        if !is_cloud(&self.provider) && !url.trim().is_empty() && !logic::is_local_url(&url) {
            // Not a loopback address: photos would leave this machine — its own opt-in.
            let note = format!(
                "{} is not on this machine: each photo you tag is uploaded to it. Private tags are not sent.",
                url.trim()
            );
            body = body
                .child(div().id("ai-remote-note").text_color(colors.dim).child(note.clone()).aria_label(note).test_support())
                .child(
                    ui::checkbox("ai-set-ollama-remote", "Send photos to this Ollama server")
                        .checked(self.ollama_remote)
                        .on_change(cx.listener(|this, checked: &bool, _, cx| {
                            this.ollama_remote = *checked;
                            this.edited_at = None;
                            cx.notify();
                        })),
                );
        }
        body = body
            .child(
                ui::checkbox("ai-set-existing-only", "Suggest only existing tags (no new-tag discovery)")
                    .checked(self.existing_only)
                    .on_change(cx.listener(|this, checked: &bool, _, cx| {
                        this.existing_only = *checked;
                        this.edited_at = None;
                        cx.notify();
                    })),
            )
            .child(ui::label("Min confidence (0–1)", colors))
            .child(Input::new(&self.inputs["min_confidence"]).small())
            .child(ui::row().child(ui::clickable(
                ui::chip("ai-set-advanced", if self.advanced { "▾ Advanced — prompt" } else { "▸ Advanced — edit prompt" }, true, colors),
                true,
                cx.listener(|this, _, _, cx| {
                    this.advanced = !this.advanced;
                    cx.notify();
                }),
            )));
        if self.advanced {
            let has_prompt = !self.prompt.read(cx).value().is_empty();
            body = body
                .child(ui::sub(
                    "The exact instructions sent to the model. Leave blank to use the built-in default. Placeholders are \
                     filled in automatically: {taxonomy} (your tag list), {new_tags} (new-tag rules), {rejected} (tags \
                     you've rejected). Keep the JSON-shape line or suggestions won't parse.",
                    colors,
                ))
                .child(div().id("ai-set-prompt").child(Textarea::new(&self.prompt)))
                .child(
                    ui::row()
                        .gap(px(6.))
                        .child(ui::clickable(ui::chip("ai-set-load-default", "Load default", true, colors), true, {
                            cx.listener(|this, _, window, cx| {
                                let d = this.state.read(cx).default_prompt.clone();
                                this.prompt.update(cx, |i, cx| i.set_value(d, window, cx));
                                this.edited_at = None;
                                cx.notify();
                            })
                        }))
                        .child(ui::clickable(ui::chip("ai-set-reset-prompt", "Reset to default", has_prompt, colors), has_prompt, {
                            cx.listener(|this, _, window, cx| {
                                this.prompt.update(cx, |i, cx| i.set_value("", window, cx));
                                this.edited_at = None;
                                cx.notify();
                            })
                        })),
                );
        }
        body.child(ui::row().child(ui::clickable(
            ui::primary("ai-set-save", if saved { "Saved" } else { "Save AI settings" }, ready, colors),
            ready,
            cx.listener(|this, _, _, cx| this.save(cx)),
        )))
        .test_support()
    }
}
