//! Headless tests that a grid tile's thumbnail fills its picture box the way React's
//! `.thumb { object-fit: cover }` does (#186): the picture's laid-out element is the box,
//! and the texture covers it, centred, cropped by the box. Each covers a landscape frame, a
//! portrait frame, and the rotated frame (DSC07441's row: 7008×4672 stored, EXIF
//! orientation 6), whose thumbnail comes from the core already upright, so it reaches the
//! grid as a portrait texture — see `loupe/fit_tests.rs` for the same three frames.
//!
//! Before the fix the picture was `img(..).size_full()` in the box's flow: GPUI's `img`
//! gives its element the image's aspect ratio, so a portrait thumbnail's element took its
//! height from the box's width and ran far below the box (the box's `overflow_hidden`
//! clipped the paint), so the box showed the frame's top instead of its centre.
//!
//! What a headless test sees is layout: the element's bounds. Where the texture is painted
//! is computed here with `ObjectFit::Cover`'s own maths for those bounds; the fit mode the
//! element was given is not observable, so Cover rather than Contain is checked by reading
//! `render_tile` and in the app, not by this test.

use crate::image_tests::{pixels, FakePool};
use crate::library::layout::NAME_H;
use crate::loupe::fit_tests::{assert_fills, store_rotated, LANDSCAPE, PORTRAIT};
use crate::shell::actions::ProposeStacks;
use crate::tests::{open_catalog_with_photos, start_with_pool, App, TempDir};
use chairphoto_core::image_pool::{ImageKind, JobKey};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{size, AppContext as _, Bounds, DevicePixels, ObjectFit, Pixels, SharedString, TestAppContext};
use std::sync::Arc;

fn render(app: &App, cx: &mut TestAppContext) {
    cx.update_window(app.window(), |_, window, cx| window.render_frame(cx)).unwrap();
    cx.run_until_parked();
}

/// (the tile, its picture box, the picture's laid-out element) for photo `id`.
fn tile_bounds(app: &App, id: i64, cx: &mut TestAppContext) -> (Bounds<Pixels>, Bounds<Pixels>, Bounds<Pixels>) {
    cx.update_window(app.window(), |_, window, _| {
        let tile = window.find(("tile", id as u64)).bounds();
        let frame = window.find(("tile-frame", id as u64)).bounds();
        let picture = window
            .try_find(("tile-picture", id as u64))
            .unwrap_or_else(|| panic!("photo {id}: no picture drawn"))
            .bounds();
        (tile, frame, picture)
    })
    .unwrap()
}

/// Each tile's thumbnail — landscape, portrait, rotated (upright portrait) — is laid out as
/// its picture box and painted covering it, centred; every tile keeps the row's 3:2 picture
/// height plus the name strip.
#[gpui_kit::test]
fn grid_tiles_cover_their_box_with_landscape_portrait_and_rotated_thumbnails(cx: &mut TestAppContext) {
    let dir = TempDir::new("fit-grid");
    let pool = Arc::new(FakePool::default());
    let app = start_with_pool(cx, pool.clone());
    let ids = open_catalog_with_photos(&app, &dir, 3, cx);
    store_rotated(&app, ids[2]);
    let frames = [(ids[0], LANDSCAPE), (ids[1], PORTRAIT), (ids[2], PORTRAIT)];
    render(&app, cx);
    for &(id, (w, h)) in &frames {
        pool.finish(&JobKey::photo(id, ImageKind::Thumb), Ok(pixels(w, h)));
    }
    cx.run_until_parked();
    render(&app, cx);

    let f = |p: Pixels| f32::from(p);
    let near = |a: Pixels, b: Pixels| (f(a) - f(b)).abs() <= 0.5;
    for &(id, image) in &frames {
        let (tile, frame, picture) = tile_bounds(&app, id, cx);
        let msg = format!("photo {id} {image:?}: tile {tile:?}, box {frame:?}, picture {picture:?}");
        // The tile keeps the row's height: a 3:2 picture box plus the name strip.
        assert!(
            (f(tile.size.height) - ((f(tile.size.width) * 2. / 3.).round() + NAME_H)).abs() <= 0.5,
            "{msg}: the tile is not the row's height"
        );
        assert!(f(frame.size.width) > 1. && f(frame.size.height) > 1., "{msg}: the box has no size");
        assert!(frame.bottom() <= tile.bottom() && frame.top() >= tile.top(), "{msg}: the box leaves the tile");
        // The picture's element is the box — not a taller element running below it.
        assert!(
            near(picture.origin.x, frame.origin.x)
                && near(picture.origin.y, frame.origin.y)
                && near(picture.size.width, frame.size.width)
                && near(picture.size.height, frame.size.height),
            "{msg}: the picture's element is not its box"
        );
        // Painted with Cover: it covers the box, touches it along one axis, keeps its shape
        // and is centred on it.
        let painted =
            ObjectFit::Cover.get_bounds(picture, size(DevicePixels(image.0 as i32), DevicePixels(image.1 as i32)));
        let msg = format!("{msg}, painted {painted:?}");
        assert!(
            f(painted.left()) <= f(frame.left()) + 0.5
                && f(painted.top()) <= f(frame.top()) + 0.5
                && f(painted.right()) >= f(frame.right()) - 0.5
                && f(painted.bottom()) >= f(frame.bottom()) - 0.5,
            "{msg}: the picture does not cover its box"
        );
        assert!(
            near(painted.size.width, frame.size.width) || near(painted.size.height, frame.size.height),
            "{msg}: covered more than it needs to"
        );
        let (ratio, want) = (f(painted.size.width) / f(painted.size.height), image.0 as f32 / image.1 as f32);
        assert!((ratio - want).abs() / want < 0.01, "{msg}: the shape changed");
        assert!(
            near(painted.center().x, frame.center().x) && near(painted.center().y, frame.center().y),
            "{msg}: not centred on the box"
        );
    }
}

// --- the bench's pile and the Stack bursts dialog -----------------------------------------

/// The bench's pile thumbnails (52×35) fill their cell with a portrait and a landscape frame
/// (`.bench-strip .thumbwrap .thumb { object-fit: cover }`); the marked one has a 2 px ring.
#[gpui_kit::test]
fn bench_pile_thumbnails_fill_their_cell(cx: &mut TestAppContext) {
    let dir = TempDir::new("fit-bench");
    let pool = Arc::new(FakePool::default());
    let app = start_with_pool(cx, pool.clone());
    let ids = open_catalog_with_photos(&app, &dir, 2, cx);
    app.wired.shell.update(cx, |s, cx| s.select_with(cx, |l| l.select_all()));
    render(&app, cx);
    let frames = [(ids[0], PORTRAIT), (ids[1], LANDSCAPE)];
    for &(id, (w, h)) in &frames {
        pool.finish(&JobKey::photo(id, ImageKind::Thumb), Ok(pixels(w, h)));
    }
    cx.run_until_parked();
    render(&app, cx);
    cx.update_window(app.window(), |_, window, _| {
        for &(id, image) in &frames {
            let cell = ("bench-thumb", id as u64);
            let marked = window.find(cell).label().unwrap_or_default().ends_with("(marking)");
            let border = if marked { 2. } else { 0. };
            assert_fills(&format!("bench photo {id} {image:?}"), window, cell, ("bench-thumb-picture", id as u64), border);
        }
    })
    .unwrap();
}

/// The Stack bursts dialog's frames fill their 64 px picture box — landscape, portrait and
/// rotated (`.stack-proposal-frame img { object-fit: cover }`).
#[gpui_kit::test]
fn stack_proposal_frames_fill_their_box(cx: &mut TestAppContext) {
    let dir = TempDir::new("fit-stacks");
    let pool = Arc::new(FakePool::default());
    let app = start_with_pool(cx, pool.clone());
    let ids = open_catalog_with_photos(&app, &dir, 4, cx);
    super::tests::make_burst(app.state.catalog.lock().unwrap().as_ref().unwrap(), &ids);
    store_rotated(&app, ids[2]);
    cx.update_window(app.window(), |_, window, cx| window.dispatch_action(Box::new(ProposeStacks), cx)).unwrap();
    cx.run_until_parked();
    render(&app, cx);
    let frames = [(ids[0], LANDSCAPE), (ids[1], PORTRAIT), (ids[2], PORTRAIT)];
    for &(id, (w, h)) in &frames {
        pool.finish(&JobKey::photo(id, ImageKind::Thumb), Ok(pixels(w, h)));
    }
    cx.run_until_parked();
    render(&app, cx);
    let dialog = super::tests::stack_dialog(&app, cx);
    let group = dialog.read_with(cx, |d, _| d.result.as_ref().expect("proposed").proposals[0].keeper_id);
    cx.update_window(app.window(), |_, window, _| {
        for &(id, image) in &frames {
            let cell = SharedString::from(format!("frame-{group}-{id}-thumb"));
            let picture = SharedString::from(format!("frame-{group}-{id}-picture"));
            assert_fills(&format!("stack frame {id} {image:?}"), window, cell, picture, 0.);
        }
    })
    .unwrap();
}
