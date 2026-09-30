//! What reaches the running app from outside its window, on the main thread: second launches
//! ([`crate::single_instance`]) and quit signals ([`crate::signals`]). Both arrive on worker
//! threads and cross to GPUI over a channel, as core events do ([`crate::events`]).

use crate::keymap::Quit;
use crate::model::AppModel;
use crate::single_instance::{Closer, Request};
use crate::QuitRequested;
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

/// Close the single-instance endpoint as soon as a quit is asked for (the [`QuitRequested`]
/// global, set by every quit path: Ctrl+Q, the main window closing, a quit signal), and again
/// as the app quits, for a platform-initiated quit. From then on a second launch is told
/// `closing` and starts fresh once this process has let go of the lock, rather than being
/// told `ok` for a link an exiting app would drop.
pub fn close_instance_on_quit(closer: Closer, cx: &mut App) {
    let on_request = closer.clone();
    cx.observe_global::<QuitRequested>(move |_| on_request.close()).detach();
    cx.on_app_quit(move |_| {
        closer.close();
        async {}
    })
    .detach();
}

/// Turn a quit signal into the [`Quit`] action — the same path as Ctrl+Q, so the quit
/// observers (and `crash_marker::clean_exit`) run. Detach it.
pub fn spawn_quit_on_signal(mut rx: UnboundedReceiver<i32>, cx: &mut App) -> Task<()> {
    cx.spawn(async move |cx| {
        if let Some(signal) = rx.next().await {
            eprintln!("signal {signal}: quitting");
            cx.update(|cx| cx.dispatch_action(&Quit));
        }
    })
}
