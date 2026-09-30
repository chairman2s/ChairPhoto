//! ChairPhoto's native GPUI front end (docs/plans/gpui, map #92): a window over
//! `chairphoto-core`, with no Tauri and no webview.
//!
//! [`run`] is the whole startup, in this order:
//!
//! 0. the single-instance claim ([`single_instance`]): a second launch for the same app data
//!    dir forwards its `chairphoto://` URLs (or just a focus request) to the running instance
//!    and exits 0, before anything below runs; then the quit-signal handlers ([`signals`])
//!    and, in a debug build, the dev scheme-handler entry ([`desktop`]);
//! 1. an [`AppState`] with the [`events::GpuiSink`] installed — first, so nothing the core
//!    starts can send into the void;
//! 2. `app::boot` — crash markers, upload sweep, Omarchy watcher, decode analyzers, image
//!    pool (the same startup the Tauri shell runs);
//! 3. the GPUI application: embedded fonts, gpui-kit's init, the theme from the current system
//!    theme (before the first window: `theme::init` switches to Light), the keymap;
//! 4. the [`model::AppModel`] entity and the event router;
//! 5. `app::open_default_catalog`, off the UI thread;
//! 6. the main window, 1400×900, `app_id` `chairphoto`; then the routers for second launches
//!    and quit signals ([`launch`]), and this launch's own `chairphoto://` URLs.
//!
//! Quitting — Ctrl+Q, closing the main window, or `SIGTERM`/`SIGINT`/`SIGHUP` — runs
//! `crash_marker::clean_exit()`, as the Tauri shell does at `RunEvent::Exit`: decodes a
//! deliberate quit cuts short are not crashes. `clean_exit` also disarms the markers, so a
//! decode that starts between the quit and the process exit cannot leave one either.

pub mod assets;
pub mod desktop;
pub mod events;
pub mod keymap;
pub mod launch;
pub mod model;
pub mod signals;
pub mod single_instance;
pub mod theme;
pub mod view;

#[cfg(test)]
mod tests;

use chairphoto_core::app::AppState;
use futures::channel::mpsc::{unbounded, UnboundedSender};
use single_instance::{Claim, ClaimError, Primary, Request};
use gpui_kit::{
    px, size, AppContext as _, Bounds, QuitMode, TitlebarOptions, WindowBackgroundAppearance,
    WindowBounds, WindowOptions,
};
use std::sync::Arc;

/// Wayland `app_id` / X11 class: the `.desktop` file and the Hyprland rules match on it.
pub const APP_ID: &str = "chairphoto";

/// The main window's options: 1400×900 centred, server-side decorations (shell-apis.md § 7).
pub fn main_window_options(cx: &gpui_kit::App) -> WindowOptions {
    WindowOptions {
        app_id: Some(APP_ID.into()),
        titlebar: Some(TitlebarOptions { title: Some("ChairPhoto".into()), ..Default::default() }),
        window_min_size: Some(size(px(960.), px(600.))),
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(1400.), px(900.)),
            cx,
        ))),
        window_background: WindowBackgroundAppearance::Opaque,
        ..Default::default()
    }
}

/// Become this app data dir's primary instance, serving second launches into `tx`; or hand
/// `launch` to the instance that already is, and exit. `None`: single-instance could not be
/// set up (the reason is logged) and the app runs without it.
fn claim_single_instance(launch: &Request, tx: UnboundedSender<Request>) -> Option<Primary> {
    let endpoint = chairphoto_core::app::app_data_dir()
        .map_err(std::io::Error::other)
        .and_then(|dir| {
            // The key hashes the canonical path, which needs the directory to exist.
            std::fs::create_dir_all(&dir)?;
            single_instance::Endpoint::for_app_data_dir(&dir)
        });
    let endpoint = match endpoint {
        Ok(endpoint) => endpoint,
        Err(e) => {
            eprintln!("single instance: disabled: {e}");
            return None;
        }
    };
    match single_instance::claim(&endpoint, launch, single_instance::CONNECT_PATIENCE) {
        Ok(Claim::Primary(primary)) => {
            let served = primary.serve(move |request| {
                // Fails only once the app is shutting down.
                let _ = tx.unbounded_send(request);
            });
            served.map_err(|e| eprintln!("single instance: disabled: {e}")).ok()
        }
        Ok(Claim::Forwarded) => {
            eprintln!(
                "ChairPhoto is already running; handed it {} link(s) and asked it to come forward.",
                launch.urls.len()
            );
            std::process::exit(0);
        }
        Err(e @ ClaimError::NoAnswer(_)) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
        Err(e @ ClaimError::Endpoint(_)) => {
            eprintln!("single instance: disabled: {e}");
            None
        }
    }
}

/// Start the app and run until it quits.
pub fn run() {
    let launch = Request::from_args(std::env::args().skip(1));
    let (instance_tx, instance_rx) = unbounded::<Request>();
    // Held for the process's lifetime: the lock, and the socket file it removes on the way out.
    let _instance = claim_single_instance(&launch, instance_tx);

    let (quit_tx, quit_rx) = unbounded::<i32>();
    if let Err(e) = signals::install(move |signal| {
        let _ = quit_tx.unbounded_send(signal);
    }) {
        eprintln!("signals: {e}; SIGTERM/SIGINT will not quit cleanly");
    }

    // A dev build is not installed, so nothing else registers the scheme for it.
    #[cfg(debug_assertions)]
    std::thread::spawn(|| {
        let (Some(home), Ok(exe)) = (desktop::data_home(), std::env::current_exe()) else { return };
        let claim = std::env::var(desktop::CLAIM_ENV).is_ok_and(|v| v == "1");
        if let Err(e) = desktop::register_dev_handler(&home, &exe, claim) {
            eprintln!("deep-link dev registration failed: {e}");
        }
    });

    let state = AppState::default();
    let (sink, events_rx) = events::channel();
    state.set_events(Arc::new(sink));
    let boot = chairphoto_core::app::boot(&state);
    // Two small files under ~/.local/state/omarchy, read before the event loop starts so the
    // first frame is already in the right palette.
    let initial_theme = chairphoto_core::appearance::read_current_theme();

    gpui_kit::application()
        .with_assets(assets::Assets)
        // Quit only on request: Ctrl+Q, or the main window closing (below). With the default,
        // a pop-out loupe left open would keep the app alive after the main window closed.
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx| {
            if let Err(e) = assets::load_fonts(cx) {
                eprintln!("fonts: {e}; falling back to the system UI font");
            }
            gpui_kit::init(cx);
            theme::apply_system_theme(&initial_theme, cx);
            cx.bind_keys(keymap::bindings());
            cx.on_action(|_: &keymap::Quit, cx| cx.quit());
            cx.on_app_quit(|_| async { chairphoto_core::crash_marker::clean_exit() })
                .detach();

            let model = cx.new(|_| model::AppModel::new(state.clone(), Some(boot.pool.clone())));
            events::spawn_router(events_rx, model.clone(), cx).detach();
            model.update(cx, |m, cx| m.open_default_catalog(cx));

            let options = main_window_options(cx);
            let opened = gpui_kit::open_window(options, cx, {
                let model = model.clone();
                move |window, cx| cx.new(|cx| view::RootView::new(model, window, cx))
            });
            let main_window = match opened {
                Ok((handle, _)) => {
                    let main = handle.window_id();
                    cx.on_window_closed(move |cx, closed| {
                        if closed == main {
                            cx.quit();
                        }
                    })
                    .detach();
                    Some(handle)
                }
                Err(e) => {
                    eprintln!("could not open the main window: {e}");
                    cx.quit();
                    None
                }
            };

            launch::spawn_quit_on_signal(quit_rx, cx).detach();
            launch::spawn_request_router(instance_rx, model.clone(), main_window, cx).detach();
            // This launch's own links (the React app got them from onOpenUrl's getCurrent()).
            launch::apply_request(launch, &model, None, cx);
        });
    // Also on the way out of the event loop, for a platform that returns without running
    // the quit observers. Idempotent.
    chairphoto_core::crash_marker::clean_exit();
}
