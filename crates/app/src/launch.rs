//! What reaches the running app from outside its window, on the main thread: second launches
//! ([`crate::single_instance`]). They arrive on a worker thread and cross to GPUI over a
//! channel, as core events do ([`crate::events`]).

use crate::model::AppModel;
use crate::single_instance::Request;
use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt as _;
use gpui_kit::{AnyWindowHandle, App, Entity, Task};

/// Apply one second-launch request: bring the main window forward, then open each URL in
/// order (the last one wins, as in the React app).
///
/// On Wayland, raising a window needs the compositor's consent: GPUI asks with an
/// xdg-activation token of its own, which a compositor may refuse (Hyprland does unless
/// `misc:focus_on_activate` is set) and only mark the window urgent.
pub fn apply_request(request: Request, model: &Entity<AppModel>, main_window: Option<AnyWindowHandle>, cx: &mut App) {
    if let Some(window) = main_window {
        window.update(cx, |_, window, _| window.activate_window()).ok();
    }
    for url in &request.urls {
        model.update(cx, |m, cx| m.open_url(url, cx));
    }
}

/// Drain second-launch requests on the main thread for the app's lifetime; detach it.
pub fn spawn_request_router(
    mut rx: UnboundedReceiver<Request>,
    model: Entity<AppModel>,
    main_window: Option<AnyWindowHandle>,
    cx: &mut App,
) -> Task<()> {
    cx.spawn(async move |cx| {
        while let Some(request) = rx.next().await {
            cx.update(|cx| apply_request(request, &model, main_window, cx));
        }
    })
}
