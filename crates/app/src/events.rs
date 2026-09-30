//! The event bridge: the core's [`CoreEvent`]s, sent from worker threads, reach GPUI entities
//! on the main thread.
//!
//! [`GpuiSink`] is the [`EventSink`] the app installs with `AppState::set_events` before
//! `app::boot`, so nothing the core starts can send into the void. `send` only pushes onto an
//! unbounded channel — it never blocks a worker. [`spawn_router`] is the one foreground task
//! that drains the channel and hands each event to [`route`], which gives it to the entity
//! that owns it:
//!
//! | Event | Owner |
//! |---|---|
//! | `appearance:theme_changed` | the theme ([`crate::theme::apply_system_theme`]) |
//! | everything else | [`AppModel`], which refreshes what the event invalidates |
//!
//! As views are ported they take their events here (the grid its `scan:progress`, the faces
//! panel its `faces:*`), each routed by variant to the entity that owns that state. A job's
//! events carry its job id; the owning entity drops a superseded job's stragglers, as the
//! React listeners do.

use crate::model::AppModel;
use chairphoto_core::app::{CoreEvent, EventSink};
use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::StreamExt as _;
use gpui_kit::{App, Entity, Task};

/// The GPUI app's event sink: every core event onto one channel for the main thread.
pub struct GpuiSink(UnboundedSender<CoreEvent>);

impl EventSink for GpuiSink {
    fn send(&self, event: CoreEvent) {
        // Fails only once the receiver is gone, i.e. the app is shutting down.
        let _ = self.0.unbounded_send(event);
    }
}

/// A sink and the receiver [`spawn_router`] drains.
pub fn channel() -> (GpuiSink, UnboundedReceiver<CoreEvent>) {
    let (tx, rx) = unbounded();
    (GpuiSink(tx), rx)
}

/// Hand one event to the entity that owns it.
pub fn route(event: CoreEvent, model: &Entity<AppModel>, cx: &mut App) {
    match event {
        CoreEvent::ThemeChanged(result) => {
            crate::theme::apply_system_theme(&result, cx);
            model.update(cx, |m, cx| m.note_event("appearance:theme_changed", theme_line(&result), cx));
        }
        other => model.update(cx, |m, cx| m.on_core_event(&other, cx)),
    }
}

fn theme_line(result: &chairphoto_core::appearance::SystemThemeResult) -> String {
    match (&result.theme_name, result.available) {
        (Some(name), true) => format!("following Omarchy theme {name}"),
        (None, true) => "following the Omarchy theme".into(),
        _ => "no Omarchy theme — ChairPhoto Standard".into(),
    }
}

/// Drain `rx` on the main thread for the app's lifetime, routing each event. The task ends
/// when every sender is gone, or is dropped with the app; detach it.
pub fn spawn_router(mut rx: UnboundedReceiver<CoreEvent>, model: Entity<AppModel>, cx: &mut App) -> Task<()> {
    cx.spawn(async move |cx| {
        while let Some(event) = rx.next().await {
            cx.update(|cx| route(event, &model, cx));
        }
    })
}
