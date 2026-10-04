//! Headless tests of the shell's per-machine layout, the panel keys' scope and the shell
//! timing host (#159), through the real wiring (`start` → `wire` → the main window).

use crate::machine_prefs::{MachinePrefs, FILE_NAME};
use crate::shell::actions::{StartCullSession, ToggleLeftPanel};
use crate::shell::state::{InspectorTab, Section, Side, Surface};
use crate::shell::timing::ShellTimer;
use crate::storage::Runner;
use crate::tests::{click, open_catalog_with_photos, press, start, start_with_options, App, TempDir};
use crate::WireOptions;
use chairphoto_model::shell_timing::SHELL_TIMING_KEY;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{point, px, AppContext as _, TestAppContext};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// Run every queued storage job (here: preference writes, setting writes), then let the UI
/// take the results.
fn work(cx: &mut TestAppContext) -> usize {
    let mut total = 0;
    loop {
        let ran = cx.update(|cx| Runner::get(cx).run_pending());
        cx.run_until_parked();
        if ran == 0 {
            return total;
        }
        total += ran;
    }
}

fn pending(cx: &mut TestAppContext) -> usize {
    cx.update(|cx| Runner::get(cx).pending())
}

fn visible(app: &App, side: Side, cx: &mut TestAppContext) -> bool {
    app.wired.shell.read_with(cx, |s, _| s.panel_visible(side))
}

/// The preference file as it is on disk now.
fn on_disk(path: &PathBuf) -> BTreeMap<String, String> {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The app with this machine's preferences read from `path`.
fn start_with_prefs(path: &PathBuf, cx: &mut TestAppContext) -> App {
    let prefs = MachinePrefs::load(path.clone());
    start_with_options(cx, move |o| WireOptions { machine_prefs: prefs, ..o })
}

// --- layout persistence -----------------------------------------------------------------

/// What the last session left in `machine-prefs.json` is what the shell opens with: React's
/// keys and values (`panel.*`, `panel.section.*`, `inspector.section.*`).
#[gpui_kit::test]
fn the_layout_opens_as_this_machine_left_it(cx: &mut TestAppContext) {
    let dir = TempDir::new("layout-restore");
    let path = dir.0.join(FILE_NAME);
    let stored = serde_json::json!({
        "panel.leftW": "250",
        "panel.rightW": "333.5",
        "panel.leftHidden": "0",
        "panel.rightHidden": "1",
        "panel.thumbSize": "200",
        "panel.inspectorTab": "tags",
        "panel.section.albums": "0",
        "inspector.section.iptc": "1",
        "inspector.section.stack": "0",
    });
    std::fs::write(&path, stored.to_string()).unwrap();
    let app = start_with_prefs(&path, cx);
    app.wired.shell.read_with(cx, |s, _| {
        assert_eq!((s.layout.left_w, s.layout.right_w), (250., 333.5));
        assert!(!s.layout.left_hidden && s.layout.right_hidden);
        assert_eq!(s.layout.thumb_size, 200.);
        assert_eq!(s.inspector_tab, InspectorTab::Tags);
        assert!(!s.section_open(Section::Albums), "panel.section.albums = 0");
        assert!(s.section_open(Section::Tags), "an unset section is open");
    });
    let root = app.wired.root.clone().unwrap();
    let inspector = root.read_with(cx, |r, _| r.inspector.clone());
    inspector.read_with(cx, |i, _| {
        use crate::inspector::Section as S;
        assert!(i.section_open(S::Iptc), "inspector.section.iptc = 1");
        assert!(!i.section_open(S::Stack) && !i.section_open(S::Metadata), "the rest collapsed");
    });
    // The thumbnail slider starts where the size is.
    let slider = root.read_with(cx, |r, _| r.thumb_slider.clone());
    assert_eq!(slider.read_with(cx, |s, _| s.value().start()), 200.);
    assert_eq!(pending(cx), 0, "opening writes nothing");
}

/// Each change writes its key with React's value, off the UI thread; the narrow overlays are
/// never stored; a column drag writes once, when it ends.
#[gpui_kit::test]
fn layout_changes_are_written_with_reacts_keys(cx: &mut TestAppContext) {
    let dir = TempDir::new("layout-write");
    let path = dir.0.join(FILE_NAME);
    let app = start_with_prefs(&path, cx);

    press(&app, "[", cx);
    assert!(!visible(&app, Side::Left, cx));
    assert!(!path.exists(), "nothing written on the UI thread");
    work(cx);
    assert_eq!(on_disk(&path).get("panel.leftHidden").map(String::as_str), Some("1"));

    app.wired.shell.update(cx, |s, cx| {
        s.toggle_section(Section::SmartAlbums, cx);
        s.set_inspector_tab(InspectorTab::Versions, cx);
        s.set_thumb_size(248., cx);
    });
    let root = app.wired.root.clone().unwrap();
    let inspector = root.read_with(cx, |r, _| r.inspector.clone());
    inspector.update(cx, |i, cx| i.toggle_section(crate::inspector::Section::Metadata, cx));
    work(cx);
    let disk = on_disk(&path);
    assert_eq!(disk.get("panel.section.smartAlbums").map(String::as_str), Some("0"));
    assert_eq!(disk.get("panel.inspectorTab").map(String::as_str), Some("versions"));
    assert_eq!(disk.get("panel.thumbSize").map(String::as_str), Some("248"));
    assert_eq!(disk.get("inspector.section.metadata").map(String::as_str), Some("1"));
    assert_eq!(disk.get("panel.rightHidden").map(String::as_str), Some("0"));

    // Narrow: `[` opens an overlay, which is never stored.
    app.wired.shell.update(cx, |s, cx| {
        s.set_narrow(true);
        s.toggle_panel(Side::Right, cx);
    });
    assert!(visible(&app, Side::Right, cx), "the overlay opened");
    work(cx);
    assert_eq!(on_disk(&path).get("panel.rightHidden").map(String::as_str), Some("0"), "the desktop preference");
    app.wired.shell.update(cx, |s, _| s.set_narrow(false));

    // Show the left column again, then drag its edge 40 px wider.
    press(&app, "[", cx);
    work(cx);
    let w0 = app.wired.shell.read_with(cx, |s, _| s.layout.left_w);
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        // The drag handle straddles the column's inner edge.
        let column = window.find("left-column").bounds();
        let from = point(column.origin.x + column.size.width - px(2.), column.center().y);
        window.drag(from, point(from.x + px(40.), from.y), cx);
    })
    .unwrap();
    cx.run_until_parked();
    let w1 = app.wired.shell.read_with(cx, |s, _| s.layout.left_w);
    assert_eq!(w1, w0 + 40., "the drag widened the column");
    assert_eq!(pending(cx), 1, "one write for the whole drag, not one per pointer move");
    work(cx);
    assert_eq!(on_disk(&path).get("panel.leftW"), Some(&w1.to_string()));

    // A fresh launch on the same file opens with all of it.
    let again = MachinePrefs::load(path.clone());
    let restored = crate::shell::layout_prefs::restore(|k| again.get(k));
    assert_eq!(restored.layout.left_w, w1);
    assert_eq!(restored.inspector_tab, InspectorTab::Versions);
    assert!(!restored.sections_open[1]);
}

// --- panel keys -------------------------------------------------------------------------

/// React's window handler was off in module views: `[`/`]` do nothing while a module's main
/// view is on the stage, and work again back in the Library.
#[gpui_kit::test]
fn panel_keys_are_off_in_a_module_view(cx: &mut TestAppContext) {
    let dir = TempDir::new("keys-module");
    let app = start(cx);
    open_catalog_with_photos(&app, &dir, 1, cx);
    cx.update(|cx| crate::modules::ModuleRegistry::enable(&app.wired.modules, crate::modules::dev_module::DEV_MODULE_ID, cx));
    cx.run_until_parked();
    click(&app, "rail-view-dev-view", cx);
    assert_eq!(app.wired.shell.read_with(cx, |s, _| s.surface.clone()), Surface::Module("dev-view".into()));
    press(&app, "[", cx);
    press(&app, "]", cx);
    assert!(visible(&app, Side::Left, cx) && visible(&app, Side::Right, cx), "[ or ] toggled a column in a module view");
    // Gated by the surface, not by focus: with the root itself focused the keys stay off.
    focus_root(&app, cx);
    press(&app, "[", cx);
    assert!(visible(&app, Side::Left, cx), "[ toggled a column in a module view with the root focused");

    // The View menu still toggles: it dispatches the action, not the key.
    cx.update_window(app.window(), |_, window, cx| window.dispatch_action(Box::new(ToggleLeftPanel), cx)).unwrap();
    cx.run_until_parked();
    assert!(!visible(&app, Side::Left, cx), "the menu's toggle works in a module view");

    click(&app, "rail-library", cx);
    press(&app, "[", cx);
    assert!(visible(&app, Side::Left, cx), "back in the Library, [ toggles again");
}

/// The cull session too.
#[gpui_kit::test]
fn panel_keys_are_off_in_a_cull_session(cx: &mut TestAppContext) {
    let dir = TempDir::new("keys-cull");
    let app = start(cx);
    open_catalog_with_photos(&app, &dir, 2, cx);
    cx.update_window(app.window(), |_, window, cx| window.dispatch_action(Box::new(StartCullSession), cx)).unwrap();
    cx.run_until_parked();
    assert!(app.wired.root.as_ref().unwrap().read_with(cx, |r, _| r.cull().is_some()), "the session opened");
    press(&app, "[", cx);
    assert!(visible(&app, Side::Left, cx), "[ toggled a column under the cull session");
    // Gated by the session, not by focus: with the root itself focused the keys stay off.
    focus_root(&app, cx);
    press(&app, "[", cx);
    assert!(visible(&app, Side::Left, cx), "[ toggled a column under the cull session with the root focused");
}

/// And the Darkroom: its key context mutes them, also after a title-bar menu has taken focus
/// back to the root (React checked the surface, not the focus).
#[cfg(feature = "edit")]
#[gpui_kit::test]
fn panel_keys_are_off_in_the_darkroom(cx: &mut TestAppContext) {
    let dir = TempDir::new("keys-darkroom");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 1, cx);
    app.wired.shell.update(cx, |s, cx| {
        s.select_with(cx, |l| l.select(ids[0], chairphoto_model::library::session::SelectMods::default()))
    });
    click(&app, "rail-develop", cx);
    assert_eq!(app.wired.shell.read_with(cx, |s, _| s.surface.clone()), Surface::Develop);
    press(&app, "[", cx);
    assert!(visible(&app, Side::Left, cx), "[ toggled the left column in the Darkroom");

    // Focus lands on the root itself (a menu's action context, a dialog closing) with no
    // shell change to wake the Darkroom's own focus: the root hands it back to the Darkroom.
    cx.update_window(app.window(), |_, window, _| window.activate_window()).unwrap();
    cx.run_until_parked();
    let root = app.wired.root.clone().unwrap();
    cx.update_window(app.window(), |_, window, cx| {
        let focus = root.read(cx).focus_handle().clone();
        focus.focus(window, cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(app.window(), |_, window, cx| window.render_frame(cx)).unwrap();
    let darkroom = root.read_with(cx, |r, _| r.darkroom.clone());
    let focused = cx
        .update_window(app.window(), |_, window, cx| darkroom.read(cx).focus_handle().contains_focused(window, cx))
        .unwrap();
    assert!(focused, "the Darkroom has the keys again after the root took focus");
    press(&app, "[", cx);
    assert!(visible(&app, Side::Left, cx), "[ toggled the left column after the root took focus");
    // Focus on a handle outside the Darkroom that is not an input (the grid's, not drawn
    // now): the keys stay off, since the surface gates them.
    let grid = root.read_with(cx, |r, cx| r.library.read(cx).focus_handle().clone());
    cx.update_window(app.window(), |_, window, cx| grid.focus(window, cx)).unwrap();
    press(&app, "]", cx);
    assert!(app.wired.shell.read_with(cx, |s, _| !s.layout.right_hidden), "] toggled the inspector from the Darkroom");
    // The View menu's toggle still works there (a menu focuses the root, then dispatches).
    focus_root(&app, cx);
    cx.update_window(app.window(), |_, window, cx| window.dispatch_action(Box::new(ToggleLeftPanel), cx)).unwrap();
    cx.run_until_parked();
    assert!(!visible(&app, Side::Left, cx), "the menu's toggle works in the Darkroom");
}

/// Focus the root view itself, with the window active.
fn focus_root(app: &App, cx: &mut TestAppContext) {
    cx.update_window(app.window(), |_, window, _| window.activate_window()).unwrap();
    cx.run_until_parked();
    let root = app.wired.root.clone().unwrap();
    cx.update_window(app.window(), |_, window, cx| {
        let focus = root.read(cx).focus_handle().clone();
        focus.focus(window, cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
}

/// Focus the collection browser's tag search, and a check that it still has focus.
fn focus_sidebar_search(app: &App, cx: &mut TestAppContext) -> impl Fn(&mut TestAppContext) -> bool {
    let root = app.wired.root.clone().unwrap();
    let search = root.read_with(cx, |r, cx| r.tag_panel.read(cx).search.clone());
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        search.update(cx, |i, cx| i.focus(window, cx));
    })
    .unwrap();
    cx.run_until_parked();
    let window = app.window();
    move |cx: &mut TestAppContext| {
        let s = search.clone();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            gpui_kit::Focusable::focus_handle(s.read(cx), cx).is_focused(window)
        })
        .unwrap()
    }
}

fn scan_progress(app: &App, cx: &mut TestAppContext) {
    use chairphoto_core::app::{CoreEvent, EventSink as _};
    app.state.send(CoreEvent::ScanProgress(chairphoto_core::scanner::ScanProgress {
        phase: "indexing".into(),
        done: 1,
        total: 10,
    }));
    cx.run_until_parked();
}

/// Review probe (claude-159-160, M1): a module view on the stage, the user typing in the left
/// column's tag search; a shell notify (a real `scan:progress`) must not take the focus.
#[gpui_kit::test]
fn probe_module_view_keeps_sidebar_input_focus(cx: &mut TestAppContext) {
    let dir = TempDir::new("probe-focus");
    let app = start(cx);
    open_catalog_with_photos(&app, &dir, 1, cx);
    cx.update(|cx| crate::modules::ModuleRegistry::enable(&app.wired.modules, crate::modules::dev_module::DEV_MODULE_ID, cx));
    cx.run_until_parked();
    click(&app, "rail-view-dev-view", cx);
    assert_eq!(app.wired.shell.read_with(cx, |s, _| s.surface.clone()), Surface::Module("dev-view".into()));
    let focused = focus_sidebar_search(&app, cx);
    assert!(focused(cx), "precondition: the sidebar search has focus");
    scan_progress(&app, cx);
    assert!(focused(cx), "a scan:progress event (shell notify) took focus from the sidebar's search input in a module view");
}

/// Review probe (P-pre): the same in the Darkroom, whose left column still shows.
#[cfg(feature = "edit")]
#[gpui_kit::test]
fn probe_darkroom_keeps_sidebar_input_focus(cx: &mut TestAppContext) {
    let dir = TempDir::new("probe-focus-dk");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 1, cx);
    app.wired.shell.update(cx, |s, cx| {
        s.select_with(cx, |l| l.select(ids[0], chairphoto_model::library::session::SelectMods::default()))
    });
    click(&app, "rail-develop", cx);
    assert_eq!(app.wired.shell.read_with(cx, |s, _| s.surface.clone()), Surface::Develop);
    let darkroom = app.wired.root.as_ref().unwrap().read_with(cx, |r, _| r.darkroom.clone());
    let dk_focused = cx
        .update_window(app.window(), |_, window, cx| darkroom.read(cx).focus_handle().contains_focused(window, cx))
        .unwrap();
    assert!(dk_focused, "entering Develop gives the Darkroom the keys");
    let focused = focus_sidebar_search(&app, cx);
    assert!(focused(cx), "precondition: the sidebar search has focus");
    scan_progress(&app, cx);
    assert!(focused(cx), "a scan:progress event took focus from the sidebar's search input in the Darkroom");
}

// --- shell timing -----------------------------------------------------------------------

fn written(cx: &mut TestAppContext) -> Vec<String> {
    cx.update(|cx| cx.global::<ShellTimer>().written.clone())
}

fn stored(app: &App) -> Option<String> {
    app.state.catalog.lock().unwrap().as_ref().unwrap().get_setting(SHELL_TIMING_KEY).unwrap()
}

/// With `editor.renderTiming` on, leaving the Darkroom records Develop → Library: the started
/// marker at once, then — once no tile has loaded for the quiet time — the summary with the
/// grid's first render and its tiles, both written to the catalog off the UI thread.
#[gpui_kit::test]
fn a_develop_to_library_transition_is_timed_and_stored(cx: &mut TestAppContext) {
    let dir = TempDir::new("shell-timing");
    let app = start(cx);
    open_catalog_with_photos(&app, &dir, 3, cx);
    // Off: nothing.
    let from = app.wired.shell.read_with(cx, |s, _| s.rows_from());
    cx.update(|cx| ShellTimer::leave("develop", from, cx));
    assert!(written(cx).is_empty(), "the instrument is off by default");

    cx.update(|cx| {
        ShellTimer::set_enabled(true, cx);
        ShellTimer::leave("develop", from, cx);
    });
    assert_eq!(written(cx), [r#"{"from":"develop","started":true}"#]);
    work(cx);
    assert_eq!(stored(&app).as_deref(), Some(r#"{"from":"develop","started":true}"#), "the marker is in the catalog");

    // The grid renders (its "commit"), building its three tiles; the rows are re-read.
    app.wired.shell.update(cx, |s, cx| s.refresh_rows(cx));
    cx.update_window(app.window(), |_, window, cx| window.render_frame(cx)).unwrap();
    cx.run_until_parked();
    let t = cx.update(|cx| ShellTimer::current(cx)).unwrap();
    assert!(t.commit_at.is_some(), "the grid's first render after Back");
    assert_eq!(t.tiles_mounted, 3);
    assert!(t.marks.contains("timeout0"));
    assert!(!t.finished);

    cx.executor().advance_clock(Duration::from_millis(9_000));
    cx.run_until_parked();
    assert_eq!(written(cx).len(), 1, "still inside the quiet time");
    cx.executor().advance_clock(Duration::from_millis(1_100));
    cx.run_until_parked();
    let w = written(cx);
    assert_eq!(w.len(), 2, "the summary: {w:?}");
    let summary: serde_json::Value = serde_json::from_str(&w[1]).unwrap();
    assert_eq!(summary["from"], "develop");
    assert_eq!(summary["tilesMounted"], 3);
    assert!(summary["toCommitMs"].is_number(), "{summary}");
    assert!(!cx.update(|cx| ShellTimer::live(cx)));
    work(cx);
    assert_eq!(stored(&app).as_deref(), Some(w[1].as_str()), "the summary is in the catalog");
}

/// **Forced interleaving** (review claude-159-160, L3). The "started" marker's write is held
/// up (here: run after the summary's); it must not overwrite the summary.
#[gpui_kit::test]
fn a_late_started_marker_never_overwrites_the_summary(cx: &mut TestAppContext) {
    let dir = TempDir::new("shell-timing-order");
    let app = start(cx);
    open_catalog_with_photos(&app, &dir, 1, cx);
    work(cx); // the launch checks
    let from = app.wired.shell.read_with(cx, |s, _| s.rows_from());
    cx.update(|cx| {
        ShellTimer::set_enabled(true, cx);
        ShellTimer::leave("develop", from, cx);
    });
    let marker = cx.update(|cx| Runner::get(cx).hold_pending());
    assert_eq!(marker.len(), 1, "the marker's write, held");
    cx.executor().advance_clock(Duration::from_millis(10_100));
    cx.run_until_parked();
    let w = written(cx);
    assert_eq!(w.len(), 2, "the summary was handed out: {w:?}");
    cx.update(|cx| Runner::get(cx).run_pending()); // the summary's write
    cx.update(|cx| Runner::get(cx).release(marker));
    work(cx); // then the late marker's
    assert_eq!(stored(&app).as_deref(), Some(w[1].as_str()), "the late marker overwrote the summary");
}

/// The timing record is bound to the catalog the Library rows came from (review
/// claude-159-160, L4): the core swaps in a catalog with colliding ids and keys between Back
/// and the writes — with `catalog:switched` delivered and without — and neither the started
/// marker nor the summary lands in it.
#[gpui_kit::test]
fn the_timing_record_never_lands_in_another_catalog(cx: &mut TestAppContext) {
    use crate::tests::{colliding_catalog, core_switch, deliver_switch};
    for deliver in [false, true] {
        let dir = TempDir::new(&format!("shell-timing-swap-{deliver}"));
        let app = start(cx);
        open_catalog_with_photos(&app, &dir, 2, cx);
        work(cx);
        let from = app.wired.shell.read_with(cx, |s, _| s.rows_from());
        cx.update(|cx| {
            ShellTimer::set_enabled(true, cx);
            ShellTimer::leave("develop", from, cx);
        });
        let (b, _) = colliding_catalog(&dir, "b", 2);
        b.set_setting(SHELL_TIMING_KEY, "b's own").unwrap();
        core_switch(&app, b);
        if deliver {
            deliver_switch(&app, cx);
        }
        work(cx); // the started marker's write
        cx.executor().advance_clock(Duration::from_millis(10_100));
        cx.run_until_parked();
        assert_eq!(written(cx).len(), 2, "deliver={deliver}: the summary was handed out");
        work(cx); // the summary's write
        assert_eq!(stored(&app).as_deref(), Some("b's own"), "deliver={deliver}: a timing write landed in B");
    }
}

/// N3 (batch 7 review): `leave` with no `rows_from` (rows had not landed yet) has no catalog
/// to bind the write to. It must not fall back to writing into whichever catalog happens to
/// be open when the write finally runs — the record is dropped instead.
/// (Mutation-checked: restoring the old `None => with_catalog(&state, write)` fallback makes
/// the started marker land in the open catalog and this fails.)
#[gpui_kit::test]
fn a_leave_with_no_catalog_drops_the_record_rather_than_guessing(cx: &mut TestAppContext) {
    let dir = TempDir::new("shell-timing-unbound");
    let app = start(cx);
    open_catalog_with_photos(&app, &dir, 2, cx);
    work(cx);
    cx.update(|cx| {
        ShellTimer::set_enabled(true, cx);
        ShellTimer::leave("develop", None, cx); // no rows_from yet
    });
    work(cx);
    assert_eq!(stored(&app), None, "the started marker must not land in the open catalog");

    cx.executor().advance_clock(Duration::from_millis(10_100));
    cx.run_until_parked();
    work(cx);
    assert_eq!(stored(&app), None, "the summary must not land in the open catalog either");
}

/// The Darkroom's switch turns the instrument on, and its ← Library starts the transition.
#[cfg(feature = "edit")]
#[gpui_kit::test]
fn the_darkrooms_back_starts_the_transition(cx: &mut TestAppContext) {
    let dir = TempDir::new("shell-timing-back");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 1, cx);
    app.state.catalog.lock().unwrap().as_ref().unwrap().set_setting("editor.renderTiming", "1").unwrap();
    app.wired.shell.update(cx, |s, cx| {
        s.select_with(cx, |l| l.select(ids[0], chairphoto_model::library::session::SelectMods::default()))
    });
    click(&app, "rail-develop", cx);
    work(cx); // the Darkroom's settings read
    click(&app, "dk-back", cx);
    assert_eq!(app.wired.shell.read_with(cx, |s, _| s.surface.clone()), Surface::Library);
    assert_eq!(written(cx), [r#"{"from":"develop","started":true}"#]);
    assert!(cx.update(|cx| ShellTimer::live(cx)));
}

// --- startup splash (#160) --------------------------------------------------------------

fn splash(app: &App, cx: &mut TestAppContext) -> crate::shell::splash::Splash {
    app.wired.model.read_with(cx, |m, _| m.splash.clone())
}

fn splash_line(app: &App, cx: &mut TestAppContext) -> Option<String> {
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.try_find("splash-stage").and_then(|e| e.label().map(str::to_string))
    })
    .unwrap()
}

/// A launch without the default catalog (the headless tests) shows no splash.
#[gpui_kit::test]
fn no_boot_no_splash(cx: &mut TestAppContext) {
    let app = start(cx);
    assert!(!splash(&app, cx).showing());
    assert_eq!(splash_line(&app, cx), None);
}

/// The boot after the catalog opened, stage by stage (App.tsx's `initCatalog()` chain):
/// auto-tags on a worker — the monochrome tag reaches a B&W photo imported before the rule —
/// then the catalog read (the modules start on it), then the first rows; "Ready", and gone
/// after the fade.
#[gpui_kit::test]
fn the_splash_follows_the_boot_stages_then_fades(cx: &mut TestAppContext) {
    use crate::shell::splash::{BootStage, Splash, FADE};
    let dir = TempDir::new("splash");
    let app = start(cx);
    let root = dir.0.join("photos");
    let db = dir.0.join("boot.chairphoto");
    let catalog = chairphoto_core::catalog::Catalog::open(&db, &root).unwrap();
    let id = catalog.upsert_photo(&root.join("2026/bw.ARW"), None, 0, 1).unwrap().id;
    catalog.set_grayscale(id, true).unwrap();
    *app.state.catalog.lock().unwrap() = Some(catalog);
    let tags = |app: &App| -> Vec<String> {
        let guard = app.state.catalog.lock().unwrap();
        guard.as_ref().unwrap().get_photo_tags(id).unwrap().into_iter().map(|t| t.full_path).collect()
    };
    assert!(tags(&app).is_empty(), "the photo predates the rule");

    app.wired.model.update(cx, |m, cx| {
        m.splash = Splash::booting();
        cx.notify();
    });
    assert_eq!(splash_line(&app, cx).as_deref(), Some("Opening catalog…"));
    app.wired.model.update(cx, |m, cx| m.boot_after_open(Ok(db.clone()), cx));
    assert_eq!(splash(&app, cx).stage(), Some(BootStage::UpdatingAutoTags));
    assert_eq!(splash_line(&app, cx).as_deref(), Some("Updating auto-tags…"));
    cx.run_until_parked(); // everything but the worker
    assert_eq!(splash(&app, cx).stage(), Some(BootStage::UpdatingAutoTags), "still waiting for the auto-tags");
    assert!(app.wired.model.read_with(cx, |m, _| m.catalog.is_none()), "nothing read before the auto-tags ran");

    assert_eq!(cx.update(|cx| Runner::get(cx).run_pending()), 1, "apply_auto_tags, on a worker");
    assert_eq!(tags(&app), ["Treatment/Black & White"]);
    cx.run_until_parked();
    work(cx);
    let s = splash(&app, cx);
    assert!(s.hiding(), "the catalog, the modules and the first rows are in: {s:?}");
    assert_eq!(splash_line(&app, cx).as_deref(), Some("Ready"));
    assert!(app.wired.shell.read_with(cx, |s, _| s.rows_loaded));

    cx.executor().advance_clock(FADE);
    cx.run_until_parked();
    assert!(!splash(&app, cx).showing());
    assert_eq!(splash_line(&app, cx), None, "gone after the fade");
}

/// A row read failing before the catalog is even open (a filter key reaching the Library
/// through the splash's pointer-only occlusion) must not mark "Loading photos…" done — else
/// the splash could fade as soon as the init chain ends, before the real first rows do
/// (#194). Checked directly against `Splash::finish_init` (rather than driving the whole
/// catalog-open → auto-tags → modules cascade to the same point): in this harness that
/// cascade's own successful row read lands within the same `run_until_parked` pass as the
/// init chain's completion, so there is no separately observable moment between them to
/// assert on; `finish_init` alone reproduces exactly the "the init chain just ended" instant.
/// (Mutation-checked: dropping the `catalog_current` guard in `boot_photos_loaded` makes the
/// stray failure mark `photos_done`, so `finish_init` alone then ends the boot — `hiding()`
/// turns true — and this fails.)
#[gpui_kit::test]
fn a_premature_row_read_failure_does_not_end_the_splash_early(cx: &mut TestAppContext) {
    use crate::shell::splash::Splash;
    let app = start(cx);
    app.wired.model.update(cx, |m, cx| {
        m.splash = Splash::booting();
        cx.notify();
    });

    // A filter key, say: it reaches the Library (the splash occludes only the pointer) before
    // any catalog is open, so the row read it asks for fails with "No catalog is open".
    app.wired.shell.update(cx, |s, cx| s.update_scope(cx, |l| l.toggle_label("Red")));
    cx.run_until_parked();
    assert!(!app.wired.shell.read_with(cx, |s, _| s.rows_loaded), "no real rows have landed");
    assert!(splash(&app, cx).showing() && !splash(&app, cx).hiding(), "the stray failure must not end the boot");

    // The rest of the init chain finishing, without needing to drive the whole cascade: if
    // the stray failure had wrongly marked the photos part done, this alone would now end it.
    app.wired.model.update(cx, |m, cx| {
        m.splash.finish_init();
        cx.notify();
    });
    let s = splash(&app, cx);
    assert!(!s.hiding() && s.showing(), "the init chain ending alone must not fade the splash: {s:?}");
}

/// A failed open ends the boot at once: the splash fades rather than hanging over the error.
#[gpui_kit::test]
fn a_failed_boot_never_leaves_the_splash_up(cx: &mut TestAppContext) {
    use crate::shell::splash::{Splash, FADE};
    let app = start(cx);
    app.wired.model.update(cx, |m, cx| {
        m.splash = Splash::booting();
        m.boot_after_open(Err("disk full".into()), cx);
    });
    assert!(splash(&app, cx).hiding());
    assert_eq!(crate::tests::status(&app, cx), "Failed to open catalog: disk full");
    cx.executor().advance_clock(FADE);
    cx.run_until_parked();
    assert!(!splash(&app, cx).showing());
}

/// #218: the splash logo's white sliver sits centred on the red/blue seam, 46% of the disc's
/// width from the left and 8% wide, running its full height — the part of the shape a
/// headless test can check. The disc itself (`rounded_full()`, a red/blue gradient fill) is
/// not observable here: `ElementSnapshot` exposes bounds, not a background/gradient accessor.
#[gpui_kit::test]
fn the_splash_logos_white_sliver_sits_on_the_red_blue_seam(cx: &mut TestAppContext) {
    use crate::shell::splash::Splash;
    use gpui_kit::px;
    let app = start(cx);
    app.wired.model.update(cx, |m, cx| {
        m.splash = Splash::booting();
        cx.notify();
    });
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        let logo = window.find("splash-logo").bounds();
        let seam = window.find("splash-logo-seam").bounds();
        assert_eq!(logo.size.width, px(64.), "the disc is 64 px square");
        assert_eq!(logo.size.height, px(64.));
        assert_eq!(seam.size.height, logo.size.height, "the sliver runs the disc's full height");
        let expected_left = logo.origin.x + logo.size.width * 0.46;
        let expected_width = logo.size.width * 0.08;
        assert!(
            (seam.origin.x - expected_left).abs() < px(0.5),
            "the sliver starts at 46% of the disc: {:?} vs {expected_left:?}",
            seam.origin.x,
        );
        assert!(
            (seam.size.width - expected_width).abs() < px(0.5),
            "the sliver is 8% of the disc wide: {:?} vs {expected_width:?}",
            seam.size.width,
        );
    })
    .unwrap();
}
