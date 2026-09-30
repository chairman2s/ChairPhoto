//! ChairPhoto's native GPUI front end (docs/plans/gpui, map #92): a window over
//! `chairphoto-core`, with no Tauri and no webview.
//!
//! [`run`] is the whole startup, in this order:
//!
//! 1. an [`AppState`] with the [`events::GpuiSink`] installed — first, so nothing the core
//!    starts can send into the void ([`start_core`]);
//! 2. `app::boot_with` — crash markers, upload sweep, Omarchy watcher, decode analyzers, image
//!    pool (the same startup the Tauri shell runs; this pool's runner decodes to BGRA
//!    textures, [`image_store::runner`]);
//! 3. the GPUI application ([`wire`]): embedded fonts, gpui-kit's init, the theme from the
//!    current system theme (before the first window: `theme::init` switches to Light), the
//!    keymap and the quit wiring;
//! 4. the [`model::AppModel`] entity and the event router, the [`shell::ShellState`], and the
//!    [`image_store::ImageStore`] (cleared on every catalog switch);
//! 5. `app::open_default_catalog`, off the UI thread;
//! 6. the main window, 1400×900, `app_id` `chairphoto`.
//!
//! Quitting — Ctrl+Q, or closing the main window — goes through [`quit_app`], and the quit
//! runs `crash_marker::clean_exit()`, as the Tauri shell does at `RunEvent::Exit`: decodes a
//! deliberate quit cuts short are not crashes.
//!
//! [`start_core`] and [`wire`] are the startup itself, not a copy of it: `run` is those two
//! calls around the event loop, and the tests call the same two functions with a test boot,
//! no default catalog and a counter for `clean_exit`.

pub mod assets;
pub mod darkroom;
pub mod events;
pub mod image_store;
pub mod keymap;
pub mod model;
pub mod shell;
pub mod theme;
pub mod view;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod image_tests;

use chairphoto_core::app::{AppState, CoreEvent};
use chairphoto_core::appearance::SystemThemeResult;
use chairphoto_core::image_pool::ImagePool;
use image_store::{ImageStore, Loaded};
use futures::channel::mpsc::UnboundedReceiver;
use model::AppModel;
use gpui_kit::{
    px, size, AnyWindowHandle, App, AppContext as _, Bounds, Entity, Global, QuitMode, TitlebarOptions,
    Subscription, WindowBackgroundAppearance, WindowBounds, WindowDecorations, WindowOptions,
};
use std::rc::Rc;
use std::sync::Arc;

/// Wayland `app_id` / X11 class: the `.desktop` file and the Hyprland rules match on it.
pub const APP_ID: &str = "chairphoto";

/// The main window's options: 1400×900 centred, server-side decorations (shell-apis.md § 7;
/// the title bar is the app's own header, see `shell::title_bar`).
pub fn main_window_options(cx: &App) -> WindowOptions {
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
        window_decorations: Some(WindowDecorations::Server),
        ..Default::default()
    }
}

/// Steps 1–2: a fresh [`AppState`] with the GPUI sink installed, *then* `boot` on it — so
/// whatever `boot` starts (the Omarchy watcher, workers) already has somewhere to send. Returns
/// the state, the receiver [`wire`] routes, and what `boot` returned.
pub fn start_core<B>(boot: impl FnOnce(&AppState) -> B) -> (AppState, UnboundedReceiver<CoreEvent>, B) {
    let state = AppState::default();
    let (sink, events_rx) = events::channel();
    let installed = state.set_events(Arc::new(sink));
    debug_assert!(installed, "a fresh AppState has no sink yet");
    let booted = boot(&state);
    (state, events_rx, booted)
}

/// Why the app is quitting. Recorded by [`quit_app`] as the [`QuitRequested`] global.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitReason {
    /// The `Quit` action (Ctrl+Q).
    Requested,
    /// The main window closed. With `QuitMode::Explicit` a pop-out loupe left open would
    /// otherwise keep the app alive.
    MainWindowClosed,
}

/// Set once a quit has been asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuitRequested(pub QuitReason);

impl Global for QuitRequested {}

/// Quit the app: record why, then ask the platform. The quit observers [`wire`] registered
/// (`clean_exit`) run as the event loop ends.
pub fn quit_app(reason: QuitReason, cx: &mut App) {
    eprintln!("quit: {reason:?}");
    cx.set_global(QuitRequested(reason));
    cx.quit();
}

/// How [`wire`] starts and ends. `run` passes [`WireOptions::production`]; tests replace
/// what would touch the user's data.
pub struct WireOptions {
    /// Runs once when the app quits: `crash_marker::clean_exit` in production.
    pub on_exit: Rc<dyn Fn()>,
    /// Open the default catalog under the XDG data dir (production), or leave the catalog
    /// to the caller (tests).
    pub open_default_catalog: bool,
}

impl WireOptions {
    pub fn production() -> Self {
        WireOptions { on_exit: Rc::new(chairphoto_core::crash_marker::clean_exit), open_default_catalog: true }
    }
}

/// What [`wire`] built.
pub struct Wired {
    pub model: Entity<AppModel>,
    /// The image layer's cache and request queue (#101), cleared on every catalog switch.
    pub images: Entity<ImageStore>,
    pub shell: Entity<shell::ShellState>,
    /// The main window, or why it could not open (the app has then been asked to quit).
    pub main_window: Result<AnyWindowHandle, String>,
}

/// Steps 3–6, inside the GPUI application: fonts, components, theme, keymap, quit wiring,
/// the entities and the event router, the default catalog, the main window.
pub fn wire(
    cx: &mut App,
    state: AppState,
    events_rx: UnboundedReceiver<CoreEvent>,
    pool: Option<Arc<ImagePool<Loaded>>>,
    initial_theme: &SystemThemeResult,
    options: WireOptions,
) -> Wired {
    if let Err(e) = assets::load_fonts(cx) {
        eprintln!("fonts: {e}; falling back to the system UI font");
    }
    gpui_kit::init(cx);
    theme::apply_system_theme(initial_theme, cx);
    cx.bind_keys(keymap::bindings());
    cx.on_action(|_: &keymap::Quit, cx| quit_app(QuitReason::Requested, cx));
    let on_exit = options.on_exit;
    cx.on_app_quit(move |_| {
        on_exit();
        async {}
    })
    .detach();

    let model = cx.new(|_| AppModel::new(state, pool.clone()));
    events::spawn_router(events_rx, model.clone(), cx).detach();
    let shell = cx.new(|cx| shell::ShellState::new(&model, cx));
    // Without a pool (tests), every image request fails at once instead of waiting forever.
    let submit: Arc<dyn image_store::Submit> = match &pool {
        Some(pool) => pool.clone(),
        None => Arc::new(NoPool),
    };
    let images = cx.new(|cx| ImageStore::new(submit, image_store::DEFAULT_BUDGET_BYTES, cx));
    clear_images_on_catalog_switch(&model, &images, cx).detach();
    if options.open_default_catalog {
        model.update(cx, |m, cx| m.open_default_catalog(cx));
    }

    let window_options = main_window_options(cx);
    let opened = gpui_kit::open_window(window_options, cx, {
        let (model, shell, images) = (model.clone(), shell.clone(), images.clone());
        move |window, cx| cx.new(|cx| view::RootView::new(model, shell, images, window, cx))
    });
    let main_window = match opened {
        Ok((handle, _)) => {
            let main = handle.window_id();
            cx.on_window_closed(move |cx, closed| {
                if closed == main {
                    quit_app(QuitReason::MainWindowClosed, cx);
                }
            })
            .detach();
            Ok(handle)
        }
        Err(e) => {
            eprintln!("could not open the main window: {e}");
            cx.quit();
            Err(e.to_string())
        }
    };
    Wired { model, images, shell, main_window }
}

/// Start the app and run until it quits.
pub fn run() {
    // The pool's runner decodes straight to BGRA textures (`image_store::runner`), not JPEG.
    let (state, events_rx, boot) =
        start_core(|state| chairphoto_core::app::boot_with(state, image_store::runner(state.clone())));
    // Two small files under ~/.local/state/omarchy, read before the event loop starts so the
    // first frame is already in the right palette.
    let initial_theme = chairphoto_core::appearance::read_current_theme();

    gpui_kit::application()
        .with_assets(assets::Assets)
        // Quit only on request: Ctrl+Q, or the main window closing (`wire`). With the default,
        // a pop-out loupe left open would keep the app alive after the main window closed.
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx| {
            wire(cx, state, events_rx, Some(boot.pool.clone()), &initial_theme, WireOptions::production());
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

/// [`wire`]'s image source when there is no decode pool (tests that pass `None`): every
/// request is answered at once with an error, so no cell waits forever.
struct NoPool;

impl image_store::Submit for NoPool {
    fn submit_batch(&self, batch: Vec<(chairphoto_core::image_pool::JobKey, chairphoto_core::image_pool::Respond<Loaded>)>) {
        for (_, respond) in batch {
            respond(Err("no image pool".into()));
        }
    }

    fn cancel(&self, _key: &chairphoto_core::image_pool::JobKey) -> bool {
        false
    }
}
