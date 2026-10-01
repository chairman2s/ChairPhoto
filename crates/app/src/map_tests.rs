//! Headless tests of the Map module (#119) through the real window: the per-host tile
//! consent (nothing is fetched before the user allows the host — decision #118), tiles
//! following the view and a catalog switch, markers and the filmstrip, drawing and editing
//! fences, pan and zoom by pointer and wheel. Tiles come from a recording fake
//! ([`FakeTiles`]): no test touches the network. Catalog work runs on `Runner::manual`
//! ([`work`]).

use super::*;
use crate::modules::map::state::fake::FakeGeocode;
use crate::modules::map::state::{Load, MapGeocode};
use chairphoto_core::app::GeocodeProgress;
use chairphoto_core::plugins::map::geocode::GeocodeAllSummary;
use crate::modules::map::tiles::fake::{tiny, FakeTiles};
use crate::modules::map::tiles::MapTiles;
use crate::modules::map::view::MapView;
use crate::modules::map::{MAP_MODULE_ID, MAP_VIEW_ID};
use crate::modules::ModuleRegistry;
use crate::shell::state::Surface;
use crate::storage::Runner;
use chairphoto_core::plugins::map::{self as backend, LatLng};
use gpui_kit::{point, px, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point,
    ScrollDelta, ScrollWheelEvent};
use gpui_kit::component::input::InputState;
use gpui_kit::{ElementId, InputEvent as _, SharedString};
use std::sync::Arc;

const OSM: &str = "tile.openstreetmap.org";
const OSLO: LatLng = (59.91, 10.75);
const OSLO2: LatLng = (59.912, 10.752);
const SYDNEY: LatLng = (-33.87, 151.21);

/// Run queued catalog work and repaint until nothing more happens.
fn work(app: &App, cx: &mut TestAppContext) {
    for _ in 0..20 {
        let ran = cx.update(|cx| Runner::get(cx).run_pending());
        cx.run_until_parked();
        frame(app, cx);
        if ran == 0 && cx.update(|cx| Runner::get(cx).pending()) == 0 {
            cx.run_until_parked();
            frame(app, cx);
            return;
        }
    }
}

fn set_input(app: &App, input: &Entity<InputState>, text: &str, cx: &mut TestAppContext) {
    let (input, text) = (input.clone(), text.to_string());
    cx.update_window(app.window(), |_, window, cx| input.update(cx, |i, cx| i.set_value(text, window, cx))).unwrap();
}

fn frame(app: &App, cx: &mut TestAppContext) {
    cx.update_window(app.window(), |_, window, cx| window.render_frame(cx)).unwrap();
    cx.run_until_parked();
}

struct Map {
    app: App,
    fake: Arc<FakeTiles>,
    ids: Vec<i64>,
}

/// The app with a catalog of photos at `gps`, the Map module enabled and its view showing.
fn open_map(dir: &TempDir, gps: &[LatLng], cx: &mut TestAppContext) -> Map {
    let fake = Arc::new(FakeTiles::default());
    cx.update(|cx| cx.set_global(MapTiles(fake.clone())));
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, dir, gps.len(), cx);
    {
        let guard = app.state.catalog.lock().unwrap();
        let c = guard.as_ref().unwrap();
        backend::ensure_schema_for(c).unwrap();
        for (id, &(lat, lng)) in ids.iter().zip(gps) {
            backend::set_photo_gps(c, &[*id], lat, lng).unwrap();
        }
    }
    work(&app, cx);
    cx.update(|cx| ModuleRegistry::enable(&app.wired.modules, MAP_MODULE_ID, cx));
    work(&app, cx);
    show_map(&app, cx);
    Map { app, fake, ids }
}

fn show_map(app: &App, cx: &mut TestAppContext) {
    app.wired.shell.update(cx, |s, cx| s.show_module_view(MAP_VIEW_ID, cx));
    work(app, cx);
}

impl Map {
    fn view(&self, cx: &mut TestAppContext) -> Entity<MapView> {
        let modules = self.app.wired.modules.clone();
        cx.update_window(self.app.window(), |_, window, cx| {
            ModuleRegistry::main_view(&modules, MAP_VIEW_ID, window, cx)
                .expect("the map view")
                .view
                .downcast::<MapView>()
                .expect("a MapView")
        })
        .unwrap()
    }

    fn has(&self, id: impl Into<ElementId>, cx: &mut TestAppContext) -> bool {
        let id = id.into();
        cx.update_window(self.app.window(), |_, window, _| window.try_find(id).is_some()).unwrap()
    }

    fn click(&self, id: &'static str, cx: &mut TestAppContext) {
        cx.update_window(self.app.window(), |_, window, cx| window.click(id, cx)).unwrap();
        work(&self.app, cx);
    }

    fn setting(&self, key: &str) -> Option<String> {
        self.app.state.catalog.lock().unwrap().as_ref().unwrap().get_setting(key).unwrap()
    }

    /// Window position of canvas-local point `(x, y)`.
    fn at(&self, (x, y): (f64, f64), cx: &mut TestAppContext) -> Point<Pixels> {
        let origin = cx
            .update_window(self.app.window(), |_, window, cx| {
                window.render_frame(cx);
                window.find("map-surface").bounds().origin
            })
            .unwrap();
        origin + point(px(x as f32), px(y as f32))
    }

    /// A press and release at canvas-local `p`, as click number `count` of a series.
    fn press_at(&self, p: (f64, f64), count: usize, cx: &mut TestAppContext) {
        let position = self.at(p, cx);
        cx.update_window(self.app.window(), |_, window, cx| {
            window.dispatch_event(
                MouseMoveEvent { position, pressed_button: None, modifiers: Modifiers::default() }.to_platform_input(),
                cx,
            );
            window.dispatch_event(
                MouseDownEvent { button: MouseButton::Left, position, modifiers: Modifiers::default(), click_count: count, first_mouse: false }
                    .to_platform_input(),
                cx,
            );
            window.render_frame(cx);
            window.dispatch_event(
                MouseUpEvent { button: MouseButton::Left, position, modifiers: Modifiers::default(), click_count: count }
                    .to_platform_input(),
                cx,
            );
            window.render_frame(cx);
        })
        .unwrap();
        work(&self.app, cx);
    }

    /// A drag from canvas-local `from` to `to` in a few steps.
    fn drag(&self, from: (f64, f64), to: (f64, f64), cx: &mut TestAppContext) {
        let (a, b) = (self.at(from, cx), self.at(to, cx));
        cx.update_window(self.app.window(), |_, window, cx| {
            window.dispatch_event(MouseMoveEvent { position: a, pressed_button: None, modifiers: Modifiers::default() }.to_platform_input(), cx);
            window.dispatch_event(
                MouseDownEvent { button: MouseButton::Left, position: a, modifiers: Modifiers::default(), click_count: 1, first_mouse: false }
                    .to_platform_input(),
                cx,
            );
            window.render_frame(cx);
            for i in 1..=4 {
                let t = i as f32 / 4.;
                let p = a + (b - a) * t;
                window.dispatch_event(
                    MouseMoveEvent { position: p, pressed_button: Some(MouseButton::Left), modifiers: Modifiers::default() }.to_platform_input(),
                    cx,
                );
                window.render_frame(cx);
            }
            window.dispatch_event(
                MouseUpEvent { button: MouseButton::Left, position: b, modifiers: Modifiers::default(), click_count: 1 }.to_platform_input(),
                cx,
            );
            window.render_frame(cx);
        })
        .unwrap();
        work(&self.app, cx);
    }

    fn screen(&self, ll: LatLng, cx: &mut TestAppContext) -> (f64, f64) {
        self.view(cx).read_with(cx, |v, _| v.viewport.to_screen(ll))
    }

    /// Every tile load so far went to `host`, and how many there were.
    fn loads_to(&self, host: &str) -> usize {
        let loads = self.fake.loads.lock().unwrap();
        assert!(loads.iter().all(|l| l.host == host), "a load went elsewhere: {:?}", loads.iter().map(|l| &l.host).collect::<Vec<_>>());
        loads.len()
    }

    fn allow(&self, cx: &mut TestAppContext) {
        assert!(self.has("map-consent", cx), "the consent card is up");
        self.click("map-consent-allow", cx);
    }
}

// --- privacy -----------------------------------------------------------------------------

/// Decision #118: opening the map asks before the first tile request to a host; until the
/// user allows it nothing is fetched — not while the card is up, not after "Not now", not
/// while panning and zooming — and markers still show. The answer is remembered per host in
/// `map.tileHosts`; allowing (here from the status bar's chip) starts fetching, and only from
/// that host.
#[gpui_kit::test]
fn no_tile_is_fetched_before_the_user_allows_the_host(cx: &mut TestAppContext) {
    let dir = TempDir::new("map-consent");
    let m = open_map(&dir, &[OSLO, OSLO2, SYDNEY], cx);
    let view = m.view(cx);
    assert!(m.has("map-consent", cx), "first open asks");
    assert_eq!(m.fake.count(), 0, "nothing fetched while asking");
    view.read_with(cx, |v, _| {
        assert!(!v.clusters.is_empty(), "markers show without tiles");
        assert!(v.tiles.source().is_none());
    });

    m.click("map-consent-deny", cx);
    assert!(!m.has("map-consent", cx));
    assert_eq!(m.setting("map.tileHosts").as_deref(), Some(r#"{"tile.openstreetmap.org":false}"#));
    view.update(cx, |v, cx| {
        v.zoom_by(3.0, cx);
        v.viewport.pan_by(120.0, -40.0);
    });
    work(&m.app, cx);
    m.drag((300.0, 300.0), (200.0, 250.0), cx);
    assert_eq!(m.fake.count(), 0, "\u{201c}Not now\u{201d} fetches nothing, however the map moves");
    assert!(!m.has("map-attribution", cx), "no attribution without tiles");

    // Reopening does not ask again: the answer is remembered.
    m.app.wired.shell.update(cx, |s, cx| s.show_library(cx));
    work(&m.app, cx);
    show_map(&m.app, cx);
    assert!(!m.has("map-consent", cx));
    assert_eq!(m.fake.count(), 0);

    m.click("map-tiles-off", cx);
    assert_eq!(m.setting("map.tileHosts").as_deref(), Some(r#"{"tile.openstreetmap.org":true}"#));
    let n = m.loads_to(OSM);
    let visible = view.read_with(cx, |v, _| v.viewport.visible_tiles().len());
    assert!(n > 0 && n <= visible, "only the visible tiles load: {n} of {visible}");
    assert!(m.has("map-attribution", cx), "attribution while tiles show");
}

/// A new tile host is a new question: changing the URL to another server drops the old
/// host's tiles, asks again, and fetches nothing from the new host meanwhile.
#[gpui_kit::test]
fn another_tile_host_asks_again_and_fetches_nothing_meanwhile(cx: &mut TestAppContext) {
    let dir = TempDir::new("map-host");
    let m = open_map(&dir, &[OSLO], cx);
    m.allow(cx);
    assert!(m.fake.answer_all() > 0);
    work(&m.app, cx);
    let view = m.view(cx);
    assert!(view.read_with(cx, |v, _| v.tiles.held()) > 0);
    let before = m.fake.count();

    let state = view.read_with(cx, |v, _| v.state.clone());
    state.update(cx, |s, cx| s.set_tile_url("https://{s}.tiles.example.org/{z}/{x}/{y}.png", cx)).unwrap();
    work(&m.app, cx);
    assert!(m.has("map-consent", cx), "a new host is asked about");
    assert_eq!(m.fake.count(), before, "nothing from the new host before it is allowed");
    view.read_with(cx, |v, _| assert_eq!((v.tiles.held(), v.tiles.pending()), (0, 0), "the old host's tiles are dropped"));
    assert_eq!(m.setting("map.tileUrl").as_deref(), Some("https://a.tiles.example.org/{z}/{x}/{y}.png"), "no {{s}} subdomains");

    m.allow(cx);
    let loads = m.fake.loads.lock().unwrap();
    assert!(loads[before..].iter().all(|l| l.host == "a.tiles.example.org") && loads.len() > before);
}

/// A catalog switch makes the old catalog's answer and every pending load unreachable: the
/// loads are cancelled, a result that arrives anyway is dropped, and the new catalog's map
/// asks again before fetching.
#[gpui_kit::test]
fn a_catalog_switch_cancels_tile_loads_and_drops_late_results(cx: &mut TestAppContext) {
    let dir = TempDir::new("map-switch");
    let m = open_map(&dir, &[OSLO], cx);
    m.allow(cx);
    let pending = m.fake.count();
    assert!(pending > 0);
    let view = m.view(cx);

    let other = TempDir::new("map-switch-b");
    open_catalog(&m.app, &other, cx);
    work(&m.app, cx);
    {
        let mut loads = m.fake.loads.lock().unwrap();
        assert!(loads.iter().all(|l| l.cancelled.load(std::sync::atomic::Ordering::SeqCst)), "every pending load cancelled");
        // The network does not always stop in time: answer them anyway.
        for l in loads.iter_mut() {
            if let Some(respond) = l.respond.take() {
                respond(Ok(tiny()));
            }
        }
    }
    work(&m.app, cx);
    view.read_with(cx, |v, _| {
        assert_eq!(v.tiles.held(), 0, "a late tile from before the switch is not shown");
        assert!(v.tiles.stats.stale_dropped >= pending as u64);
    });
    show_map(&m.app, cx);
    assert!(m.has("map-consent", cx), "the new catalog has not been asked");
    assert_eq!(m.fake.count(), pending);
}

/// Tiles follow the view: panning far away cancels the loads that left it; a tile already
/// loaded stands in (stretched) for its children while zooming in.
#[gpui_kit::test]
fn tiles_follow_the_view(cx: &mut TestAppContext) {
    let dir = TempDir::new("map-follow");
    let m = open_map(&dir, &[OSLO], cx);
    m.allow(cx);
    let first: Vec<_> = m.fake.loads.lock().unwrap().iter().map(|l| l.key).collect();
    let view = m.view(cx);
    view.update(cx, |v, cx| {
        v.viewport.set_center(SYDNEY);
        cx.notify();
    });
    work(&m.app, cx);
    let loads = m.fake.loads.lock().unwrap();
    for l in loads.iter().filter(|l| first.contains(&l.key)) {
        assert!(l.cancelled.load(std::sync::atomic::Ordering::SeqCst), "{:?} left the view but kept loading", l.key);
    }
    assert!(loads.len() > first.len(), "the new view's tiles load");
}

// --- markers and the filmstrip -----------------------------------------------------------

/// The view fits the points (max zoom 12), clusters nearby photos, and a click on a cluster
/// opens the filmstrip of its photos and selects the first without leaving the map; "Show in
/// Library" selects the active one and switches to the Library.
#[gpui_kit::test]
fn a_marker_opens_the_filmstrip_and_show_in_library_navigates(cx: &mut TestAppContext) {
    let dir = TempDir::new("map-strip");
    let m = open_map(&dir, &[OSLO, OSLO2], cx);
    m.click("map-consent-deny", cx);
    let view = m.view(cx);
    view.read_with(cx, |v, _| {
        assert_eq!(v.viewport.zoom(), 12.0, "fitted, capped at 12");
        assert_eq!(v.clusters.len(), 1, "two photos 200 m apart are one marker at zoom 12");
    });
    assert!(m.has("map-count", cx));

    let (x, y) = m.screen(OSLO, cx);
    let (x2, y2) = m.screen(OSLO2, cx);
    m.press_at(((x + x2) / 2.0, (y + y2) / 2.0), 1, cx);
    let strip = view.read_with(cx, |v, _| v.filmstrip.clone()).expect("the filmstrip opened");
    let mut ids = strip.ids.clone();
    ids.sort();
    assert_eq!(ids, m.ids);
    let selected = m.app.wired.shell.read_with(cx, |s, _| (s.library.selection().active_id, s.surface.clone()));
    assert_eq!(selected, (Some(strip.ids[0]), Surface::Module(MAP_VIEW_ID.into())), "selected quietly, still on the map");

    view.update(cx, |v, cx| v.filmstrip_select(strip.ids[1], cx));
    m.click("map-show-in-library", cx);
    let after = m.app.wired.shell.read_with(cx, |s, _| (s.library.selection().active_id, s.surface.clone()));
    assert_eq!(after, (Some(strip.ids[1]), Surface::Library));
}

#[gpui_kit::test]
fn no_gps_shows_the_empty_state(cx: &mut TestAppContext) {
    let dir = TempDir::new("map-empty");
    let m = open_map(&dir, &[], cx);
    assert!(m.has("map-empty", cx));
    assert!(matches!(m.view(cx).read_with(cx, |v, cx| v.state.read(cx).points.clone()), Load::Ready(p) if p.is_empty()));
}

// --- pan and zoom ------------------------------------------------------------------------

/// A drag pans by exactly the pointer's travel; the wheel zooms one level per line about
/// the cursor; a double-click zooms in.
#[gpui_kit::test]
fn drag_pans_and_the_wheel_zooms_about_the_cursor(cx: &mut TestAppContext) {
    let dir = TempDir::new("map-pan");
    let m = open_map(&dir, &[OSLO, SYDNEY], cx);
    m.click("map-consent-deny", cx);
    let view = m.view(cx);
    let grabbed = view.read_with(cx, |v, _| v.viewport.screen_to_latlng(200.0, 200.0));
    m.drag((200.0, 200.0), (260.0, 170.0), cx);
    let (x, y) = m.screen(grabbed, cx);
    assert!((x - 260.0).abs() < 0.5 && (y - 170.0).abs() < 0.5, "the grabbed point follows the pointer: {x} {y}");

    let z0 = view.read_with(cx, |v, _| v.viewport.zoom());
    let under = view.read_with(cx, |v, _| v.viewport.screen_to_latlng(300.0, 220.0));
    let position = m.at((300.0, 220.0), cx);
    cx.update_window(m.app.window(), |_, window, cx| {
        window.dispatch_event(MouseMoveEvent { position, pressed_button: None, modifiers: Modifiers::default() }.to_platform_input(), cx);
        window.dispatch_event(
            ScrollWheelEvent { position, delta: ScrollDelta::Lines(point(0., 1.)), ..Default::default() }.to_platform_input(),
            cx,
        );
        window.render_frame(cx);
    })
    .unwrap();
    work(&m.app, cx);
    let z1 = view.read_with(cx, |v, _| v.viewport.zoom());
    assert_eq!(z1, z0 + 1.0, "one wheel line up is one level in");
    let (x, y) = m.screen(under, cx);
    assert!((x - 300.0).abs() < 0.5 && (y - 220.0).abs() < 0.5, "zoomed about the cursor: {x} {y}");

    m.press_at((400.0, 300.0), 1, cx);
    m.press_at((400.0, 300.0), 2, cx);
    assert_eq!(view.read_with(cx, |v, _| v.viewport.zoom()), z1.round() + 1.0, "double-click zooms in");
}

// --- fences ------------------------------------------------------------------------------

/// "+ Draw", three clicks and a double-click close a polygon around the Oslo photos; the
/// editor requires both fields, saves the fence, and Apply all tags the photos inside.
#[gpui_kit::test]
fn draw_a_fence_save_it_and_apply_it(cx: &mut TestAppContext) {
    let dir = TempDir::new("map-draw");
    let m = open_map(&dir, &[OSLO, OSLO2, SYDNEY], cx);
    m.click("map-consent-deny", cx);
    let view = m.view(cx);
    // Zoom so Oslo's two photos are well inside a polygon around them.
    view.update(cx, |v, cx| {
        v.viewport.set_zoom(13.0);
        v.viewport.set_center(OSLO);
        cx.notify();
    });
    work(&m.app, cx);
    m.click("map-draw", cx);
    assert!(view.read_with(cx, |v, _| v.draft.is_some()));
    let (cx0, cy0) = m.screen((59.911, 10.751), cx);
    for (dx, dy) in [(-80.0, -80.0), (80.0, -80.0), (80.0, 80.0)] {
        m.press_at((cx0 + dx, cy0 + dy), 1, cx);
    }
    m.press_at((cx0 - 80.0, cy0 + 80.0), 1, cx);
    m.press_at((cx0 - 80.0, cy0 + 80.0), 2, cx);
    assert!(view.read_with(cx, |v, _| v.draft.is_none() && v.editor.is_some()), "the double-click closed it");

    m.click("map-editor-save", cx);
    assert!(m.has("map-editor-error", cx), "a name is required");
    let (name, tag) = view.read_with(cx, |v, _| {
        let e = v.editor.as_ref().unwrap();
        (e.name.clone(), e.tag_path.clone())
    });
    set_input(&m.app, &name, "Sentrum", cx);
    set_input(&m.app, &tag, "Places/Oslo/Sentrum", cx);
    m.click("map-editor-save", cx);
    let fences = {
        let guard = m.app.state.catalog.lock().unwrap();
        backend::list_fences_for(guard.as_ref().unwrap()).unwrap()
    };
    assert_eq!(fences.len(), 1);
    assert_eq!((fences[0].name.as_str(), fences[0].tag_path.as_str(), fences[0].polygon.len()), ("Sentrum", "Places/Oslo/Sentrum", 4));
    assert!(m.has(SharedString::from(format!("map-fence-{}", fences[0].id)), cx), "listed");

    m.click("map-apply-all", cx);
    let status = m.app.wired.model.read_with(cx, |m, _| m.status.to_string());
    assert_eq!(status, "Applied all fences: 2 photos newly tagged.");
}

/// Dragging a fence's vertex reshapes it live and saves the new polygon on release.
#[gpui_kit::test]
fn dragging_a_vertex_saves_the_new_shape(cx: &mut TestAppContext) {
    let dir = TempDir::new("map-vertex");
    let m = open_map(&dir, &[OSLO], cx);
    m.click("map-consent-deny", cx);
    let square = vec![(59.90, 10.74), (59.90, 10.76), (59.92, 10.76), (59.92, 10.74)];
    {
        let guard = m.app.state.catalog.lock().unwrap();
        backend::create_fence_for(guard.as_ref().unwrap(), "Sq", "Places/Sq", &square).unwrap();
    }
    let view = m.view(cx);
    let state = view.read_with(cx, |v, _| v.state.clone());
    state.update(cx, |s, cx| s.reload_fences(cx));
    work(&m.app, cx);
    let from = m.screen(square[2], cx);
    let to = (from.0 + 40.0, from.1 - 30.0);
    let target = view.read_with(cx, |v, _| v.viewport.screen_to_latlng(to.0, to.1));
    m.drag(from, to, cx);
    let saved = {
        let guard = m.app.state.catalog.lock().unwrap();
        backend::list_fences_for(guard.as_ref().unwrap()).unwrap().remove(0).polygon
    };
    assert_eq!(saved.len(), 4);
    // Within a hundredth of a pixel (positions travel as f32 pixels).
    assert!((saved[2].0 - target.0).abs() < 1e-6 && (saved[2].1 - target.1).abs() < 1e-6, "{saved:?} vs {target:?}");
    assert_eq!(saved[0], square[0], "the other vertices stay");
}

/// **Forced interleaving** (review #119, catalog identity): the core switches to a catalog
/// whose fence and photo ids collide, and `catalog:switched` has not reached the module
/// yet. Every fence write the user can still click — save a drawn fence, edit, delete,
/// apply (one and all share the guard) — is bound to the catalog the fences were read from and fails
/// closed; the new catalog's fence and tags are untouched.
#[gpui_kit::test]
fn old_fence_ids_never_reach_the_new_catalog(cx: &mut TestAppContext) {
    let dir = TempDir::new("map-ident");
    let m = open_map(&dir, &[OSLO], cx);
    m.click("map-consent-deny", cx);
    let square = vec![(59.90, 10.74), (59.90, 10.76), (59.92, 10.76), (59.92, 10.74)];
    {
        let guard = m.app.state.catalog.lock().unwrap();
        backend::create_fence_for(guard.as_ref().unwrap(), "Old", "Places/Old", &square).unwrap();
    }
    let state = m.view(cx).read_with(cx, |v, _| v.state.clone());
    state.update(cx, |s, cx| s.reload_fences(cx));
    work(&m.app, cx);
    let old = state.read_with(cx, |s, _| s.fences[0].clone());

    // The core switch, with the event not delivered.
    let other = dir.0.join("other");
    let b = chairphoto_core::catalog::Catalog::open(&other.join("b.chairphoto"), &other).unwrap();
    let photo = b.upsert_photo(&other.join("2026/q0.ARW"), None, 0, 1).unwrap().id;
    assert_eq!(photo, m.ids[0], "the photo ids collide");
    backend::ensure_schema_for(&b).unwrap();
    backend::set_photo_gps(&b, &[photo], OSLO.0, OSLO.1).unwrap();
    let theirs = backend::create_fence_for(&b, "Theirs", "Places/Theirs", &square).unwrap();
    assert_eq!(theirs.id, old.id, "the fence ids collide");
    chairphoto_core::app::detach_catalog_and_trip_jobs(&m.app.state).unwrap();
    chairphoto_core::app::publish_catalog_and_reset_jobs(&m.app.state, b).unwrap();

    // All queued before any lands (a failed edit re-reads the fences, which are then the
    // new catalog's, read with its identity: consistent, so later writes may go there).
    state.update(cx, |s, cx| {
        s.apply(None, cx);
        let mut edited = old.clone();
        edited.name = "Renamed".into();
        s.update_fence(edited, cx);
        s.create_fence("Drawn".into(), "Places/Drawn".into(), square.clone(), cx);
        s.delete_fence(old.id, cx);
    });
    work(&m.app, cx);

    let status = m.app.wired.model.read_with(cx, |m, _| m.status.to_string());
    assert!(status.contains(chairphoto_core::app::CATALOG_CHANGED), "the write failed closed: {status}");
    let guard = m.app.state.catalog.lock().unwrap();
    let c = guard.as_ref().unwrap();
    let fences = backend::list_fences_for(c).unwrap();
    assert_eq!(fences.len(), 1, "nothing created or deleted in the new catalog: {fences:?}");
    assert_eq!((fences[0].name.as_str(), fences[0].tag_path.as_str()), ("Theirs", "Places/Theirs"), "not renamed");
    assert_eq!(backend::apply_fence(c, theirs.id).unwrap(), 1, "the new catalog's photo had not been tagged");
}

// --- Geocode all -------------------------------------------------------------------------

/// The map with a recording Geocode all backend installed (no network; the real backend's
/// cancel is `state.rs`'s `cancelling_a_net_run_drops_its_pending_request`).
fn with_fake_geocode(cx: &mut TestAppContext) -> Arc<FakeGeocode> {
    let fake = Arc::new(FakeGeocode::default());
    cx.update(|cx| cx.set_global(MapGeocode(fake.clone())));
    fake
}

fn map_state(m: &Map, cx: &mut TestAppContext) -> Entity<crate::modules::map::state::MapState> {
    m.view(cx).read_with(cx, |v, _| v.state.clone())
}

/// Review #119: Geocode all ran on the core runtime with no owner. Disabling the module now
/// stops it (its abort flag and its task), and a result that arrives anyway lands nowhere.
#[gpui_kit::test]
fn disabling_the_module_stops_geocode_all(cx: &mut TestAppContext) {
    let dir = TempDir::new("map-geo-off");
    let fake = with_fake_geocode(cx);
    let m = open_map(&dir, &[OSLO], cx);
    m.click("map-consent-deny", cx);
    let state = map_state(&m, cx);
    state.update(cx, |s, cx| s.geocode_all(cx));
    work(&m.app, cx);
    assert!(state.read_with(cx, |s, _| s.geocode_running().is_some() && s.geocode.busy));
    let now = chairphoto_core::app::catalog_identity(&m.app.state).unwrap();
    assert_eq!(fake.runs.lock().unwrap()[0].from, now, "bound to the shown catalog");

    cx.update(|cx| ModuleRegistry::disable(&m.app.wired.modules, MAP_MODULE_ID, cx));
    work(&m.app, cx);
    assert!(fake.runs.lock().unwrap()[0].stopped(), "the run outlived the module");
    let status = m.app.wired.model.read_with(cx, |m, _| m.status.to_string());
    let done = fake.runs.lock().unwrap()[0].done.take().unwrap();
    let _ = done.send(Ok(GeocodeAllSummary { total: 1, filled: 1, skipped: 0 }));
    work(&m.app, cx);
    assert_eq!(m.app.wired.model.read_with(cx, |m, _| m.status.to_string()), status, "a late result reported");
}

/// A catalog switch stops the run (its photo ids are the old catalog's); a new run on the
/// new catalog starts, and the old run's stragglers — progress and result, sent by its
/// backend after the switch — never touch it. At most one run: a second start while one
/// runs does nothing. Cancel stops the run and says so; the current run's own progress and
/// result land.
#[gpui_kit::test]
fn a_switch_stops_geocode_all_and_its_stragglers_never_reach_the_next_run(cx: &mut TestAppContext) {
    let dir = TempDir::new("map-geo-switch");
    let fake = with_fake_geocode(cx);
    let m = open_map(&dir, &[OSLO], cx);
    m.click("map-consent-deny", cx);
    let state = map_state(&m, cx);
    state.update(cx, |s, cx| s.geocode_all(cx));
    work(&m.app, cx);

    let other = TempDir::new("map-geo-switch-b");
    open_catalog_with_photos(&m.app, &other, 1, cx);
    work(&m.app, cx);
    assert!(fake.runs.lock().unwrap()[0].stopped(), "the switch stopped the old run");
    assert!(state.read_with(cx, |s, _| s.geocode_running().is_none() && !s.geocode.busy));

    state.update(cx, |s, cx| s.geocode_all(cx));
    work(&m.app, cx);
    state.update(cx, |s, cx| s.geocode_all(cx));
    assert_eq!(fake.runs.lock().unwrap().len(), 2, "one run at a time");
    let now = chairphoto_core::app::catalog_identity(&m.app.state).unwrap();
    assert_eq!(fake.runs.lock().unwrap()[1].from, now, "the new run is bound to the new catalog");

    // The old run's stragglers, through its own channels.
    {
        let mut runs = fake.runs.lock().unwrap();
        runs[0].progress.unbounded_send(GeocodeProgress { done: 1, total: 1, filled: 1 }).ok();
        let _ = runs[0].done.take().unwrap().send(Ok(GeocodeAllSummary { total: 1, filled: 1, skipped: 0 }));
    }
    work(&m.app, cx);
    state.read_with(cx, |s, _| {
        assert!(s.geocode_running().is_some(), "a stale result ended the new run");
        assert_eq!((s.geocode.busy, s.geocode.done, s.geocode.status.as_str()), (true, 0, "Starting…"), "stale progress shown");
    });
    // The current run's own progress lands.
    fake.runs.lock().unwrap()[1].progress.unbounded_send(GeocodeProgress { done: 1, total: 3, filled: 1 }).ok();
    work(&m.app, cx);
    assert_eq!(state.read_with(cx, |s, _| (s.geocode.done, s.geocode.total)), (1, 3));

    state.update(cx, |s, cx| s.cancel_geocode(cx));
    work(&m.app, cx);
    assert!(fake.runs.lock().unwrap()[1].stopped(), "Cancel stopped the run");
    let status = state.read_with(cx, |s, _| s.geocode.status.clone());
    assert_eq!(status, "Geocoding cancelled after 1 photos; 1 had location fields filled.");

    // A third run's result lands.
    state.update(cx, |s, cx| s.geocode_all(cx));
    work(&m.app, cx);
    let _ = fake.runs.lock().unwrap()[2].done.take().unwrap().send(Ok(GeocodeAllSummary { total: 2, filled: 1, skipped: 1 }));
    work(&m.app, cx);
    state.read_with(cx, |s, _| {
        assert!(s.geocode_running().is_none() && !s.geocode.busy);
        assert_eq!(s.geocode.status, "Done: 1 of 2 photos had location fields filled. 1 already set or no result.");
    });
}

/// Escape closes the editor first, then cancels drawing, then closes the filmstrip.
#[gpui_kit::test]
fn escape_unwinds_editor_then_drawing_then_filmstrip(cx: &mut TestAppContext) {
    let dir = TempDir::new("map-esc");
    let m = open_map(&dir, &[OSLO], cx);
    m.click("map-consent-deny", cx);
    let view = m.view(cx);
    let (x, y) = m.screen(OSLO, cx);
    m.press_at((x, y), 1, cx); // a marker: the filmstrip, and focus on the map
    assert!(view.read_with(cx, |v, _| v.filmstrip.is_some()));
    m.click("map-draw", cx);
    cx.update_window(m.app.window(), |_, window, cx| {
        let focus = view.read(cx).focus.clone();
        window.focus(&focus, cx);
    })
    .unwrap();
    press(&m.app, "escape", cx);
    work(&m.app, cx);
    assert!(view.read_with(cx, |v, _| v.draft.is_none() && v.filmstrip.is_some()), "drawing cancelled first");
    press(&m.app, "escape", cx);
    work(&m.app, cx);
    assert!(view.read_with(cx, |v, _| v.filmstrip.is_none()));
}

/// Disabling the module drops the view and with it every load still pending.
#[gpui_kit::test]
fn disabling_the_module_cancels_pending_tile_loads(cx: &mut TestAppContext) {
    let dir = TempDir::new("map-off");
    let m = open_map(&dir, &[OSLO], cx);
    m.allow(cx);
    assert!(m.fake.count() > 0);
    cx.update(|cx| ModuleRegistry::disable(&m.app.wired.modules, MAP_MODULE_ID, cx));
    work(&m.app, cx);
    let loads = m.fake.loads.lock().unwrap();
    assert!(loads.iter().all(|l| l.cancelled.load(std::sync::atomic::Ordering::SeqCst)));
}
