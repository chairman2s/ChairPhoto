//! Headless tests of the wiring: event bridge → entity, keymap → action, theme event → theme.
//! `#[gpui_kit::test]` runs on GPUI's test platform (no window server, deterministic executor).

use crate::events;
use crate::keymap::{self, Quit};
use crate::model::{AppModel, CatalogSummary};
use crate::theme::Palette;
use crate::view::RootView;
use chairphoto_core::app::{AppState, CoreEvent, EventSink as _};
use chairphoto_core::catalog::Catalog;
use gpui_kit::{AppContext as _, Entity, TestAppContext};
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

/// An `AppState` with the GPUI sink installed, and the model the router feeds.
fn wired(cx: &mut TestAppContext) -> (AppState, Entity<AppModel>) {
    let state = AppState::default();
    let (sink, rx) = events::channel();
    assert!(state.set_events(Arc::new(sink)));
    let model = cx.update(|cx| {
        gpui_kit::init(cx);
        let model = cx.new(|_| AppModel::new(state.clone(), None));
        events::spawn_router(rx, model.clone(), cx).detach();
        model
    });
    (state, model)
}

/// A core event sent from a worker thread through the installed sink reaches the model on the
/// main thread, and `catalog:switched` makes the model re-read the open catalog.
#[gpui_kit::test]
fn a_core_event_from_a_worker_thread_reaches_the_model(cx: &mut TestAppContext) {
    let dir = TempDir::new("bridge");
    let (state, model) = wired(cx);
    let db = dir.0.join("bridge.chairphoto");
    let catalog = Catalog::open(&db, &dir.0.join("photos")).unwrap();
    *state.catalog.lock().unwrap() = Some(catalog);

    let worker = state.clone();
    let path = db.to_string_lossy().to_string();
    std::thread::spawn(move || worker.send(CoreEvent::CatalogSwitched(path)))
        .join()
        .unwrap();
    cx.run_until_parked();

    model.read_with(cx, |m, _| {
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
    let (state, model) = wired(cx);
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
    state.send(CoreEvent::ThemeChanged(chairphoto_core::appearance::SystemThemeResult {
        available: true,
        theme_name: Some("tokyo-night-day".into()),
        palette: Some(palette),
    }));
    cx.run_until_parked();

    cx.update(|cx| {
        let p = cx.global::<Palette>();
        assert_eq!(p.omarchy_theme.as_deref(), Some("tokyo-night-day"));
        assert_eq!(p.tokens.panel, "#e1e2e7");
        assert_eq!(p.mode, gpui_kit::component::ThemeMode::Light);
    });
    model.read_with(cx, |m, _| {
        let line = m.last_event.as_ref().unwrap().to_string();
        assert!(line.contains("tokyo-night-day"), "{line}");
    });
}

/// The real keymap, the real root view: Ctrl+Q in the root context dispatches `Quit`.
#[gpui_kit::test]
fn ctrl_q_in_the_root_view_dispatches_quit(cx: &mut TestAppContext) {
    let (_state, model) = wired(cx);
    let quits = Rc::new(Cell::new(0));
    cx.update(|cx| {
        cx.bind_keys(keymap::bindings());
        let quits = quits.clone();
        // Stands in for `run`'s handler, which calls `cx.quit()`.
        cx.on_action(move |_: &Quit, _| quits.set(quits.get() + 1));
    });
    let (_view, cx) = cx.add_window_view(|window, cx| RootView::new(model, window, cx));
    cx.run_until_parked();

    cx.simulate_keystrokes("ctrl-q");
    assert_eq!(quits.get(), 1, "ctrl-q did not reach the Quit handler");
    // A key with no binding in the root context dispatches nothing.
    cx.simulate_keystrokes("q");
    assert_eq!(quits.get(), 1);
}
