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
use crate::machine_prefs::{MachinePrefs, FILE_NAME as MACHINE_PREFS_FILE};
use crate::modules::map::logic::{Consent, MACHINE_TILE_HOSTS};
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
use crate::image_store::ImageState;
use crate::image_tests::{pixels, FakePool};
use crate::modules::map::view::{frame_image, strip_wanted, STRIP_OVERSCAN};
use chairphoto_core::image_pool::{ImageKind, JobKey};
use gpui_kit::ScrollStrategy;
use std::sync::Arc;

const OSM: &str = "tile.openstreetmap.org";
const OSLO: LatLng = (59.91, 10.75);
const OSLO2: LatLng = (59.912, 10.752);
const SYDNEY: LatLng = (-33.87, 151.21);
/// The Map module's old per-catalog consent setting (`map.tileHosts`), as the first port
/// stored it through `ModuleSettings`.
const LEGACY_HOSTS: &str = "map.tileHosts";

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
    open_map_with(dir, gps, None, cx)
}

/// [`open_map`] with the image layer on `pool` (a [`FakePool`] the test answers by hand).
fn open_map_with(dir: &TempDir, gps: &[LatLng], pool: Option<Arc<FakePool>>, cx: &mut TestAppContext) -> Map {
    let fake = Arc::new(FakeTiles::default());
    cx.update(|cx| cx.set_global(MapTiles(fake.clone())));
    let app = match pool {
        Some(pool) => crate::tests::start_with_pool(cx, pool),
        None => start(cx),
    };
    let ids = open_catalog_with_photos(&app, dir, gps.len(), cx);
    {
        let guard = app.state.catalog.lock().unwrap();
        let c = guard.as_ref().unwrap();
        backend::ensure_schema_for(c).unwrap();
        // One write per place (a big cluster is many photos at one place).
        let mut places: Vec<(LatLng, Vec<i64>)> = Vec::new();
        for (&id, &ll) in ids.iter().zip(gps) {
            match places.iter_mut().find(|(p, _)| *p == ll) {
                Some((_, at)) => at.push(id),
                None => places.push((ll, vec![id])),
            }
        }
        for ((lat, lng), at) in places {
            backend::set_photo_gps(c, &at, lat, lng).unwrap();
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
/// this machine's preferences (`map.tileHosts`), not the catalog; allowing (here from the status bar's chip) starts fetching, and only from
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
    assert_eq!(machine_hosts(cx).as_deref(), Some(r#"{"tile.openstreetmap.org":false}"#));
    assert_eq!(m.setting(LEGACY_HOSTS), None, "not a catalog setting");
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
    assert_eq!(machine_hosts(cx).as_deref(), Some(r#"{"tile.openstreetmap.org":true}"#));
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
    // A redirect may lead only to the source's own host or another allowed one.
    assert!(loads[..before].iter().all(|l| l.redirect_hosts.is_empty()), "nothing else was allowed then");
    assert!(loads[before..].iter().all(|l| l.redirect_hosts == [OSM]), "OSM is the other allowed host");
}

/// A catalog switch makes every pending load unreachable: the loads are cancelled and a
/// result that arrives anyway is dropped. The answer is this machine's, so the new
/// catalog's map does not ask again; it fetches its own view once its settings are read.
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
    assert!(!m.has("map-consent", cx), "the answer is per machine, not per catalog");
    assert!(m.fake.count() > pending, "the new catalog's view loads from the allowed host");
    m.loads_to(OSM);
}

/// The tile URL stays a catalog setting, written to the catalog its settings were read from:
/// a save that lands after the core switched (event not delivered yet) fails closed instead
/// of setting the new catalog's URL.
#[gpui_kit::test]
fn a_tile_url_saved_across_a_switch_does_not_reach_the_new_catalog(cx: &mut TestAppContext) {
    let dir = TempDir::new("map-url-switch");
    let m = open_map(&dir, &[OSLO], cx);
    m.click("map-consent-deny", cx);
    let state = map_state(&m, cx);
    let other = dir.0.join("other");
    let b = chairphoto_core::catalog::Catalog::open(&other.join("b.chairphoto"), &other).unwrap();
    chairphoto_core::app::detach_catalog_and_trip_jobs(&m.app.state).unwrap();
    chairphoto_core::app::publish_catalog_and_reset_jobs(&m.app.state, b).unwrap();
    state.update(cx, |s, cx| s.set_tile_url("https://tiles.example.org/{z}/{x}/{y}.png", cx)).unwrap();
    work(&m.app, cx);
    let url_key = format!("{MAP_MODULE_ID}.tileUrl");
    assert_eq!(m.setting(&url_key), None, "the old catalog's URL landed in the new catalog");
}

fn machine_hosts(cx: &mut TestAppContext) -> Option<String> {
    cx.update(|cx| MachinePrefs::read(cx, MACHINE_TILE_HOSTS))
}

/// Gate #119, and #214's fail-closed fix. Here every write fails (the store's directory is
/// a file): the catalog keeps its copy (gate #119, unchanged), and — unlike before #214 —
/// the merge is not applied to this machine's answers either, in memory, in `MachinePrefs`'s
/// own cache, or on disk: the map still asks about the legacy hosts, and the failure is
/// surfaced (`MapState::consent_write_error`). `set_then`'s candidate used to fill
/// `MachinePrefs`'s cache before it knew whether the write would land (#214 M1); it no
/// longer does, so `machine_hosts` reads `None` here too, not just the file on disk. Once the
/// store can be written, the next read of the catalog merges, saves and only then empties
/// the copy.
#[gpui_kit::test]
fn a_failed_machine_prefs_write_keeps_the_catalogs_old_answers(cx: &mut TestAppContext) {
    let dir = TempDir::new("map-migrate-fail");
    cx.update(|cx| cx.set_global(MapTiles(Arc::new(FakeTiles::default()))));
    let app = start(cx);
    let blocker = dir.0.join("blocker");
    std::fs::write(&blocker, "a file where the store's directory should be").unwrap();
    let prefs = blocker.join(MACHINE_PREFS_FILE);
    cx.update(|cx| cx.set_global(MachinePrefs::load(prefs.clone())));
    open_catalog_with_photos(&app, &dir, 1, cx);
    let legacy = r#"{"b.example":true,"tile.openstreetmap.org":false}"#;
    app.state.catalog.lock().unwrap().as_ref().unwrap().set_setting(LEGACY_HOSTS, legacy).unwrap();
    work(&app, cx);
    cx.update(|cx| ModuleRegistry::enable(&app.wired.modules, MAP_MODULE_ID, cx));
    work(&app, cx);
    let setting = |app: &App| app.state.catalog.lock().unwrap().as_ref().unwrap().get_setting(LEGACY_HOSTS).unwrap();
    assert!(!prefs.exists(), "the write failed");
    assert_eq!(setting(&app).as_deref(), Some(legacy), "the catalog keeps its answers: nothing was saved");
    let map_state_entity = map_state_via_settings(&app, cx);
    assert!(
        map_state_entity.read_with(cx, |s, _| s.host_consent().is_empty()),
        "#214: not merged in memory either, so the legacy hosts still ask"
    );
    assert!(
        map_state_entity.read_with(cx, |s, _| s.consent_write_error().is_some()),
        "the failed write is surfaced"
    );
    assert_eq!(machine_hosts(cx), None, "#214 M1: the failed candidate never reached MachinePrefs's cache either");

    std::fs::remove_file(&blocker).unwrap();
    open_catalog_with_photos(&app, &dir, 1, cx); // the same catalog, read again
    work(&app, cx);
    assert_eq!(MachinePrefs::load(prefs).get(MACHINE_TILE_HOSTS), Some(legacy), "saved now");
    assert_eq!(setting(&app).as_deref(), Some("{}"), "and only then emptied");
    assert_eq!(machine_hosts(cx).as_deref(), Some(legacy), "the machine's copy matches what was saved");
}

// --- r5 review probes (#214 M1) ------------------------------------------------------------
//
// `MachinePrefs::persist` used to insert a `set_then` candidate into its in-memory `values`
// before the write even started, and left it there on a failed write: `MapState::new` reads
// `MachinePrefs` directly, so a module disable/enable after an unconfirmed (or failed)
// migration could adopt a legacy Allow that was never actually durable (P214a), and a later
// successful write of an unrelated key (here, Preferences' own "appearance.mode") would
// persist that unconfirmed merge to disk regardless (P214b).

/// P214a: after a migration that never confirmed (the store is in memory only, so
/// `set_then` always fails), disabling and re-enabling the Map module — a fresh
/// `MapState::new` — must still not allow the legacy host. Before the fix, the failed
/// candidate was already sitting in `MachinePrefs`'s cache, so the fresh state adopted it.
#[gpui_kit::test]
fn probe_r5_module_reload_after_failed_migration_does_not_allow_legacy_host(cx: &mut TestAppContext) {
    let dir = TempDir::new("r5-map-reload");
    let fake = Arc::new(FakeTiles::default());
    cx.update(|cx| cx.set_global(MapTiles(fake.clone())));
    let app = start(cx);
    open_catalog_with_photos(&app, &dir, 1, cx);
    seed_b(&app);
    work(&app, cx);
    cx.update(|cx| ModuleRegistry::enable(&app.wired.modules, MAP_MODULE_ID, cx));
    work(&app, cx);
    let state = map_state_via_settings(&app, cx);
    assert!(state.read_with(cx, |s, _| s.host_consent().is_empty()), "precondition: not merged");
    assert_eq!(machine_hosts(cx), None, "precondition: nothing in MachinePrefs's cache either");

    cx.update(|cx| ModuleRegistry::disable(&app.wired.modules, MAP_MODULE_ID, cx));
    work(&app, cx);
    cx.update(|cx| ModuleRegistry::enable(&app.wired.modules, MAP_MODULE_ID, cx));
    work(&app, cx);
    let state = map_state_via_settings(&app, cx);
    let b = state.read_with(cx, |s, _| s.host_consent().get("b.example"));
    assert_ne!(b, Consent::Allowed, "the unconfirmed legacy Allow must not reach a fresh MapState");
}

/// P214b: a transient write failure, then an unrelated `MachinePrefs::set` (Preferences'
/// "appearance.mode") that succeeds, must not persist the unconfirmed merge to disk — its
/// candidate was never in `values` to ride along on that later, unrelated write.
#[gpui_kit::test]
fn probe_r5_unrelated_pref_write_does_not_persist_an_unconfirmed_merge(cx: &mut TestAppContext) {
    let dir = TempDir::new("r5-map-flaky");
    cx.update(|cx| cx.set_global(MapTiles(Arc::new(FakeTiles::default()))));
    let app = start(cx);
    let blocker = dir.0.join("blocker");
    std::fs::write(&blocker, "x").unwrap();
    let prefs = blocker.join(MACHINE_PREFS_FILE);
    cx.update(|cx| cx.set_global(MachinePrefs::load(prefs.clone())));
    open_catalog_with_photos(&app, &dir, 1, cx);
    seed_b(&app);
    work(&app, cx);
    cx.update(|cx| ModuleRegistry::enable(&app.wired.modules, MAP_MODULE_ID, cx));
    work(&app, cx);
    let state = map_state_via_settings(&app, cx);
    assert!(state.read_with(cx, |s, _| s.host_consent().is_empty()), "precondition: not merged");

    std::fs::remove_file(&blocker).unwrap();
    cx.update(|cx| MachinePrefs::set(cx, "appearance.mode", "standard"));
    work(&app, cx);
    let on_disk = MachinePrefs::load(prefs).get(MACHINE_TILE_HOSTS).map(str::to_string);
    assert!(on_disk.is_none_or(|j| !j.contains("b.example")), "the unconfirmed merge must not be persisted by an unrelated write");
    assert_eq!(
        state.read_with(cx, |s, _| s.host_consent().get("b.example")),
        Consent::Unknown,
        "and still not adopted in memory"
    );
}

/// Answers stored per catalog by the first port move to this machine on each catalog's
/// first read: only allowed/denied entries; where catalogs (or the machine) disagree,
/// denied wins. The catalog's copy is emptied so it merges once — a later Allow in
/// Preferences is not undone by reopening that catalog. Preferences' Map tab lists the
/// machine's answers and edits them.
#[gpui_kit::test]
fn per_catalog_answers_migrate_to_this_machine_and_show_in_preferences(cx: &mut TestAppContext) {
    let dir = TempDir::new("map-migrate");
    let fake = Arc::new(FakeTiles::default());
    cx.update(|cx| cx.set_global(MapTiles(fake.clone())));
    let app = start(cx);
    // On disk (in the test's dir): the catalog's copy is emptied only once this is written.
    let prefs = dir.0.join("prefs").join(MACHINE_PREFS_FILE);
    cx.update(|cx| cx.set_global(MachinePrefs::load(prefs.clone())));
    open_catalog_with_photos(&app, &dir, 1, cx);
    {
        let guard = app.state.catalog.lock().unwrap();
        guard.as_ref().unwrap().set_setting(
            LEGACY_HOSTS,
            r#"{"tile.openstreetmap.org":true,"b.example":true,"odd.example":"yes"}"#,
        ).unwrap();
    }
    work(&app, cx);
    cx.update(|cx| ModuleRegistry::enable(&app.wired.modules, MAP_MODULE_ID, cx));
    work(&app, cx);
    assert_eq!(machine_hosts(cx).as_deref(), Some(r#"{"b.example":true,"tile.openstreetmap.org":true}"#));
    let setting = |app: &App| app.state.catalog.lock().unwrap().as_ref().unwrap().get_setting(LEGACY_HOSTS).unwrap();
    assert_eq!(setting(&app).as_deref(), Some("{}"), "the catalog's copy was emptied");
    assert_eq!(MachinePrefs::load(prefs.clone()).get(MACHINE_TILE_HOSTS), machine_hosts(cx).as_deref(), "after the machine's was saved");
    show_map(&app, cx);
    assert!(fake.count() > 0, "allowed by the migrated answer: no question");

    // A second catalog blocked OSM: denied wins.
    let other = TempDir::new("map-migrate-b");
    let db = other.0.join("photos.chairphoto");
    {
        let c = chairphoto_core::catalog::Catalog::open(&db, &other.0.join("photos")).unwrap();
        c.set_setting(LEGACY_HOSTS, r#"{"tile.openstreetmap.org":false}"#).unwrap();
    }
    open_catalog_with_photos(&app, &other, 1, cx);
    work(&app, cx);
    assert_eq!(machine_hosts(cx).as_deref(), Some(r#"{"b.example":true,"tile.openstreetmap.org":false}"#));

    // Preferences → Map lists the machine's answers; Allow there sticks across catalogs.
    click(&app, "rail-preferences", cx);
    work(&app, cx);
    click(&app, "prefs-tab-module-map", cx);
    work(&app, cx);
    let listed = cx
        .update_window(app.window(), |_, window, cx| {
            window.render_frame(cx);
            window.try_find("map-host-b.example").is_some()
        })
        .unwrap();
    assert!(listed, "the host list is in Preferences");
    click(&app, "map-host-toggle-tile.openstreetmap.org", cx);
    work(&app, cx);
    assert_eq!(machine_hosts(cx).as_deref(), Some(r#"{"b.example":true,"tile.openstreetmap.org":true}"#));
    // Away and back to the catalog that blocked OSM: already merged, so the Allow stands.
    open_catalog_with_photos(&app, &dir, 1, cx);
    work(&app, cx);
    open_catalog_with_photos(&app, &other, 1, cx);
    work(&app, cx);
    assert_eq!(setting(&app).as_deref(), Some("{}"));
    assert_eq!(machine_hosts(cx).as_deref(), Some(r#"{"b.example":true,"tile.openstreetmap.org":true}"#));
}

// --- "Ask again" across a re-merge (#198) ---------------------------------------------

/// The catalog these tests open holds an old per-catalog Allow for `b.example`, and its tile
/// URL names that host.
const B_URL: &str = "https://b.example/{z}/{x}/{y}.png";
const B_LEGACY: &str = r#"{"b.example":true}"#;

fn seed_b(app: &App) {
    let guard = app.state.catalog.lock().unwrap();
    let c = guard.as_ref().unwrap();
    c.set_setting(LEGACY_HOSTS, B_LEGACY).unwrap();
    c.set_setting(&format!("{MAP_MODULE_ID}.tileUrl"), B_URL).unwrap();
}

/// Preferences → Map → "Ask again" for `b.example`, then back to the map.
fn ask_again_for_b(app: &App, cx: &mut TestAppContext) {
    click(app, "rail-preferences", cx);
    work(app, cx);
    click(app, "prefs-tab-module-map", cx);
    work(app, cx);
    click(app, "map-host-forget-b.example", cx);
    work(app, cx);
    show_map(app, cx);
}

/// The map shows the consent card for `b.example`, and nothing more was fetched than `before`.
fn asks_for_b(m: &Map, before: usize, cx: &mut TestAppContext) {
    assert_eq!(map_state(m, cx).read_with(cx, |s, _| s.consent()), Consent::Unknown, "b.example is allowed again");
    assert!(m.has("map-consent", cx), "the map asks about b.example");
    assert_eq!(m.fake.count(), before, "a tile was fetched from a host the user reset");
}

/// Where this machine's preferences live in [`ask_again_survives_a_reread`].
enum Prefs {
    /// No app data dir: `load_default` keeps them in memory (the headless default).
    InMemory,
    /// A store whose every write fails (its directory is a file).
    WriteFails,
}

/// **#214** (a doubt the #198 fix reported). The machine's preferences cannot be saved (in
/// memory only, or every write fails): the merge is held out of memory until it is durable
/// (`MapState::migrate_consent`'s doc comment), so the catalog's legacy Allow for `b.example`
/// is never applied, not even for this session. The host asks on the first read, and keeps
/// asking across repeated reads of the same catalog ("restarts") while the store stays
/// broken — never silently re-allowed, since nothing was ever merged for a reset to undo.
/// The failure is surfaced (`consent_write_error`); the catalog keeps its old answer (gate
/// #119) so a store that starts working still merges it.
fn an_undurable_store_never_merges_the_catalogs_legacy_allow(prefs: Prefs, cx: &mut TestAppContext) {
    let dir = TempDir::new("map-undurable");
    let fake = Arc::new(FakeTiles::default());
    cx.update(|cx| cx.set_global(MapTiles(fake.clone())));
    let app = start(cx);
    let prefs_path = if let Prefs::WriteFails = prefs {
        let blocker = dir.0.join("blocker");
        std::fs::write(&blocker, "a file where the store's directory should be").unwrap();
        let path = blocker.join(MACHINE_PREFS_FILE);
        cx.update(|cx| cx.set_global(MachinePrefs::load(path.clone())));
        Some(path)
    } else {
        None
    };
    open_catalog_with_photos(&app, &dir, 1, cx);
    seed_b(&app);
    work(&app, cx);
    cx.update(|cx| ModuleRegistry::enable(&app.wired.modules, MAP_MODULE_ID, cx));
    work(&app, cx);
    let m = Map { app, fake, ids: Vec::new() };
    let before = m.fake.count();
    show_map(&m.app, cx);
    asks_for_b(&m, before, cx);
    let state = map_state(&m, cx);
    assert!(state.read_with(cx, |s, _| s.host_consent().is_empty()), "never merged, not even in memory");
    assert!(state.read_with(cx, |s, _| s.consent_write_error().is_some()), "the failed write is surfaced");
    assert_eq!(m.setting(LEGACY_HOSTS).as_deref(), Some(B_LEGACY), "the catalog kept its old answer");
    if let Some(path) = &prefs_path {
        // The actual file on disk never got the merge either — and, since #214 M1, nor does
        // `MachinePrefs`'s own in-memory cache, which an unconfirmed `set_then` candidate
        // used to fill in before it knew whether the write would land.
        assert!(!path.exists(), "the write failed: no file, let alone one with the merge");
        assert_eq!(machine_hosts(cx), None, "nor MachinePrefs's cache");
    }

    // A "restart": the same catalog read again, the store still just as broken. Still asks,
    // never silently re-allowed — there was never a merge, in memory or on disk, for a reset
    // to have to survive.
    open_catalog_with_photos(&m.app, &dir, 1, cx);
    work(&m.app, cx);
    show_map(&m.app, cx);
    asks_for_b(&m, before, cx);
    let state = map_state(&m, cx);
    assert!(state.read_with(cx, |s, _| s.host_consent().is_empty()), "still never merged after the \"restart\"");
}

#[gpui_kit::test]
fn an_undurable_store_never_merges_the_catalogs_legacy_allow_in_memory(cx: &mut TestAppContext) {
    an_undurable_store_never_merges_the_catalogs_legacy_allow(Prefs::InMemory, cx);
}

#[gpui_kit::test]
fn an_undurable_store_never_merges_the_catalogs_legacy_allow_when_writes_fail(cx: &mut TestAppContext) {
    an_undurable_store_never_merges_the_catalogs_legacy_allow(Prefs::WriteFails, cx);
}

/// **#214**: unlike the legacy migration, the user's own Allow/Block/Ask again
/// ([`MapState::set_consent`]) is not held back — it is not an invisible background merge —
/// but a write that cannot be saved is still surfaced, so the user knows that answer may not
/// survive a restart (Preferences' wording no longer claims every answer shown will be
/// re-asked: an earlier durable one is unaffected by this failure).
#[gpui_kit::test]
fn a_failed_set_consent_write_still_applies_but_is_surfaced(cx: &mut TestAppContext) {
    let dir = TempDir::new("map-set-consent-fail");
    cx.update(|cx| cx.set_global(MapTiles(Arc::new(FakeTiles::default()))));
    let app = start(cx);
    let blocker = dir.0.join("blocker");
    std::fs::write(&blocker, "a file where the store's directory should be").unwrap();
    cx.update(|cx| cx.set_global(MachinePrefs::load(blocker.join(MACHINE_PREFS_FILE))));
    open_catalog_with_photos(&app, &dir, 1, cx);
    work(&app, cx);
    cx.update(|cx| ModuleRegistry::enable(&app.wired.modules, MAP_MODULE_ID, cx));
    work(&app, cx);
    let state = map_state_via_settings(&app, cx);
    assert!(state.read_with(cx, |s, _| s.consent_write_error().is_none()), "nothing attempted yet");

    state.update(cx, |s, cx| s.set_consent("tile.example", Some(true), cx));
    assert_eq!(state.read_with(cx, |s, _| s.host_consent().get("tile.example")), Consent::Allowed, "applied at once");
    work(&app, cx);
    assert!(state.read_with(cx, |s, _| s.consent_write_error().is_some()), "the failed write is surfaced");
}

/// **Security fix**: `migrate_consent`'s confirm step used to reassign `self.consent` to a
/// snapshot taken before its write started. Here the user Blocks `b.example` — whose legacy
/// copy would Allow it — while the migration's write is still queued (held at the same
/// checkpoint `ask_again_survives_a_reread_after_a_switch_interrupted_the_clear` uses).
/// Releasing the write must not undo the Block: it must win, in memory and on disk, and
/// nothing is fetched from the host.
#[gpui_kit::test]
fn a_block_racing_the_migrations_write_is_not_overwritten(cx: &mut TestAppContext) {
    let dir = TempDir::new("map-race-block");
    let fake = Arc::new(FakeTiles::default());
    cx.update(|cx| cx.set_global(MapTiles(fake.clone())));
    let app = start(cx);
    let prefs = dir.0.join("prefs").join(MACHINE_PREFS_FILE);
    cx.update(|cx| cx.set_global(MachinePrefs::load(prefs.clone())));
    open_catalog_with_photos(&app, &dir, 1, cx);
    seed_b(&app); // the catalog's legacy copy allows b.example
    work(&app, cx);
    cx.update(|cx| ModuleRegistry::enable(&app.wired.modules, MAP_MODULE_ID, cx));
    // The reads run and land; the merge's write (b.example: true) is queued, held here.
    cx.update(|cx| Runner::get(cx).run_pending());
    cx.run_until_parked();
    assert!(cx.update(|cx| Runner::get(cx).pending()) > 0, "the migration's write is still queued");

    // The user Blocks b.example while that write is in flight.
    let m = Map { app, fake, ids: Vec::new() };
    let state = map_state_via_settings(&m.app, cx);
    state.update(cx, |s, cx| s.set_consent("b.example", Some(false), cx));
    assert_eq!(state.read_with(cx, |s, _| s.host_consent().get("b.example")), Consent::Denied, "applied at once");

    // Release both writes (the migration's, then the user's — or any interleaving).
    work(&m.app, cx);

    assert_eq!(
        state.read_with(cx, |s, _| s.host_consent().get("b.example")),
        Consent::Denied,
        "the migration's confirm must not overwrite the user's Block"
    );
    assert_eq!(
        MachinePrefs::load(prefs).get(MACHINE_TILE_HOSTS).as_deref(),
        Some(r#"{"b.example":false}"#),
        "and the persisted copy agrees, not the migration's stale Allow"
    );
    show_map(&m.app, cx);
    assert_eq!(m.fake.count(), 0, "nothing fetched from a host the user blocked");
}

/// Same race, "Ask again" instead of Block: the host must keep asking, never be silently
/// re-allowed by the migration's stale snapshot landing after the reset.
#[gpui_kit::test]
fn an_ask_again_racing_the_migrations_write_is_not_overwritten(cx: &mut TestAppContext) {
    let dir = TempDir::new("map-race-ask");
    let fake = Arc::new(FakeTiles::default());
    cx.update(|cx| cx.set_global(MapTiles(fake.clone())));
    let app = start(cx);
    let prefs = dir.0.join("prefs").join(MACHINE_PREFS_FILE);
    cx.update(|cx| cx.set_global(MachinePrefs::load(prefs.clone())));
    open_catalog_with_photos(&app, &dir, 1, cx);
    seed_b(&app);
    work(&app, cx);
    cx.update(|cx| ModuleRegistry::enable(&app.wired.modules, MAP_MODULE_ID, cx));
    cx.update(|cx| Runner::get(cx).run_pending());
    cx.run_until_parked();
    assert!(cx.update(|cx| Runner::get(cx).pending()) > 0, "the migration's write is still queued");

    let m = Map { app, fake, ids: Vec::new() };
    let state = map_state_via_settings(&m.app, cx);
    state.update(cx, |s, cx| s.set_consent("b.example", None, cx)); // "Ask again"
    assert_eq!(state.read_with(cx, |s, _| s.host_consent().get("b.example")), Consent::Unknown, "applied at once");

    work(&m.app, cx);

    assert_eq!(
        state.read_with(cx, |s, _| s.host_consent().get("b.example")),
        Consent::Unknown,
        "the migration's confirm must not silently re-allow the host"
    );
    assert_eq!(
        MachinePrefs::load(prefs).get(MACHINE_TILE_HOSTS).as_deref(),
        Some(r#"{"b.example":"ask"}"#),
        "and the persisted copy still asks, not allows"
    );
    show_map(&m.app, cx);
    let before = m.fake.count();
    asks_for_b(&m, before, cx);
}

/// **#198**, the success path: the machine's copy is saved, but the core switches to
/// another catalog before the clear runs, so `with_catalog_as(from)` refuses it and the old
/// catalog keeps its answers. The user sends `b.example` back to "ask"; on that catalog's
/// next read the answers merge again, and the host still asks. The reset is on disk too.
#[gpui_kit::test]
fn ask_again_survives_a_reread_after_a_switch_interrupted_the_clear(cx: &mut TestAppContext) {
    let dir = TempDir::new("map-ask-again-switch");
    let fake = Arc::new(FakeTiles::default());
    cx.update(|cx| cx.set_global(MapTiles(fake.clone())));
    let app = start(cx);
    let prefs = dir.0.join("prefs").join(MACHINE_PREFS_FILE);
    cx.update(|cx| cx.set_global(MachinePrefs::load(prefs.clone())));
    open_catalog_with_photos(&app, &dir, 1, cx);
    seed_b(&app);
    work(&app, cx);
    cx.update(|cx| ModuleRegistry::enable(&app.wired.modules, MAP_MODULE_ID, cx));
    // The reads run and land; the merge queues the machine's write (and, after it, the clear).
    cx.update(|cx| Runner::get(cx).run_pending());
    cx.run_until_parked();
    // #214 M1: computed and queued, but not yet confirmed durable — so not yet in
    // `MachinePrefs`'s own cache either (`migrate_consent` uses `set_confirmed_then`
    // precisely so this moment cannot be mistaken for "merged").
    assert_eq!(machine_hosts(cx), None, "not yet confirmed");
    assert!(cx.update(|cx| Runner::get(cx).pending()) > 0, "the write and the clear are still queued");
    // The core switches before they run (`catalog:switched` not delivered yet).
    let (other, _) = colliding_catalog(&dir, "other", 1);
    core_switch(&app, other);
    work(&app, cx);
    assert_eq!(MachinePrefs::load(prefs.clone()).get(MACHINE_TILE_HOSTS), Some(B_LEGACY), "the machine's copy is saved");
    let a = chairphoto_core::catalog::Catalog::open(&dir.0.join("photos.chairphoto"), &dir.0.join("photos")).unwrap();
    assert_eq!(a.get_setting(LEGACY_HOSTS).unwrap().as_deref(), Some(B_LEGACY), "the clear was refused: the old catalog keeps its copy");
    drop(a);
    app.state.send(CoreEvent::CatalogSwitched("other".into())); // now it arrives
    cx.run_until_parked();
    work(&app, cx);

    let m = Map { app, fake, ids: Vec::new() };
    ask_again_for_b(&m.app, cx);
    let before = m.fake.count();
    open_catalog_with_photos(&m.app, &dir, 1, cx); // back to the first catalog
    work(&m.app, cx);
    show_map(&m.app, cx);
    asks_for_b(&m, before, cx);
    assert_eq!(MachinePrefs::load(prefs).get(MACHINE_TILE_HOSTS), Some(r#"{"b.example":"ask"}"#), "the reset is saved");
    assert_eq!(m.setting(LEGACY_HOSTS).as_deref(), Some("{}"), "and the catalog's copy is emptied now");
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
    let mut ids = (*strip.ids).clone();
    ids.sort();
    assert_eq!(ids, m.ids);
    let selected = m.app.wired.shell.read_with(cx, |s, _| (s.library.selection().active_id, s.surface.clone()));
    assert_eq!(selected, (Some(strip.ids[0]), Surface::Module(MAP_VIEW_ID.into())), "selected quietly, still on the map");

    view.update(cx, |v, cx| v.filmstrip_select(strip.ids[1], cx));
    m.click("map-show-in-library", cx);
    let after = m.app.wired.shell.read_with(cx, |s, _| (s.library.selection().active_id, s.surface.clone()));
    assert_eq!(after, (Some(strip.ids[1]), Surface::Library));
}

// --- the filmstrip over a large cluster (#162) ----------------------------------------

/// The strip's wanted order: on screen first, nearest the active photo (clamped into the
/// visible range; ahead before behind), then the overscan nearest the screen first, never
/// past the ends.
#[test]
fn strip_wanted_is_nearest_first_and_bounded() {
    assert_eq!(strip_wanted(10..14, 100, Some(12))[..4], [12, 13, 11, 10]);
    // An active photo off screen: the nearest visible frame leads.
    assert_eq!(strip_wanted(10..14, 100, Some(90))[..4], [13, 12, 11, 10]);
    assert_eq!(strip_wanted(10..14, 100, None)[..4], [10, 11, 12, 13]);
    let w = strip_wanted(10..14, 100, Some(12));
    assert_eq!(w.len(), 4 + 2 * STRIP_OVERSCAN);
    assert_eq!(w[4..6], [14, 9], "the overscan nearest the screen first, ahead before behind");
    assert_eq!(*w.iter().min().unwrap(), 10 - STRIP_OVERSCAN);
    assert_eq!(*w.iter().max().unwrap(), 13 + STRIP_OVERSCAN);
    // Clamped at both ends.
    let end = strip_wanted(995..1000, 1000, Some(0));
    assert_eq!(end[..5], [995, 996, 997, 998, 999]);
    assert!(end.iter().all(|&i| (995 - STRIP_OVERSCAN..1000).contains(&i)));
    assert_eq!(strip_wanted(0..3, 3, Some(1)), vec![1, 2, 0]);
    assert!(strip_wanted(5..5, 10, None).is_empty());
}

/// A map whose `n` photos are all at one place, on a [`FakePool`], with the cluster's
/// filmstrip open.
fn open_big_strip(tag: &str, n: usize, cx: &mut TestAppContext) -> (TempDir, Map, Arc<FakePool>, Entity<MapView>) {
    let dir = TempDir::new(tag);
    let pool = Arc::new(FakePool::default());
    let m = open_map_with(&dir, &vec![OSLO; n], Some(pool.clone()), cx);
    m.click("map-consent-deny", cx);
    let view = m.view(cx);
    let (x, y) = m.screen(OSLO, cx);
    m.press_at((x, y), 1, cx);
    let len = view.read_with(cx, |v, _| v.filmstrip.as_ref().map(|f| f.ids.len()));
    assert_eq!(len, Some(n), "one marker, every photo in its strip");
    (dir, m, pool, view)
}

fn strip_ids(view: &Entity<MapView>, cx: &mut TestAppContext) -> Arc<Vec<i64>> {
    view.read_with(cx, |v, _| v.filmstrip.as_ref().unwrap().ids.clone())
}

/// What the strip's claim holds now.
fn strip_claim(m: &Map, view: &Entity<MapView>, cx: &mut TestAppContext) -> std::collections::HashSet<i64> {
    let claim = view.read_with(cx, |v, _| v.strip_claim);
    match claim {
        Some(c) => m.app.wired.images.read_with(cx, |s, _| s.claim(c).into_iter().map(|(p, _)| p).collect()),
        None => Default::default(),
    }
}

fn requested(pool: &FakePool, photo: i64) -> bool {
    let key = JobKey::photo(photo, ImageKind::Thumb);
    pool.batches.lock().unwrap().iter().flatten().any(|k| *k == key)
}

fn pending(m: &Map, photo: i64, cx: &mut TestAppContext) -> bool {
    m.app.wired.images.read_with(cx, |s, _| s.is_pending(photo, ImageKind::Thumb))
}

/// Scroll the strip so frame `index` is at `strategy`, and draw.
fn scroll_strip(m: &Map, view: &Entity<MapView>, index: usize, strategy: ScrollStrategy, cx: &mut TestAppContext) {
    view.read_with(cx, |v, _| v.strip_scroll.scroll_to_item(index, strategy));
    view.update(cx, |_, cx| cx.notify());
    frame(&m.app, cx);
    frame(&m.app, cx);
}

/// **#162.** A 1000-photo cluster: the strip asks for the frames on screen (plus the
/// overscan) — not the first 200 — and follows the scroll to the end; the frames that left
/// are released (no longer claimed, their queued renders cancelled), and only the frames on
/// screen are built. Closing the strip lets go of everything.
#[gpui_kit::test]
fn a_large_clusters_strip_asks_for_what_is_on_screen_as_it_scrolls(cx: &mut TestAppContext) {
    let (_dir, m, pool, view) = open_big_strip("map-strip-big", 1000, cx);
    let ids = strip_ids(&view, cx);
    let claim = strip_claim(&m, &view, cx);
    assert!(claim.contains(&ids[0]), "the first frames on screen are held");
    assert!(claim.len() < 60, "a window, not the cluster: {}", claim.len());
    assert!(!claim.contains(&ids[500]) && !claim.contains(&ids[999]));
    assert!(!requested(&pool, ids[500]), "nothing far down the strip is asked for yet");
    assert!(m.has(("map-thumb", ids[0] as u64), cx));
    assert!(!m.has(("map-thumb", ids[999] as u64), cx), "frames off screen are not built");
    assert!(pending(&m, ids[0], cx));

    // Past the old 200 cap.
    scroll_strip(&m, &view, 500, ScrollStrategy::Center, cx);
    assert!(requested(&pool, ids[500]) && strip_claim(&m, &view, cx).contains(&ids[500]));

    // To the end. (The last photos may already be pending for the Library grid, which
    // opened at the newest; the strip's claim is what says it wants them.)
    scroll_strip(&m, &view, 999, ScrollStrategy::Bottom, cx);
    assert!(pending(&m, ids[999], cx), "the last frame is asked for once it is on screen");
    let claim = strip_claim(&m, &view, cx);
    assert!(!claim.contains(&ids[500]), "the middle was let go");
    assert!(claim.contains(&ids[999]) && claim.contains(&ids[999 - STRIP_OVERSCAN]));
    assert!(!claim.contains(&ids[0]), "a frame that left the window is let go");
    assert!(!pending(&m, ids[0], cx), "and its queued render released");
    assert!(pool.cancelled.lock().unwrap().contains(&JobKey::photo(ids[0], ImageKind::Thumb)));
    assert!(m.has(("map-thumb", ids[999] as u64), cx));
    assert!(!m.has(("map-thumb", ids[0] as u64), cx));

    // A still strip asks for nothing and lets nothing go, frame after frame (the list's
    // measuring build must not move the window).
    let (batches, cancelled) = (pool.batches.lock().unwrap().len(), pool.cancelled.lock().unwrap().len());
    for _ in 0..3 {
        view.update(cx, |_, cx| cx.notify());
        frame(&m.app, cx);
    }
    assert_eq!((pool.batches.lock().unwrap().len(), pool.cancelled.lock().unwrap().len()), (batches, cancelled));
    assert_eq!(strip_claim(&m, &view, cx), claim);

    // A landed frame shows.
    pool.finish(&JobKey::photo(ids[999], ImageKind::Thumb), Ok(pixels(4, 4)));
    cx.run_until_parked();
    let images = m.app.wired.images.clone();
    let shown = view.read_with(cx, |v, cx| {
        let strip = v.filmstrip.clone().unwrap();
        frame_image(&strip, images.read(cx).peek(ids[999], ImageKind::Thumb)).is_some()
    });
    assert!(shown);

    m.click("map-filmstrip-close", cx);
    assert!(strip_claim(&m, &view, cx).is_empty(), "closing lets go of every frame");
    assert!(!pending(&m, ids[998], cx));
}

/// #186: a strip frame is filled by its thumbnail inside its 2 px ring, portrait and
/// landscape (`map.css`: `object-fit: cover`), not a portrait element taller than the frame.
#[gpui_kit::test]
fn the_strips_frames_are_filled_by_their_thumbnail(cx: &mut TestAppContext) {
    use crate::loupe::fit_tests::{assert_fills, LANDSCAPE, PORTRAIT};
    let (_dir, m, pool, view) = open_big_strip("map-strip-fit", 2, cx);
    let ids = strip_ids(&view, cx);
    let frames = [(ids[0], PORTRAIT), (ids[1], LANDSCAPE)];
    for &(id, (w, h)) in &frames {
        pool.finish(&JobKey::photo(id, ImageKind::Thumb), Ok(pixels(w, h)));
    }
    cx.run_until_parked();
    frame(&m.app, cx);
    cx.update_window(m.app.window(), |_, window, _| {
        for &(id, image) in &frames {
            let what = format!("strip frame {id} {image:?}");
            assert_fills(&what, window, ("map-thumb", id as u64), ("map-thumb-picture", id as u64), 2.);
        }
    })
    .unwrap();
}

/// The strip's frames are asked for nearest the active photo first: the active one, then
/// N+1, N−1 (the navigation rule, AGENTS.md § Performance).
#[gpui_kit::test]
fn the_strip_asks_nearest_the_active_photo_first(cx: &mut TestAppContext) {
    let (_dir, m, pool, view) = open_big_strip("map-strip-order", 1000, cx);
    let ids = strip_ids(&view, cx);
    view.update(cx, |v, cx| v.filmstrip_select(ids[500], cx));
    let before = pool.batches.lock().unwrap().len();
    scroll_strip(&m, &view, 500, ScrollStrategy::Center, cx);
    let batches = pool.batches.lock().unwrap()[before..].to_vec();
    let thumb = |i: usize| JobKey::photo(ids[i], ImageKind::Thumb);
    let batch = batches.iter().find(|b| b.contains(&thumb(500))).expect("the strip's batch");
    assert_eq!(batch[..3], [thumb(500), thumb(501), thumb(499)], "{batch:?}");
}

/// **Catalog identity** (map #92). Catalog B's photos collide with the strip's ids. The core
/// switches while a frame's render is running:
/// - `catalog:switched` withheld: the render finishes in B; the strip does not show B's
///   pixels under A's frame. Then the event arrives and the strip closes, holding nothing.
/// - delivered: the strip closes and holds nothing, and its old ids are never asked for
///   again (they would be B's photos).
fn strip_across_a_switch(delivered: bool, cx: &mut TestAppContext) {
    let (dir, m, pool, view) = open_big_strip(if delivered { "map-strip-sw-ev" } else { "map-strip-sw" }, 30, cx);
    let ids = strip_ids(&view, cx);
    let key = JobKey::photo(ids[0], ImageKind::Thumb);
    // The Library grid asked for this thumbnail first, bound to A's rows (the store refuses
    // that render in B by itself). Land it, then drop it (a rotation): the strip's own,
    // plain request is the one in flight.
    pool.finish(&key, Ok(pixels(4, 4)));
    cx.run_until_parked();
    m.app.wired.images.update(cx, |s, cx| s.invalidate(ids[0], cx));
    view.update(cx, |_, cx| cx.notify());
    frame(&m.app, cx);
    assert!(pending(&m, ids[0], cx));
    assert!(strip_claim(&m, &view, cx).contains(&ids[0]), "the strip's request");
    pool.start(key.clone());
    let (b, b_ids) = crate::tests::colliding_catalog(&dir, "b", 30);
    assert!(b_ids.contains(&ids[0]), "the ids collide");
    crate::tests::core_switch(&m.app, b);

    if !delivered {
        pool.finish(&key, Ok(pixels(4, 4)));
        cx.run_until_parked();
        frame(&m.app, cx);
        let images = m.app.wired.images.clone();
        let (strip, state) = view.read_with(cx, |v, cx| {
            (v.filmstrip.clone().expect("not closed before the event"), images.read(cx).peek(ids[0], ImageKind::Thumb))
        });
        assert!(matches!(state, ImageState::Ready(_)), "B's render landed in the store (a plain request is not bound)");
        assert!(frame_image(&strip, state).is_none(), "B's pixels are not shown under A's frame");
    }
    crate::tests::deliver_switch(&m.app, cx);
    work(&m.app, cx);
    assert!(view.read_with(cx, |v, _| v.filmstrip.is_none()), "the switch closed the strip");
    assert!(strip_claim(&m, &view, cx).is_empty(), "and it holds nothing");
    let before = pool.batches.lock().unwrap().len();
    for _ in 0..3 {
        view.update(cx, |_, cx| cx.notify());
        frame(&m.app, cx);
    }
    let asked: Vec<JobKey> = pool.batches.lock().unwrap()[before..].iter().flatten().cloned().collect();
    assert!(
        !ids.iter().any(|&id| asked.contains(&JobKey::photo(id, ImageKind::Thumb))),
        "the old strip's ids are not asked for in B: {asked:?}"
    );
}

/// **#199, catalog identity.** The markers on screen were drawn from catalog A. Between that
/// frame and the click, the core switches to B, whose ids collide (`catalog:switched` not
/// delivered), and a read of B lands in the state (a finished scan's re-read). The click is
/// bound to the catalog the markers came from: A's ids are not B's photos, so no strip opens
/// on them and nothing is selected; the next frame draws B's markers.
///
/// The ordering is forced. The test app draws a dirty window when an update's effects are
/// flushed, so B's read is done here and lands in the same update as the press and release:
/// they reach the last frame's handlers before any frame, as input between two frames does.
#[gpui_kit::test]
fn a_marker_click_is_bound_to_the_catalog_its_markers_were_drawn_from(cx: &mut TestAppContext) {
    let dir = TempDir::new("map-strip-bind");
    let m = open_map(&dir, &[OSLO, OSLO2], cx);
    m.click("map-consent-deny", cx);
    let (view, state) = (m.view(cx), map_state(&m, cx));
    let a = state.read_with(cx, |s, _| s.catalog()).expect("A's points are read");
    let (x, y) = m.screen(OSLO, cx);
    let (x2, y2) = m.screen(OSLO2, cx);
    let position = m.at(((x + x2) / 2.0, (y + y2) / 2.0), cx); // draws a frame: A's marker
    const ELSEWHERE: i64 = 424_242;
    m.app.wired.shell.update(cx, |s, _| s.library.select_quiet(ELSEWHERE));

    let (b, b_ids) = crate::tests::colliding_catalog(&dir, "b", 2);
    assert!(m.ids.iter().all(|id| b_ids.contains(id)), "the ids collide");
    crate::tests::core_switch(&m.app, b);
    let read = crate::modules::map::state::read_points(&m.app.state);
    let b_id = read.as_ref().map(|(from, _)| *from).ok();
    assert!(b_id.is_some() && b_id != Some(a), "the read is B's");

    cx.update_window(m.app.window(), |_, window, cx| {
        state.update(cx, |s, cx| {
            s.land_points(read);
            cx.notify();
        });
        let (markers, now) = view.read_with(cx, |v, cx| (v.clusters.len(), v.state.read(cx).catalog()));
        assert_eq!(markers, 1, "no frame came between: A's marker is still the one on screen");
        assert_eq!(now, b_id, "the state has read B");
        window.dispatch_event(
            MouseDownEvent { button: MouseButton::Left, position, modifiers: Modifiers::default(), click_count: 1, first_mouse: false }
                .to_platform_input(),
            cx,
        );
        window.dispatch_event(
            MouseUpEvent { button: MouseButton::Left, position, modifiers: Modifiers::default(), click_count: 1 }.to_platform_input(),
            cx,
        );
    })
    .unwrap();
    let selected = |cx: &mut TestAppContext| m.app.wired.shell.read_with(cx, |s, _| s.library.selection().active_id);
    assert!(view.read_with(cx, |v, _| v.filmstrip.is_none()), "a strip opened on A's ids in B");
    assert_eq!(selected(cx), Some(ELSEWHERE), "A's id was selected in B");
    frame(&m.app, cx);
    assert!(view.read_with(cx, |v, _| v.filmstrip.is_none()));
    assert_eq!(selected(cx), Some(ELSEWHERE));
}

#[gpui_kit::test]
fn the_strip_never_shows_the_new_catalog_before_the_switch_event(cx: &mut TestAppContext) {
    strip_across_a_switch(false, cx);
}

#[gpui_kit::test]
fn the_strip_closes_and_lets_go_after_the_switch_event(cx: &mut TestAppContext) {
    strip_across_a_switch(true, cx);
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

/// Review #181 M1: Apply all over an auto-tag fence (Panorama) between two place fences
/// applies both place fences and says what it applied and what it skipped, instead of
/// "Failed to apply fence" after writing only part of them.
#[gpui_kit::test]
fn apply_all_skips_an_auto_tag_fence_and_says_so(cx: &mut TestAppContext) {
    let dir = TempDir::new("map-apply-auto");
    let m = open_map(&dir, &[OSLO], cx);
    m.click("map-consent-deny", cx);
    let square = vec![(59.90, 10.74), (59.90, 10.76), (59.92, 10.76), (59.92, 10.74)];
    {
        let guard = m.app.state.catalog.lock().unwrap();
        let c = guard.as_ref().unwrap();
        let auto = c.create_tag("Technique/Panorama").unwrap();
        c.conn().execute("UPDATE tags SET auto_rule = 'panorama' WHERE id = ?1", [auto]).unwrap();
        backend::create_fence_for(c, "A", "Places/A", &square).unwrap();
        backend::create_fence_for(c, "Pano", "Technique/Panorama", &square).unwrap();
        backend::create_fence_for(c, "C", "Places/C", &square).unwrap();
    }
    let state = m.view(cx).read_with(cx, |v, _| v.state.clone());
    state.update(cx, |s, cx| s.reload_fences(cx));
    work(&m.app, cx);

    m.click("map-apply-all", cx);
    let status = m.app.wired.model.read_with(cx, |m, _| m.status.to_string());
    assert_eq!(
        status,
        "Applied 2 of 3 fences: 2 photos newly tagged. Skipped \u{201c}Pano\u{201d} \u{2014} an auto-tag can't be \
         assigned by a fence."
    );
    let guard = m.app.state.catalog.lock().unwrap();
    let mut paths: Vec<String> =
        guard.as_ref().unwrap().get_photo_tags(m.ids[0]).unwrap().into_iter().map(|t| t.full_path).collect();
    paths.sort();
    assert_eq!(paths, ["Places/A", "Places/C"]);
}

/// Review #181 r2 N1: one auto-tag fence beside a two-point (no-area) fence reads "0 of 1
/// fence" — singular, and the degenerate fence is not counted as applied.
#[gpui_kit::test]
fn apply_all_counts_one_fence_in_the_singular_and_not_a_degenerate_one(cx: &mut TestAppContext) {
    let dir = TempDir::new("map-apply-one");
    let m = open_map(&dir, &[OSLO], cx);
    m.click("map-consent-deny", cx);
    let square = vec![(59.90, 10.74), (59.90, 10.76), (59.92, 10.76), (59.92, 10.74)];
    {
        let guard = m.app.state.catalog.lock().unwrap();
        let c = guard.as_ref().unwrap();
        let auto = c.create_tag("Technique/Panorama").unwrap();
        c.conn().execute("UPDATE tags SET auto_rule = 'panorama' WHERE id = ?1", [auto]).unwrap();
        backend::create_fence_for(c, "Pano", "Technique/Panorama", &square).unwrap();
        backend::create_fence_for(c, "Line", "Places/Line", &square[..2]).unwrap();
    }
    let state = m.view(cx).read_with(cx, |v, _| v.state.clone());
    state.update(cx, |s, cx| s.reload_fences(cx));
    work(&m.app, cx);

    m.click("map-apply-all", cx);
    let status = m.app.wired.model.read_with(cx, |m, _| m.status.to_string());
    assert_eq!(
        status,
        "Applied 0 of 1 fence: 0 photos newly tagged. Skipped \u{201c}Pano\u{201d} \u{2014} an auto-tag can't be \
         assigned by a fence."
    );
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

/// The map module's state, through its settings panel — for a test that never opens the map
/// view itself (the module is enabled, but no [`Map`]/[`MapView`] exists yet).
fn map_state_via_settings(app: &App, cx: &mut TestAppContext) -> Entity<crate::modules::map::state::MapState> {
    let modules = app.wired.modules.clone();
    let settings = cx
        .update_window(app.window(), |_, window, cx| {
            ModuleRegistry::settings_views(&modules, &MAP_MODULE_ID.into(), window, cx)
                .into_iter()
                .next()
                .expect("the map settings panel")
                .downcast::<crate::modules::map::settings::MapSettings>()
                .expect("a MapSettings")
        })
        .unwrap();
    settings.read_with(cx, |s, _| s.state.clone())
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
