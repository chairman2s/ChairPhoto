//! `OAuthSettings` (publishing.tsx): a service's settings section — API key, secret (masked)
//! and max long edge, stored in the module's own settings (`<id>.api_key`, `<id>.api_secret`,
//! `<id>.max_long_edge`, only in the local catalog); "Save keys"; Connect / Reconnect, which
//! saves, asks the service for its authorize URL and opens it in the browser (with a link as
//! the fallback); the verifier field and Finish (Enter finishes); "Connected ✓".
//!
//! Every settings read/write and service call runs on a worker. The section outlives a catalog
//! switch (Preferences keeps a module's settings views while it is enabled), so it follows the
//! open catalog: `catalog:switched` clears it, drops every answer still in flight and unbinds
//! its settings handle ([`ModuleSettings::unbound`]); the next catalog read — whether it
//! arrives before the event or after — rebinds it and reloads ([`ModuleSettings::rebound`]).
//! Save and Connect write the fields only once they show the stored values of the catalog the
//! handle is bound to; until that load lands they refuse ([`NOT_LOADED`]), so blank fields are
//! never saved over a catalog's keys.
//!
//! **Secrets.** The secret field is masked; nothing here logs a key, a secret or a token, and
//! the status line only ever shows the service's error text.

use super::PublishService;
use crate::model::{AppModel, AppModelEvent};
use crate::modules::ModuleSettings;
use crate::shell::style::Colors;
use crate::storage::{ui, Runner};
use chairphoto_core::app::{CatalogIdentity, CoreEvent};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::*;
use gpui_kit::{div, px, Context, Entity, Subscription, TestSupportExt as _, Window};
use std::sync::Arc;

/// The keys core's sign-in reads (`chairphoto_core::app::oauth::API_KEY` / `API_SECRET`,
/// compiled only with a service's feature) and the max long edge every upload reads.
pub const API_KEY: &str = "api_key";
pub const API_SECRET: &str = "api_secret";
pub const MAX_LONG_EDGE: &str = chairphoto_core::app::uploads::MAX_LONG_EDGE;

/// What Save and Connect answer while the fields do not show the bound catalog's settings.
pub const NOT_LOADED: &str = "The settings are still loading for this catalog — try again in a moment.";

pub struct OAuthSettings {
    settings: ModuleSettings,
    service: Arc<dyn PublishService>,
    pub key: Entity<InputState>,
    pub secret: Entity<InputState>,
    pub max_long_edge: Entity<InputState>,
    pub verifier: Entity<InputState>,
    /// Values to put in the fields on the next render (it needs the window): a load's answer,
    /// or the empty fields of a catalog switch.
    fill: Option<[String; 3]>,
    /// Clear the verifier field on the next render.
    clear_verifier: bool,
    /// The authorize URL while a Connect is waiting for its verifier.
    pub auth_url: Option<String>,
    pub connected: bool,
    pub status: String,
    /// The catalog whose stored settings the fields were loaded from; `None` until a load for
    /// the bound catalog lands (and again from a switch or a rebind until the next one does).
    loaded: Option<CatalogIdentity>,
    /// Bumped by a catalog switch and by a rebind: an answer for the catalog before is dropped.
    generation: u64,
    _subscriptions: Vec<Subscription>,
}

impl OAuthSettings {
    pub fn new(
        settings: ModuleSettings,
        model: &Entity<AppModel>,
        service: Arc<dyn PublishService>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let key = cx.new(|cx| InputState::new(window, cx));
        let secret = cx.new(|cx| InputState::new(window, cx).masked(true));
        let max_long_edge = cx.new(|cx| InputState::new(window, cx).placeholder("0 = full resolution"));
        let verifier = cx.new(|cx| InputState::new(window, cx).placeholder("Paste verifier code"));
        let subs = vec![
            cx.subscribe(&verifier, |this: &mut Self, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.finish(cx);
                }
            }),
            cx.subscribe(model, |this: &mut Self, model, event: &AppModelEvent, cx| match event {
                AppModelEvent::Core(CoreEvent::CatalogSwitched(_)) => this.catalog_switched(cx),
                AppModelEvent::CatalogRead => {
                    let open = model.read(cx).catalog_identity();
                    if let Some(open) = open.filter(|&o| Some(o) != this.settings.catalog()) {
                        this.generation += 1;
                        this.loaded = None;
                        this.settings = this.settings.rebound(open);
                        this.load(cx);
                    }
                }
                _ => {}
            }),
        ];
        let mut view = OAuthSettings {
            settings,
            service,
            key,
            secret,
            max_long_edge,
            verifier,
            fill: None,
            clear_verifier: false,
            auth_url: None,
            connected: false,
            status: String::new(),
            loaded: None,
            generation: 0,
            _subscriptions: subs,
        };
        view.load(cx);
        view
    }

    /// Another catalog is open: its keys are not this one's. Clear everything, drop what is in
    /// flight and unbind the settings, so the next catalog read always rebinds and loads — even
    /// one for the new catalog that arrived before this event and bound the handle to it.
    fn catalog_switched(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.settings = self.settings.unbound();
        self.loaded = None;
        self.fill = Some(Default::default());
        self.clear_verifier = true;
        self.auth_url = None;
        self.connected = false;
        self.status.clear();
        cx.notify();
    }

    /// Run `work` on a worker with this section's settings; `land` its answer unless the
    /// catalog was switched meanwhile.
    fn run<R: Send + 'static>(
        &mut self,
        cx: &mut Context<Self>,
        work: impl FnOnce(&ModuleSettings, &dyn PublishService) -> R + Send + 'static,
        land: impl FnOnce(&mut Self, R, &mut Context<Self>) + 'static,
    ) {
        let (settings, service, generation) = (self.settings.clone(), self.service.clone(), self.generation);
        let rx = Runner::get(cx).run(move || work(&settings, &*service));
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else { return };
            this.update(cx, |v, cx| {
                if v.generation == generation {
                    land(v, result, cx);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Read the bound catalog's stored keys and whether the service is connected. Bound to
    /// none: nothing to read (the next catalog read binds and loads).
    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(from) = self.settings.catalog() else { return };
        self.run(
            cx,
            |settings, service| {
                // A read that fails must not pass for an empty setting: the fields would then
                // count as loaded, and Save would write blanks over the catalog's keys.
                let fields = [API_KEY, API_SECRET, MAX_LONG_EDGE].map(|k| settings.get(k).map(Option::unwrap_or_default));
                let connected = service.connected(settings).unwrap_or(false);
                match fields {
                    [Ok(key), Ok(secret), Ok(edge)] => Ok(([key, secret, edge], connected)),
                    [a, b, c] => Err(a.and(b).and(c).unwrap_err()),
                }
            },
            move |v, result, _| match result {
                Ok((fields, connected)) => {
                    v.fill = Some(fields);
                    v.connected = connected;
                    v.loaded = Some(from);
                }
                Err(e) => v.status = e,
            },
        );
    }

    /// The catalog the settings handle is bound to (tests).
    #[cfg(test)]
    pub(crate) fn bound_catalog(&self) -> Option<CatalogIdentity> {
        self.settings.catalog()
    }

    /// Whether the fields show the stored settings of the catalog the handle is bound to —
    /// the only state Save and Connect may write from.
    fn writable(&self) -> bool {
        self.loaded.is_some() && self.loaded == self.settings.catalog()
    }

    /// What Save writes: the trimmed fields (an empty max long edge = full resolution) — the
    /// values a landed load is about to put in them, if the render has not yet.
    fn fields(&self, cx: &Context<Self>) -> [(&'static str, String); 3] {
        let value = |i: &Entity<InputState>| i.read(cx).value().trim().to_string();
        let [key, secret, edge] = match &self.fill {
            Some(fill) => fill.clone().map(|v| v.trim().to_string()),
            None => [value(&self.key), value(&self.secret), value(&self.max_long_edge)],
        };
        [(API_KEY, key), (API_SECRET, secret), (MAX_LONG_EDGE, edge)]
    }

    /// Refuse a write while the fields are not loaded for the bound catalog.
    fn refuse_unloaded(&mut self, cx: &mut Context<Self>) -> bool {
        if self.writable() {
            return false;
        }
        self.status = NOT_LOADED.into();
        cx.notify();
        true
    }

    /// "Save keys".
    pub fn save(&mut self, cx: &mut Context<Self>) {
        if self.refuse_unloaded(cx) {
            return;
        }
        let fields = self.fields(cx);
        self.run(
            cx,
            move |settings, _| save(settings, &fields),
            |v, result, _| {
                v.status = match result {
                    Ok(()) => "Saved.".into(),
                    Err(e) => e,
                }
            },
        );
    }

    /// Connect / Reconnect: save, get the authorize URL, open it in the browser.
    pub fn connect(&mut self, cx: &mut Context<Self>) {
        if self.refuse_unloaded(cx) {
            return;
        }
        self.status.clear();
        let fields = self.fields(cx);
        self.run(
            cx,
            move |settings, service| {
                save(settings, &fields)?;
                service.begin_auth(settings)
            },
            |v, result, cx| match result {
                Ok(url) => {
                    cx.open_url(&url);
                    v.auth_url = Some(url);
                    v.status = "Authorize in your browser, then paste the verifier code below.".into();
                }
                Err(e) => v.status = e,
            },
        );
    }

    /// Finish / Enter: hand the verifier to the service.
    pub fn finish(&mut self, cx: &mut Context<Self>) {
        let verifier = self.verifier.read(cx).value().trim().to_string();
        if verifier.is_empty() {
            return;
        }
        self.status.clear();
        self.run(
            cx,
            move |settings, service| {
                service.complete_auth(settings, &verifier)?;
                service.connected(settings)
            },
            |v, result, _| match result {
                Ok(connected) => {
                    v.connected = connected;
                    v.auth_url = None;
                    v.clear_verifier = true;
                    v.status = "Connected.".into();
                }
                Err(e) => v.status = e,
            },
        );
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
        if let Some([key, secret, edge]) = self.fill.take() {
            self.key.update(cx, |i, cx| i.set_value(key, window, cx));
            self.secret.update(cx, |i, cx| i.set_value(secret, window, cx));
            self.max_long_edge.update(cx, |i, cx| i.set_value(edge, window, cx));
        }
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
