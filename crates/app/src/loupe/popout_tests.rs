//! Headless tests of the pop-out loupe window (#110): a second window over the same entities,
//! through the real wiring, with the decode pool answered by hand.

use crate::image_tests::{pixels, FakePool};
use crate::loupe::window;
use crate::loupe::zoom::{Drawn, ZoomImage};
use crate::modules::dev_module::DEV_MODULE_ID;
use crate::modules::ModuleRegistry;
use crate::shell::state::StageView;
use crate::tests::{
    colliding_catalog, core_switch, deliver_switch, open_catalog_with_photos, press, start_with_pool, App, TempDir,
};
use crate::{QuitReason, QuitRequested};
use chairphoto_core::catalog::PickState;
use chairphoto_core::image_pool::{ImageKind, JobKey};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    point, AnyWindowHandle, AppContext as _, Entity, InputEvent as _, Modifiers, ScrollDelta, ScrollWheelEvent, TestAppContext,
    TouchPhase,
};
use std::sync::Arc;

fn app_with(n: usize, tag: &str, cx: &mut TestAppContext) -> (App, Arc<FakePool>, TempDir, Vec<i64>) {
    let dir = TempDir::new(tag);
    let pool = Arc::new(FakePool::default());
    let app = start_with_pool(cx, pool.clone());
    let ids = open_catalog_with_photos(&app, &dir, n, cx);
    (app, pool, dir, ids)
}

fn select(app: &App, id: i64, cx: &mut TestAppContext) {
    app.wired.shell.update(cx, |s, cx| s.select_with(cx, |l| l.select_single(id)));
    cx.run_until_parked();
}

fn stage(app: &App, cx: &mut TestAppContext) -> StageView {
    app.wired.shell.read_with(cx, |s, _| s.stage_view())
}

fn active(app: &App, cx: &mut TestAppContext) -> Option<i64> {
    app.wired.shell.read_with(cx, |s, _| s.library.selection().active_id)
}

fn culling(app: &App, id: i64) -> (i64, PickState) {
    let guard = app.state.catalog.lock().unwrap();
    let p = guard.as_ref().unwrap().get_photo(id).unwrap();
    (p.rating, p.pick_state)
}

fn preview(id: i64) -> JobKey {
    JobKey::photo(id, ImageKind::Preview)
}

fn zoom_key(id: i64) -> JobKey {
    JobKey::photo(id, ImageKind::Zoom)
}

fn cached(app: &App, id: i64, kind: ImageKind, cx: &mut TestAppContext) -> bool {
    app.wired.images.read_with(cx, |s, _| s.lru().peek(&s.key(id, kind)).is_some())
}

fn pending(app: &App, id: i64, cx: &mut TestAppContext) -> bool {
    app.wired.images.read_with(cx, |s, _| s.is_pending(id, ImageKind::Preview))
}

fn render_main(app: &App, cx: &mut TestAppContext) {
    cx.update_window(app.window(), |_, window, cx| window.render_frame(cx)).unwrap();
    cx.run_until_parked();
}

fn open(cx: &mut TestAppContext) -> AnyWindowHandle {
    cx.update(window::open);
    cx.run_until_parked();
    cx.update(|cx| window::handle(cx)).expect("the pop-out opened")
}

fn close(cx: &mut TestAppContext) {
    cx.update(window::close);
    cx.run_until_parked();
}

fn is_open(cx: &mut TestAppContext) -> bool {
    cx.update(|cx| window::handle(cx)).is_some()
}

fn popout_zoom(cx: &mut TestAppContext) -> Entity<ZoomImage> {
    let view = cx.update(|cx| window::view(cx)).expect("open");
    view.read_with(cx, |v, cx| v.loupe().read(cx).zoom().clone())
}

fn render_popout(h: AnyWindowHandle, cx: &mut TestAppContext) {
    cx.update_window(h, |_, window, cx| window.render_frame(cx)).unwrap();
    cx.run_until_parked();
}

/// The photo the pop-out shows, after a frame.
fn shown(h: AnyWindowHandle, cx: &mut TestAppContext) -> Option<i64> {
    render_popout(h, cx);
    popout_zoom(cx).read_with(cx, |z, _| z.photo())
}

fn press_in(h: AnyWindowHandle, key: &str, cx: &mut TestAppContext) {
    cx.update_window(h, |_, window, cx| {
        window.render_frame(cx);
        window.press(key, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn present_in(h: AnyWindowHandle, id: &'static str, cx: &mut TestAppContext) -> bool {
    let mut out = false;
    cx.update_window(h, |_, window, cx| {
        window.render_frame(cx);
        out = window.try_find(id).is_some();
    })
    .unwrap();
    out
}

fn windows(cx: &mut TestAppContext) -> usize {
    cx.update(|cx| cx.windows().len())
}

fn quit_requested(cx: &mut TestAppContext) -> Option<QuitRequested> {
    cx.update(|cx| cx.try_global::<QuitRequested>().copied())
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

/// One pop-out at a time; it follows the selection, and Compare's focused pane, whatever the
/// main stage shows; closing it leaves the app running, and it opens again. Closing the main
/// window still quits with the pop-out open.
#[gpui_kit::test]
fn the_pop_out_opens_once_follows_the_target_and_closes_without_quitting(cx: &mut TestAppContext) {
    let (app, pool, _dir, ids) = app_with(6, "pop-follow", cx);
    let h = open(cx);
    assert_eq!(windows(cx), 2);
    assert_eq!(open(cx), h, "a second open brings the same window forward");
    assert_eq!(windows(cx), 2);
    assert!(present_in(h, "loupe", cx));
    assert_eq!(shown(h, cx), None, "nothing selected");

    select(&app, ids[2], cx);
    assert_eq!(stage(&app, cx), StageView::Grid, "the main window stays on the grid");
    assert_eq!(shown(h, cx), Some(ids[2]));
    assert_eq!(last_previews(&pool).first(), Some(&ids[2]), "the target first");
    assert!(!present_in(h, "loupe-back", cx), "no grid to go back to");
    select(&app, ids[4], cx);
    assert_eq!(shown(h, cx), Some(ids[4]));

    // Compare: the focused pane, not the active photo.
    app.wired.shell.update(cx, |s, cx| s.select_with(cx, |l| l.select_all()));
    cx.run_until_parked();
    press(&app, "c", cx);
    assert_eq!(stage(&app, cx), StageView::Compare);
    let focused = app.wired.shell.read_with(cx, |s, _| s.compare_focused()).unwrap();
    assert_eq!(shown(h, cx), Some(focused));
    press(&app, "down", cx);
    let next = app.wired.shell.read_with(cx, |s, _| s.compare_focused()).unwrap();
    assert_ne!(next, focused);
    assert_eq!(shown(h, cx), Some(next), "follows the focus");
    press(&app, "escape", cx);

    close(cx);
    assert!(!is_open(cx));
    assert_eq!(windows(cx), 1);
    assert_eq!(quit_requested(cx), None, "the pop-out is not the main window");
    select(&app, ids[1], cx);
    press(&app, "enter", cx);
    assert_eq!(stage(&app, cx), StageView::Loupe, "the main window still has its keys");
    let again = open(cx);
    assert_ne!(again, h);
    assert_eq!(shown(again, cx), Some(ids[1]), "a new pop-out reads the target at once");

    cx.update_window(app.window(), |_, window, _| window.remove_window()).unwrap();
    cx.run_until_parked();
    assert_eq!(quit_requested(cx), Some(QuitRequested(QuitReason::MainWindowClosed)));
}

/// Keys in the pop-out: the arrows step the shared selection, the culling keys mark the photo
/// shown and advance, Ctrl+A selects the view; Enter/Esc and C leave the main stage alone.
#[gpui_kit::test]
fn keys_in_the_pop_out_step_and_mark(cx: &mut TestAppContext) {
    let (app, _pool, _dir, ids) = app_with(4, "pop-keys", cx);
    select(&app, ids[0], cx);
    let h = open(cx);
    press_in(h, "right", cx);
    assert_eq!(active(&app, cx), Some(ids[1]));
    press_in(h, "p", cx);
    assert_eq!(culling(&app, ids[1]).1, PickState::Pick);
    assert_eq!(active(&app, cx), Some(ids[2]), "advanced");
    assert_eq!(shown(h, cx), Some(ids[2]));
    press_in(h, "3", cx);
    assert_eq!(culling(&app, ids[2]).0, 3);
    // With the inline loupe on in the main window, the pop-out's Enter/Esc leave it on.
    press(&app, "enter", cx);
    assert_eq!(stage(&app, cx), StageView::Loupe);
    for key in ["escape", "enter"] {
        press_in(h, key, cx);
        assert_eq!(stage(&app, cx), StageView::Loupe, "{key}: the main stage is untouched");
        assert!(is_open(cx), "{key}: the pop-out stays");
    }
    press_in(h, "ctrl-a", cx);
    assert_eq!(app.wired.shell.read_with(cx, |s, _| s.library.selection().ids.len()), 4);
    press_in(h, "c", cx);
    assert_eq!(stage(&app, cx), StageView::Loupe, "C opens no Compare from the pop-out");
}

/// Closing the pop-out releases what it alone wanted — its pending preloads and its loaded
/// full-resolution tier — and leaves what the inline loupe still wants; the inline loupe
/// closing leaves the pop-out's.
#[gpui_kit::test]
fn closing_the_pop_out_releases_only_its_own_images(cx: &mut TestAppContext) {
    let (app, pool, _dir, ids) = app_with(8, "pop-release", cx);
    select(&app, ids[3], cx);
    let h = open(cx);
    pool.finish(&preview(ids[3]), Ok(pixels(300, 200)));
    cx.run_until_parked();
    cx.update_window(h, |_, window, cx| {
        window.render_frame(cx);
        let position = window.find("loupe-image").bounds().center();
        let event = ScrollWheelEvent {
            position,
            delta: ScrollDelta::Lines(point(0., 1.)),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
        };
        window.dispatch_event(event.to_platform_input(), cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(pool.batches.lock().unwrap().iter().any(|b| b.contains(&zoom_key(ids[3]))), "the pop-out zoomed in");
    pool.finish(&zoom_key(ids[3]), Ok(pixels(3000, 2000)));
    cx.run_until_parked();
    assert!(cached(&app, ids[3], ImageKind::Zoom, cx));
    assert!(pending(&app, ids[4], cx), "a preload in flight");

    close(cx);
    assert!(!cached(&app, ids[3], ImageKind::Zoom, cx), "its zoom tier is released");
    let cancelled = pool.cancelled.lock().unwrap().clone();
    for i in [2, 4, 5] {
        assert!(cancelled.contains(&preview(ids[i])), "preload {i} released: {cancelled:?}");
    }
    assert!(cached(&app, ids[3], ImageKind::Preview, cx), "the preview stays cached");

    // The inline loupe on the same photo keeps its preloads when the pop-out closes …
    pool.cancelled.lock().unwrap().clear();
    select(&app, ids[6], cx);
    press(&app, "enter", cx);
    assert_eq!(stage(&app, cx), StageView::Loupe);
    open(cx);
    close(cx);
    assert!(pool.cancelled.lock().unwrap().is_empty(), "{:?}", pool.cancelled.lock().unwrap());
    for i in [5, 6, 7] {
        assert!(pending(&app, ids[i], cx), "preload {i}");
    }
    // … and the pop-out keeps its own when the inline loupe closes.
    open(cx);
    press(&app, "escape", cx);
    assert_eq!(stage(&app, cx), StageView::Grid);
    assert!(pool.cancelled.lock().unwrap().is_empty(), "{:?}", pool.cancelled.lock().unwrap());
}

/// A catalog switch to colliding ids leaves the pop-out open and empty; a photo of the new
/// catalog is then asked for afresh, never drawn from the old catalog's pixels.
#[gpui_kit::test]
fn a_catalog_switch_empties_the_pop_out(cx: &mut TestAppContext) {
    let (app, pool, dir, ids) = app_with(3, "pop-switch", cx);
    select(&app, ids[1], cx);
    let h = open(cx);
    pool.finish(&preview(ids[1]), Ok(pixels(300, 200)));
    cx.run_until_parked();
    render_popout(h, cx);
    assert_eq!(popout_zoom(cx).read_with(cx, |z, _| z.drawn()), Some((ids[1], Drawn::Preview)));

    let (other, new_ids) = colliding_catalog(&dir, "other", 3);
    assert_eq!(new_ids, ids, "ids collide");
    core_switch(&app, other);
    deliver_switch(&app, cx);
    assert_eq!(shown(h, cx), None, "No photo selected");
    assert!(is_open(cx), "still open");

    let before = pool.batches.lock().unwrap().len();
    select(&app, new_ids[1], cx);
    assert_eq!(shown(h, cx), Some(new_ids[1]));
    assert_eq!(popout_zoom(cx).read_with(cx, |z, _| z.drawn()), None, "not the old catalog's pixels");
    let asked = pool.batches.lock().unwrap()[before..].iter().any(|b| b.contains(&preview(new_ids[1])));
    assert!(asked, "the new catalog's photo is asked for afresh");
}

/// Loupe-slot module panels: the pop-out mounts its own view of each, which goes when it
/// closes while the main window's stays cached.
#[gpui_kit::test]
fn module_panels_are_per_window_and_go_with_the_pop_out(cx: &mut TestAppContext) {
    let (app, _pool, _dir, ids) = app_with(2, "pop-modules", cx);
    let registry = app.wired.modules.clone();
    cx.update(|cx| ModuleRegistry::enable(&registry, DEV_MODULE_ID, cx));
    select(&app, ids[0], cx);
    press(&app, "enter", cx);
    render_main(&app, cx);
    let main_views = registry.read_with(cx, |r, _| r.cached_view_count());
    let h = open(cx);
    assert!(present_in(h, "dev-loupe", cx), "the pop-out mounts the loupe slot");
    // Each window's loupe-slot panels find the image they draw over (`loupe_image`), with no
    // reach into a root view: the inline loupe's in the main window, the pop-out's there.
    let image_in = |w: AnyWindowHandle, cx: &mut TestAppContext| {
        cx.update_window(w, |_, window, cx| crate::loupe::view::loupe_image(window, cx).map(|z| z.entity_id()))
            .unwrap()
    };
    let inline = app.wired.root.clone().unwrap().read_with(cx, |r, cx| r.loupe().read(cx).zoom().entity_id());
    let popped = popout_zoom(cx).entity_id();
    assert_ne!(inline, popped);
    assert_eq!(image_in(app.window(), cx), Some(inline));
    assert_eq!(image_in(h, cx), Some(popped));
    assert_eq!(registry.read_with(cx, |r, _| r.cached_view_count()), main_views + 1, "a view of its own");
    close(cx);
    assert_eq!(registry.read_with(cx, |r, _| r.cached_view_count()), main_views, "dropped with the window");
    render_main(&app, cx);
    assert_eq!(registry.read_with(cx, |r, _| r.cached_view_count()), main_views, "the main window's is still cached");
}

/// The Darkroom's print (DarkroomView.tsx's "Loupe print"): while one is up the pop-out —
/// not the inline loupe — shows that record rendered from the print's own pixels; taking it
/// down returns the pop-out to the target and drops the render; a catalog switch takes it down.
#[cfg(feature = "edit")]
#[gpui_kit::test]
fn the_pop_out_shows_the_darkrooms_print(cx: &mut TestAppContext) {
    use crate::shell::state::LoupePrint;
    use chairphoto_core::plugins::edit::SourceToken;
    let (app, pool, _dir, ids) = app_with(3, "pop-print", cx);
    select(&app, ids[0], cx);
    press(&app, "enter", cx);
    let h = open(cx);
    let photo = app.wired.shell.read_with(cx, |s, _| s.library.photos().iter().find(|p| p.id == ids[1]).cloned()).unwrap();
    let source = SourceToken::Working { photo_id: ids[1], generation: 7 };
    let print = LoupePrint { photo, edit_json: "{\"ev\":1}".into(), source: source.clone() };
    app.wired.shell.update(cx, |s, cx| s.set_loupe_print(Some(print), cx));
    cx.run_until_parked();
    assert_eq!(shown(h, cx), Some(ids[1]), "the print's photo");
    let edits: Vec<_> = pool
        .batches
        .lock()
        .unwrap()
        .iter()
        .flatten()
        .filter_map(|k| match k {
            JobKey::Edit(job) => Some(job.clone()),
            _ => None,
        })
        .collect();
    let lo = edits.into_iter().find(|j| j.max_edge == 2560).expect("the print's render was asked for");
    assert_eq!((lo.photo_id, lo.edit_json.as_str(), &lo.source), (ids[1], "{\"ev\":1}", &source));
    pool.finish(&JobKey::Edit(lo.clone()), Ok(pixels(40, 20)));
    cx.run_until_parked();
    render_popout(h, cx);
    assert_eq!(popout_zoom(cx).read_with(cx, |z, _| z.drawn()), Some((ids[1], Drawn::OverrideLo)));
    let inline = app.wired.root.clone().unwrap().read_with(cx, |r, cx| r.loupe().read(cx).zoom().read(cx).photo());
    assert_eq!(inline, Some(ids[0]), "the inline loupe keeps the target");

    app.wired.shell.update(cx, |s, cx| s.set_loupe_print(None, cx));
    cx.run_until_parked();
    assert_eq!(shown(h, cx), Some(ids[0]), "the target again");
    assert_eq!(popout_zoom(cx).read_with(cx, |z, _| z.drawn()), None, "not the print's render");

    let photo = app.wired.shell.read_with(cx, |s, _| s.library.photos()[1].clone());
    app.wired.shell.update(cx, |s, cx| s.set_loupe_print(Some(LoupePrint { photo, edit_json: "{}".into(), source }), cx));
    cx.run_until_parked();
    deliver_switch(&app, cx);
    assert!(app.wired.shell.read_with(cx, |s, _| s.loupe_print().is_none()), "a switch takes it down");
    assert_eq!(shown(h, cx), None);
}
