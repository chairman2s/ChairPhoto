//! Headless tests of the Collage module: the action opens the dialog and auto-arranges on the
//! worker; templates lock the layout and a locked drag swaps; an unlocked drag moves and the
//! wheel zooms (pointer events through the real window); Save to library indexes and tags the
//! collage in the catalog the selection came from; Render to a folder; a catalog switch with
//! and without `catalog:switched` delivered. Previews are decoded straight from the test's
//! PNGs (no thumbnail cache); outputs go to the test's temp dir.

use super::view::{CollageDialog, SaveTo};
use super::{CollageBackend, COLLAGE_ACTION, COLLAGE_ID};
use crate::modules::dialog::DialogHost;
use crate::modules::ModuleRegistry;
use crate::storage::Runner;
use crate::tests::{colliding_catalog, core_switch, deliver_switch, start, status, App, TempDir};
use chairphoto_core::app::collage::{decode_upright, PreviewLoader};
use chairphoto_core::app::{CoreEvent, EventSink as _, CATALOG_CHANGED};
use chairphoto_core::catalog::Catalog;
use gpui_kit::component::WindowExt as _;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{point, px, AppContext as _, Entity, ScrollDelta, TestAppContext};
use std::path::Path;
use std::sync::Arc;

/// A catalog in `dir` with two real PNGs (red landscape, blue portrait), both selected, the
/// Library's rows read; returns their ids.
fn catalog_with_pngs(app: &App, dir: &TempDir, cx: &mut TestAppContext) -> Vec<i64> {
    let db = dir.0.join("photos.chairphoto");
    let root = dir.0.join("photos");
    std::fs::create_dir_all(root.join("2026")).unwrap();
    let catalog = Catalog::open(&db, &root).unwrap();
    let ids = [(40u32, 20u32, [255u8, 0, 0]), (20, 40, [0, 0, 255])]
        .iter()
        .enumerate()
        .map(|(i, (w, h, rgb))| {
            let p = root.join(format!("2026/p{i}.png"));
            image::RgbImage::from_pixel(*w, *h, image::Rgb(*rgb)).save(&p).unwrap();
            let len = std::fs::metadata(&p).unwrap().len() as i64;
            catalog.upsert_photo(&p, None, 0, len).unwrap().id
        })
        .collect();
    *app.state.catalog.lock().unwrap() = Some(catalog);
    app.state.send(CoreEvent::CatalogSwitched(db.to_string_lossy().to_string()));
    cx.run_until_parked();
    work(cx);
    app.wired.shell.update(cx, |s, cx| {
        s.library.select_all();
        cx.notify();
    });
    ids
}

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

fn direct() -> CollageBackend {
    let previews: PreviewLoader = Arc::new(|path: &Path| decode_upright(&std::fs::read(path).map_err(|e| e.to_string())?));
    CollageBackend { previews }
}

fn open(app: &App, cx: &mut TestAppContext) -> Entity<CollageDialog> {
    let view = cx
        .update_window(app.window(), |_, window, cx| {
            let host = DialogHost::new(&app.wired.model, &app.wired.shell, None, cx);
            super::open(host, direct(), window, cx)
        })
        .unwrap();
    cx.run_until_parked();
    // The dialog's entrance animation runs on the wall clock (see modules::tests::settle_dialog).
    std::thread::sleep(std::time::Duration::from_millis(450));
    cx.run_until_parked();
    view
}

fn has_dialog(app: &App, cx: &mut TestAppContext) -> bool {
    cx.update_window(app.window(), |_, window, cx| window.has_active_dialog(cx)).unwrap()
}

fn present(app: &App, id: &'static str, cx: &mut TestAppContext) -> bool {
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.try_find(id).is_some()
    })
    .unwrap()
}

fn click(app: &App, id: &'static str, cx: &mut TestAppContext) {
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.click(id, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn photos_in(app: &App) -> usize {
    app.state.catalog.lock().unwrap().as_ref().unwrap().count_photos(&Default::default()).unwrap()
}

/// More ⋯ → Modules → "Make collage" opens the dialog, which auto-arranges the selection on
/// the worker (nothing on the UI thread), as the justified mosaic.
#[gpui_kit::test]
fn the_action_opens_the_dialog_and_auto_arranges(cx: &mut TestAppContext) {
    let dir = TempDir::new("collage-action");
    let app = start(cx);
    let ids = catalog_with_pngs(&app, &dir, cx);
    cx.update(|cx| ModuleRegistry::enable(&app.wired.modules, COLLAGE_ID, cx));
    cx.run_until_parked();
    cx.update_window(app.window(), |_, window, cx| ModuleRegistry::activate(&app.wired.modules, COLLAGE_ID, COLLAGE_ACTION, window, cx))
        .unwrap();
    cx.run_until_parked();
    assert!(has_dialog(&app, cx));
    assert!(present(&app, "collage-dialog", cx));
    assert_eq!(cx.update(|cx| Runner::get(cx).pending()), 1, "the auto-arrange is queued, not run here");
    work(cx);
    let view = open(&app, cx); // a second dialog over the same selection, to read its state
    work(cx);
    view.read_with(cx, |d, _| {
        assert_eq!(d.canvas.placements.iter().map(|p| p.photo_id).collect::<Vec<_>>(), ids);
        assert_eq!(d.canvas.layout_kind, "Mosaic");
        assert!(!d.canvas.locked);
        let (a, b) = (d.canvas.placements[0], d.canvas.placements[1]);
        assert!(a.w > a.h && b.h > b.w, "aspects kept: {a:?} {b:?}");
    });
}

/// A template locks the layout; dragging one tile onto the other swaps the photos between the
/// fixed slots (through real pointer events).
#[gpui_kit::test]
fn a_template_locks_and_a_drag_swaps(cx: &mut TestAppContext) {
    let dir = TempDir::new("collage-swap");
    let app = start(cx);
    let ids = catalog_with_pngs(&app, &dir, cx);
    let view = open(&app, cx);
    work(cx);
    click(&app, "collage-template-columns", cx);
    view.read_with(cx, |d, _| {
        assert!(d.canvas.locked);
        assert_eq!(d.canvas.layout_kind, "Columns");
        assert_eq!(d.canvas.placements[0].photo_id, ids[0]);
    });
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.drag_to(("collage-tile", ids[1] as u64), ("collage-tile", ids[0] as u64), cx);
    })
    .unwrap();
    cx.run_until_parked();
    view.read_with(cx, |d, _| {
        let p = &d.canvas.placements;
        assert_eq!((p[0].photo_id, p[0].x), (ids[1], 0.0), "the second photo is in the first slot");
        assert_eq!((p[1].photo_id, p[1].x), (ids[0], 0.5));
        assert!(!d.canvas.dragging() && d.canvas.swap_target.is_none());
    });
}

/// Unlocked: a drag moves the tile by the pointer's travel over the canvas (the release lands
/// at window level), making the layout Freeform; the wheel zooms the photo in its frame.
#[gpui_kit::test]
fn an_unlocked_drag_moves_and_the_wheel_zooms(cx: &mut TestAppContext) {
    let dir = TempDir::new("collage-move");
    let app = start(cx);
    let ids = catalog_with_pngs(&app, &dir, cx);
    let view = open(&app, cx);
    work(cx);
    click(&app, "collage-template-rows", cx);
    click(&app, "collage-lock", cx);
    view.read_with(cx, |d, _| assert!(!d.canvas.locked));
    // The 1:1 canvas is 460 px: a 46 px drag right is 0.1 of it.
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        let b = window.find(("collage-tile", ids[0] as u64)).bounds();
        let from = b.center();
        window.drag(from, point(from.x + px(46.), from.y), cx);
    })
    .unwrap();
    cx.run_until_parked();
    view.read_with(cx, |d, _| {
        let p = d.canvas.placements[0];
        // A full-width row cannot move right (x clamps at 1 - w = 0).
        assert_eq!(p.x, 0.0);
        assert_eq!(d.canvas.layout_kind, "Freeform", "a move is a layout edit");
        assert!(!d.canvas.dragging(), "the release ended the gesture");
    });
    // Shrink it, then move it: now it travels.
    view.update(cx, |d, _| {
        d.canvas.placements[0].w = 0.5;
    });
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        let b = window.find(("collage-tile", ids[0] as u64)).bounds();
        let from = b.center();
        window.drag(from, point(from.x + px(46.), from.y), cx);
    })
    .unwrap();
    cx.run_until_parked();
    let x = view.read_with(cx, |d, _| d.canvas.placements[0].x);
    assert!((x - 0.1).abs() < 0.01, "moved by 46/460: {x}");

    cx.update_window(app.window(), |_, window, cx| {
        window.scroll(("collage-tile", ids[1] as u64), ScrollDelta::Pixels(point(px(0.), px(200.))), cx);
    })
    .unwrap();
    cx.run_until_parked();
    view.read_with(cx, |d, _| {
        let zoom = d.canvas.placements[1].zoom;
        assert!(zoom > 1.0, "{zoom}");
        assert_eq!(d.canvas.selected, Some(ids[1]));
    });
}

/// Save to library: the collage is written under the library root, indexed into the catalog
/// the selection came from and tagged `Collage/<kind>`; the status line says so.
#[gpui_kit::test]
fn save_to_library_indexes_and_tags_it(cx: &mut TestAppContext) {
    let dir = TempDir::new("collage-library");
    let app = start(cx);
    catalog_with_pngs(&app, &dir, cx);
    let view = open(&app, cx);
    work(cx);
    click(&app, "collage-template-columns", cx);
    run(&view, cx);
    view.read_with(cx, |d, _| assert!(d.busy));
    work(cx);
    view.read_with(cx, |d, _| {
        assert_eq!(d.error, None);
        assert!(d.saved_to_library && !d.busy);
    });
    assert_eq!(photos_in(&app), 3);
    let path = dir.0.join("photos/Collages/collage.jpg");
    assert!(path.exists());
    let tagged = chairphoto_core::app::with_catalog(&app.state, |c| {
        let id = c.list_photos(&Default::default())?.into_iter().find(|p| p.path.ends_with("collage.jpg")).unwrap().id;
        c.get_photo_tags(id)
    })
    .unwrap();
    assert!(tagged.iter().any(|t| t.full_path == "Collage/Columns"), "{tagged:?}");
    assert_eq!(status(&app, cx), "Collage saved to your library.");
    assert!(present(&app, "collage-saved", cx));
}

/// Render to a folder: a PNG of the chosen size in the folder; the catalog is unchanged.
#[gpui_kit::test]
fn render_to_a_folder_writes_the_file(cx: &mut TestAppContext) {
    let dir = TempDir::new("collage-folder");
    let app = start(cx);
    catalog_with_pngs(&app, &dir, cx);
    let view = open(&app, cx);
    work(cx);
    let out = dir.0.join("out");
    cx.update_window(app.window(), |_, window, cx| {
        view.update(cx, |d, cx| {
            d.save_to = SaveTo::Folder;
            d.format = super::view::Format::Png;
            d.width = 1080;
            d.aspect = "16:9";
            d.dest.update(cx, |i, cx| i.set_value(out.to_string_lossy().to_string(), window, cx));
        })
    })
    .unwrap();
    click(&app, "collage-run", cx);
    work(cx);
    let written = out.join("collage.png");
    view.read_with(cx, |d, _| assert_eq!(d.output.as_deref(), Some(written.as_path()), "{:?}", d.error));
    assert_eq!(image::image_dimensions(&written).unwrap(), (1080, 608));
    assert_eq!(photos_in(&app), 2, "a folder render indexes nothing");
    assert!(present(&app, "collage-output", cx));
}

/// A switch to a catalog whose ids collide: before `catalog:switched` arrives, Save to library
/// is refused and the new catalog gains nothing; the event closes the dialog.
#[gpui_kit::test]
fn a_catalog_switch_refuses_the_save_and_closes_the_dialog(cx: &mut TestAppContext) {
    let dir = TempDir::new("collage-switch");
    let app = start(cx);
    let ids = catalog_with_pngs(&app, &dir, cx);
    let view = open(&app, cx);
    work(cx);
    let (b, b_ids) = colliding_catalog(&dir, "b", 2);
    assert_eq!(b_ids, ids);
    core_switch(&app, b);
    run(&view, cx);
    work(cx);
    view.read_with(cx, |d, _| assert_eq!(d.error.as_deref(), Some(CATALOG_CHANGED)));
    assert_eq!(photos_in(&app), 2, "B gained no collage");
    assert!(!dir.0.join("photos/Collages").exists() || std::fs::read_dir(dir.0.join("photos/Collages")).unwrap().next().is_none());
    assert!(has_dialog(&app, cx));
    deliver_switch(&app, cx);
    assert!(!has_dialog(&app, cx), "the dialog closed on catalog:switched");
}

/// Press Save to library / Render. Called on the view: with the 1:1 canvas the button sits
/// below the fold of the headless window's dialog (the folder test clicks it for real).
fn run(view: &Entity<CollageDialog>, cx: &mut TestAppContext) {
    view.update(cx, |d, cx| d.run(cx));
    cx.run_until_parked();
}
