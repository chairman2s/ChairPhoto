//! Headless tests of the wiring and the shell: `run`'s own startup functions ([`start_core`],
//! [`wire`]) with a test boot, the event bridge, the keymap in its contexts, and the shell's
//! menus and controls driven through the real window. `#[gpui_kit::test]` runs on GPUI's test
//! platform (no window server, deterministic executor).
//!
//! What the test platform cannot show: `cx.quit()` is a no-op there
//! (gpui-pre platform/test/platform.rs `fn quit(&self) {}`), so these tests see that a quit
//! was *asked for* ([`QuitRequested`]) and, separately, that quitting runs `on_exit`
//! (`TestAppContext::quit` runs the app's quit observers); the platform's own quit →
//! event-loop exit is GPUI's. `QuitMode::Explicit` is set on the `Application` in `run` and
//! has no test-platform counterpart.
//!
//! Also: deep link → model, second launch → model, quit signal → `Quit` (gpui #100).

use crate::keymap::{self, contexts, Quit};
use crate::model::{not_yet_ported_line, AppModel, CatalogSummary, DeepLinkTarget};
use crate::shell::actions::{self as shell_actions, ToggleLeftPanel};
use crate::shell::state::Side;
use crate::{start_core, wire, QuitReason, QuitRequested, WireOptions, Wired};
use crate::launch;
use crate::single_instance::Request;
use chairphoto_model::deep_link::DeepLinkView;
use chairphoto_core::app::{AppState, CoreEvent, EventSink as _};
use chairphoto_core::appearance::SystemThemeResult;
use chairphoto_core::catalog::{Catalog, CullingFilter};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{div, AnyWindowHandle, Entity, AppContext as _, Context, FocusHandle, InteractiveElement as _, IntoElement, ParentElement as _, Render, Styled as _, TestAppContext, Window};
use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

/// A private directory under the system temp dir, removed on drop.
pub(crate) struct TempDir(pub(crate) PathBuf);

impl TempDir {
    pub(crate) fn new(tag: &str) -> Self {
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
pub(crate) struct App {
    pub(crate) state: AppState,
    pub(crate) wired: Wired,
    exits: Rc<Cell<u32>>,
}

impl App {
    pub(crate) fn window(&self) -> AnyWindowHandle {
        *self.wired.main_window.as_ref().expect("the main window opened")
    }
}

pub(crate) fn start(cx: &mut TestAppContext) -> App {
    // Storage jobs queue until a test runs them (`Runner::manual`): the core runtime's
    // threads could not wake GPUI's deterministic test scheduler.
    cx.update(|cx| cx.set_global(crate::storage::Runner::manual()));
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
            WireOptions { on_exit, open_default_catalog: false, unthrottled: false },
        )
    });
    // Not parked here: the event router has not been polled yet, which the worker-thread
    // test relies on (below).
    App { state, wired, exits }
}

/// Open a fresh catalog in `dir` and announce it as `catalog:switched`, as `switch_catalog`
/// does; the model and the shell re-read it.
fn open_catalog(app: &App, dir: &TempDir, cx: &mut TestAppContext) {
    let db = dir.0.join("shell.chairphoto");
    let catalog = Catalog::open(&db, &dir.0.join("photos")).unwrap();
    *app.state.catalog.lock().unwrap() = Some(catalog);
    app.state.send(CoreEvent::CatalogSwitched(db.to_string_lossy().to_string()));
    cx.run_until_parked();
}

/// Like [`open_catalog`], with `n` photos in it (unrated, unpicked); returns their ids.
pub(crate) fn open_catalog_with_photos(app: &App, dir: &TempDir, n: usize, cx: &mut TestAppContext) -> Vec<i64> {
    let db = dir.0.join("photos.chairphoto");
    let root = dir.0.join("photos");
    let catalog = Catalog::open(&db, &root).unwrap();
    let ids = (0..n)
        .map(|i| catalog.upsert_photo(&root.join(format!("2026/p{i}.ARW")), None, 0, 1).unwrap().id)
        .collect();
    *app.state.catalog.lock().unwrap() = Some(catalog);
    app.state.send(CoreEvent::CatalogSwitched(db.to_string_lossy().to_string()));
    cx.run_until_parked();
    ids
}

/// Take focus away from the main window (to a second window) and give it back: the
/// platform's activation change, as a window manager would send it.
fn refocus_main_window(app: &App, cx: &mut TestAppContext) {
    let other = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|_| gpui_kit::Empty)).unwrap()
    });
    other.update(cx, |_, window, _| window.activate_window()).unwrap();
    cx.run_until_parked();
    cx.update_window(app.window(), |_, window, _| window.activate_window()).unwrap();
    cx.run_until_parked();
}

pub(crate) fn press(app: &App, key: &str, cx: &mut TestAppContext) {
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.press(key, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

pub(crate) fn click(app: &App, id: &'static str, cx: &mut TestAppContext) {
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.click(id, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

/// Open the menu behind `trigger`, check row `index` is `label`, and click it.
fn click_menu_row(app: &App, trigger: &'static str, index: usize, label: &str, cx: &mut TestAppContext) {
    click(app, trigger, cx);
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        let mut menu = window.within("popup-menu");
        assert_eq!(menu.find(index).label(), Some(label), "{trigger} row {index}");
        menu.click(index, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

pub(crate) fn status(app: &App, cx: &mut TestAppContext) -> String {
    app.wired.model.read_with(cx, |m, _| m.status.to_string())
}

fn left_visible(app: &App, cx: &mut TestAppContext) -> bool {
    app.wired.shell.read_with(cx, |s, _| s.panel_visible(Side::Left))
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
/// main thread, and `catalog:switched` makes the model and the shell re-read the catalog.
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
        // The line is the payload cut at 160 characters (`payload_line`), so with a long
        // TMPDIR the file name may fall past the cut: compare as a prefix of the full line.
        let full = format!("catalog:switched {}", serde_json::to_string(&db.to_string_lossy()).unwrap());
        assert!(full.starts_with(line.trim_end_matches('…')), "{line}\nis not a prefix of\n{full}");
        assert_eq!(
            m.catalog,
            Some(CatalogSummary { name: "bridge.chairphoto".into(), photo_count: 0 }),
            "catalog:switched must refresh the catalog summary"
        );
    });
    app.wired.shell.read_with(cx, |s, _| {
        assert_eq!(s.scope_info.total, Some(0), "the shell counted the new catalog's matches");
        assert_eq!(s.counts.identity_debt, Some(0));
        assert_eq!(s.counts.trash, Some(0));
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

// --- keys --------------------------------------------------------------------------------

/// `[` and `]` toggle the side columns from the root context (App.tsx's window handler).
#[gpui_kit::test]
fn brackets_toggle_the_side_columns(cx: &mut TestAppContext) {
    let app = start(cx);
    assert!(left_visible(&app, cx));
    press(&app, "[", cx);
    assert!(!left_visible(&app, cx));
    press(&app, "[", cx);
    assert!(left_visible(&app, cx));
    press(&app, "]", cx);
    assert!(!app.wired.shell.read_with(cx, |s, _| s.panel_visible(Side::Right)));
}

/// An open menu owns the keyboard: `[` does nothing while it is focused, Escape closes it,
/// and then `[` works again (Menu.tsx swallowed every key but its own).
#[gpui_kit::test]
fn an_open_menu_swallows_the_shell_keys(cx: &mut TestAppContext) {
    let app = start(cx);
    click(&app, "more-menu", cx);
    press(&app, "[", cx);
    assert!(left_visible(&app, cx), "[ reached the shell through an open menu");
    press(&app, "escape", cx);
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("popup-menu").is_none(), "Escape closed the menu");
    })
    .unwrap();
    press(&app, "[", cx);
    assert!(!left_visible(&app, cx), "the root has focus back");
}

/// A focused gpui-component `Input` gets `[` and `]` as text rather than toggling a column,
/// and the same key toggles once focus is back on the root: the `Input` context is what
/// mutes it (React's `INPUT`/`TEXTAREA` guard).
#[gpui_kit::test]
fn brackets_are_text_in_a_focused_input(cx: &mut TestAppContext) {
    use gpui_kit::component::input::{Input, InputState};
    struct Field {
        root: FocusHandle,
        input: Entity<InputState>,
        toggles: Rc<Cell<u32>>,
    }
    impl Render for Field {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let toggles = self.toggles.clone();
            div()
                .id("field-root")
                .key_context(contexts::ROOT)
                .track_focus(&self.root)
                .on_action(move |_: &ToggleLeftPanel, _, _| toggles.set(toggles.get() + 1))
                .size_full()
                .child(Input::new(&self.input).id("field").w(gpui_kit::px(240.)))
        }
    }
    cx.update(|cx| {
        gpui_kit::init(cx);
        cx.bind_keys(keymap::bindings());
    });
    let toggles = Rc::new(Cell::new(0));
    let (handle, view) = cx.update(|cx| {
        gpui_kit::open_window(Default::default(), cx, {
            let toggles = toggles.clone();
            move |window, cx| {
                let input = cx.new(|cx| InputState::new(window, cx));
                cx.new(|cx| Field { root: cx.focus_handle(), input, toggles })
            }
        })
        .unwrap()
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("field", cx);
        window.press("[", cx);
        window.press("]", cx);
        window.input("x", cx);
        assert_eq!(window.find("field").value(), Some("[]x"), "the brackets were typed");
    })
    .unwrap();
    assert_eq!(toggles.get(), 0, "[ toggled a column from inside an input");
    assert_eq!(view.read_with(cx, |f, cx| f.input.read(cx).value().to_string()), "[]x");

    cx.update_window(handle, |_, window, cx| {
        view.read(cx).root.clone().focus(window, cx);
        window.render_frame(cx);
        window.press("[", cx);
    })
    .unwrap();
    assert_eq!(toggles.get(), 1, "with the root focused, [ toggles");
}

// --- menus and controls ------------------------------------------------------------------

/// The menus dispatch real actions to the root: a check item toggles shell state, and an
/// unported command answers with its ticket.
#[gpui_kit::test]
fn menu_items_dispatch_their_actions(cx: &mut TestAppContext) {
    let dir = TempDir::new("menus");
    let app = start(cx);
    open_catalog(&app, &dir, cx);

    click_menu_row(&app, "more-menu", 13, "Tags & collections panel", cx);
    assert!(!left_visible(&app, cx), "More ⋯ → View → Tags & collections panel");

    click_menu_row(&app, "more-menu", 0, "Open loupe in a new window", cx);
    assert_eq!(status(&app, cx), not_yet_ported_line("Open loupe in a new window", 110));

    assert!(app.wired.shell.read_with(cx, |s, _| s.cache_previews));
    click_menu_row(&app, "import-menu", 4, "Cache previews on import", cx);
    assert!(!app.wired.shell.read_with(cx, |s, _| s.cache_previews));

    // Enabled once a catalog is open.
    click_menu_row(&app, "import-menu", 3, "Rescan library", cx);
    assert_eq!(status(&app, cx), "Scanning library…");
}

/// Every not-yet-ported action is handled, with a status line naming its ticket.
#[gpui_kit::test]
fn every_unported_action_reports_its_ticket(cx: &mut TestAppContext) {
    let app = start(cx);
    for (action, label, ticket) in shell_actions::not_yet_ported_actions() {
        let name = action.name();
        cx.update_window(app.window(), |_, window, cx| window.dispatch_action(action, cx)).unwrap();
        cx.run_until_parked();
        assert_eq!(status(&app, cx), not_yet_ported_line(label, ticket), "{name}");
    }
}

/// The command pill edits the Library scope; the collection browser's "All photos" widens it
/// again; a section header collapses its section.
#[gpui_kit::test]
fn the_command_pill_and_browser_edit_the_scope(cx: &mut TestAppContext) {
    let dir = TempDir::new("pill");
    let app = start(cx);
    // Two unrated, unpicked photos: the whole library counts 2, and Picks counts 0, so a
    // count that was not re-read after the click stays visibly at 2.
    open_catalog_with_photos(&app, &dir, 2, cx);
    let revision = app.wired.shell.read_with(cx, |s, _| s.library.query_revision());
    assert_eq!(app.wired.shell.read_with(cx, |s, _| s.scope_info.total), Some(2));

    click(&app, "filter-Unrated", cx);
    assert_eq!(app.wired.shell.read_with(cx, |s, _| s.scope_info.total), Some(2), "both are unrated");
    click(&app, "filter-Picks", cx);
    app.wired.shell.read_with(cx, |s, _| {
        assert_eq!(s.library.scope().filter, CullingFilter::Pick);
        assert!(s.library.query_revision() > revision);
        assert_eq!(s.scope_info.total, Some(0), "the filtered count was re-read");
    });
    click(&app, "filter-All", cx);
    assert_eq!(app.wired.shell.read_with(cx, |s, _| s.scope_info.total), Some(2), "and back");
    click(&app, "label-Red", cx);
    app.wired.shell.read_with(cx, |s, _| {
        assert_eq!(s.library.scope().labels, vec!["Red".to_string()]);
        assert_eq!(s.scope_info.total, Some(0), "no photo carries the Red label");
    });

    app.wired.shell.update(cx, |s, cx| s.update_scope(cx, |l| l.select_album(Some(7))));
    cx.run_until_parked();
    assert!(!app.wired.shell.read_with(cx, |s, _| s.is_all_scope()));
    click(&app, "browser-all", cx);
    assert!(app.wired.shell.read_with(cx, |s, _| s.is_all_scope()), "All photos widened the scope");

    use crate::shell::state::Section;
    assert!(app.wired.shell.read_with(cx, |s, _| s.section_open(Section::Albums)));
    click(&app, "section-albums", cx);
    assert!(!app.wired.shell.read_with(cx, |s, _| s.section_open(Section::Albums)));
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

/// The deep-link tests' view of `start`: the real wiring's state and model (gpui #100's tests
/// predate `start_core`/`wire` and read the model directly).
fn wired(cx: &mut TestAppContext) -> (AppState, Entity<AppModel>) {
    let app = start(cx);
    (app.state.clone(), app.wired.model.clone())
}

fn model_status(model: &Entity<AppModel>, cx: &mut TestAppContext) -> String {
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
    assert_eq!(model_status(&model, cx), "Deep link: waiting for the catalog…");
    model.read_with(cx, |m, _| assert!(m.deep_link.is_none()));

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
    // The shell selected it in the widened grid; the loupe itself is #109.
    assert_eq!(model_status(&model, cx), not_yet_ported_line("Deep link into the loupe", 109));
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
    assert_eq!(model_status(&model, cx), "Deep link: filter by tag Places/Oslo");

    let missing = "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d";
    model.update(cx, |m, cx| m.open_url(&format!("chairphoto://{missing}/develop"), cx));
    cx.run_until_parked();
    assert_eq!(model_status(&model, cx), format!("Deep link: no photo {missing} in this catalog"));
    model.update(cx, |m, cx| m.open_url(&format!("chairphoto://tag/{missing}"), cx));
    cx.run_until_parked();
    assert_eq!(model_status(&model, cx), format!("Deep link: no tag {missing} in this catalog"));

    model.update(cx, |m, cx| m.open_url("chairphoto://album/x", cx));
    assert_eq!(model_status(&model, cx), "Deep link: not a ChairPhoto link: chairphoto://album/x");
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

    let (tx, rx) = launch::request_queue();
    cx.update(|cx| launch::spawn_request_router(rx, model.clone(), None, cx).detach());
    let request = Request {
        urls: vec![format!("chairphoto://tag/{tag_uuid}"), format!("chairphoto://{photo_uuid}")],
    };
    std::thread::spawn(move || tx.send(request).unwrap()).join().unwrap();
    cx.run_until_parked();

    model.read_with(cx, |m, _| match &m.deep_link {
        Some(DeepLinkTarget::Photo { uuid, view, .. }) => {
            assert_eq!(uuid, &photo_uuid);
            assert_eq!(*view, DeepLinkView::Grid);
        }
        other => panic!("expected the photo (the last link), got {other:?}"),
    });
    assert_eq!(model_status(&model, cx), "Deep link: 2026/a.ARW → Library");
}

/// A second catalog (its own directory) holding a photo with the same relative path; returns
/// the photo's uuid.
fn second_catalog(dir: &TempDir) -> (Catalog, String) {
    let root = dir.0.join("photos-b");
    let catalog = Catalog::open(&dir.0.join("other.chairphoto"), &root).unwrap();
    let photo = catalog.upsert_photo(&root.join("2026/b.ARW"), None, 0, 1).unwrap();
    (catalog, photo.uuid)
}

/// Links that arrive before the catalog opens do not pile up: only the newest can land (each
/// resolution supersedes the one before), so only the newest waits, and it is the one
/// applied once the catalog is in.
#[gpui_kit::test]
fn links_waiting_for_the_catalog_are_coalesced_to_the_newest(cx: &mut TestAppContext) {
    let dir = TempDir::new("link-flood");
    let (state, model) = wired(cx);
    let (photo_uuid, tag_uuid) = catalog_with_a_photo_and_a_tag(&dir, &state);
    for _ in 0..100 {
        model.update(cx, |m, cx| m.open_url(&format!("chairphoto://{photo_uuid}"), cx));
    }
    model.update(cx, |m, cx| m.open_url(&format!("chairphoto://tag/{tag_uuid}"), cx));
    assert_eq!(model.read_with(cx, |m, _| m.pending_link_count()), 1);

    model.update(cx, |m, cx| m.refresh(cx));
    cx.run_until_parked();
    model.read_with(cx, |m, _| assert!(matches!(m.deep_link, Some(DeepLinkTarget::Tag { .. }))));
    assert_eq!(model.read_with(cx, |m, _| m.pending_link_count()), 0);
}

/// The queue between the single-instance thread and the main thread is bounded: past
/// `MAX_QUEUED_REQUESTS` a request is refused `Busy` (the second launch hears `busy`), and
/// draining makes room again.
#[gpui_kit::test]
fn the_second_launch_queue_is_bounded(cx: &mut TestAppContext) {
    let (_state, model) = wired(cx);
    let (tx, rx) = launch::request_queue();
    for _ in 0..launch::MAX_QUEUED_REQUESTS {
        assert_eq!(tx.send(Request::default()), Ok(()));
    }
    assert_eq!(tx.send(Request::default()), Err(crate::single_instance::Refused::Busy));
    cx.update(|cx| launch::spawn_request_router(rx, model.clone(), None, cx).detach());
    cx.run_until_parked();
    assert_eq!(tx.send(Request::default()), Ok(()), "draining did not make room");
}

/// A catalog switch drops the resolved link: its photo id is the old catalog's.
#[gpui_kit::test]
fn a_catalog_switch_drops_the_resolved_link(cx: &mut TestAppContext) {
    let dir = TempDir::new("link-switch");
    let (state, model) = wired(cx);
    let (photo_uuid, _) = catalog_with_a_photo_and_a_tag(&dir, &state);
    model.update(cx, |m, cx| m.refresh(cx));
    cx.run_until_parked();
    model.update(cx, |m, cx| m.open_url(&format!("chairphoto://{photo_uuid}"), cx));
    cx.run_until_parked();
    model.read_with(cx, |m, _| assert!(matches!(m.deep_link, Some(DeepLinkTarget::Photo { .. }))));

    let (other, _) = second_catalog(&dir);
    *state.catalog.lock().unwrap() = Some(other);
    model.update(cx, |m, cx| m.on_core_event(&CoreEvent::CatalogSwitched("other".into()), cx));
    cx.run_until_parked();
    model.read_with(cx, |m, _| assert!(m.deep_link.is_none(), "the old catalog's photo id survived the switch"));
}

/// A link still resolving when the catalog switches is neither lost nor landed from the old
/// resolution: it waits for the new catalog and resolves there. Here the new catalog is not
/// readable at first (state holds none while it opens), so the old resolution fails; that
/// failure must not land, and the link must resolve once the new catalog is in.
#[gpui_kit::test]
fn a_link_in_flight_at_a_switch_resolves_against_the_new_catalog(cx: &mut TestAppContext) {
    let dir = TempDir::new("link-inflight");
    let (state, model) = wired(cx);
    catalog_with_a_photo_and_a_tag(&dir, &state);
    model.update(cx, |m, cx| m.refresh(cx));
    cx.run_until_parked();
    let (other, other_uuid) = second_catalog(&dir);

    // Started against the first catalog, not yet run; then the switch begins.
    model.update(cx, |m, cx| m.open_url(&format!("chairphoto://{other_uuid}/develop"), cx));
    *state.catalog.lock().unwrap() = None;
    model.update(cx, |m, cx| m.on_core_event(&CoreEvent::CatalogSwitched("other".into()), cx));
    cx.run_until_parked();
    // The switch's refresh found no catalog; the old resolution's error did not land.
    assert_eq!(model_status(&model, cx), "Catalog unavailable: No catalog is open");
    model.read_with(cx, |m, _| assert!(m.deep_link.is_none()));

    *state.catalog.lock().unwrap() = Some(other);
    model.update(cx, |m, cx| m.refresh(cx));
    cx.run_until_parked();
    model.read_with(cx, |m, _| match &m.deep_link {
        Some(DeepLinkTarget::Photo { uuid, path, view, .. }) => {
            assert_eq!(uuid, &other_uuid);
            assert_eq!(path, "2026/b.ARW");
            assert_eq!(*view, DeepLinkView::Develop);
        }
        other => panic!("the link was lost across the switch: {other:?}"),
    });
}

/// A newer link always wins across a switch (Codex gate, #100): link A is in flight when the
/// catalog switches, so it is carried over to wait for the new catalog's read; link B
/// arrives after the switch but before that read lands. B must be the one applied — the
/// refresh must not replay the older A over it.
#[gpui_kit::test]
fn a_link_after_a_switch_supersedes_the_carried_over_one(cx: &mut TestAppContext) {
    let dir = TempDir::new("link-newer");
    let (state, model) = wired(cx);
    catalog_with_a_photo_and_a_tag(&dir, &state);
    model.update(cx, |m, cx| m.refresh(cx));
    cx.run_until_parked();
    let (other, other_uuid) = second_catalog(&dir);
    let tag_id = other.create_tag("Places/Bergen").unwrap();
    let tag_uuid = other.get_tag(tag_id).unwrap().uuid;

    // A starts against the first catalog; the switch lands (the new catalog already in
    // state, as `switch_catalog` leaves it) before A or the switch's refresh has run.
    model.update(cx, |m, cx| m.open_url(&format!("chairphoto://{other_uuid}"), cx));
    *state.catalog.lock().unwrap() = Some(other);
    model.update(cx, |m, cx| m.on_core_event(&CoreEvent::CatalogSwitched("other".into()), cx));
    // B, newer, before the refresh lands.
    model.update(cx, |m, cx| m.open_url(&format!("chairphoto://tag/{tag_uuid}"), cx));
    cx.run_until_parked();

    model.read_with(cx, |m, _| match &m.deep_link {
        Some(DeepLinkTarget::Tag { uuid, full_path, .. }) => {
            assert_eq!(uuid, &tag_uuid);
            assert_eq!(full_path, "Places/Bergen");
        }
        other => panic!("the older link landed over the newer one: {other:?}"),
    });
    assert_eq!(model_status(&model, cx), "Deep link: filter by tag Places/Bergen");
}

/// Asking to quit closes the single-instance endpoint at once (before the event loop ends),
/// so a second launch from then on is told `closing` rather than `ok`.
#[gpui_kit::test]
fn a_quit_request_closes_the_single_instance_endpoint(cx: &mut TestAppContext) {
    let _app = start(cx);
    let closer = crate::single_instance::Closer::default();
    cx.update(|cx| launch::close_instance_on_quit(closer.clone(), cx));
    cx.run_until_parked();
    assert!(!closer.is_closed());
    cx.update(|cx| crate::quit_app(QuitReason::Requested, cx));
    cx.run_until_parked();
    assert!(closer.is_closed(), "the endpoint stayed open after the quit was requested");
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

/// React re-read the back-up queue and the trash count on window focus: a change made
/// elsewhere (another tool, a finished backup) shows when the user comes back.
#[gpui_kit::test]
fn window_focus_rereads_the_pending_and_trash_counts(cx: &mut TestAppContext) {
    let dir = TempDir::new("focus");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 2, cx);
    app.wired.shell.read_with(cx, |s, _| assert_eq!((s.counts.pending, s.counts.trash), (0, Some(0))));

    // Changed behind the shell's back: nothing announces these.
    {
        let guard = app.state.catalog.lock().unwrap();
        let c = guard.as_ref().unwrap();
        c.enqueue_operation("backup", ids[0]).unwrap();
        c.trash_photos(&[ids[1]]).unwrap();
    }
    cx.run_until_parked();
    app.wired.shell.read_with(cx, |s, _| assert_eq!((s.counts.pending, s.counts.trash), (0, Some(0))));

    refocus_main_window(&app, cx);
    app.wired.shell.read_with(cx, |s, _| {
        assert_eq!((s.counts.pending, s.counts.trash), (1, Some(1)), "focus re-read the counts")
    });
}

/// A catalog switch disowns every read the old catalog started: reads issued just before
/// the switch, which then run against the old catalog and land after it, are dropped
/// (AGENTS.md: a catalog switch makes older workers unreachable as owners). The switch is
/// fed to the shell directly so no fresh read follows it and nothing can mask a stale one.
#[gpui_kit::test]
fn reads_started_before_a_catalog_switch_are_dropped(cx: &mut TestAppContext) {
    let dir = TempDir::new("switch");
    let app = start(cx);
    open_catalog_with_photos(&app, &dir, 2, cx);
    app.wired.shell.read_with(cx, |s, _| {
        assert_eq!(s.scope_info.total, Some(2));
        assert_eq!(s.counts.trash, Some(0));
        assert!(!s.lists.facets.is_empty());
    });

    // Issue every kind of read, then switch — before any of them has run.
    app.wired.shell.update(cx, |s, cx| {
        s.refresh_catalog_data(cx); // lists + counts, and the scope count
        s.refresh_on_focus(cx); // pending + trash
        s.on_core_event(&CoreEvent::CatalogSwitched("another.chairphoto".into()), cx);
    });
    cx.run_until_parked(); // the reads run now, still against the old catalog

    app.wired.shell.read_with(cx, |s, _| {
        assert_eq!(s.scope_info.total, None, "the old catalog's scope count landed after the switch");
        assert_eq!(s.counts.trash, None, "the old catalog's trash count landed after the switch");
        assert!(s.lists.facets.is_empty(), "the old catalog's lists landed after the switch");
    });
}

// Storage and import (#114).
#[path = "storage_tests.rs"]
mod storage_tests;
