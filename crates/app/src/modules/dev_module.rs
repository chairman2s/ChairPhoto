//! A trivial first-party module that contributes to every contribution point, to prove the
//! registry and the shell's slots end to end. Compiled only into tests and into builds with
//! the `dev-module` feature (`cargo run -p chairphoto-app --features dev-module`); never
//! shipped. It needs no backend.
//!
//! Each view is a line of text whose element id is `dev-<where>`, and every panel shows how
//! many core events the module has seen, so a glance at any slot shows the event delivery.

use super::{
    view, ActionKind, Contributions, MainView, Module, ModuleAction, ModuleHost, ModuleInstance, ModuleMeta, Panel,
    PanelSlot, PublishTarget, SettingsPanel,
};
use crate::shell::style::Colors;
use chairphoto_core::app::CoreEvent;
use gpui_kit::prelude::*;
use gpui_kit::{div, px, App, Context, Entity, SharedString, Subscription, TestSupportExt as _, Window};
use std::rc::Rc;

pub const DEV_MODULE_ID: &str = "dev";

pub struct DevModule;

/// What the dev module's views show.
#[derive(Default)]
pub struct DevState {
    pub events: u64,
    pub last_event: Option<&'static str>,
}

struct DevInstance {
    state: Entity<DevState>,
    host: ModuleHost,
}

/// One of the dev module's views: `dev-<where>` and the event count.
pub struct DevView {
    id: SharedString,
    text: SharedString,
    state: Entity<DevState>,
    _observe: Subscription,
}

impl Render for DevView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let state = self.state.read(cx);
        let seen = match state.last_event {
            Some(last) => format!("{} core events, last {last}", state.events),
            None => "no core events yet".to_string(),
        };
        div()
            .id(self.id.clone())
            .text_size(px(11.))
            .text_color(colors.dim)
            .child(format!("{} · {seen}", self.text))
            .test_support()
    }
}

fn dev_view(place: &'static str, text: &'static str, state: &Entity<DevState>) -> super::ViewFactory {
    let state = state.clone();
    view(move |_, cx: &mut Context<DevView>| {
        let _observe = cx.observe(&state, |_, _, cx| cx.notify());
        DevView { id: format!("dev-{place}").into(), text: text.into(), state: state.clone(), _observe }
    })
}

impl Module for DevModule {
    fn meta(&self) -> ModuleMeta {
        ModuleMeta::new(DEV_MODULE_ID, "Dev module")
            .description("Contributes to every slot, to check the module registry. Not shipped.")
    }

    fn load(&self, host: ModuleHost, cx: &mut App) -> Result<Box<dyn ModuleInstance>, String> {
        let state = cx.new(|_| DevState::default());
        Ok(Box::new(DevInstance { state, host }))
    }
}

impl ModuleInstance for DevInstance {
    fn contributions(&self) -> Contributions {
        let s = &self.state;
        let model = self.host.model().clone();
        let panel = |slot: PanelSlot, label: &'static str| Panel {
            id: format!("dev-{}", slot.name()).into(),
            label: label.into(),
            slot,
            view: dev_view(slot.name(), label, s),
        };
        Contributions {
            panels: vec![
                panel(PanelSlot::Inspector, "Dev inspector"),
                panel(PanelSlot::Sidebar, "Dev sidebar"),
                panel(PanelSlot::Loupe, "Dev loupe"),
                panel(PanelSlot::TagEditor, "Dev tag editor"),
            ],
            actions: vec![
                ModuleAction {
                    id: "dev-run".into(),
                    label: "Dev action".into(),
                    kind: ActionKind::Run(Rc::new(move |_: &mut Window, cx: &mut App| {
                        model.update(cx, |m, cx| {
                            m.status = "Dev module: action ran".into();
                            cx.notify();
                        })
                    })),
                },
                ModuleAction {
                    id: "dev-modal".into(),
                    label: "Dev dialog…".into(),
                    kind: ActionKind::Modal(dev_view("modal", "Dev dialog", s)),
                },
            ],
            publish_targets: vec![PublishTarget {
                id: "dev-target".into(),
                label: "Dev target".into(),
                view: dev_view("publish", "Dev publish form", s),
            }],
            settings: vec![SettingsPanel { id: "dev-settings".into(), view: dev_view("settings", "Dev settings", s) }],
            main_views: vec![MainView {
                id: "dev-view".into(),
                label: "Dev view".into(),
                icon: None,
                view: dev_view("main", "Dev main view", s),
            }],
        }
    }

    fn on_event(&mut self, event: &CoreEvent, cx: &mut App) {
        let name = event.name();
        self.state.update(cx, |s, cx| {
            s.events += 1;
            s.last_event = Some(name);
            cx.notify();
        });
    }
}
