//! ChairPhoto's native GPUI front end (docs/plans/gpui, map #92): a window over
//! `chairphoto-core`, with no Tauri and no webview.
//!
//! [`run`] is the whole startup, in this order:
//!
//! 1. an [`AppState`] with the [`events::GpuiSink`] installed — first, so nothing the core
//!    starts can send into the void;
//! 2. `app::boot_with` — crash markers, upload sweep, Omarchy watcher, decode analyzers, image
//!    pool (the same startup the Tauri shell runs; this pool's runner decodes to BGRA
//!    textures, [`image_store::runner`]);
//! 3. the GPUI application: embedded fonts, gpui-kit's init, the theme from the current system
//!    theme (before the first window: `theme::init` switches to Light), the keymap;
//! 4. the [`model::AppModel`] entity and the event router, and the
//!    [`image_store::ImageStore`] (cleared on every catalog switch);
//! 5. `app::open_default_catalog`, off the UI thread;
//! 6. the main window, 1400×900, `app_id` `chairphoto`.
//!
//! Quitting — Ctrl+Q, or closing the main window — runs `crash_marker::clean_exit()`, as the
//! Tauri shell does at `RunEvent::Exit`: decodes a deliberate quit cuts short are not crashes.

pub mod assets;
#[cfg(feature = "edit")]
pub mod darkroom;
pub mod events;
pub mod image_store;
pub mod keymap;
pub mod model;
pub mod theme;
pub mod view;

#[cfg(test)]
mod image_tests;
#[cfg(test)]
mod tests;

use chairphoto_core::app::AppState;
use gpui_kit::{
    px, size, App, AppContext as _, Bounds, Entity, QuitMode, Subscription, TitlebarOptions,
    WindowBackgroundAppearance, WindowBounds, WindowOptions,
};
use image_store::ImageStore;
use model::AppModel;
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

/// Start the app and run until it quits.
pub fn run() {
    let state = AppState::default();
    let (sink, events_rx) = events::channel();
    state.set_events(Arc::new(sink));
    let boot = chairphoto_core::app::boot_with(&state, image_store::runner(state.clone()));
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
            let pool: Arc<dyn image_store::Submit> = boot.pool.clone();
            let images = cx.new(|cx| ImageStore::new(pool, image_store::DEFAULT_BUDGET_BYTES, cx));
            clear_images_on_catalog_switch(&model, &images, cx).detach();
            model.update(cx, |m, cx| m.open_default_catalog(cx));

            let options = main_window_options(cx);
            let opened = gpui_kit::open_window(options, cx, {
                let model = model.clone();
                move |window, cx| cx.new(|cx| view::RootView::new(model, images, window, cx))
            });
            match opened {
                Ok((handle, _)) => {
                    let main = handle.window_id();
                    cx.on_window_closed(move |cx, closed| {
                        if closed == main {
                            cx.quit();
                        }
                    })
                    .detach();
                }
                Err(e) => {
                    eprintln!("could not open the main window: {e}");
                    cx.quit();
                }
            }
        });
    // Also on the way out of the event loop, for a platform that returns without running
    // the quit observers. Idempotent.
    chairphoto_core::crash_marker::clean_exit();
}

/// Photo ids mean other photos after a catalog switch, so the image cache is dropped whenever
/// [`AppModel::catalog_epoch`] moves.
pub fn clear_images_on_catalog_switch(
    model: &Entity<AppModel>,
    images: &Entity<ImageStore>,
    cx: &mut App,
) -> Subscription {
    let images = images.clone();
    let mut seen = model.read(cx).catalog_epoch;
    cx.observe(model, move |model, cx| {
        let epoch = model.read(cx).catalog_epoch;
        if epoch != seen {
            seen = epoch;
            images.update(cx, |store, cx| store.clear(cx));
        }
    })
}
