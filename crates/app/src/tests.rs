//! Headless tests of the wiring: event bridge → entity, keymap → action, theme event → theme,
//! deep link → model, second launch → model, quit signal → `Quit`.
//! `#[gpui_kit::test]` runs on GPUI's test platform (no window server, deterministic executor).

use crate::events;
use crate::keymap::{self, Quit};
use crate::launch;
use crate::model::{AppModel, CatalogSummary, DeepLinkTarget};
use crate::single_instance::Request;
use chairphoto_model::deep_link::DeepLinkView;
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

/// A catalog with one photo and one tag, opened into `state`; returns the photo's and the
/// tag's uuids.
fn catalog_with_a_photo_and_a_tag(dir: &TempDir, state: &AppState) -> (String, String) {
    let root = dir.0.join("photos");
    let catalog = Catalog::open(&dir.0.join("links.chairphoto"), &root).unwrap();
    let photo = catalog.upsert_photo(&root.join("2026/a.ARW"), None, 0, 1).unwrap();
    let tag_id = catalog.create_tag("Places/Oslo").unwrap();
    let tag_uuid = catalog.get_tag(tag_id).unwrap().uuid;
    *state.catalog.lock().unwrap() = Some(catalog);
    (photo.uuid, tag_uuid)
}

fn status(model: &Entity<AppModel>, cx: &mut TestAppContext) -> String {
    model.read_with(cx, |m, _| m.status.to_string())
}

/// A link that arrives before the catalog is open waits for it (React's `ready` gate), then
/// resolves to the photo and the requested view, and says so on the status line.
#[gpui_kit::test]
fn a_photo_link_waits_for_the_catalog_then_resolves(cx: &mut TestAppContext) {
    let dir = TempDir::new("link-wait");
    let (state, model) = wired(cx);
    let (photo_uuid, _) = catalog_with_a_photo_and_a_tag(&dir, &state);

    let url = format!("chairphoto:///{}/LOUPE", photo_uuid.to_uppercase());
    model.update(cx, |m, cx| m.open_url(&url, cx));
    cx.run_until_parked();
    assert_eq!(status(&model, cx), "Deep link: waiting for the catalog…");
    model.read_with(cx, |m, _| assert_eq!(m.deep_link, None));

    model.update(cx, |m, cx| m.refresh(cx));
    cx.run_until_parked();
    model.read_with(cx, |m, _| match &m.deep_link {
        Some(DeepLinkTarget::Photo { uuid, path, view, .. }) => {
            assert_eq!(uuid, &photo_uuid);
            assert_eq!(path, "2026/a.ARW");
            assert_eq!(*view, DeepLinkView::Loupe);
        }
        other => panic!("expected the photo, got {other:?}"),
    });
    assert_eq!(status(&model, cx), "Deep link: 2026/a.ARW → loupe (view not ported yet)");
}

/// Tag links resolve by uuid; unknown uuids and non-links are reported, as App.tsx did.
#[gpui_kit::test]
fn tag_links_resolve_and_misses_are_reported(cx: &mut TestAppContext) {
    let dir = TempDir::new("link-tag");
    let (state, model) = wired(cx);
    let (_, tag_uuid) = catalog_with_a_photo_and_a_tag(&dir, &state);
    model.update(cx, |m, cx| m.refresh(cx));
    cx.run_until_parked();

    model.update(cx, |m, cx| m.open_url(&format!("chairphoto://tag/{tag_uuid}"), cx));
    cx.run_until_parked();
    model.read_with(cx, |m, _| match &m.deep_link {
        Some(DeepLinkTarget::Tag { uuid, full_path, .. }) => {
            assert_eq!(uuid, &tag_uuid);
            assert_eq!(full_path, "Places/Oslo");
        }
        other => panic!("expected the tag, got {other:?}"),
    });
    assert_eq!(status(&model, cx), "Deep link: filter by tag Places/Oslo (view not ported yet)");

    let missing = "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d";
    model.update(cx, |m, cx| m.open_url(&format!("chairphoto://{missing}/develop"), cx));
    cx.run_until_parked();
    assert_eq!(status(&model, cx), format!("Deep link: no photo {missing} in this catalog"));
    model.update(cx, |m, cx| m.open_url(&format!("chairphoto://tag/{missing}"), cx));
    cx.run_until_parked();
    assert_eq!(status(&model, cx), format!("Deep link: no tag {missing} in this catalog"));

    model.update(cx, |m, cx| m.open_url("chairphoto://album/x", cx));
    assert_eq!(status(&model, cx), "Deep link: not a ChairPhoto link: chairphoto://album/x");
    // A miss does not clear the last link that did resolve.
    model.read_with(cx, |m, _| assert!(matches!(m.deep_link, Some(DeepLinkTarget::Tag { .. }))));
}

/// A second launch's request, sent from the single-instance thread, reaches the model on the
/// main thread; of several links the last one wins, as each supersedes the one before.
#[gpui_kit::test]
fn a_second_launch_request_from_a_worker_thread_opens_its_links(cx: &mut TestAppContext) {
    let dir = TempDir::new("link-forward");
    let (state, model) = wired(cx);
    let (photo_uuid, tag_uuid) = catalog_with_a_photo_and_a_tag(&dir, &state);
    model.update(cx, |m, cx| m.refresh(cx));
    cx.run_until_parked();

    let (tx, rx) = futures::channel::mpsc::unbounded::<Request>();
    cx.update(|cx| launch::spawn_request_router(rx, model.clone(), None, cx).detach());
    let request = Request {
        urls: vec![format!("chairphoto://tag/{tag_uuid}"), format!("chairphoto://{photo_uuid}")],
    };
    std::thread::spawn(move || tx.unbounded_send(request).unwrap()).join().unwrap();
    cx.run_until_parked();

    model.read_with(cx, |m, _| match &m.deep_link {
        Some(DeepLinkTarget::Photo { uuid, view, .. }) => {
            assert_eq!(uuid, &photo_uuid);
            assert_eq!(*view, DeepLinkView::Grid);
        }
        other => panic!("expected the photo (the last link), got {other:?}"),
    });
    assert_eq!(status(&model, cx), "Deep link: 2026/a.ARW → Library (view not ported yet)");
}

/// A quit signal, delivered by the signal thread, dispatches `Quit` once — the Ctrl+Q path,
/// whose handler in `run` calls `cx.quit()` and so the quit observers and `clean_exit`.
#[gpui_kit::test]
fn a_quit_signal_dispatches_quit(cx: &mut TestAppContext) {
    let quits = Rc::new(Cell::new(0));
    cx.update(|cx| {
        let quits = quits.clone();
        cx.on_action(move |_: &Quit, _| quits.set(quits.get() + 1));
    });
    let (tx, rx) = futures::channel::mpsc::unbounded::<i32>();
    cx.update(|cx| launch::spawn_quit_on_signal(rx, cx).detach());
    // Sent before the router is first polled: GPUI's deterministic scheduler rejects a wakeup
    // from a foreign thread, so the channel must already hold the signals when it runs.
    assert_eq!(quits.get(), 0);
    std::thread::spawn(move || {
        tx.unbounded_send(libc::SIGTERM).unwrap();
        // A second signal is the handler's business (it forces the exit); the router quits once.
        tx.unbounded_send(libc::SIGINT).unwrap();
    })
    .join()
    .unwrap();
    cx.run_until_parked();
    assert_eq!(quits.get(), 1);
}
