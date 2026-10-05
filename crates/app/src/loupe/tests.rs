//! Headless tests of the loupe, Compare, the cull session and the Darkroom overlays through
//! the real wiring (`start` → `wire` → the main window), with the decode pool a
//! [`FakePool`] the tests answer by hand, so every answer order is forced.

use crate::image_tests::{pixels, FakePool};
use crate::loupe::compare::MODE_PREF;
use crate::loupe::cull::{CullView, CURSOR_DEBOUNCE, CURSOR_KEY};
use crate::loupe::view::SystemOpener;
use crate::loupe::zoom::{hundred_percent, Drawn, ZoomImage, ZoomView};
use crate::machine_prefs::MachinePrefs;
use crate::shell::actions::StartCullSession;
use crate::shell::state::StageView;
use crate::tests::{
    colliding_catalog, core_switch, deliver_switch, open_catalog_with_photos, press, start_with_pool, status, App,
    TempDir,
};
use crate::view::RootView;
use chairphoto_core::app::EventSink as _;
use chairphoto_core::catalog::{CullingFilter, PickState};
use chairphoto_core::image_pool::{ImageKind, JobKey};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    point, AppContext as _, Entity, InputEvent as _, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ScrollDelta, ScrollWheelEvent, TestAppContext, TouchPhase,
};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

fn app_with(n: usize, tag: &str, cx: &mut TestAppContext) -> (App, Arc<FakePool>, TempDir, Vec<i64>) {
    let dir = TempDir::new(tag);
    let pool = Arc::new(FakePool::default());
    let app = start_with_pool(cx, pool.clone());
    let ids = open_catalog_with_photos(&app, &dir, n, cx);
    (app, pool, dir, ids)
}

fn root(app: &App) -> Entity<RootView> {
    app.wired.root.clone().expect("the main window opened")
}

fn select(app: &App, id: i64, cx: &mut TestAppContext) {
    app.wired.shell.update(cx, |s, cx| s.select_with(cx, |l| l.select_single(id)));
    cx.run_until_parked();
}

fn select_all(app: &App, cx: &mut TestAppContext) {
    app.wired.shell.update(cx, |s, cx| s.select_with(cx, |l| l.select_all()));
    cx.run_until_parked();
}

fn stage(app: &App, cx: &mut TestAppContext) -> StageView {
    app.wired.shell.read_with(cx, |s, _| s.stage_view())
}

fn active(app: &App, cx: &mut TestAppContext) -> Option<i64> {
    app.wired.shell.read_with(cx, |s, _| s.library.selection().active_id)
}

fn render(app: &App, cx: &mut TestAppContext) {
    cx.update_window(app.window(), |_, window, cx| window.render_frame(cx)).unwrap();
    cx.run_until_parked();
}

fn preview(id: i64) -> JobKey {
    JobKey::photo(id, ImageKind::Preview)
}

fn zoom_key(id: i64) -> JobKey {
    JobKey::photo(id, ImageKind::Zoom)
}

fn thumb(id: i64) -> JobKey {
    JobKey::photo(id, ImageKind::Thumb)
}

/// The photo ids of the last batch that asked for previews.
fn last_previews(pool: &FakePool) -> Vec<i64> {
    let batches = pool.batches.lock().unwrap();
    batches
        .iter()
        .rev()
        .map(|b| {
            b.iter()
                .filter_map(|k| match k {
                    JobKey::Photo { id, kind: ImageKind::Preview } => Some(*id),
                    _ => None,
                })
                .collect::<Vec<_>>()
        })
        .find(|b| !b.is_empty())
        .unwrap_or_default()
}

fn submitted(pool: &FakePool, key: &JobKey) -> bool {
    pool.batches.lock().unwrap().iter().any(|b| b.contains(key))
}

fn loupe_zoom(app: &App, cx: &mut TestAppContext) -> Entity<ZoomImage> {
    root(app).read_with(cx, |r, cx| r.loupe().read(cx).zoom().clone())
}

fn drawn(app: &App, cx: &mut TestAppContext) -> Option<(i64, Drawn)> {
    render(app, cx);
    loupe_zoom(app, cx).read_with(cx, |z, _| z.drawn())
}

fn culling(app: &App, id: i64) -> (i64, PickState, String) {
    let guard = app.state.catalog.lock().unwrap();
    let p = guard.as_ref().unwrap().get_photo(id).unwrap();
    (p.rating, p.pick_state, p.label)
}

fn label_of(app: &App, id: &'static str, cx: &mut TestAppContext) -> Option<String> {
    let mut out = None;
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        out = window.try_find(id).and_then(|e| e.label().map(str::to_string));
    })
    .unwrap();
    out
}

fn wheel(app: &App, id: &'static str, up: bool, cx: &mut TestAppContext) {
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        let position = window.find(id).bounds().center();
        let event = ScrollWheelEvent {
            position,
            delta: ScrollDelta::Lines(point(0., if up { 1. } else { -1. })),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
        };
        window.dispatch_event(event.to_platform_input(), cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
}

// --- the loupe ---------------------------------------------------------------------------------

/// AGENTS.md § Performance: Enter opens the loupe and asks for the photo first, then N+1 and
/// N−1, then the rest of the window — one batch. Stepping promotes the next photo (already a
/// preload) to the top and cancels what fell out of the window.
#[gpui_kit::test]
fn the_loupe_loads_the_photo_first_then_its_neighbours(cx: &mut TestAppContext) {
    let (app, pool, _dir, ids) = app_with(10, "loupe-order", cx);
    select(&app, ids[4], cx);
    press(&app, "enter", cx);
    assert_eq!(stage(&app, cx), StageView::Loupe);
    let want: Vec<i64> = [4, 5, 3, 6, 7, 8, 9, 2].iter().map(|&i| ids[i]).collect();
    assert_eq!(last_previews(&pool), want, "current, N+1, N−1, N+2…N+5, N−2");

    press(&app, "right", cx);
    assert_eq!(active(&app, cx), Some(ids[5]));
    let want: Vec<i64> = [5, 6, 4, 7, 8, 9, 3].iter().map(|&i| ids[i]).collect();
    assert_eq!(last_previews(&pool), want, "the new current first, promoted");
    assert!(pool.cancelled.lock().unwrap().contains(&preview(ids[2])), "N−3 left the window");
}

/// The loupe never draws another photo's frame: while the next photo's preview loads it
/// draws that photo's own thumbnail, or nothing.
#[gpui_kit::test]
fn the_loupe_never_draws_the_previous_photo(cx: &mut TestAppContext) {
    let (app, pool, _dir, ids) = app_with(4, "loupe-stale", cx);
    select(&app, ids[0], cx);
    press(&app, "enter", cx);
    pool.finish(&preview(ids[0]), Ok(pixels(30, 20)));
    cx.run_until_parked();
    assert_eq!(drawn(&app, cx), Some((ids[0], Drawn::Preview)));

    press(&app, "right", cx);
    assert_eq!(drawn(&app, cx), None, "photo 1 has nothing yet; photo 0's preview is not shown");
    pool.finish(&thumb(ids[1]), Ok(pixels(6, 4)));
    cx.run_until_parked();
    assert_eq!(drawn(&app, cx), Some((ids[1], Drawn::Thumb)));
    pool.finish(&preview(ids[1]), Ok(pixels(30, 20)));
    cx.run_until_parked();
    assert_eq!(drawn(&app, cx), Some((ids[1], Drawn::Preview)));
}

/// The first zoom-in asks for the full-resolution tier and swaps to it; stepping away
/// releases it and starts the next photo at fit.
#[gpui_kit::test]
fn zooming_in_swaps_to_the_zoom_tier_and_stepping_releases_it(cx: &mut TestAppContext) {
    let (app, pool, _dir, ids) = app_with(3, "loupe-zoom", cx);
    select(&app, ids[0], cx);
    press(&app, "enter", cx);
    pool.finish(&preview(ids[0]), Ok(pixels(300, 200)));
    cx.run_until_parked();
    render(&app, cx);
    assert!(!submitted(&pool, &zoom_key(ids[0])), "no zoom tier at fit");

    wheel(&app, "loupe-image", true, cx);
    let zoom = loupe_zoom(&app, cx);
    assert!(zoom.read_with(cx, |z, cx| z.view(cx).zoomed()));
    assert!(submitted(&pool, &zoom_key(ids[0])));
    assert_eq!(drawn(&app, cx), Some((ids[0], Drawn::Preview)), "the preview until the zoom tier is in");
    pool.finish(&zoom_key(ids[0]), Ok(pixels(3000, 2000)));
    cx.run_until_parked();
    assert_eq!(drawn(&app, cx), Some((ids[0], Drawn::Zoom)));

    // Fit N% returns to fit and the preview.
    cx.update_window(app.window(), |_, window, cx| window.click("loupe-image-fit", cx)).unwrap();
    cx.run_until_parked();
    assert_eq!(zoom.read_with(cx, |z, cx| z.view(cx)), ZoomView::FIT);
    assert_eq!(drawn(&app, cx), Some((ids[0], Drawn::Preview)));

    // Zoom again, then step while the next photo's zoom tier would be queued.
    wheel(&app, "loupe-image", true, cx);
    press(&app, "right", cx);
    assert_eq!(zoom.read_with(cx, |z, cx| z.view(cx)), ZoomView::FIT, "the next photo starts at fit");
    wheel(&app, "loupe-image", true, cx);
    assert!(submitted(&pool, &zoom_key(ids[1])));
    press(&app, "right", cx);
    assert!(pool.cancelled.lock().unwrap().contains(&zoom_key(ids[1])), "the old zoom tier is released");
}

fn cached(app: &App, id: i64, kind: ImageKind, cx: &mut TestAppContext) -> bool {
    app.wired.images.read_with(cx, |s, _| s.lru().peek(&s.key(id, kind)).is_some())
}

/// Stepping away releases the loaded full-resolution tier of the photo left, not just a
/// pending one; its preview stays cached, and the new photo's own zoom tier is kept.
#[gpui_kit::test]
fn stepping_away_evicts_the_loaded_zoom_tier_of_the_photo_left(cx: &mut TestAppContext) {
    let (app, pool, _dir, ids) = app_with(3, "loupe-evict", cx);
    select(&app, ids[0], cx);
    press(&app, "enter", cx);
    pool.finish(&preview(ids[0]), Ok(pixels(300, 200)));
    cx.run_until_parked();
    wheel(&app, "loupe-image", true, cx);
    pool.finish(&zoom_key(ids[0]), Ok(pixels(3000, 2000)));
    cx.run_until_parked();
    assert!(cached(&app, ids[0], ImageKind::Zoom, cx), "the zoom tier is in");

    press(&app, "right", cx);
    assert!(!cached(&app, ids[0], ImageKind::Zoom, cx), "released with the step");
    assert!(cached(&app, ids[0], ImageKind::Preview, cx), "the preview stays for a step back");

    pool.finish(&preview(ids[1]), Ok(pixels(300, 200)));
    cx.run_until_parked();
    wheel(&app, "loupe-image", true, cx);
    pool.finish(&zoom_key(ids[1]), Ok(pixels(3000, 2000)));
    cx.run_until_parked();
    // A re-sync that is no step (a render, a shell change) keeps the target's tier.
    app.wired.shell.update(cx, |_, cx| cx.notify());
    render(&app, cx);
    assert!(cached(&app, ids[1], ImageKind::Zoom, cx), "the target keeps its own");
    // Stepping onto a photo whose zoom tier is already in (another view loaded it) keeps it.
    app.wired.images.update(cx, |s, _| s.request(ids[2], ImageKind::Zoom));
    pool.finish(&zoom_key(ids[2]), Ok(pixels(3000, 2000)));
    cx.run_until_parked();
    press(&app, "right", cx);
    assert!(!cached(&app, ids[1], ImageKind::Zoom, cx));
    assert!(cached(&app, ids[2], ImageKind::Zoom, cx), "the target's tier is kept");
    press(&app, "escape", cx);
    assert!(!cached(&app, ids[2], ImageKind::Zoom, cx), "closing the loupe leaves the photo too");
}

/// A double-click at fit waits for the full-resolution image, then shows it at 100 %.
#[gpui_kit::test]
fn a_double_click_waits_for_the_zoom_tier_then_shows_100_percent(cx: &mut TestAppContext) {
    let (app, pool, _dir, ids) = app_with(2, "loupe-dbl", cx);
    select(&app, ids[0], cx);
    press(&app, "enter", cx);
    pool.finish(&preview(ids[0]), Ok(pixels(300, 200)));
    cx.run_until_parked();
    render(&app, cx);
    cx.update_window(app.window(), |_, window, cx| window.double_click("loupe-image", cx)).unwrap();
    cx.run_until_parked();
    let zoom = loupe_zoom(&app, cx);
    assert_eq!(zoom.read_with(cx, |z, cx| z.view(cx)), ZoomView::FIT, "nothing to measure yet");
    assert!(submitted(&pool, &zoom_key(ids[0])));
    pool.finish(&zoom_key(ids[0]), Ok(pixels(3000, 2000)));
    cx.run_until_parked();
    render(&app, cx);
    let (view, bounds) = zoom.read_with(cx, |z, cx| (z.view(cx), z.bounds().unwrap()));
    let container = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    assert!((view.scale - hundred_percent((3000., 2000.), container)).abs() < 1e-3, "{view:?} in {container:?}");
    assert!(view.scale > 1.);
    // A second double-click returns to fit.
    cx.update_window(app.window(), |_, window, cx| window.double_click("loupe-image", cx)).unwrap();
    cx.run_until_parked();
    assert_eq!(zoom.read_with(cx, |z, cx| z.view(cx)), ZoomView::FIT);
}

/// The loupe's keys: the culling keys mark and advance, Escape returns to the grid, whose keys
/// then work again.
#[gpui_kit::test]
fn loupe_keys_mark_advance_and_close(cx: &mut TestAppContext) {
    let (app, _pool, _dir, ids) = app_with(4, "loupe-keys", cx);
    select(&app, ids[1], cx);
    press(&app, "enter", cx);
    press(&app, "p", cx);
    assert_eq!(culling(&app, ids[1]).1, PickState::Pick);
    assert_eq!(active(&app, cx), Some(ids[2]), "advanced");
    assert_eq!(stage(&app, cx), StageView::Loupe);
    press(&app, "escape", cx);
    assert_eq!(stage(&app, cx), StageView::Grid);
    press(&app, "right", cx);
    assert_eq!(active(&app, cx), Some(ids[3]), "the grid has the keys again");
    press(&app, "enter", cx);
    assert_eq!(stage(&app, cx), StageView::Loupe);
    press(&app, "enter", cx);
    assert_eq!(stage(&app, cx), StageView::Grid, "Enter toggles");
}

/// Ctrl+A in the loupe selects every photo in the view, as in the grid, and the loupe stays
/// on the photo it showed.
#[gpui_kit::test]
fn ctrl_a_in_the_loupe_selects_the_whole_view(cx: &mut TestAppContext) {
    let (app, _pool, _dir, ids) = app_with(4, "loupe-all", cx);
    select(&app, ids[2], cx);
    press(&app, "enter", cx);
    press(&app, "ctrl-a", cx);
    let selected = app.wired.shell.read_with(cx, |s, _| s.library.selection().ids.to_vec());
    assert_eq!(selected, ids);
    assert_eq!((active(&app, cx), stage(&app, cx)), (Some(ids[2]), StageView::Loupe));
}

/// A video shows its poster and hands the file to the system player.
#[gpui_kit::test]
fn a_video_plays_in_the_system_player(cx: &mut TestAppContext) {
    let dir = TempDir::new("loupe-video");
    let pool = Arc::new(FakePool::default());
    let app = start_with_pool(cx, pool.clone());
    let opened: Rc<RefCell<Vec<PathBuf>>> = Rc::default();
    cx.update(|cx| {
        let opened = opened.clone();
        cx.set_global(SystemOpener(Rc::new(move |p, _| opened.borrow_mut().push(p.to_path_buf()))))
    });
    let root_dir = dir.0.join("photos");
    std::fs::create_dir_all(root_dir.join("2026")).unwrap();
    std::fs::write(root_dir.join("2026/clip.mp4"), b"not really a video").unwrap();
    let catalog = chairphoto_core::catalog::Catalog::open(&dir.0.join("v.chairphoto"), &root_dir).unwrap();
    let id = catalog.upsert_photo(&root_dir.join("2026/clip.mp4"), None, 0, 1).unwrap().id;
    *app.state.catalog.lock().unwrap() = Some(catalog);
    app.state.send(chairphoto_core::app::CoreEvent::CatalogSwitched("v".into()));
    cx.run_until_parked();
    select(&app, id, cx);
    press(&app, "enter", cx);
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.click("loupe-play", cx)
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(*opened.borrow(), vec![root_dir.join("2026/clip.mp4")]);
}

/// Catalog identity: a mark from the loupe after the core switched (event not yet delivered)
/// fails closed against the colliding ids; the event then closes the loupe.
#[gpui_kit::test]
fn a_catalog_switch_closes_the_loupe_and_its_marks_fail_closed(cx: &mut TestAppContext) {
    let (app, _pool, dir, ids) = app_with(3, "loupe-switch", cx);
    select(&app, ids[1], cx);
    press(&app, "enter", cx);
    let (other, other_ids) = colliding_catalog(&dir, "other", 3);
    assert_eq!(other_ids, ids, "the ids collide");
    core_switch(&app, other);
    press(&app, "3", cx);
    assert_eq!(culling(&app, ids[1]).0, 0, "the other catalog's photo is untouched");
    assert!(status(&app, cx).starts_with("Could not mark"), "{}", status(&app, cx));
    deliver_switch(&app, cx);
    assert_eq!(stage(&app, cx), StageView::Grid);
    assert!(!app.wired.shell.read_with(cx, |s, _| s.loupe_open));
}

/// A catalog holding one video, `clip.mp4`, under `root` (written to disk, so its path
/// resolves); its id.
fn video_catalog(path: &std::path::Path, root: &std::path::Path) -> (chairphoto_core::catalog::Catalog, i64) {
    std::fs::create_dir_all(root.join("2026")).unwrap();
    std::fs::write(root.join("2026/clip.mp4"), b"not really a video").unwrap();
    let catalog = chairphoto_core::catalog::Catalog::open(path, root).unwrap();
    let id = catalog.upsert_photo(&root.join("2026/clip.mp4"), None, 0, 1).unwrap().id;
    (catalog, id)
}

/// Press and release at each of `points` against the frame on screen, with no draw before or
/// between (`window.click` draws first, so it would click the new frame's button).
fn click_undrawn(app: &App, points: &[gpui_kit::Point<gpui_kit::Pixels>], cx: &mut TestAppContext) {
    cx.update_window(app.window(), |_, window, cx| {
        let modifiers = Modifiers::default();
        for &at in points {
            let down = MouseDownEvent { button: MouseButton::Left, position: at, modifiers, click_count: 1, first_mouse: false };
            window.dispatch_event(down.to_platform_input(), cx);
            let up = MouseUpEvent { button: MouseButton::Left, position: at, modifiers, click_count: 1 };
            window.dispatch_event(up.to_platform_input(), cx);
        }
    })
    .unwrap();
    cx.run_until_parked();
}

/// #207, catalog identity: the loupe bar's rotate and the video's Play act on the row and
/// the catalog the frame on screen was drawn with. Catalog B gives the video's id to its own
/// video. The core switches to B, and the clicks land on A's frame:
/// - `catalog:switched` withheld: the rows are still A's, and both fail closed against B.
/// - the switch taken in and B's rows landed (`rows_from` is B), but not yet drawn: a click
///   that read the catalog at click time would rotate and play B's video.
fn loupe_actions_across_a_switch(delivered: bool, cx: &mut TestAppContext) {
    let dir = TempDir::new(if delivered { "loupe-act-switch-ev" } else { "loupe-act-switch" });
    let pool = Arc::new(FakePool::default());
    let app = start_with_pool(cx, pool.clone());
    let opened: Rc<RefCell<Vec<PathBuf>>> = Rc::default();
    cx.update(|cx| {
        let opened = opened.clone();
        cx.set_global(SystemOpener(Rc::new(move |p, _| opened.borrow_mut().push(p.to_path_buf()))))
    });
    let (a, id) = video_catalog(&dir.0.join("a.chairphoto"), &dir.0.join("a"));
    *app.state.catalog.lock().unwrap() = Some(a);
    app.state.send(chairphoto_core::app::CoreEvent::CatalogSwitched("a".into()));
    cx.run_until_parked();
    select(&app, id, cx);
    press(&app, "enter", cx);
    let (rotate_at, play_at) = cx
        .update_window(app.window(), |_, window, cx| {
            window.render_frame(cx);
            (window.find("loupe-rotate-right").bounds().center(), window.find("loupe-play").bounds().center())
        })
        .unwrap();

    let (b, b_id) = video_catalog(&dir.0.join("b.chairphoto"), &dir.0.join("b"));
    assert_eq!(b_id, id, "the ids collide");
    core_switch(&app, b);
    if delivered {
        let b_from = chairphoto_core::app::catalog_identity(&app.state).unwrap();
        app.wired.shell.update(cx, |s, _| s.set_rows_from_undrawn(b_from));
    }
    // Both on the same frame: the first click's answer redraws the window.
    click_undrawn(&app, &[rotate_at, play_at], cx);
    let rotation = app.state.catalog.lock().unwrap().as_ref().unwrap().photo_rotation(id).unwrap();
    assert_eq!(rotation, 0, "delivered={delivered}: B's video was rotated");
    assert!(opened.borrow().is_empty(), "delivered={delivered}: B's video was played: {:?}", opened.borrow());
    let line = status(&app, cx);
    assert!(line.starts_with("Could not rotate") || line.starts_with("Could not play"), "{line}");
}

#[gpui_kit::test]
fn loupe_actions_never_reach_the_new_catalog_before_the_switch_event(cx: &mut TestAppContext) {
    loupe_actions_across_a_switch(false, cx);
}

#[gpui_kit::test]
fn loupe_actions_never_reach_the_new_catalog_after_the_switch_event(cx: &mut TestAppContext) {
    loupe_actions_across_a_switch(true, cx);
}

/// #172: the loupe bar's rotate chips are icon chips (Lucide's rotate arrows — the UI font
/// has no ↺ / ↻), named for tests and assistive tech, and still turn the photo; the key hint
/// is App.tsx's, without "F faces" while the Faces module is off (its key would do nothing).
#[gpui_kit::test]
fn the_loupe_bar_rotates_with_icon_chips_and_hints_its_keys(cx: &mut TestAppContext) {
    let (app, _pool, _dir, ids) = app_with(2, "loupe-bar", cx);
    select(&app, ids[0], cx);
    press(&app, "enter", cx);
    assert_eq!(label_of(&app, "loupe-rotate-left", cx).as_deref(), Some("Rotate left"));
    assert_eq!(label_of(&app, "loupe-rotate-right", cx).as_deref(), Some("Rotate right"));
    assert_eq!(
        label_of(&app, "loupe-hint", cx).as_deref(),
        Some("scroll zoom · drag pan · dbl-click 100% · P pick · X reject · ← →")
    );
    let rotation = |app: &App| app.state.catalog.lock().unwrap().as_ref().unwrap().photo_rotation(ids[0]).unwrap();
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.click("loupe-rotate-right", cx)
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(rotation(&app), 90);
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.click("loupe-rotate-left", cx)
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(rotation(&app), 0);
}

/// #197: the loupe hint's trailing "← →" draws as Lucide icons at the app's 13 px stroke-icon
/// size, not the UI font's tiny fallback mark for U+2190/U+2192 — its accessible label
/// ([`crate::loupe::view::loupe_hint`]) keeps the literal arrows unchanged.
#[gpui_kit::test]
fn the_loupe_hint_draws_its_arrows_as_normal_sized_icons(cx: &mut TestAppContext) {
    let (app, _pool, _dir, ids) = app_with(1, "loupe-hint-arrows", cx);
    select(&app, ids[0], cx);
    press(&app, "enter", cx);
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        for id in ["loupe-hint-arrow-left", "loupe-hint-arrow-right"] {
            let size = window.find(id).bounds().size;
            assert_eq!(size.width, gpui_kit::px(13.), "{id} is the 13 px icon, not a tiny fallback glyph");
            assert_eq!(size.height, gpui_kit::px(13.), "{id}");
        }
    })
    .unwrap();
}

// --- Compare ----------------------------------------------------------------------------------

/// The duel: → crowns the challenger (the champion is rejected), ← keeps the champion; the last
/// verdict picks the winner. Esc closes.
#[gpui_kit::test]
fn a_duel_rejects_losers_and_picks_the_winner(cx: &mut TestAppContext) {
    let (app, _pool, _dir, ids) = app_with(3, "cmp-duel", cx);
    select_all(&app, cx);
    press(&app, "c", cx);
    assert_eq!(stage(&app, cx), StageView::Compare);
    assert_eq!(label_of(&app, "compare-status", cx).as_deref(), Some("Duel 1 of 2"));
    press(&app, "right", cx);
    assert_eq!(culling(&app, ids[0]).1, PickState::Reject);
    press(&app, "left", cx);
    assert_eq!(culling(&app, ids[2]).1, PickState::Reject);
    assert_eq!(culling(&app, ids[1]).1, PickState::Pick);
    assert_eq!(
        label_of(&app, "compare-status", cx).as_deref(),
        Some("Champion — p1.ARW · picked, rivals rejected · Esc to finish")
    );
    press(&app, "escape", cx);
    assert_eq!(stage(&app, cx), StageView::Grid);
}

/// Grid mode (remembered per machine): the culling keys mark the focused pane only; K picks it
/// and rejects its on-screen rivals — never the rest of the pool — then pages on.
#[gpui_kit::test]
fn grid_compare_marks_the_focused_pane_and_keep_is_batch_scoped(cx: &mut TestAppContext) {
    let (app, _pool, _dir, ids) = app_with(6, "cmp-grid", cx);
    select_all(&app, cx);
    press(&app, "c", cx);
    cx.update_window(app.window(), |_, window, cx| window.click("compare-mode-grid", cx)).unwrap();
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| MachinePrefs::read(cx, MODE_PREF)).as_deref(), Some("grid"));
    let focused = || app.wired.shell.read_with(cx, |s, _| s.compare_focused());
    let first = focused().unwrap();
    assert!(ids[..4].contains(&first));
    press(&app, "3", cx);
    for &id in &ids {
        assert_eq!(culling(&app, id).0, if id == first { 3 } else { 0 }, "photo {id}");
    }
    press(&app, "k", cx);
    let batch: Vec<i64> = ids[..4].to_vec();
    for &id in &batch {
        let want = if id == first { PickState::Pick } else { PickState::Reject };
        assert_eq!(culling(&app, id).1, want, "photo {id}");
    }
    assert_eq!(culling(&app, ids[4]).1, PickState::None, "outside the batch");
    assert_eq!(label_of(&app, "compare-status", cx).as_deref(), Some("Comparing 5–6 of 6"));
}

/// The panes share one transform; a new set resets it to fit.
#[gpui_kit::test]
fn compare_panes_share_one_pan_and_zoom(cx: &mut TestAppContext) {
    let (app, pool, _dir, ids) = app_with(3, "cmp-zoom", cx);
    select_all(&app, cx);
    press(&app, "c", cx);
    for &id in &ids {
        pool.finish(&preview(id), Ok(pixels(300, 200)));
    }
    cx.run_until_parked();
    wheel(&app, "compare-image-1", true, cx);
    let compare = root(&app).read_with(cx, |r, _| r.compare().clone());
    let shared = compare.read_with(cx, |c, _| c.shared_view().clone());
    let view = shared.read_with(cx, |s, _| s.view);
    assert!(view.zoomed());
    let fit_shown = cx
        .update_window(app.window(), |_, window, cx| {
            window.render_frame(cx);
            window.try_find("compare-fit").is_some()
        })
        .unwrap();
    assert!(fit_shown, "the bar offers Fit");
    // A verdict brings a new challenger: back to fit.
    press(&app, "right", cx);
    render(&app, cx);
    assert_eq!(shared.read_with(cx, |s, _| s.view), ZoomView::FIT);
}

/// A duel verdict whose write fails does not advance: the loser's row is gone from the
/// catalog, so its reject fails, and the duel stays on round 1 with the challenger unpicked.
#[gpui_kit::test]
fn a_failed_duel_verdict_stays_on_the_pair(cx: &mut TestAppContext) {
    let (app, _pool, _dir, ids) = app_with(3, "cmp-fail", cx);
    select_all(&app, cx);
    press(&app, "c", cx);
    app.state.catalog.lock().unwrap().as_ref().unwrap().remove_photo(ids[0]).unwrap();
    press(&app, "right", cx);
    assert!(status(&app, cx).starts_with("Could not mark"), "{}", status(&app, cx));
    let session = app.wired.shell.read_with(cx, |s, _| s.compare().cloned()).expect("Compare stays open");
    assert_eq!(session.duel_progress(), (1, 2), "still the first pair");
    assert_eq!((session.batch(), session.focus()), (vec![ids[0], ids[1]], Some(ids[1])));
    assert!(!session.pending(), "settled: the next verdict may be decided");
    assert_eq!(culling(&app, ids[2]).1, PickState::None);
}

/// Select exactly `set`, in order.
fn select_set(app: &App, set: &[i64], cx: &mut TestAppContext) {
    app.wired.shell.update(cx, |s, cx| s.select_with(cx, select_set_verb(set)));
    cx.run_until_parked();
}

fn select_set_verb(set: &[i64]) -> impl FnOnce(&mut chairphoto_model::library::session::LibrarySession) {
    use chairphoto_model::library::session::SelectMods;
    let set = set.to_vec();
    move |l| {
        l.select_single(set[0]);
        for &id in &set[1..] {
            l.select(id, SelectMods::CTRL);
        }
    }
}

/// #205: a verdict still being written when Compare closes and reopens on another set lands
/// its marks, but settles nothing in the new session — not its round, not its pending flag.
#[gpui_kit::test]
fn a_verdict_from_a_closed_compare_does_not_move_the_next_one(cx: &mut TestAppContext) {
    use chairphoto_model::compare_duel::DuelSide;
    let (app, _pool, _dir, ids) = app_with(6, "cmp-epoch", cx);
    let (first, second) = (ids[..3].to_vec(), ids[3..].to_vec());
    select_set(&app, &first, cx);
    assert!(app.wired.shell.update(cx, |s, cx| s.open_compare(cx)));
    // Nothing parks between the verdict and the reopen: its write is still in flight.
    app.wired.shell.update(cx, |s, cx| {
        s.compare_verdict(DuelSide::Right, cx);
        assert!(s.compare().unwrap().pending());
        s.close_compare(cx);
        s.select_with(cx, select_set_verb(&second));
        assert!(s.open_compare(cx));
        assert_eq!(s.compare().unwrap().pool(), &second[..]);
    });
    cx.run_until_parked();
    assert_eq!(culling(&app, first[0]).1, PickState::Reject, "the old verdict's write landed");
    let session = app.wired.shell.read_with(cx, |s, _| s.compare().cloned()).unwrap();
    assert_eq!(
        (session.batch(), session.duel_progress(), session.focus(), session.pending()),
        (vec![second[0], second[1]], (1, 2), Some(second[1]), false),
        "the new duel is where it opened"
    );
    for &id in &second {
        assert_eq!(culling(&app, id).1, PickState::None, "photo {id}");
    }
    // Its own verdict judges its first pair.
    press(&app, "right", cx);
    assert_eq!(culling(&app, second[0]).1, PickState::Reject);
    let session = app.wired.shell.read_with(cx, |s, _| s.compare().cloned()).unwrap();
    assert_eq!((session.batch(), session.duel_progress()), (vec![second[1], second[2]], (2, 2)));
}

/// #205: Duel→Grid→Duel while a verdict is being written restarts the duel on the round the
/// verdict was decided in; the verdict's write lands but does not move it. (That a stale
/// verdict leaves the new duel's own pending verdict alone is covered in `compare.rs`.)
#[gpui_kit::test]
fn a_verdict_from_before_a_mode_round_trip_does_not_move_the_duel(cx: &mut TestAppContext) {
    use crate::loupe::compare::CompareMode;
    use chairphoto_model::compare_duel::DuelSide;
    let (app, _pool, _dir, ids) = app_with(3, "cmp-epoch-mode", cx);
    select_all(&app, cx);
    press(&app, "c", cx);
    app.wired.shell.update(cx, |s, cx| {
        s.compare_verdict(DuelSide::Right, cx);
        s.set_compare_mode(CompareMode::Grid, cx);
        s.set_compare_mode(CompareMode::Duel, cx);
        assert!(!s.compare().unwrap().pending());
    });
    cx.run_until_parked();
    assert_eq!(culling(&app, ids[0]).1, PickState::Reject, "the old verdict's write landed");
    let session = app.wired.shell.read_with(cx, |s, _| s.compare().cloned()).unwrap();
    assert_eq!(
        (session.batch(), session.duel_progress(), session.pending()),
        (vec![ids[0], ids[1]], (1, 2), false),
        "the restarted duel did not move"
    );
}

/// When every pane drops out of the view — here the duel's champion, rated under the Unrated
/// filter once the duel is done — Compare ends, and the grid's keys mark the selection again
/// (React's `inCompare` required a pane).
#[gpui_kit::test]
fn compare_ends_when_every_pane_leaves_the_view(cx: &mut TestAppContext) {
    let (app, _pool, _dir, ids) = app_with(3, "cmp-gone", cx);
    app.wired.shell.update(cx, |s, cx| s.update_scope(cx, |l| l.set_filter(CullingFilter::Unrated)));
    cx.run_until_parked();
    select_all(&app, cx);
    press(&app, "c", cx);
    press(&app, "right", cx);
    press(&app, "left", cx);
    assert!(app.wired.shell.read_with(cx, |s, _| s.compare().unwrap().duel().done));
    assert_eq!(culling(&app, ids[1]).1, PickState::Pick, "the champion");
    // Rejected but unrated, the rivals stay in the view; the rated champion leaves it.
    press(&app, "3", cx);
    assert_eq!(culling(&app, ids[1]).0, 3);
    assert!(app.wired.shell.read_with(cx, |s, _| s.compare().is_none()), "Compare ended");
    assert_eq!(stage(&app, cx), StageView::Grid);
    select(&app, ids[0], cx);
    press(&app, "2", cx);
    assert_eq!(culling(&app, ids[0]).0, 2, "the grid's selection is marked");
}

/// Closing Compare resets its view: reopened on the same frames it starts at fit, on the
/// preview tier.
#[gpui_kit::test]
fn reopening_compare_on_the_same_set_starts_fresh(cx: &mut TestAppContext) {
    let (app, pool, _dir, ids) = app_with(3, "cmp-reopen", cx);
    select_all(&app, cx);
    press(&app, "c", cx);
    for &id in &ids {
        pool.finish(&preview(id), Ok(pixels(300, 200)));
    }
    cx.run_until_parked();
    wheel(&app, "compare-image-1", true, cx);
    let compare = root(&app).read_with(cx, |r, _| r.compare().clone());
    let shared = compare.read_with(cx, |c, _| c.shared_view().clone());
    let pane = compare.read_with(cx, |c, _| c.panes()[1].clone());
    assert!(shared.read_with(cx, |s, _| s.view.zoomed()));
    assert!(pane.read_with(cx, |z, _| z.wants_hi()), "zoomed: the full-resolution tier");
    press(&app, "escape", cx);
    assert_eq!(stage(&app, cx), StageView::Grid);
    press(&app, "c", cx);
    assert_eq!(stage(&app, cx), StageView::Compare);
    render(&app, cx);
    assert_eq!(shared.read_with(cx, |s, _| s.view), ZoomView::FIT, "the same frames, at fit");
    assert!(!pane.read_with(cx, |z, _| z.wants_hi()), "on the preview tier again");
}

/// Catalog identity: a verdict after the core switched writes nothing to the colliding ids;
/// `catalog:switched` closes Compare.
#[gpui_kit::test]
fn compare_writes_fail_closed_across_a_catalog_switch(cx: &mut TestAppContext) {
    let (app, _pool, dir, ids) = app_with(3, "cmp-switch", cx);
    select_all(&app, cx);
    press(&app, "c", cx);
    let (other, _) = colliding_catalog(&dir, "other", 3);
    core_switch(&app, other);
    press(&app, "right", cx);
    assert_eq!(culling(&app, ids[0]).1, PickState::None, "the other catalog's photo is untouched");
    deliver_switch(&app, cx);
    assert_eq!(stage(&app, cx), StageView::Grid);
    assert!(app.wired.shell.read_with(cx, |s, _| s.compare().is_none()));
}

/// #170: in Compare the inspector's header and the bench's marking name the focused pane —
/// the photo the inspector's body describes and `apply_mark` writes (React's `shellPhoto`) —
/// not the selection's active photo, and the bench's toggles resolve against that pane.
#[gpui_kit::test]
fn compare_header_and_bench_follow_the_focused_pane(cx: &mut TestAppContext) {
    let (app, _pool, _dir, ids) = app_with(3, "cmp-shown", cx);
    select(&app, ids[0], cx);
    select_all(&app, cx);
    press(&app, "c", cx);
    assert_eq!(stage(&app, cx), StageView::Compare);
    let name_of = |id: i64| format!("p{}.ARW", ids.iter().position(|&i| i == id).unwrap());
    let focused = app.wired.shell.read_with(cx, |s, _| s.compare_focused()).expect("a focused pane");
    assert_eq!(active(&app, cx), Some(ids[0]));
    assert_ne!(focused, ids[0], "the focused pane is not the active photo");
    assert_eq!(label_of(&app, "inspector-filename", cx), Some(name_of(focused)));
    assert_eq!(label_of(&app, "bench-mark-name", cx), Some(name_of(focused)));

    // The focused pane is picked, the active photo is not: the bench's Pick shows the pane's
    // state, so a click clears the pick rather than writing it again.
    app.wired.shell.update(cx, |s, cx| s.apply_mark(crate::shell::state::Mark::Pick(PickState::Pick), false, cx));
    cx.run_until_parked();
    assert_eq!(culling(&app, focused).1, PickState::Pick);
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.click("bench-pick", cx)
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(culling(&app, focused).1, PickState::None, "the bench toggled the focused pane's pick off");
    assert_eq!(culling(&app, ids[0]).1, PickState::None, "the active photo is untouched");
}

// --- the cull session -------------------------------------------------------------------------

fn cull(app: &App, cx: &mut TestAppContext) -> Option<Entity<CullView>> {
    root(app).read_with(cx, |r, _| r.cull().cloned())
}

fn start_cull(app: &App, cx: &mut TestAppContext) {
    cx.update_window(app.window(), |_, window, cx| window.dispatch_action(Box::new(StartCullSession), cx)).unwrap();
    cx.run_until_parked();
}

/// A session resumes at the saved photo, preloads ahead of it, writes each decision and moves
/// on, saves the cursor after a pause, and ends with a summary and a status line.
#[gpui_kit::test]
fn a_cull_session_resumes_decides_saves_and_summarises(cx: &mut TestAppContext) {
    let (app, pool, _dir, ids) = app_with(5, "cull", cx);
    {
        let guard = app.state.catalog.lock().unwrap();
        guard.as_ref().unwrap().set_setting(CURSOR_KEY, &ids[2].to_string()).unwrap();
    }
    start_cull(&app, cx);
    let view = cull(&app, cx).expect("the session opened");
    assert_eq!(view.read_with(cx, |v, _| v.state.at()), Some(2));
    assert_eq!(
        label_of(&app, "cull-note", cx).as_deref(),
        Some("Resumed where you left off — photo 3 of 5.")
    );
    let want: Vec<i64> = [2, 3, 1, 4].iter().map(|&i| ids[i]).collect();
    assert_eq!(last_previews(&pool), want, "current, N+1, N−1, then ahead");

    press(&app, "p", cx);
    assert_eq!(culling(&app, ids[2]).1, PickState::Pick);
    assert_eq!(view.read_with(cx, |v, _| v.state.at()), Some(3), "moved on");
    press(&app, "left", cx);
    assert!(label_of(&app, "cull-pick", cx).is_some() || view.read_with(cx, |v, _| v.state.shown().unwrap().1) == PickState::Pick);
    press(&app, "[", cx);
    assert!(app.wired.shell.read_with(cx, |s, _| s.panel_visible(crate::shell::state::Side::Left)), "panel keys are muted");

    // The debounced save: on photo 4 (not the seeded photo 3), only once the pause is over.
    press(&app, "right", cx);
    let saved = || app.state.catalog.lock().unwrap().as_ref().unwrap().get_setting(CURSOR_KEY).unwrap();
    assert_eq!(saved(), Some(ids[2].to_string()), "not before the pause");
    cx.executor().advance_clock(CURSOR_DEBOUNCE);
    cx.run_until_parked();
    assert_eq!(saved(), Some(ids[3].to_string()), "saved after the pause");

    // The finish save: back to photo 3 and end at once — the end saves it, not the debounce.
    press(&app, "left", cx);
    press(&app, "escape", cx);
    assert_eq!(saved(), Some(ids[2].to_string()), "saved at the end, without the pause");
    assert!(view.read_with(cx, |v, _| v.state.summary.is_some()));
    assert_eq!(label_of(&app, "cull-summary-lead", cx).as_deref(), Some("2 of 5 photos reviewed · 3 still to go"));
    press(&app, "enter", cx);
    assert!(cull(&app, cx).is_none());
    assert_eq!(status(&app, cx), "Cull session: 2 reviewed, 1 picked, 0 rejected, 3 left.");
    let grid_focused = cx
        .update_window(app.window(), |_, window, cx| {
            root(&app).read(cx).library().read(cx).focus_handle().is_focused(window)
        })
        .unwrap();
    assert!(grid_focused, "the grid has the keys again");
}

/// The session occludes the shell: a click on the cull photo, over a grid tile, neither focuses
/// the grid nor changes its selection, and the culling keys still mark the cull photo.
#[gpui_kit::test]
fn a_click_on_the_cull_session_does_not_reach_the_grid(cx: &mut TestAppContext) {
    let (app, _pool, _dir, ids) = app_with(4, "cull-occlude", cx);
    select_all(&app, cx);
    render(&app, cx);
    start_cull(&app, cx);
    let view = cull(&app, cx).unwrap();
    assert_eq!(view.read_with(cx, |v, _| v.state.at()), Some(0));
    let selection = |cx: &mut TestAppContext| app.wired.shell.read_with(cx, |s, _| s.library.selection().ids.to_vec());
    let before = selection(cx);
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        // A tile under the session, which a click there would select alone.
        let position = window.find(("tile", ids[3] as u64)).bounds().center();
        let hover = MouseMoveEvent { position, pressed_button: None, modifiers: Modifiers::default() };
        window.dispatch_event(hover.to_platform_input(), cx);
        let down = MouseDownEvent {
            button: MouseButton::Left,
            position,
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        };
        window.dispatch_event(down.to_platform_input(), cx);
        let up = MouseUpEvent { button: MouseButton::Left, position, modifiers: Modifiers::default(), click_count: 1 };
        window.dispatch_event(up.to_platform_input(), cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
    let (cull_focused, grid_focused) = cx
        .update_window(app.window(), |_, window, cx| {
            (
                view.read(cx).focus_handle().is_focused(window),
                root(&app).read(cx).library().read(cx).focus_handle().is_focused(window),
            )
        })
        .unwrap();
    assert!(cull_focused && !grid_focused, "the session keeps the keys");
    assert_eq!(selection(cx), before, "the grid's selection is untouched");
    press(&app, "p", cx);
    assert_eq!(culling(&app, ids[0]).1, PickState::Pick, "the cull photo is marked");
    assert_eq!(culling(&app, ids[3]).1, PickState::None, "the tile under the click is not");
    assert_eq!(view.read_with(cx, |v, _| v.state.at()), Some(1));
}

/// A decision the catalog did not take (the core switched under the session) is rolled back
/// and said out loud; the colliding catalog is untouched; `catalog:switched` ends the session.
#[gpui_kit::test]
fn a_failed_cull_write_is_rolled_back_and_shown(cx: &mut TestAppContext) {
    let (app, _pool, dir, ids) = app_with(3, "cull-fail", cx);
    start_cull(&app, cx);
    let view = cull(&app, cx).unwrap();
    let (other, _) = colliding_catalog(&dir, "other", 3);
    core_switch(&app, other);
    press(&app, "4", cx);
    assert_eq!(culling(&app, ids[0]).0, 0);
    assert!(view.read_with(cx, |v, _| v.state.decision(ids[0]).is_none()), "rolled back");
    let failure = label_of(&app, "cull-failure", cx).unwrap();
    assert!(failure.starts_with("Not saved — p0.ARW: The catalog changed"), "{failure}");
    deliver_switch(&app, cx);
    assert!(cull(&app, cx).is_none());
}

/// #197: the cull help's → / ← rows and its note draw Lucide's arrow icons at the app's 13 px
/// stroke-icon size, not the UI font's tiny fallback mark for U+2192/U+2190 — ↓ and ↑ stay
/// plain text (the font has them).
#[gpui_kit::test]
fn the_cull_help_draws_its_arrows_as_normal_sized_icons(cx: &mut TestAppContext) {
    let (app, _pool, _dir, _ids) = app_with(2, "cull-help-arrows", cx);
    start_cull(&app, cx);
    press(&app, "h", cx);
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        for id in ["cull-help-next-arrow", "cull-help-back-arrow", "cull-help-note-arrow"] {
            let size = window.find(id).bounds().size;
            assert_eq!(size.width, gpui_kit::px(13.), "{id} is the 13 px icon, not a tiny fallback glyph");
            assert_eq!(size.height, gpui_kit::px(13.), "{id}");
        }
    })
    .unwrap();
}

// --- the Darkroom overlays ---------------------------------------------------------------------

#[cfg(feature = "edit")]
mod overlays {
    use super::*;
    use crate::loupe::duel::{DuelEvent, DuelView, VariantSource, DUEL_EDGE};
    use crate::loupe::edit_renders::{EditRenders, RenderState};
    use crate::loupe::proof_sheet::{ProofEvent, ProofSheet, PROOF_EDGE};
    use chairphoto_core::plugins::edit::SourceToken;
    use chairphoto_model::darkroom::spreads::proof_spread;
    use chairphoto_model::editing::VersionEdit;


    fn edits(pool: &FakePool) -> Vec<chairphoto_core::image_pool::EditJob> {
        pool.last_batch()
            .into_iter()
            .filter_map(|k| match k {
                JobKey::Edit(job) => Some(job),
                _ => None,
            })
            .collect()
    }

    /// The duel renders its pair, a click or → applies the variant and moves to the next
    /// dimension (cancelling the old pair's renders), ↓ skips, Esc closes.
    #[gpui_kit::test]
    fn the_duel_renders_its_pair_applies_picks_and_closes(cx: &mut TestAppContext) {
        let (app, pool, _dir, ids) = app_with(1, "duel", cx);
        let images = app.wired.images.clone();
        let source = VariantSource::new(ids[0], 3, SourceToken::Preview);
        let events: Rc<RefCell<Vec<DuelEvent>>> = Rc::default();
        let (handle, duel) = cx
            .update(|cx| {
                gpui_kit::open_window(Default::default(), cx, |_, cx| {
                    cx.new(|cx| DuelView::new(&images, source, VersionEdit::default(), None, cx))
                })
            })
            .unwrap();
        cx.update(|cx| {
            let events = events.clone();
            cx.subscribe(&duel, move |_, e: &DuelEvent, _| events.borrow_mut().push(e.clone())).detach()
        });
        let first = edits(&pool);
        assert_eq!(first.len(), 2);
        assert!(first.iter().all(|j| j.max_edge == DUEL_EDGE && j.catalog_epoch == 3 && j.photo_id == ids[0]));
        let pair = duel.read_with(cx, |d, _| d.pair());
        assert_eq!(first[1].edit_json, pair[1].to_json());

        cx.update_window(handle, |_, window, cx| {
            duel.read(cx).focus_handle().clone().focus(window, cx);
            window.render_frame(cx);
            window.press("right", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(events.borrow().first(), Some(&DuelEvent::Apply(pair[1].clone())));
        assert_eq!(duel.read_with(cx, |d, _| d.round()), 2);
        let cancelled = pool.cancelled.lock().unwrap().clone();
        assert!(first.iter().all(|j| cancelled.contains(&JobKey::Edit(j.clone()))), "the old pair is cancelled");

        cx.update_window(handle, |_, window, cx| window.press("down", cx)).unwrap();
        assert_eq!(duel.read_with(cx, |d, _| d.round()), 3, "↓ = same");
        cx.update_window(handle, |_, window, cx| window.press("escape", cx)).unwrap();
        cx.run_until_parked();
        assert_eq!(events.borrow().last(), Some(&DuelEvent::Close));
    }

    /// The proof sheet renders every candidate at 320 px; a click adopts one; Esc declines.
    #[gpui_kit::test]
    fn the_proof_sheet_renders_every_candidate_and_adopts_one(cx: &mut TestAppContext) {
        let (app, pool, _dir, ids) = app_with(1, "proofs", cx);
        let images = app.wired.images.clone();
        let candidates = proof_spread(&VersionEdit::default(), &VersionEdit::default(), &[], None);
        let n = candidates.len();
        let source = VariantSource::new(ids[0], 0, SourceToken::Preview);
        let events: Rc<RefCell<Vec<ProofEvent>>> = Rc::default();
        let (handle, sheet) = cx
            .update(|cx| {
                let candidates = candidates.clone();
                let shell = app.wired.shell.clone();
                gpui_kit::open_window(Default::default(), cx, |window, cx| {
                    cx.new(|cx| ProofSheet::new(&images, shell, source, candidates, window, cx))
                })
            })
            .unwrap();
        cx.update(|cx| {
            let events = events.clone();
            cx.subscribe(&sheet, move |_, e: &ProofEvent, _| events.borrow_mut().push(e.clone())).detach()
        });
        let jobs = edits(&pool);
        let distinct: std::collections::HashSet<String> = candidates.iter().map(|c| c.record.to_json()).collect();
        assert!(n > distinct.len(), "the fixture deals identical cells");
        assert_eq!(jobs.len(), distinct.len(), "one render per distinct record: identical cells share it");
        assert!(jobs.iter().all(|j| j.max_edge == PROOF_EDGE));
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click(("proof-cell", 2u64), cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(events.borrow().as_slice(), &[ProofEvent::Adopt(candidates[2].clone())]);
        // A sheet closed by Escape.
        let (handle, sheet) = cx
            .update(|cx| {
                let images = app.wired.images.clone();
                let shell = app.wired.shell.clone();
                let source = VariantSource::new(ids[0], 0, SourceToken::Preview);
                gpui_kit::open_window(Default::default(), cx, |window, cx| {
                    cx.new(|cx| ProofSheet::new(&images, shell, source, candidates, window, cx))
                })
            })
            .unwrap();
        let closed = Rc::new(RefCell::new(false));
        cx.update(|cx| {
            let closed = closed.clone();
            cx.subscribe(&sheet, move |_, e: &ProofEvent, _| *closed.borrow_mut() = *e == ProofEvent::Close).detach()
        });
        cx.update_window(handle, |_, window, cx| {
            sheet.read(cx).focus_handle().clone().focus(window, cx);
            window.render_frame(cx);
            window.press("escape", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert!(*closed.borrow());
    }

    // --- the proof sheet's preview on the pop-out (#250) --------------------------------------

    /// Hovering a proof cell previews it (`ShellState::loupe_proof_preview`); leaving it falls
    /// back to the Tab-focused cell, else nothing (the pop-out shows the photo as it is).
    #[gpui_kit::test]
    fn hovering_a_proof_previews_it_falling_back_to_the_focused_cell(cx: &mut TestAppContext) {
        let (app, _pool, _dir, ids) = app_with(1, "proof-preview-hover", cx);
        let images = app.wired.images.clone();
        let shell = app.wired.shell.clone();
        let candidates = proof_spread(&VersionEdit::default(), &VersionEdit::default(), &[], None);
        let source = VariantSource::new(ids[0], 0, SourceToken::Preview);
        let (handle, sheet) = cx
            .update(|cx| {
                let candidates = candidates.clone();
                let shell = shell.clone();
                gpui_kit::open_window(Default::default(), cx, |window, cx| {
                    cx.new(|cx| ProofSheet::new(&images, shell, source, candidates, window, cx))
                })
            })
            .unwrap();
        let preview =
            |cx: &mut TestAppContext| shell.read_with(cx, |s, _| s.loupe_proof_preview().map(|p| p.candidate.clone()));
        assert_eq!(preview(cx), None, "the backdrop has focus, nothing hovered");

        // Tab from the backdrop to cell 0: focus alone previews it.
        cx.update_window(handle, |_, window, cx| {
            sheet.read(cx).focus_handle().clone().focus(window, cx);
            window.render_frame(cx);
            window.press("tab", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(preview(cx), Some(candidates[0].clone()), "Tab focus alone previews it");

        // Hovering cell 1 overrides the focused cell.
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.hover(("proof-cell", 1u64), cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(preview(cx), Some(candidates[1].clone()), "hover wins over focus");

        // Leaving the hovered cell falls back to the still Tab-focused one.
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.hover("proof-close", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(preview(cx), Some(candidates[0].clone()), "falls back to the focused cell");
    }

    /// Clicking a proof adopts it; Esc declines. Either way the preview clears.
    #[gpui_kit::test]
    fn adopting_or_declining_a_proof_clears_its_preview(cx: &mut TestAppContext) {
        let (app, _pool, _dir, ids) = app_with(1, "proof-preview-end", cx);
        let images = app.wired.images.clone();
        let shell = app.wired.shell.clone();
        let candidates = proof_spread(&VersionEdit::default(), &VersionEdit::default(), &[], None);
        let previewed = |cx: &mut TestAppContext| shell.read_with(cx, |s, _| s.loupe_proof_preview().is_some());

        let (handle, _sheet) = cx
            .update(|cx| {
                let candidates = candidates.clone();
                let shell = shell.clone();
                let source = VariantSource::new(ids[0], 0, SourceToken::Preview);
                gpui_kit::open_window(Default::default(), cx, |window, cx| {
                    cx.new(|cx| ProofSheet::new(&images, shell, source, candidates, window, cx))
                })
            })
            .unwrap();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.hover(("proof-cell", 2u64), cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert!(previewed(cx), "hovered");
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click(("proof-cell", 2u64), cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert!(!previewed(cx), "adopting clears it");

        // A fresh sheet, declined by Escape.
        let (handle, sheet) = cx
            .update(|cx| {
                let images = app.wired.images.clone();
                let shell = shell.clone();
                let source = VariantSource::new(ids[0], 0, SourceToken::Preview);
                gpui_kit::open_window(Default::default(), cx, |window, cx| {
                    cx.new(|cx| ProofSheet::new(&images, shell, source, candidates, window, cx))
                })
            })
            .unwrap();
        cx.update_window(handle, |_, window, cx| {
            sheet.read(cx).focus_handle().clone().focus(window, cx);
            window.render_frame(cx);
            window.hover(("proof-cell", 0u64), cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert!(previewed(cx), "hovered again");
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.press("escape", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert!(!previewed(cx), "declining clears it");
    }

    /// A pointer click on the panel's own padding (not a cell, and not the bare backdrop,
    /// which declines) still focuses the backdrop — `track_focus` on a mouse down, GPUI's own
    /// behaviour — moving focus off a Tab-focused cell without going through `cycle`'s own
    /// resync. Must not leave a stale preview (#250 review, probe E): `ProofSheet`'s panel
    /// resyncs on the matching mouse up.
    #[gpui_kit::test]
    fn a_click_on_the_panels_padding_clears_a_tab_focused_preview(cx: &mut TestAppContext) {
        let (app, _pool, _dir, ids) = app_with(1, "proof-preview-mouse-blur", cx);
        let images = app.wired.images.clone();
        let shell = app.wired.shell.clone();
        let candidates = proof_spread(&VersionEdit::default(), &VersionEdit::default(), &[], None);
        let source = VariantSource::new(ids[0], 0, SourceToken::Preview);
        let (handle, sheet) = cx
            .update(|cx| {
                let candidates = candidates.clone();
                let shell = shell.clone();
                gpui_kit::open_window(Default::default(), cx, |window, cx| {
                    cx.new(|cx| ProofSheet::new(&images, shell, source, candidates, window, cx))
                })
            })
            .unwrap();
        let previewed = |cx: &mut TestAppContext| shell.read_with(cx, |s, _| s.loupe_proof_preview().is_some());
        let focused = |cx: &mut TestAppContext| cx.update_window(handle, |_, window, cx| sheet.read(cx).focused(window)).unwrap();

        cx.update_window(handle, |_, window, cx| {
            sheet.read(cx).focus_handle().clone().focus(window, cx);
            window.render_frame(cx);
            window.press("tab", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(focused(cx), Some(0), "Tab focuses cell 0");
        assert!(previewed(cx), "… and previews it");

        // The panel's own top-left corner, inside its 16 px padding: not a cell, and (unlike
        // the bare backdrop around the panel) not a decline either.
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click_at("proof-sheet", gpui_kit::point(gpui_kit::px(4.), gpui_kit::px(4.)), cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert!(cx.update_window(handle, |_, window, _| window.try_find("proof-backdrop").is_some()).unwrap(), "not declined");
        assert_eq!(focused(cx), None, "focus left the cell");
        assert!(!previewed(cx), "the stale preview is cleared");
    }

    /// The published preview's placeholder stays in step with the proof sheet's own 320 px
    /// render as it settles — not a one-time snapshot (#250 review, probe B / mutation M2):
    /// `ProofSheet`'s `renders` observer must republish when that render lands.
    #[gpui_kit::test]
    fn the_previews_placeholder_updates_once_the_cell_render_lands(cx: &mut TestAppContext) {
        let (app, pool, _dir, ids) = app_with(1, "proof-preview-placeholder-fresh", cx);
        let images = app.wired.images.clone();
        let shell = app.wired.shell.clone();
        let candidates = proof_spread(&VersionEdit::default(), &VersionEdit::default(), &[], None);
        let source = VariantSource::new(ids[0], 0, SourceToken::Preview);
        let job = source.job(&candidates[1].record, PROOF_EDGE);
        let (handle, _sheet) = cx
            .update(|cx| {
                let candidates = candidates.clone();
                let shell = shell.clone();
                gpui_kit::open_window(Default::default(), cx, |window, cx| {
                    cx.new(|cx| ProofSheet::new(&images, shell, source, candidates, window, cx))
                })
            })
            .unwrap();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.hover(("proof-cell", 1u64), cx);
        })
        .unwrap();
        cx.run_until_parked();
        let cell_state = |cx: &mut TestAppContext| {
            shell.read_with(cx, |s, _| match s.loupe_proof_preview().unwrap().cell {
                RenderState::Ready(_) => "ready",
                RenderState::Rendering => "rendering",
                _ => "other",
            })
        };
        assert_eq!(cell_state(cx), "rendering", "before the 320 px render lands");

        pool.finish(&JobKey::Edit(job), Ok(pixels(40, 30)));
        cx.run_until_parked();
        assert_eq!(cell_state(cx), "ready", "the published preview picks up the landed render");
    }

    /// Releasing a sheet clears only its own previewed candidate, never another live sheet's
    /// (#250 review, probe C: contrived — today only one overlay exists in the app, since a
    /// new one replaces it before it could publish — but `ProofSheet::clear_preview` should
    /// not rely on that staying true).
    #[gpui_kit::test]
    fn releasing_a_sheet_clears_only_its_own_preview(cx: &mut TestAppContext) {
        let (app, _pool, _dir, ids) = app_with(1, "proof-preview-token", cx);
        let images = app.wired.images.clone();
        let shell = app.wired.shell.clone();
        let candidates = proof_spread(&VersionEdit::default(), &VersionEdit::default(), &[], None);

        // Sheet A, in its own window, never previews anything of its own. Only the window's
        // root-view reference keeps it alive (as the Darkroom's own `rails.overlay` would be
        // the only one in the app) — the local handle is dropped at once, so closing the
        // window below is genuinely the last reference and triggers `on_release`.
        let (handle_a, sheet_a) = cx
            .update(|cx| {
                let images = images.clone();
                let candidates = candidates.clone();
                let shell = shell.clone();
                let source = VariantSource::new(ids[0], 0, SourceToken::Preview);
                gpui_kit::open_window(Default::default(), cx, |window, cx| {
                    cx.new(|cx| ProofSheet::new(&images, shell, source, candidates, window, cx))
                })
            })
            .unwrap();
        drop(sheet_a);

        // Sheet B, in a second window, hovered: B's preview is the one up.
        let (handle_b, sheet_b) = cx
            .update(|cx| {
                let candidates = candidates.clone();
                let shell = shell.clone();
                let source = VariantSource::new(ids[0], 0, SourceToken::Preview);
                gpui_kit::open_window(Default::default(), cx, |window, cx| {
                    cx.new(|cx| ProofSheet::new(&images, shell, source, candidates, window, cx))
                })
            })
            .unwrap();
        cx.update_window(handle_b, |_, window, cx| {
            window.render_frame(cx);
            window.hover(("proof-cell", 1u64), cx);
        })
        .unwrap();
        cx.run_until_parked();
        let b_candidate = sheet_b.read_with(cx, |s, _| s.candidates()[1].clone());
        let previewed = |cx: &mut TestAppContext| shell.read_with(cx, |s, _| s.loupe_proof_preview().map(|p| p.candidate.clone()));
        assert_eq!(previewed(cx), Some(b_candidate.clone()), "B previews");

        // Closing A's window releases it, with nothing of its own to clear; B's active
        // preview must survive.
        cx.update_window(handle_a, |_, window, _| window.remove_window()).unwrap();
        cx.run_until_parked();
        assert_eq!(previewed(cx), Some(b_candidate), "A's release did not clear B's preview");
    }

    /// A sole sheet's own window closing (#250 second review, F1) must still release the
    /// entity and clear both its preview and its pop-out routing handle
    /// (`ShellState::loupe_proof_sheet`). `LoupeProofSheetHandle.sheet` is a `WeakEntity`, not
    /// a strong one: before that fix, the route held the sheet alive — a reference cycle,
    /// since `ProofSheet` itself holds an `Entity<ShellState>` — so `on_release` never ran
    /// for a sole sheet whose only other owner was the window being closed here, and both the
    /// preview and the route leaked past it.
    #[gpui_kit::test]
    fn a_sole_sheets_window_closing_releases_it_and_clears_its_route(cx: &mut TestAppContext) {
        let (app, _pool, _dir, ids) = app_with(1, "proof-route-release", cx);
        let images = app.wired.images.clone();
        let shell = app.wired.shell.clone();
        let candidates = proof_spread(&VersionEdit::default(), &VersionEdit::default(), &[], None);
        let (handle, sheet) = cx
            .update(|cx| {
                let shell = shell.clone();
                let source = VariantSource::new(ids[0], 0, SourceToken::Preview);
                gpui_kit::open_window(Default::default(), cx, |window, cx| {
                    cx.new(|cx| ProofSheet::new(&images, shell, source, candidates, window, cx))
                })
            })
            .unwrap();
        // Only the window's own root-view reference keeps it alive from here, as the
        // Darkroom's own `rails.overlay` would be the only one in the app
        // (`releasing_a_sheet_clears_only_its_own_preview`'s own comment).
        let weak = sheet.downgrade();
        drop(sheet);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.hover(("proof-cell", 1u64), cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert!(shell.read_with(cx, |s, _| s.loupe_proof_preview().is_some()), "previewing");
        assert!(shell.read_with(cx, |s, _| s.loupe_proof_sheet().is_some()), "routed");

        cx.update_window(handle, |_, window, _| window.remove_window()).unwrap();
        cx.run_until_parked();
        assert!(weak.upgrade().is_none(), "the sheet is actually released, not held alive by its own route");
        assert!(shell.read_with(cx, |s, _| s.loupe_proof_preview().is_none()), "its preview is cleared");
        assert!(shell.read_with(cx, |s, _| s.loupe_proof_sheet().is_none()), "its routing handle is cleared too");
    }

    /// A sheet's own `renders` observer calling `sync_preview` with nothing of its own hovered
    /// or focused — its own 320 px render landing, say — must not clear another live sheet's
    /// preview, the same per-sheet token `clear_preview` already used (#250 review, probe
    /// P2): `sync_preview`'s `None` case now delegates to `clear_preview` instead of
    /// publishing `None` straight to `ShellState`.
    #[gpui_kit::test]
    fn a_sheets_own_render_landing_clears_only_its_own_preview(cx: &mut TestAppContext) {
        let (app, pool, _dir, ids) = app_with(2, "proof-preview-token-render", cx);
        let images = app.wired.images.clone();
        let shell = app.wired.shell.clone();
        let candidates = proof_spread(&VersionEdit::default(), &VersionEdit::default(), &[], None);

        // Sheet A, on a different photo, in its own window, never hovered or focused.
        let source_a = VariantSource::new(ids[0], 0, SourceToken::Preview);
        let a_job = source_a.job(&candidates[0].record, PROOF_EDGE);
        cx.update(|cx| {
            let images = images.clone();
            let candidates = candidates.clone();
            let shell = shell.clone();
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| ProofSheet::new(&images, shell, source_a, candidates, window, cx))
            })
        })
        .unwrap();

        // Sheet B, on another photo, hovered: B's preview is the one up.
        let (handle_b, sheet_b) = cx
            .update(|cx| {
                let candidates = candidates.clone();
                let shell = shell.clone();
                let source = VariantSource::new(ids[1], 0, SourceToken::Preview);
                gpui_kit::open_window(Default::default(), cx, |window, cx| {
                    cx.new(|cx| ProofSheet::new(&images, shell, source, candidates, window, cx))
                })
            })
            .unwrap();
        cx.update_window(handle_b, |_, window, cx| {
            window.render_frame(cx);
            window.hover(("proof-cell", 1u64), cx);
        })
        .unwrap();
        cx.run_until_parked();
        let b_candidate = sheet_b.read_with(cx, |s, _| s.candidates()[1].clone());
        let previewed = |cx: &mut TestAppContext| shell.read_with(cx, |s, _| s.loupe_proof_preview().map(|p| p.candidate.clone()));
        assert_eq!(previewed(cx), Some(b_candidate.clone()), "B previews");

        // A's own 320 px cell render lands: A's `renders` observer calls `sync_preview`,
        // which has nothing of A's own to show.
        pool.finish(&JobKey::Edit(a_job), Ok(pixels(40, 30)));
        cx.run_until_parked();
        assert_eq!(previewed(cx), Some(b_candidate), "A's own render landing did not clear B's preview");
    }

    /// Two mounted sheets, each with a hovered cell, must settle rather than loop (#250
    /// review, probe P5): `ProofSheet::resync_in_render` steps aside once another sheet owns
    /// the slot, so each sheet's own render no longer republishes its answer and notifies the
    /// other's forever. Not reachable in today's app (one Darkroom, one overlay), but
    /// `render` must not depend on that staying true. Run under a bounded `timeout` when
    /// checking for a hang — a broken guard never lets this test return.
    #[gpui_kit::test]
    fn two_hovered_sheets_settle_without_looping(cx: &mut TestAppContext) {
        let (app, _pool, _dir, ids) = app_with(2, "proof-preview-two-sheets", cx);
        let images = app.wired.images.clone();
        let shell = app.wired.shell.clone();
        let candidates = proof_spread(&VersionEdit::default(), &VersionEdit::default(), &[], None);
        let mut handles = Vec::new();
        for k in 0..2 {
            let (handle, _sheet) = cx
                .update(|cx| {
                    let images = images.clone();
                    let candidates = candidates.clone();
                    let shell = shell.clone();
                    let source = VariantSource::new(ids[k], 0, SourceToken::Preview);
                    gpui_kit::open_window(Default::default(), cx, |window, cx| {
                        cx.new(|cx| ProofSheet::new(&images, shell, source, candidates, window, cx))
                    })
                })
                .unwrap();
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                window.hover(("proof-cell", 1u64), cx);
            })
            .unwrap();
            handles.push(handle);
        }
        cx.run_until_parked();
        // Render each once more, now that both have hovered: the point of this test is that
        // this settles and returns at all (checked under a bounded `timeout` for a hang).
        for h in &handles {
            cx.update_window(*h, |_, window, cx| window.render_frame(cx)).unwrap();
        }
        cx.run_until_parked();
        assert!(shell.read_with(cx, |s, _| s.loupe_proof_preview().is_some()), "one of the two sheets' hovered candidate is shown");
    }

    // --- arrow keys over the proof sheet (#250 follow-up) ---------------------------------

    /// ← / → cycle focus the same way Tab / Shift+Tab do — wrapping, and from the backdrop
    /// landing on the first/last cell — and each move's preview follows; Enter adopts
    /// whichever cell an arrow press focused, not only a Tab-focused or clicked one.
    #[gpui_kit::test]
    fn left_and_right_cycle_focus_like_tab_and_enter_adopts_it(cx: &mut TestAppContext) {
        let (app, _pool, _dir, ids) = app_with(1, "proof-preview-left-right", cx);
        let images = app.wired.images.clone();
        let shell = app.wired.shell.clone();
        let candidates = proof_spread(&VersionEdit::default(), &VersionEdit::default(), &[], None);
        let n = candidates.len();
        let source = VariantSource::new(ids[0], 0, SourceToken::Preview);
        let events: Rc<RefCell<Vec<ProofEvent>>> = Rc::default();
        let (handle, sheet) = cx
            .update(|cx| {
                let candidates = candidates.clone();
                let shell = shell.clone();
                gpui_kit::open_window(Default::default(), cx, |window, cx| {
                    cx.new(|cx| ProofSheet::new(&images, shell, source, candidates, window, cx))
                })
            })
            .unwrap();
        cx.update(|cx| {
            let events = events.clone();
            cx.subscribe(&sheet, move |_, e: &ProofEvent, _| events.borrow_mut().push(e.clone())).detach()
        });
        let focused = |cx: &mut TestAppContext| cx.update_window(handle, |_, window, cx| sheet.read(cx).focused(window)).unwrap();
        let preview = |cx: &mut TestAppContext| shell.read_with(cx, |s, _| s.loupe_proof_preview().map(|p| p.candidate.clone()));
        let press = |key: &str, cx: &mut TestAppContext| {
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                window.press(key, cx);
            })
            .unwrap();
            cx.run_until_parked();
        };

        cx.update_window(handle, |_, window, cx| {
            sheet.read(cx).focus_handle().clone().focus(window, cx);
        })
        .unwrap();
        press("right", cx); // from the backdrop: the first cell, like Tab
        assert_eq!(focused(cx), Some(0));
        assert_eq!(preview(cx), Some(candidates[0].clone()));

        press("right", cx);
        assert_eq!(focused(cx), Some(1), "→ moves to the next cell");
        assert_eq!(preview(cx), Some(candidates[1].clone()));

        press("left", cx);
        assert_eq!(focused(cx), Some(0), "← moves back");

        press("left", cx);
        assert_eq!(focused(cx), Some(n - 1), "← from the first cell wraps to the last, like Shift+Tab");

        press("right", cx);
        assert_eq!(focused(cx), Some(0), "→ from the last cell wraps to the first, like Tab");

        press("right", cx);
        assert_eq!(focused(cx), Some(1));
        press("enter", cx);
        assert_eq!(events.borrow().as_slice(), &[ProofEvent::Adopt(candidates[1].clone())], "Enter adopts the arrow-focused cell");
    }

    /// ↑ / ↓ move focus by row in the grid as it actually renders ([`ProofSheet::columns`]),
    /// clamped to the nearest cell in a shorter row and staying put at the top/bottom edge
    /// rather than wrapping. [`row_target`] is the same pure arithmetic `ProofSheet::move_row`
    /// calls, exhaustively unit-tested on its own (next to it in `proof_sheet.rs`, with no
    /// GPUI involved) for the clamping and the edges; used here as the oracle, this proves the
    /// real dispatch, GPUI focus and the published preview actually follow it, at whatever
    /// column count the sheet's default window really measures — not a pixel count guessed in
    /// the test.
    #[gpui_kit::test]
    fn up_and_down_move_focus_by_row_and_preview_follows(cx: &mut TestAppContext) {
        use crate::loupe::proof_sheet::row_target;

        let (app, _pool, _dir, ids) = app_with(1, "proof-preview-rows", cx);
        let images = app.wired.images.clone();
        let shell = app.wired.shell.clone();
        let candidates = proof_spread(&VersionEdit::default(), &VersionEdit::default(), &[], None);
        let n = candidates.len();
        let source = VariantSource::new(ids[0], 0, SourceToken::Preview);
        let (handle, sheet) = cx
            .update(|cx| {
                let candidates = candidates.clone();
                let shell = shell.clone();
                gpui_kit::open_window(Default::default(), cx, |window, cx| {
                    cx.new(|cx| ProofSheet::new(&images, shell, source, candidates, window, cx))
                })
            })
            .unwrap();
        // Narrow enough that the panel's own flex-wrap actually wraps this fixture's 4 cells
        // (the default test window fits them all on one row): `columns()` is asserted below,
        // so a future layout change that stops wrapping here fails loudly, not silently.
        cx.simulate_window_resize(handle, gpui_kit::size(gpui_kit::px(500.), gpui_kit::px(900.)));
        cx.run_until_parked();
        let focused = |cx: &mut TestAppContext| cx.update_window(handle, |_, window, cx| sheet.read(cx).focused(window)).unwrap();
        let preview = |cx: &mut TestAppContext| shell.read_with(cx, |s, _| s.loupe_proof_preview().map(|p| p.candidate.clone()));
        let press = |key: &str, cx: &mut TestAppContext| {
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                window.press(key, cx);
            })
            .unwrap();
            cx.run_until_parked();
        };

        cx.update_window(handle, |_, window, cx| {
            sheet.read(cx).focus_handle().clone().focus(window, cx);
            window.render_frame(cx);
        })
        .unwrap();
        press("down", cx); // from the backdrop: the first cell
        let cols = sheet.read_with(cx, |s, _| s.columns());
        // Checked against the cells' own rendered positions (review mutation B: `columns()`
        // hard-coded to 1 passed this test before, since its oracle below only re-reads the
        // same value under test) — the first row's actual width, not `columns()`'s opinion of
        // it.
        let actual_cols = cx
            .update_window(handle, |_, window, _| {
                let ys: Vec<f32> = (0..n).map(|i| f32::from(window.find(("proof-cell", i as u64)).bounds().origin.y)).collect();
                ys.iter().filter(|y| (**y - ys[0]).abs() < 0.5).count()
            })
            .unwrap();
        assert_eq!(cols, actual_cols, "columns() must match how many cells actually rendered on the first row");
        assert!(cols < n, "this fixture's {n} cells must wrap past one row at the narrowed window: {cols} columns");
        assert_eq!(focused(cx), Some(0));
        assert_eq!(preview(cx), Some(candidates[0].clone()));

        // Three downs (reaching, then stuck at, the bottom edge) and four ups (back to the
        // first cell, then stuck at the top edge): every step checked against the oracle.
        for key in ["down", "down", "down", "up", "up", "up", "up"] {
            let before = focused(cx).unwrap();
            press(key, cx);
            let delta = if key == "down" { 1 } else { -1 };
            let want = row_target(before, n, cols, delta).unwrap_or(before);
            assert_eq!(focused(cx), Some(want), "{key} from {before} with {cols} columns");
            assert_eq!(preview(cx), Some(candidates[want].clone()));
        }
    }

    /// Last input wins (#250 follow-up): a pointer resting on a cell would otherwise block
    /// arrow navigation's own preview, since hover always won before. An arrow press must
    /// outrank the still-hovered cell, repeatedly, until the pointer itself actually moves —
    /// only then does hover take the preview back. Checked synchronously, right after each
    /// key — not after `run_until_parked`, which also lets this settle to the right answer
    /// under a *broken* `keyboard_wins`: GPUI's own on-hover default
    /// (`HoverListenerMode::InputModalityAware`) ends a hover after any key press too, real
    /// app included, deferred to the next paint (gpui-pre 0.3.7 `elements/div.rs`
    /// `default_hover_listener_ends_after_key_press`) — so a pointer this test never moves
    /// settles to `None` there regardless, and `None.or(focused)` gives the focused cell's
    /// preview either way, mutated `keyboard_wins` included. `keyboard_wins` only has to
    /// bridge the one frame between the key press and that deferred end (#250 review); this
    /// test's synchronous read is what actually exercises it (mutation-checked: removing the
    /// `keyboard_wins` branch in `sync_preview` fails this specific assertion).
    #[gpui_kit::test]
    fn an_arrow_press_outranks_a_resting_pointer_until_it_moves_again(cx: &mut TestAppContext) {
        let (app, _pool, _dir, ids) = app_with(1, "proof-preview-keyboard-wins", cx);
        let images = app.wired.images.clone();
        let shell = app.wired.shell.clone();
        let candidates = proof_spread(&VersionEdit::default(), &VersionEdit::default(), &[], None);
        let source = VariantSource::new(ids[0], 0, SourceToken::Preview);
        let (handle, sheet) = cx
            .update(|cx| {
                let candidates = candidates.clone();
                let shell = shell.clone();
                gpui_kit::open_window(Default::default(), cx, |window, cx| {
                    cx.new(|cx| ProofSheet::new(&images, shell, source, candidates, window, cx))
                })
            })
            .unwrap();

        cx.update_window(handle, |_, window, cx| {
            sheet.read(cx).focus_handle().clone().focus(window, cx);
            window.render_frame(cx);

            // The pointer rests on cell 2 — today, hover alone would own the preview.
            window.hover(("proof-cell", 2u64), cx);
            let preview = |cx: &mut gpui_kit::App| shell.read(cx).loupe_proof_preview().map(|p| p.candidate.clone());
            assert_eq!(preview(cx), Some(candidates[2].clone()), "hovered, nothing focused yet");

            // An arrow press focuses cell 0 and must win the preview despite the resting
            // pointer, which this test never moves.
            window.press("right", cx);
            assert_eq!(preview(cx), Some(candidates[0].clone()), "the keyboard move wins over the still-hovered cell");

            // A second arrow press keeps winning — the pointer still has not moved.
            window.press("right", cx);
            assert_eq!(preview(cx), Some(candidates[1].clone()), "still the keyboard's cell, not the hovered one");

            // The pointer actually moves, onto a different cell: hover hands the preview back.
            window.hover(("proof-cell", 3u64), cx);
            assert_eq!(preview(cx), Some(candidates[3].clone()), "a real hover change is back in charge");
        })
        .unwrap();
    }

    /// An edit render that was no longer wanted when it finished is dropped, never shown.
    #[gpui_kit::test]
    fn an_unwanted_edit_render_is_dropped(cx: &mut TestAppContext) {
        let pool = Arc::new(FakePool::default());
        let renders = cx.update(|cx| {
            let pool: Arc<dyn crate::image_store::Submit> = pool.clone();
            cx.new(|cx| EditRenders::new(pool, cx))
        });
        let a = crate::loupe::edit_renders::preview_job(1, "{}", 320, false, 0);
        let b = crate::loupe::edit_renders::preview_job(1, "{\"ev\":1}", 320, false, 0);
        renders.update(cx, |r, cx| r.want(&[a.clone()], cx));
        pool.start(JobKey::Edit(a.clone())); // running: cannot be cancelled
        renders.update(cx, |r, cx| r.want(&[b.clone()], cx));
        pool.finish(&JobKey::Edit(a.clone()), Ok(pixels(4, 4)));
        cx.run_until_parked();
        renders.read_with(cx, |r, _| {
            assert!(matches!(r.get(&a), RenderState::Absent));
            assert!(matches!(r.get(&b), RenderState::Rendering));
            assert_eq!(r.stale_dropped(), 1);
        });
        pool.finish(&JobKey::Edit(b.clone()), Ok(pixels(4, 4)));
        cx.run_until_parked();
        renders.read_with(cx, |r, _| assert!(matches!(r.get(&b), RenderState::Ready(_))));
    }

    /// The loupe shows the active version's render instead of the preview, and asks for its
    /// hi-res render on the first zoom-in.
    #[gpui_kit::test]
    fn the_loupe_shows_the_active_versions_render(cx: &mut TestAppContext) {
        let (app, pool, _dir, ids) = app_with(2, "loupe-version", cx);
        let version_id = {
            let guard = app.state.catalog.lock().unwrap();
            let c = guard.as_ref().unwrap();
            let v = c.create_version(ids[0], "Warm").unwrap();
            c.set_version_edit(v, "{\"ev\":0.5}").unwrap();
            v
        };
        select(&app, ids[0], cx);
        press(&app, "enter", cx);
        let version = {
            let guard = app.state.catalog.lock().unwrap();
            guard.as_ref().unwrap().list_versions(ids[0]).unwrap().into_iter().find(|v| v.id == version_id).unwrap()
        };
        app.wired.shell.update(cx, |s, cx| s.set_active_version(Some(version), cx));
        cx.run_until_parked();
        let lo = edits_ever(&pool).into_iter().find(|j| j.max_edge == 2560).expect("the fit render was asked for");
        assert_eq!((lo.photo_id, lo.edit_json.as_str(), lo.hi_res), (ids[0], "{\"ev\":0.5}", false));
        pool.finish(&preview(ids[0]), Ok(pixels(30, 20)));
        pool.finish(&JobKey::Edit(lo.clone()), Ok(pixels(40, 20)));
        cx.run_until_parked();
        assert_eq!(drawn(&app, cx), Some((ids[0], Drawn::OverrideLo)));
        wheel(&app, "loupe-image", true, cx);
        let hi = edits_ever(&pool).into_iter().find(|j| j.hi_res).expect("the hi-res render on zoom-in");
        assert_eq!(hi.max_edge, 0);
        // Another photo: no version, the preview again.
        press(&app, "right", cx);
        pool.finish(&preview(ids[1]), Ok(pixels(30, 20)));
        cx.run_until_parked();
        assert_eq!(drawn(&app, cx), Some((ids[1], Drawn::Preview)));
    }

    fn edits_ever(pool: &FakePool) -> Vec<chairphoto_core::image_pool::EditJob> {
        pool.batches
            .lock()
            .unwrap()
            .iter()
            .flatten()
            .filter_map(|k| match k {
                JobKey::Edit(job) => Some(job.clone()),
                _ => None,
            })
            .collect()
    }
}
