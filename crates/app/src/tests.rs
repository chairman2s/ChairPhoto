//! Headless tests of the wiring: `run`'s own startup functions ([`start_core`], [`wire`])
//! with a test boot, the event bridge, the keymap, the quit paths. `#[gpui_kit::test]` runs on GPUI's test
//! platform (no window server, deterministic executor).
//!
//! What the test platform cannot show: `cx.quit()` is a no-op there
//! (gpui-pre platform/test/platform.rs `fn quit(&self) {}`), so these tests see that a quit
//! was *asked for* ([`QuitRequested`]) and, separately, that quitting runs `on_exit`
//! (`TestAppContext::quit` runs the app's quit observers); the platform's own quit →
//! event-loop exit is GPUI's. `QuitMode::Explicit` is set on the `Application` in `run` and
//! has no test-platform counterpart.

use crate::model::CatalogSummary;
use crate::{start_core, wire, QuitReason, QuitRequested, WireOptions, Wired};
use chairphoto_core::app::{AppState, CoreEvent, EventSink as _};
use chairphoto_core::appearance::SystemThemeResult;
use chairphoto_core::catalog::Catalog;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, TestAppContext};
use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

/// A private directory under the system temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir()
            .join(format!("chairphoto-app-test-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `run`'s startup, with a boot that starts nothing, no default catalog, and a counter in
/// place of `clean_exit`.
struct App {
    state: AppState,
    wired: Wired,
    exits: Rc<Cell<u32>>,
}

impl App {
    fn window(&self) -> AnyWindowHandle {
        *self.wired.main_window.as_ref().expect("the main window opened")
    }
}

fn start(cx: &mut TestAppContext) -> App {
    let (state, events_rx, ()) = start_core(|_| ());
    let exits = Rc::new(Cell::new(0));
    let on_exit = {
        let exits = exits.clone();
        Rc::new(move || exits.set(exits.get() + 1))
    };
    let wired = cx.update(|cx| {
        wire(
            cx,
            state.clone(),
            events_rx,
            None,
            &SystemThemeResult::unavailable(),
            WireOptions { on_exit, open_default_catalog: false },
        )
    });
    // Not parked here: the event router has not been polled yet, which the worker-thread
    // test relies on (below).
    App { state, wired, exits }
}


fn press(app: &App, key: &str, cx: &mut TestAppContext) {
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.press(key, cx);
    })
    .unwrap();
    cx.run_until_parked();
}





// --- run()'s wiring ----------------------------------------------------------------------

/// `start_core` installs the sink before `boot` runs: an event `boot` sends arrives, and
/// `boot` finds the sink slot already taken.
#[test]
fn the_sink_is_installed_before_boot_runs() {
    let (state, mut events_rx, second_install) = start_core(|state| {
        state.send(CoreEvent::CatalogSwitched("sent during boot".into()));
        state.set_events(Arc::new(chairphoto_core::app::NoEvents))
    });
    assert!(!second_install, "boot found no sink installed");
    let event = events_rx.try_recv().expect("the boot-time event arrived");
    assert_eq!(event.name(), "catalog:switched");
    drop(state);
}

/// Ctrl+Q through the real keymap reaches the real `Quit` handler, which asks to quit; and
/// quitting runs `on_exit` (`clean_exit` in `run`) exactly once.
#[gpui_kit::test]
fn ctrl_q_quits_and_quitting_runs_clean_exit(cx: &mut TestAppContext) {
    let app = start(cx);
    press(&app, "q", cx);
    assert_eq!(cx.update(|cx| cx.try_global::<QuitRequested>().copied()), None, "q alone is not bound");
    press(&app, "ctrl-q", cx);
    assert_eq!(cx.update(|cx| cx.try_global::<QuitRequested>().copied()), Some(QuitRequested(QuitReason::Requested)));
    assert_eq!(app.exits.get(), 0, "nothing ran before the app quit");
    cx.quit(); // the platform's quit: runs the app's quit observers
    assert_eq!(app.exits.get(), 1);
}

/// Closing the main window quits the app (`QuitMode::Explicit` would otherwise keep it
/// running while a pop-out loupe is open).
#[gpui_kit::test]
fn closing_the_main_window_quits(cx: &mut TestAppContext) {
    let app = start(cx);
    cx.update_window(app.window(), |_, window, _| window.remove_window()).unwrap();
    cx.run_until_parked();
    assert_eq!(
        cx.update(|cx| cx.try_global::<QuitRequested>().copied()),
        Some(QuitRequested(QuitReason::MainWindowClosed))
    );
}

/// A core event sent from a worker thread through the installed sink reaches the model on the
/// main thread, and `catalog:switched` makes the model re-read the open catalog.
#[gpui_kit::test]
fn a_core_event_from_a_worker_thread_reaches_the_model(cx: &mut TestAppContext) {
    let dir = TempDir::new("bridge");
    let app = start(cx);
    let db = dir.0.join("bridge.chairphoto");
    let catalog = Catalog::open(&db, &dir.0.join("photos")).unwrap();
    *app.state.catalog.lock().unwrap() = Some(catalog);

    // Sent before the router's first poll: GPUI's test scheduler rejects a wakeup from a
    // foreign thread (`assert_correct_thread`), so the worker must not wake a parked router
    // here. In the running app such wakeups are the normal case.
    let worker = app.state.clone();
    let path = db.to_string_lossy().to_string();
    std::thread::spawn(move || worker.send(CoreEvent::CatalogSwitched(path)))
        .join()
        .unwrap();
    cx.run_until_parked();

    app.wired.model.read_with(cx, |m, _| {
        assert_eq!(m.events_seen, 1);
        let line = m.last_event.as_ref().expect("the event was noted").to_string();
        assert!(line.starts_with("catalog:switched "), "{line}");
        assert!(line.contains("bridge.chairphoto"), "{line}");
        assert_eq!(
            m.catalog,
            Some(CatalogSummary { name: "bridge.chairphoto".into(), photo_count: 0 }),
            "catalog:switched must refresh the catalog summary"
        );
    });
}

/// `appearance:theme_changed` goes to the theme, not only to the model: the mapped Omarchy
/// palette becomes the Palette global.
#[gpui_kit::test]
fn a_theme_change_event_applies_the_omarchy_palette(cx: &mut TestAppContext) {
    let app = start(cx);
    let palette = chairphoto_core::appearance::parse_palette(
        r##"
mode = "light"
accent = "#2E7DE9"
selection = "#B7C1E3"
muted = "#848CB5"
background = "#E1E2E7"
foreground = "#3760BF"
"##,
    )
    .unwrap();
    app.state.send(CoreEvent::ThemeChanged(SystemThemeResult {
        available: true,
        theme_name: Some("tokyo-night-day".into()),
        palette: Some(palette),
    }));
    cx.run_until_parked();

    cx.update(|cx| {
        let p = cx.global::<crate::theme::Palette>();
        assert_eq!(p.omarchy_theme.as_deref(), Some("tokyo-night-day"));
        assert_eq!(p.tokens.panel, "#e1e2e7");
        assert_eq!(p.mode, gpui_kit::component::ThemeMode::Light);
    });
    app.wired.model.read_with(cx, |m, _| {
        let line = m.last_event.as_ref().unwrap().to_string();
        assert!(line.contains("tokyo-night-day"), "{line}");
    });
}
