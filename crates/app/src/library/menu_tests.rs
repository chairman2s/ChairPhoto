//! Headless tests of the grid's right-click menu and the loupe's unavailable-state actions
//! (#158) through the real wiring: each command's effect on the catalog and the files, the
//! Remove confirm, enablement for unavailable photos and multi-selections, and catalog
//! switches (to a catalog whose photo ids collide) with and without `catalog:switched`
//! while the menu, the confirm, the file picker or a job is open.

use crate::image_tests::FakePool;
use crate::library::grid::LibraryView;
use crate::library::grid_menu::GridMenu;
use crate::library::photo_actions::{remove_confirm_body, SystemRevealer};
use crate::shell::state::StageView;
use crate::storage::Runner;
use super::storage_tests::{has_dialog, settle, work};
use crate::tests::{click, core_switch, deliver_switch, press, start_with_pool, status, App, TempDir};
use chairphoto_core::app::{AppState, CoreEvent, EventSink as _, CATALOG_CHANGED};
use chairphoto_core::catalog::{Catalog, PickState, StorageStatus, VolumeKind};
use chairphoto_core::image_pool::{ImageKind, JobKey};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AppContext as _, ElementId, Entity, InputEvent as _, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent,
    TestAppContext,
};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

/// A catalog under `dir/<name>` whose `n` photos are real files (`photos/2026/<name><i>.jpg`)
/// with a NAS volume beside them; ids collide across names, as two real catalogs' do.
fn catalog_with_files(dir: &TempDir, name: &str, n: usize) -> (Catalog, Vec<i64>, PathBuf) {
    let base = dir.0.join(name);
    let root = base.join("photos");
    std::fs::create_dir_all(root.join("2026")).unwrap();
    std::fs::create_dir_all(base.join("nas")).unwrap();
    let c = Catalog::open(&base.join(format!("{name}.chairphoto")), &root).unwrap();
    c.add_volume("NAS", &base.join("nas"), VolumeKind::Backup).unwrap();
    let ids = (0..n)
        .map(|i| {
            let f = root.join(format!("2026/{name}{i}.jpg"));
            std::fs::write(&f, format!("{name} bytes {i}")).unwrap();
            c.upsert_photo(&f, None, 10, 12).unwrap().id
        })
        .collect();
    (c, ids, root)
}

struct Rig {
    app: App,
    pool: Arc<FakePool>,
    dir: TempDir,
    ids: Vec<i64>,
    root: PathBuf,
}

/// The app on catalog "a" with `n` file-backed photos.
fn rig(tag: &str, n: usize, cx: &mut TestAppContext) -> Rig {
    let dir = TempDir::new(tag);
    let pool = Arc::new(FakePool::default());
    let app = start_with_pool(cx, pool.clone());
    let (catalog, ids, root) = catalog_with_files(&dir, "a", n);
    *app.state.catalog.lock().unwrap() = Some(catalog);
    app.state.send(CoreEvent::CatalogSwitched("a.chairphoto".into()));
    cx.run_until_parked();
    render(&app, cx);
    // Startup's own storage jobs (the reconcile check) run now, so a test sees only its own.
    work(cx);
    Rig { app, pool, dir, ids, root }
}

fn render(app: &App, cx: &mut TestAppContext) {
    for _ in 0..2 {
        cx.update_window(app.window(), |_, window, cx| window.render_frame(cx)).unwrap();
        cx.run_until_parked();
    }
}

fn grid(app: &App, cx: &mut TestAppContext) -> Entity<LibraryView> {
    app.wired.root.as_ref().expect("the main window opened").read_with(cx, |root, _| root.library.clone())
}

fn menu(app: &App, cx: &mut TestAppContext) -> Option<GridMenu> {
    grid(app, cx).read_with(cx, |g, _| g.menu().cloned())
}

fn tile(id: i64) -> ElementId {
    ("tile", id as u64).into()
}

fn mouse(app: &App, id: i64, button: MouseButton, modifiers: Modifiers, cx: &mut TestAppContext) {
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        let position = window.find(tile(id)).bounds().center();
        for event in [
            MouseDownEvent { button, position, modifiers, click_count: 1, first_mouse: false }.to_platform_input(),
            MouseUpEvent { button, position, modifiers, click_count: 1 }.to_platform_input(),
        ] {
            window.dispatch_event(event, cx);
        }
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn right_click(app: &App, id: i64, cx: &mut TestAppContext) {
    mouse(app, id, MouseButton::Right, Modifiers::default(), cx);
}

fn ctrl_click(app: &App, id: i64, cx: &mut TestAppContext) {
    mouse(app, id, MouseButton::Left, Modifiers { control: true, ..Default::default() }, cx);
}

fn selection(app: &App, cx: &mut TestAppContext) -> Vec<i64> {
    app.wired.shell.read_with(cx, |s, _| s.library.selection().ids.to_vec())
}

fn rows(app: &App, cx: &mut TestAppContext) -> Vec<i64> {
    app.wired.shell.read_with(cx, |s, _| s.library.photo_ids())
}

fn with_catalog<T>(app: &App, f: impl FnOnce(&Catalog) -> T) -> T {
    f(app.state.catalog.lock().unwrap().as_ref().unwrap())
}

fn trashed(app: &App) -> Vec<i64> {
    let mut ids: Vec<i64> = with_catalog(app, |c| c.list_trash().unwrap()).iter().map(|p| p.id).collect();
    ids.sort();
    ids
}

fn exists(app: &App, id: i64) -> bool {
    with_catalog(app, |c| c.get_photo(id).is_ok())
}

/// Wait for the menu's header to show `id`'s storage state (read per visible window).
fn status_of(app: &App, id: i64, cx: &mut TestAppContext) -> Option<StorageStatus> {
    render(app, cx);
    app.wired.shell.read_with(cx, |s, _| s.library.statuses().get(&id).copied())
}

/// The catalog's state was read from `state` by another test helper: swap in `catalog`
/// through the core's two-phase switch, delivering `catalog:switched` when `delivered`.
fn switch_to(app: &App, catalog: Catalog, delivered: bool, cx: &mut TestAppContext) {
    core_switch(app, catalog);
    if delivered {
        deliver_switch(app, cx);
        render(app, cx);
    }
}

fn record_reveals(cx: &mut TestAppContext) -> Rc<RefCell<Vec<PathBuf>>> {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let s = seen.clone();
    cx.update(|cx| cx.set_global(SystemRevealer(Rc::new(move |p: &Path, _| s.borrow_mut().push(p.to_path_buf())))));
    seen
}

fn back_up(state: &AppState, id: i64) {
    chairphoto_core::app::storage::backup_photo(state, id).unwrap();
}

// --- the menu ---------------------------------------------------------------------------

/// Right-click on an unselected tile selects it alone and opens the menu on it, with the
/// file name and its storage state; Escape closes it, and so does a click outside it.
#[gpui_kit::test]
fn right_click_opens_the_menu_on_the_tile(cx: &mut TestAppContext) {
    let r = rig("menu-open", 3, cx);
    let app = &r.app;
    right_click(app, r.ids[1], cx);
    let m = menu(app, cx).expect("the menu opened");
    assert_eq!((m.photo, m.name.as_ref(), m.trash.clone()), (r.ids[1], "a1.jpg", vec![r.ids[1]]));
    assert_eq!(selection(app, cx), vec![r.ids[1]], "the right-clicked tile is selected");
    assert_eq!(status_of(app, r.ids[1], cx), Some(StorageStatus::LocalOnly));
    cx.update_window(app.window(), |_, window, _| {
        assert_eq!(window.find("grid-menu-status").label(), Some("On local disk"));
        assert_eq!(window.find("grid-menu-trash").label(), Some("Move to trash"));
    })
    .unwrap();

    press(app, "escape", cx);
    assert!(menu(app, cx).is_none(), "Escape closes the menu");

    right_click(app, r.ids[1], cx);
    assert!(menu(app, cx).is_some());
    mouse(app, r.ids[0], MouseButton::Left, Modifiers::default(), cx);
    assert!(menu(app, cx).is_none(), "a click outside closes the menu");
    assert_eq!(trashed(app), Vec::<i64>::new(), "closing it did nothing");
}

/// Move to trash takes the selection when the clicked tile is part of it (a right-click
/// there keeps the selection), else only the clicked tile, which then becomes the selection.
/// The photos leave the grid, the trash count follows, and nothing on disk changes.
#[gpui_kit::test]
fn move_to_trash_takes_the_selection_or_the_clicked_tile(cx: &mut TestAppContext) {
    let r = rig("menu-trash", 5, cx);
    let app = &r.app;
    let ids = &r.ids;
    mouse(app, ids[0], MouseButton::Left, Modifiers::default(), cx);
    ctrl_click(app, ids[1], cx);
    ctrl_click(app, ids[2], cx);
    right_click(app, ids[1], cx);
    assert_eq!(selection(app, cx), vec![ids[0], ids[1], ids[2]], "the selection is kept");
    assert_eq!(menu(app, cx).unwrap().trash, vec![ids[0], ids[1], ids[2]]);
    cx.update_window(app.window(), |_, window, _| {
        assert_eq!(window.find("grid-menu-trash").label(), Some("Move 3 photos to trash"));
    })
    .unwrap();
    click(app, "grid-menu-trash", cx);
    assert!(menu(app, cx).is_none(), "choosing closes the menu");
    assert_eq!(trashed(app), vec![ids[0], ids[1], ids[2]]);
    assert_eq!(status(app, cx), "Moved 3 to the trash.");
    render(app, cx);
    assert_eq!(rows(app, cx), vec![ids[3], ids[4]], "the grid re-read without them");
    assert_eq!(app.wired.shell.read_with(cx, |s, _| s.counts.trash), Some(3));

    // Outside the selection: only the clicked tile.
    mouse(app, ids[3], MouseButton::Left, Modifiers::default(), cx);
    right_click(app, ids[4], cx);
    assert_eq!(selection(app, cx), vec![ids[4]], "the clicked tile became the selection");
    click(app, "grid-menu-trash", cx);
    assert_eq!(trashed(app), vec![ids[0], ids[1], ids[2], ids[4]]);
    assert_eq!(status(app, cx), "Moved 1 to the trash.");
    for i in 0..5 {
        assert!(r.root.join(format!("2026/a{i}.jpg")).exists(), "trashing touched no file");
    }
}

/// Reveal in Files hands the resolved file to the file manager; an unreachable original says
/// so and reveals nothing.
#[gpui_kit::test]
fn reveal_in_files_resolves_the_copy_or_says_it_is_offline(cx: &mut TestAppContext) {
    let r = rig("menu-reveal", 2, cx);
    let app = &r.app;
    let seen = record_reveals(cx);
    right_click(app, r.ids[0], cx);
    click(app, "grid-menu-reveal", cx);
    assert_eq!(*seen.borrow(), vec![r.root.join("2026/a0.jpg")]);

    std::fs::remove_file(r.root.join("2026/a1.jpg")).unwrap();
    right_click(app, r.ids[1], cx);
    click(app, "grid-menu-reveal", cx);
    assert_eq!(seen.borrow().len(), 1, "nothing to reveal");
    let line = status(app, cx);
    assert!(line.starts_with("Couldn't reveal: ") && line.ends_with("(the file may be offline)"), "{line}");
}

/// Relocate… asks for the file; cancelling changes nothing; choosing the moved file re-points
/// the photo at it, binds its sidecar to the photo's UUID and drops its cached images.
#[gpui_kit::test]
fn relocate_points_the_photo_at_the_chosen_file(cx: &mut TestAppContext) {
    let r = rig("menu-relocate", 2, cx);
    let app = &r.app;
    let moved = r.root.join("moved/a0.jpg");
    std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
    std::fs::rename(r.root.join("2026/a0.jpg"), &moved).unwrap();

    right_click(app, r.ids[0], cx);
    click(app, "grid-menu-relocate", cx);
    assert!(cx.did_prompt_for_paths(), "the file picker opened");
    cx.simulate_path_prompt_response(|_| None);
    cx.run_until_parked();
    assert_eq!(work(cx), 0, "a cancelled pick starts nothing");
    assert_eq!(with_catalog(app, |c| c.get_photo(r.ids[0]).unwrap().path), "2026/a0.jpg");

    // A tile image cached before, to see it dropped.
    let pool = r.pool.clone();
    pool.finish(&JobKey::photo(r.ids[0], ImageKind::Thumb), Ok(crate::image_tests::pixels(4, 4)));
    cx.run_until_parked();
    let images = app.wired.images.clone();
    assert!(images.read_with(cx, |s, _| s.lru().len()) > 0);

    right_click(app, r.ids[0], cx);
    click(app, "grid-menu-relocate", cx);
    let m = moved.clone();
    cx.simulate_path_prompt_response(move |_| Some(vec![m]));
    cx.run_until_parked();
    assert_eq!(work(cx), 1, "the relocation ran on the storage runner");
    assert_eq!(status(app, cx), "Photo relocated to its new file.");
    let photo = with_catalog(app, |c| c.get_photo(r.ids[0]).unwrap());
    assert_eq!(photo.path, "moved/a0.jpg");
    assert_eq!(chairphoto_core::xmp::read_identifier(&moved).as_deref(), Some(photo.uuid.as_str()));
    assert!(
        images.read_with(cx, |s, _| s.lru().peek(&s.key(r.ids[0], ImageKind::Thumb)).is_none()),
        "the old tile image was dropped"
    );

    // A file outside the library root is refused.
    let outside = r.dir.0.join("elsewhere.jpg");
    std::fs::write(&outside, "x").unwrap();
    right_click(app, r.ids[1], cx);
    click(app, "grid-menu-relocate", cx);
    cx.simulate_path_prompt_response(move |_| Some(vec![outside]));
    cx.run_until_parked();
    work(cx);
    assert!(status(app, cx).starts_with("Couldn't relocate: "), "{}", status(app, cx));
    assert_eq!(with_catalog(app, |c| c.get_photo(r.ids[1]).unwrap().path), "2026/a1.jpg");
}

/// Retrieve from NAS is offered only when a backup may exist: disabled for a local-only photo
/// (a click does nothing), enabled for a backed-up one, whose deleted original it copies back.
#[gpui_kit::test]
fn retrieve_from_nas_is_enabled_only_with_a_backup_and_copies_it_back(cx: &mut TestAppContext) {
    let r = rig("menu-retrieve", 2, cx);
    let app = &r.app;
    right_click(app, r.ids[0], cx);
    assert_eq!(status_of(app, r.ids[0], cx), Some(StorageStatus::LocalOnly));
    let before = status(app, cx);
    click(app, "grid-menu-retrieve", cx);
    assert!(menu(app, cx).is_some(), "the disabled row takes no click");
    assert_eq!(cx.update(|cx| Runner::get(cx).pending()), 0, "nothing was started");
    assert_eq!(status(app, cx), before);
    press(app, "escape", cx);

    back_up(&app.state, r.ids[1]);
    let original = r.root.join("2026/a1.jpg");
    std::fs::remove_file(&original).unwrap();
    app.wired.shell.update(cx, |s, cx| s.refresh_rows(cx));
    cx.run_until_parked();
    right_click(app, r.ids[1], cx);
    assert_eq!(status_of(app, r.ids[1], cx), Some(StorageStatus::BackedUp));
    click(app, "grid-menu-retrieve", cx);
    assert_eq!(status(app, cx), "Retrieving from NAS…");
    assert_eq!(work(cx), 1);
    assert_eq!(status(app, cx), "Retrieved from NAS.");
    assert_eq!(std::fs::read_to_string(&original).unwrap(), "a bytes 1", "the original is back");
}

/// Remove from catalog asks first. Cancel keeps the photo; OK deletes its catalog row and
/// never the file.
#[gpui_kit::test]
fn remove_from_catalog_asks_first_and_never_deletes_the_file(cx: &mut TestAppContext) {
    let r = rig("menu-remove", 2, cx);
    let app = &r.app;
    right_click(app, r.ids[0], cx);
    click(app, "grid-menu-remove", cx);
    settle(app, cx);
    assert!(has_dialog(app, cx), "the confirm is open");
    click(app, "cancel", cx);
    assert!(!has_dialog(app, cx));
    assert!(exists(app, r.ids[0]), "Cancel kept the photo");

    right_click(app, r.ids[0], cx);
    click(app, "grid-menu-remove", cx);
    settle(app, cx);
    click(app, "ok", cx);
    assert!(!exists(app, r.ids[0]), "OK removed the catalog row");
    assert!(r.root.join("2026/a0.jpg").exists(), "the file is untouched");
    assert_eq!(status(app, cx), "Removed from catalog (files left untouched).");
    render(app, cx);
    assert_eq!(rows(app, cx), vec![r.ids[1]]);
    assert_eq!(remove_confirm_body("a0.jpg"), "Remove \"a0.jpg\" from the catalog? This deletes its catalog entry (tags, rating, versions) but never deletes the file on disk or the NAS.");
}

// --- catalog switches -------------------------------------------------------------------

/// A menu opened over catalog A, then the core switches to B (colliding ids) and
/// `catalog:switched` has **not** arrived: every row it sends fails closed, and B's photo with
/// the same id is neither trashed, removed nor relocated. Once the event arrives the menu
/// closes.
#[gpui_kit::test]
fn a_menu_left_open_across_a_switch_never_touches_the_new_catalog(cx: &mut TestAppContext) {
    let r = rig("menu-switch", 2, cx);
    let app = &r.app;
    let (b, b_ids, b_root) = catalog_with_files(&r.dir, "b", 2);
    assert_eq!(b_ids, r.ids, "the ids collide");
    right_click(app, r.ids[0], cx);
    switch_to(app, b, false, cx);

    click(app, "grid-menu-trash", cx);
    assert_eq!(trashed(app), Vec::<i64>::new(), "B's photo was not trashed");
    assert!(status(app, cx).ends_with(CATALOG_CHANGED), "{}", status(app, cx));

    right_click(app, r.ids[0], cx); // the rows on screen are still A's
    click(app, "grid-menu-remove", cx);
    settle(app, cx);
    click(app, "ok", cx);
    assert!(exists(app, b_ids[0]), "B's photo was not removed");
    assert!(status(app, cx).ends_with(CATALOG_CHANGED));

    let seen = record_reveals(cx);
    right_click(app, r.ids[0], cx);
    click(app, "grid-menu-reveal", cx);
    assert!(seen.borrow().is_empty(), "B's file was not revealed");

    let moved = b_root.join("moved.jpg");
    std::fs::write(&moved, "b moved").unwrap();
    right_click(app, r.ids[0], cx);
    click(app, "grid-menu-relocate", cx);
    let m = moved.clone();
    cx.simulate_path_prompt_response(move |_| Some(vec![m]));
    cx.run_until_parked();
    work(cx);
    assert_eq!(with_catalog(app, |c| c.get_photo(b_ids[0]).unwrap().path), "2026/b0.jpg", "B's photo kept its path");
    assert!(status(app, cx).ends_with(CATALOG_CHANGED));

    right_click(app, r.ids[0], cx);
    assert!(menu(app, cx).is_some());
    deliver_switch(app, cx);
    render(app, cx);
    assert!(menu(app, cx).is_none(), "catalog:switched closes the menu");
}

/// The Remove confirm opened over A: with the switch delivered it closes, and B's photo with
/// the same id stays; without the event an OK fails closed (the other test above).
#[gpui_kit::test]
fn a_remove_confirm_closes_when_the_catalog_switches(cx: &mut TestAppContext) {
    let r = rig("menu-confirm-switch", 1, cx);
    let app = &r.app;
    let (b, b_ids, _) = catalog_with_files(&r.dir, "b", 1);
    right_click(app, r.ids[0], cx);
    click(app, "grid-menu-remove", cx);
    settle(app, cx);
    assert!(has_dialog(app, cx));
    switch_to(app, b, true, cx);
    settle(app, cx);
    assert!(!has_dialog(app, cx), "the switch closed the confirm");
    assert!(exists(app, b_ids[0]));
    assert!(exists(app, r.ids[0]), "same id: B's photo");
}

/// A Retrieve started over A whose copy has not run when the core switches to B: the job
/// fails closed and nothing is copied for B's photo with the same id.
#[gpui_kit::test]
fn a_retrieve_queued_across_a_switch_fails_closed(cx: &mut TestAppContext) {
    let r = rig("menu-retrieve-switch", 1, cx);
    let app = &r.app;
    back_up(&app.state, r.ids[0]);
    app.wired.shell.update(cx, |s, cx| s.refresh_rows(cx));
    cx.run_until_parked();
    let (b, b_ids, b_root) = catalog_with_files(&r.dir, "b", 1);
    let b_nas = b.list_volumes().unwrap().into_iter().find(|v| v.kind == VolumeKind::Backup).unwrap().id;
    let b_file = b_root.join("2026/b0.jpg");
    std::fs::copy(&b_file, r.dir.0.join("b/nas/b0.jpg")).unwrap();
    b.add_location(b_ids[0], b_nas, "b0.jpg", chairphoto_core::catalog::LocationRole::Backup).unwrap();
    std::fs::remove_file(&b_file).unwrap();

    right_click(app, r.ids[0], cx);
    assert_eq!(status_of(app, r.ids[0], cx), Some(StorageStatus::BackedUp));
    click(app, "grid-menu-retrieve", cx);
    switch_to(app, b, false, cx);
    work(cx);
    assert!(status(app, cx).ends_with(CATALOG_CHANGED), "{}", status(app, cx));
    assert!(!b_file.exists(), "nothing was restored for B's photo");
}

// --- the loupe's unavailable state ------------------------------------------------------

/// The inline loupe on a photo whose preview failed offers Relocate…, Retrieve from NAS and
/// Remove from catalog, and each acts on that photo — no longer a "not yet ported" line.
#[gpui_kit::test]
fn the_loupes_unavailable_state_runs_the_actions(cx: &mut TestAppContext) {
    let r = rig("loupe-unavailable", 2, cx);
    let app = &r.app;
    back_up(&app.state, r.ids[0]);
    app.wired.shell.update(cx, |s, cx| {
        s.select_with(cx, |l| l.select_single(r.ids[0]));
        s.set_loupe(true, cx);
    });
    cx.run_until_parked();
    render(app, cx);
    assert_eq!(app.wired.shell.read_with(cx, |s, _| s.stage_view()), StageView::Loupe);
    r.pool.finish(&JobKey::photo(r.ids[0], ImageKind::Preview), Err("gone".into()));
    cx.run_until_parked();
    render(app, cx);

    let original = r.root.join("2026/a0.jpg");
    std::fs::remove_file(&original).unwrap();
    click(app, "loupe-retrieve", cx);
    assert_eq!(status(app, cx), "Retrieving from NAS…");
    work(cx);
    assert_eq!(status(app, cx), "Retrieved from NAS.");
    assert!(original.exists());

    std::fs::rename(&original, r.root.join("a0.jpg")).unwrap();
    render(app, cx);
    r.pool.finish(&JobKey::photo(r.ids[0], ImageKind::Preview), Err("gone".into()));
    cx.run_until_parked();
    render(app, cx);
    click(app, "loupe-relocate", cx);
    assert!(cx.did_prompt_for_paths());
    let moved = r.root.join("a0.jpg");
    cx.simulate_path_prompt_response(move |_| Some(vec![moved]));
    cx.run_until_parked();
    work(cx);
    assert_eq!(status(app, cx), "Photo relocated to its new file.");
    assert_eq!(with_catalog(app, |c| c.get_photo(r.ids[0]).unwrap().path), "a0.jpg");

    render(app, cx);
    r.pool.finish(&JobKey::photo(r.ids[0], ImageKind::Preview), Err("gone".into()));
    cx.run_until_parked();
    render(app, cx);
    click(app, "loupe-remove", cx);
    settle(app, cx);
    click(app, "ok", cx);
    assert!(!exists(app, r.ids[0]));
    assert_eq!(status(app, cx), "Removed from catalog (files left untouched).");
    assert!(r.root.join("a0.jpg").exists());
}

// --- the selection the menu and the keys act on (#158 review, M1/M2) ---------------------

fn marks(app: &App, ids: &[i64]) -> Vec<(i64, PickState, String)> {
    ids.iter()
        .map(|&id| {
            let p = with_catalog(app, |c| c.get_photo(id).unwrap());
            (p.rating, p.pick_state, p.label)
        })
        .collect()
}

/// Select A, B, C; a filter then hides B and C. The landed page unselects them, so the menu
/// over A trashes A only, and the row reads "Move to trash".
#[gpui_kit::test]
fn trash_never_takes_selected_photos_a_filter_hides(cx: &mut TestAppContext) {
    let r = rig("menu-hidden", 3, cx);
    let app = &r.app;
    let ids = r.ids.clone();
    with_catalog(app, |c| c.set_culling(ids[0], None, Some("Red"), None).unwrap());
    mouse(app, ids[0], MouseButton::Left, Modifiers::default(), cx);
    ctrl_click(app, ids[1], cx);
    ctrl_click(app, ids[2], cx);
    app.wired.shell.update(cx, |s, cx| s.update_scope(cx, |l| l.toggle_label("Red")));
    cx.run_until_parked();
    render(app, cx);
    assert_eq!(rows(app, cx), vec![ids[0]], "only the red photo is visible");
    assert_eq!(selection(app, cx), vec![ids[0]], "the hidden photos were unselected");
    right_click(app, ids[0], cx);
    assert_eq!(menu(app, cx).unwrap().trash, vec![ids[0]]);
    cx.update_window(app.window(), |_, window, _| {
        assert_eq!(window.find("grid-menu-trash").label(), Some("Move to trash"));
    })
    .unwrap();
    click(app, "grid-menu-trash", cx);
    assert_eq!(trashed(app), vec![ids[0]], "only what the user can see is trashed");
    assert_eq!(status(app, cx), "Moved 1 to the trash.");
}

/// After Move to trash (and Remove from catalog) the photos are no longer selected or
/// active, so a rating or flag key pressed next reaches none of them.
#[gpui_kit::test]
fn a_key_after_trash_or_remove_never_marks_those_photos(cx: &mut TestAppContext) {
    let r = rig("menu-after-trash", 4, cx);
    let app = &r.app;
    let ids = r.ids.clone();
    mouse(app, ids[0], MouseButton::Left, Modifiers::default(), cx);
    ctrl_click(app, ids[1], cx);
    right_click(app, ids[1], cx);
    // What was selected the moment the trash reported success — before its refresh lands.
    let at_report: Rc<RefCell<Option<Vec<i64>>>> = Rc::default();
    let _watch = {
        let (seen, shell) = (at_report.clone(), app.wired.shell.clone());
        cx.update(|cx| {
            cx.observe(&app.wired.model, move |m, cx| {
                if m.read(cx).status.starts_with("Moved") && seen.borrow().is_none() {
                    *seen.borrow_mut() = Some(shell.read(cx).library.selection().ids.to_vec());
                }
            })
        })
    };
    click(app, "grid-menu-trash", cx);
    assert_eq!(*at_report.borrow(), Some(vec![]), "unselected as the trash succeeded, not only when the rows re-read");
    assert!(selection(app, cx).is_empty(), "the trashed photos were unselected at once");
    assert_eq!(app.wired.shell.read_with(cx, |s, _| s.library.selection().active_id), None);
    render(app, cx);
    press(app, "5", cx);
    work(cx);
    assert!(marks(app, &ids).iter().all(|m| m.0 == 0), "no rating landed: {:?}", marks(app, &ids));

    mouse(app, ids[2], MouseButton::Left, Modifiers::default(), cx);
    right_click(app, ids[2], cx);
    click(app, "grid-menu-remove", cx);
    settle(app, cx);
    click(app, "ok", cx);
    assert!(selection(app, cx).is_empty(), "the removed photo was unselected");
    press(app, "x", cx);
    work(cx);
    assert_eq!(marks(app, &ids[3..])[0].1, PickState::None, "no flag landed on another photo");
}

/// The other bulk actions act on the trimmed selection too: once a filter hides some selected
/// photos, rating, flag and label keys mark only the visible ones, and the tagging block's
/// targets (assign, remove, tag paste) are the visible ones.
#[gpui_kit::test]
fn bulk_keys_and_tag_targets_act_on_the_visible_selection_only(cx: &mut TestAppContext) {
    let r = rig("menu-bulk", 4, cx);
    let app = &r.app;
    let ids = r.ids.clone();
    for &id in &ids[..2] {
        with_catalog(app, |c| c.set_culling(id, None, Some("Blue"), None).unwrap());
    }
    mouse(app, ids[0], MouseButton::Left, Modifiers::default(), cx);
    for &id in &ids[1..] {
        ctrl_click(app, id, cx);
    }
    assert_eq!(selection(app, cx).len(), 4);
    app.wired.shell.update(cx, |s, cx| s.update_scope(cx, |l| l.toggle_label("Blue")));
    cx.run_until_parked();
    render(app, cx);
    assert_eq!(rows(app, cx), vec![ids[0], ids[1]]);
    assert_eq!(selection(app, cx), vec![ids[0], ids[1]]);
    let photo_tags = app.wired.root.clone().unwrap().read_with(cx, |r, _| r.photo_tags.clone());
    assert_eq!(photo_tags.read_with(cx, |p, _| p.target.targets.clone()), vec![ids[0], ids[1]]);

    // The active photo (the last ctrl-clicked, now hidden) was dropped; give the keys one.
    mouse(app, ids[1], MouseButton::Left, Modifiers { control: true, ..Default::default() }, cx);
    mouse(app, ids[1], MouseButton::Left, Modifiers { control: true, ..Default::default() }, cx);
    assert_eq!(selection(app, cx), vec![ids[0], ids[1]]);
    for key in ["3", "p", "g"] {
        press(app, key, cx);
        work(cx);
    }
    let m = marks(app, &ids);
    assert_eq!(m[0], (3, PickState::Pick, "Green".into()));
    assert_eq!(m[1], (3, PickState::Pick, "Green".into()));
    assert_eq!((m[2].0, m[2].1), (0, PickState::None), "hidden photo 3 was not marked");
    assert_eq!((m[3].0, m[3].1), (0, PickState::None), "hidden photo 4 was not marked");
}
