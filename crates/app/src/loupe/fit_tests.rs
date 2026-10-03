//! Headless tests that a fitted picture is drawn whole inside its box (React's
//! `object-fit: contain`): the cull stage (#174) here, the Darkroom duel (#178) in
//! `darkroom/tests.rs` with the Darkroom rig. Each covers a
//! landscape frame, a portrait frame, and a frame whose catalog row is stored landscape with
//! a rotating EXIF orientation (DSC07441: 7008×4672, orientation 6) — its preview comes from
//! the core already turned upright, so it reaches the view as a portrait texture.
//!
//! "Drawn" is where GPUI paints the texture: the element's laid-out bounds, through
//! `ObjectFit::Contain`'s own maths for a contained element (a Fill element paints exactly its
//! bounds, which the same maths returns when the bounds already have the image's shape).

use crate::image_tests::{pixels, FakePool};
use crate::loupe::cull::CullView;
use crate::shell::actions::StartCullSession;
use crate::tests::{open_catalog_with_photos, press, start_with_pool, App, TempDir};
use chairphoto_core::image_pool::{ImageKind, JobKey};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    point, px, size, AppContext as _, Bounds, DevicePixels, Entity, ObjectFit, Pixels, TestAppContext, Window,
};
use std::collections::HashMap;
use std::sync::Arc;

/// A landscape frame, a portrait frame, and the rotated frame's upright preview.
pub(crate) const LANDSCAPE: (u32, u32) = (300, 200);
pub(crate) const PORTRAIT: (u32, u32) = (200, 300);

/// Store photo `id`'s row as DSC07441's: 7008×4672 as stored, EXIF orientation 6 (90° CW).
pub(crate) fn store_rotated(app: &App, id: i64) {
    let guard = app.state.catalog.lock().unwrap();
    guard
        .as_ref()
        .unwrap()
        .conn()
        .execute("UPDATE photos SET width = 7008, height = 4672, exif_orientation = 6 WHERE id = ?1", [id])
        .unwrap();
}

/// Where Contain paints an `image`-sized texture in an element laid out at `bounds`.
pub(crate) fn drawn(bounds: Bounds<Pixels>, image: (u32, u32)) -> Bounds<Pixels> {
    ObjectFit::Contain.get_bounds(bounds, size(DevicePixels(image.0 as i32), DevicePixels(image.1 as i32)))
}

/// `picture` is `image` fitted to `boxed`: inside it, as large as it can be (touching both
/// sides along one axis), its shape kept, and centred.
pub(crate) fn assert_fitted(what: &str, picture: Bounds<Pixels>, boxed: Bounds<Pixels>, image: (u32, u32)) {
    let f = |p: Pixels| f32::from(p);
    let (pl, pt, pw, ph) = (f(picture.origin.x), f(picture.origin.y), f(picture.size.width), f(picture.size.height));
    let (bl, bt, bw, bh) = (f(boxed.origin.x), f(boxed.origin.y), f(boxed.size.width), f(boxed.size.height));
    let near = |a: f32, b: f32| (a - b).abs() <= 0.5;
    let msg = format!("{what}: {image:?} drawn at {picture:?} in the box {boxed:?}");
    assert!(bw > 1. && bh > 1., "{msg}: the box has no size");
    assert!(
        pl >= bl - 0.5 && pt >= bt - 0.5 && pl + pw <= bl + bw + 0.5 && pt + ph <= bt + bh + 0.5,
        "{msg}: the picture leaves the box"
    );
    assert!(near(pw, bw) || near(ph, bh), "{msg}: not fitted to the box");
    let (ratio, want) = (pw / ph, image.0 as f32 / image.1 as f32);
    assert!((ratio - want).abs() / want < 0.01, "{msg}: the shape changed");
    assert!(near(pl - bl, bl + bw - pl - pw) && near(pt - bt, bt + bh - pt - ph), "{msg}: not centred");
}

fn app_with(n: usize, tag: &str, cx: &mut TestAppContext) -> (App, Arc<FakePool>, TempDir, Vec<i64>) {
    let dir = TempDir::new(tag);
    let pool = Arc::new(FakePool::default());
    let app = start_with_pool(cx, pool.clone());
    let ids = open_catalog_with_photos(&app, &dir, n, cx);
    (app, pool, dir, ids)
}

fn preview(id: i64) -> JobKey {
    JobKey::photo(id, ImageKind::Preview)
}

/// Each photo's texture: the first landscape, the second portrait, the third the rotated
/// frame (stored landscape, previewed upright).
fn three_frames(app: &App, ids: &[i64]) -> HashMap<i64, (u32, u32)> {
    store_rotated(app, ids[2]);
    HashMap::from([(ids[0], LANDSCAPE), (ids[1], PORTRAIT), (ids[2], PORTRAIT)])
}

/// Answer every preview asked for so far with that photo's texture.
fn answer_previews(pool: &FakePool, frames: &HashMap<i64, (u32, u32)>, cx: &mut TestAppContext) {
    for (&id, &(w, h)) in frames {
        pool.finish(&preview(id), Ok(pixels(w, h)));
    }
    cx.run_until_parked();
}

fn bounds_in(window: &Window, id: impl Into<gpui_kit::ElementId>) -> Bounds<Pixels> {
    window.find(id).bounds()
}

// --- the cull session (#174) --------------------------------------------------------------

fn cull(app: &App, cx: &mut TestAppContext) -> Entity<CullView> {
    app.wired.root.as_ref().unwrap().read_with(cx, |r, _| r.cull().cloned()).expect("the session opened")
}

/// Every frame of a cull session is drawn whole inside the stage (the window less the
/// stage's 48 px padding), letterboxed — never filling the width and running off the
/// bottom, as a portrait frame did.
#[gpui_kit::test]
fn the_cull_stage_fits_landscape_portrait_and_rotated_frames(cx: &mut TestAppContext) {
    let (app, pool, _dir, ids) = app_with(3, "fit-cull", cx);
    let frames = three_frames(&app, &ids);
    app.wired.shell.update(cx, |s, cx| s.select_with(cx, |l| l.select_all()));
    cx.run_until_parked();
    cx.update_window(app.window(), |_, window, cx| window.dispatch_action(Box::new(StartCullSession), cx)).unwrap();
    cx.run_until_parked();
    answer_previews(&pool, &frames, cx);
    let view = cull(&app, cx);
    let mut seen = Vec::new();
    for _ in 0..ids.len() {
        let id = view.read_with(cx, |v, _| v.state.current().map(|p| p.id)).expect("a photo on the stage");
        let image = frames[&id];
        cx.update_window(app.window(), |_, window, cx| {
            window.render_frame(cx);
            let stage = bounds_in(window, "cull-stage");
            let inset = px(48.);
            let boxed = Bounds::new(
                point(stage.origin.x + inset, stage.origin.y + inset),
                size(stage.size.width - inset * 2., stage.size.height - inset * 2.),
            );
            let picture = drawn(bounds_in(window, "cull-image"), image);
            assert_fitted(&format!("cull photo {id}"), picture, boxed, image);
        })
        .unwrap();
        seen.push(id);
        press(&app, "right", cx);
    }
    seen.sort();
    assert_eq!(seen, ids, "every frame was checked");
}
