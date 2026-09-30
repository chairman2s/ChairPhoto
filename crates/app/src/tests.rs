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

use crate::keymap::{self, contexts};
use crate::model::{not_yet_ported_line, CatalogSummary};
use crate::shell::actions::{self as shell_actions, ToggleLeftPanel};
use crate::shell::state::Side;
use crate::{start_core, wire, QuitReason, QuitRequested, WireOptions, Wired};
use chairphoto_core::app::{AppState, CoreEvent, EventSink as _};
use chairphoto_core::appearance::SystemThemeResult;
use chairphoto_core::catalog::{Catalog, CullingFilter};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{div, AnyWindowHandle, AppContext as _, Context, FocusHandle, InteractiveElement as _, IntoElement, ParentElement as _, Render, Styled as _, TestAppContext, Window};
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

/// Open a fresh catalog in `dir` and announce it as `catalog:switched`, as `switch_catalog`
/// does; the model and the shell re-read it.
fn open_catalog(app: &App, dir: &TempDir, cx: &mut TestAppContext) {
    let db = dir.0.join("shell.chairphoto");
    let catalog = Catalog::open(&db, &dir.0.join("photos")).unwrap();
    *app.state.catalog.lock().unwrap() = Some(catalog);
    app.state.send(CoreEvent::CatalogSwitched(db.to_string_lossy().to_string()));
    cx.run_until_parked();
}

fn press(app: &App, key: &str, cx: &mut TestAppContext) {
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.press(key, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn click(app: &App, id: &'static str, cx: &mut TestAppContext) {
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

fn status(app: &App, cx: &mut TestAppContext) -> String {
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
        assert!(line.starts_with("catalog:switched "), "{line}");
        assert!(line.contains("bridge.chairphoto"), "{line}");
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

/// A focused text input types `[` rather than toggling a column.
#[gpui_kit::test]
fn brackets_are_text_in_a_focused_input(cx: &mut TestAppContext) {
    struct Field {
        root: FocusHandle,
        input: FocusHandle,
        toggles: Rc<Cell<u32>>,
    }
    impl Render for Field {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let toggles = self.toggles.clone();
            div()
                .key_context(contexts::ROOT)
                .track_focus(&self.root)
                .on_action(move |_: &ToggleLeftPanel, _, _| toggles.set(toggles.get() + 1))
                .size_full()
                .child(div().key_context(contexts::INPUT).track_focus(&self.input).size_full())
        }
    }
    cx.update(|cx| cx.bind_keys(keymap::bindings()));
    let toggles = Rc::new(Cell::new(0));
    let (view, cx) = cx.add_window_view({
        let toggles = toggles.clone();
        move |_, cx| Field { root: cx.focus_handle(), input: cx.focus_handle(), toggles }
    });
    cx.update(|window, cx| view.read(cx).input.clone().focus(window, cx));
    cx.simulate_keystrokes("[");
    assert_eq!(toggles.get(), 0, "[ toggled a column from inside an input");
    // The same key with the root focused does toggle: the input's context is what mutes it.
    cx.update(|window, cx| view.read(cx).root.clone().focus(window, cx));
    cx.simulate_keystrokes("[");
    assert_eq!(toggles.get(), 1);
}

// --- menus and controls ------------------------------------------------------------------

/// The menus dispatch real actions to the root: a check item toggles shell state, and an
/// unported command answers with its ticket.
#[gpui_kit::test]
fn menu_items_dispatch_their_actions(cx: &mut TestAppContext) {
    let dir = TempDir::new("menus");
    let app = start(cx);
    open_catalog(&app, &dir, cx);

    click_menu_row(&app, "more-menu", 12, "Tags & collections panel", cx);
    assert!(!left_visible(&app, cx), "More ⋯ → View → Tags & collections panel");

    click_menu_row(&app, "more-menu", 0, "Open loupe in a new window", cx);
    assert_eq!(status(&app, cx), not_yet_ported_line("Open loupe in a new window", 110));

    assert!(app.wired.shell.read_with(cx, |s, _| s.cache_previews));
    click_menu_row(&app, "import-menu", 4, "Cache previews on import", cx);
    assert!(!app.wired.shell.read_with(cx, |s, _| s.cache_previews));

    // Enabled once a catalog is open.
    click_menu_row(&app, "import-menu", 3, "Rescan library", cx);
    assert_eq!(status(&app, cx), not_yet_ported_line("Rescan library", 114));
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
    open_catalog(&app, &dir, cx);
    let revision = app.wired.shell.read_with(cx, |s, _| s.library.query_revision());

    click(&app, "filter-Picks", cx);
    click(&app, "label-Red", cx);
    app.wired.shell.read_with(cx, |s, _| {
        assert_eq!(s.library.scope().filter, CullingFilter::Pick);
        assert_eq!(s.library.scope().labels, vec!["Red".to_string()]);
        assert!(s.library.query_revision() > revision);
        assert_eq!(s.scope_info.total, Some(0), "the filtered count was re-read");
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
