//! `OAuthSettings` (publishing.tsx): a service's settings section — API key, secret (masked)
//! and max long edge, stored in the module's own settings (`<id>.api_key`, `<id>.api_secret`,
//! `<id>.max_long_edge`, only in the local catalog); "Save keys"; Connect / Reconnect, which
//! saves, asks the service for its authorize URL and opens it in the browser (with a link as
//! the fallback); the verifier field and Finish (Enter finishes); "Connected ✓".
//!
//! Every settings read/write and service call runs on a worker.

use super::PublishService;
use crate::modules::ModuleSettings;
use crate::shell::style::Colors;
use crate::storage::{ui, Runner};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::*;
use gpui_kit::{div, px, Context, Entity, Subscription, TestSupportExt as _, Window};
use std::sync::Arc;

pub const API_KEY: &str = "api_key";
pub const API_SECRET: &str = "api_secret";
pub const MAX_LONG_EDGE: &str = "max_long_edge";

pub struct OAuthSettings {
    settings: ModuleSettings,
    service: Arc<dyn PublishService>,
    pub key: Entity<InputState>,
    pub secret: Entity<InputState>,
    pub max_long_edge: Entity<InputState>,
    pub verifier: Entity<InputState>,
    /// Clear the verifier field on the next render (it needs the window).
    clear_verifier: bool,
    /// The authorize URL while a Connect is waiting for its verifier.
    pub auth_url: Option<String>,
    pub connected: bool,
    pub status: String,
    _subscriptions: Vec<Subscription>,
}

impl OAuthSettings {
    pub fn new(settings: ModuleSettings, service: Arc<dyn PublishService>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let key = cx.new(|cx| InputState::new(window, cx));
        let secret = cx.new(|cx| InputState::new(window, cx).masked(true));
        let max_long_edge = cx.new(|cx| InputState::new(window, cx).placeholder("0 = full resolution"));
        let verifier = cx.new(|cx| InputState::new(window, cx).placeholder("Paste verifier code"));
        let subs = vec![cx.subscribe(&verifier, |this: &mut Self, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.finish(cx);
            }
        })];
        let view = OAuthSettings {
            settings,
            service,
            key,
            secret,
            max_long_edge,
            verifier,
            clear_verifier: false,
            auth_url: None,
            connected: false,
            status: String::new(),
            _subscriptions: subs,
        };
        view.load(window, cx);
        view
    }

    /// Read the stored keys and whether the service is connected.
    fn load(&self, window: &mut Window, cx: &mut Context<Self>) {
        let (settings, service) = (self.settings.clone(), self.service.clone());
        let rx = Runner::get(cx).run(move || {
            let get = |k: &str| settings.get(k).ok().flatten().unwrap_or_default();
            let connected = service.connected(&settings).unwrap_or(false);
            (get(API_KEY), get(API_SECRET), get(MAX_LONG_EDGE), connected)
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok((key, secret, edge, connected)) = rx.await else { return };
            this.update_in(cx, |v, window, cx| {
                v.key.update(cx, |i, cx| i.set_value(key, window, cx));
                v.secret.update(cx, |i, cx| i.set_value(secret, window, cx));
                v.max_long_edge.update(cx, |i, cx| i.set_value(edge, window, cx));
                v.connected = connected;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// What Save writes: the trimmed fields (an empty max long edge = full resolution).
    fn fields(&self, cx: &Context<Self>) -> [(&'static str, String); 3] {
        let value = |i: &Entity<InputState>| i.read(cx).value().trim().to_string();
        [(API_KEY, value(&self.key)), (API_SECRET, value(&self.secret)), (MAX_LONG_EDGE, value(&self.max_long_edge))]
    }

    /// "Save keys".
    pub fn save(&mut self, cx: &mut Context<Self>) {
        let (settings, fields) = (self.settings.clone(), self.fields(cx));
        let rx = Runner::get(cx).run(move || save(&settings, &fields));
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("Couldn't save".into()));
            this.update(cx, |v, cx| {
                v.status = match result {
                    Ok(()) => "Saved.".into(),
                    Err(e) => e,
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Connect / Reconnect: save, get the authorize URL, open it in the browser.
    pub fn connect(&mut self, cx: &mut Context<Self>) {
        self.status.clear();
        let (settings, fields, service) = (self.settings.clone(), self.fields(cx), self.service.clone());
        let rx = Runner::get(cx).run(move || {
            save(&settings, &fields)?;
            service.begin_auth(&settings)
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("Couldn't start connecting".into()));
            this.update(cx, |v, cx| {
                match result {
                    Ok(url) => {
                        cx.open_url(&url);
                        v.auth_url = Some(url);
                        v.status = "Authorize in your browser, then paste the verifier code below.".into();
                    }
                    Err(e) => v.status = e,
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Finish / Enter: hand the verifier to the service.
    pub fn finish(&mut self, cx: &mut Context<Self>) {
        let verifier = self.verifier.read(cx).value().trim().to_string();
        if verifier.is_empty() {
            return;
        }
        self.status.clear();
        let (settings, service) = (self.settings.clone(), self.service.clone());
        let rx = Runner::get(cx).run(move || {
            service.complete_auth(&settings, &verifier)?;
            service.connected(&settings)
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("Couldn't finish connecting".into()));
            this.update(cx, |v, cx| {
                match result {
                    Ok(connected) => {
                        v.connected = connected;
                        v.auth_url = None;
                        v.clear_verifier = true;
                        v.status = "Connected.".into();
                    }
                    Err(e) => v.status = e,
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }
}

fn save(settings: &ModuleSettings, fields: &[(&'static str, String)]) -> Result<(), String> {
    for (k, v) in fields {
        settings.set(k, v)?;
    }
    Ok(())
}

impl Render for OAuthSettings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        if std::mem::take(&mut self.clear_verifier) {
            self.verifier.update(cx, |i, cx| i.set_value("", window, cx));
        }
        let name = self.service.name();
        let field = |label: String, id: &'static str, input: &Entity<InputState>| {
            div().flex().flex_col().gap(px(4.)).child(ui::label(label, colors)).child(div().id(id).w(px(320.)).child(Input::new(input)).test_support())
        };
        let mut body = ui::body()
            .id("oauth-settings")
            .child(field(format!("{name} API key"), "oauth-key", &self.key))
            .child(field(format!("{name} API secret"), "oauth-secret", &self.secret))
            .child(field("Max long edge (px)".into(), "oauth-max-long-edge", &self.max_long_edge))
            .child(ui::sub(
                format!(
                    "Register an app at {} to get a key + secret. Stored only in your local catalog. Max long edge: leave \
                     empty or 0 for full resolution; set e.g. 2048 to cap uploads so the longer dimension does not exceed \
                     that size (never upscales).",
                    self.service.signup_url()
                ),
                colors,
            ))
            .child(
                ui::row()
                    .child(ui::clickable(ui::chip("oauth-save", "Save keys", true, colors), true, cx.listener(|v, _, _, cx| v.save(cx))))
                    .child(ui::clickable(
                        ui::primary("oauth-connect", if self.connected { "Reconnect" } else { "Connect" }, true, colors),
                        true,
                        cx.listener(|v, _, _, cx| v.connect(cx)),
                    ))
                    .child(
                        div()
                            .id("oauth-connected")
                            .child(ui::sub(if self.connected { "Connected ✓" } else { "Not connected" }, colors))
                            .test_support(),
                    ),
            );
        if let Some(url) = self.auth_url.clone() {
            let can = !self.verifier.read(cx).value().trim().is_empty();
            body = body
                .child(ui::row().child(ui::sub("If the browser didn't open,", colors)).child(ui::clickable(
                    ui::chip("oauth-authorize-link", "open this authorize link", true, colors),
                    true,
                    move |_, _, cx| cx.open_url(&url),
                )))
                .child(
                    ui::row()
                        .child(div().id("oauth-verifier").w(px(260.)).child(Input::new(&self.verifier)).test_support())
                        .child(ui::clickable(ui::chip("oauth-finish", "Finish", can, colors), can, cx.listener(|v, _, _, cx| v.finish(cx)))),
                );
        }
        if !self.status.is_empty() {
            body = body.child(div().id("oauth-status").child(ui::sub(self.status.clone(), colors)).test_support());
        }
        body.test_support()
    }
}
