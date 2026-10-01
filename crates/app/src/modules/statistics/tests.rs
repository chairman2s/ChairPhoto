//! Headless tests of the Statistics module through `run`'s real wiring: the registry
//! enables it, the rail shows it, and every read goes through the background executor
//! against a real catalog.
//!
//! The three vitest cases (`statistics.test.tsx`) are ported one to one:
//! - "shows the scope chip after setFilterContext()" → `the_scope_chip_follows_the_library_scope`;
//! - "renders the keeper-analysis sections from the stats payload" →
//!   `the_keeper_sections_render_from_the_catalog` (and, on the payload alone, the model's
//!   `the_vitest_fixture_renders_the_keeper_sections`);
//! - "shows the skeleton on a cold load, then repaints instantly from cache on remount" →
//!   `a_cold_load_shows_the_skeleton_and_a_return_repaints_from_the_cache`.

use super::view::StatisticsView;
use super::{Statistics, STATISTICS_ID, STATISTICS_VIEW_ID};
use crate::modules::ModuleRegistry;
use crate::shell::state::Surface;
use crate::{start_core, wire, WireOptions, Wired};
use chairphoto_core::app::{AppState, CoreEvent, EventSink as _};
use chairphoto_core::appearance::SystemThemeResult;
use chairphoto_core::catalog::{Catalog, PickState};
use chairphoto_model::statistics::RateMetric;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{point, px, AnyWindowHandle, AppContext as _, Entity, ScrollDelta, SharedString, TestAppContext};
use std::path::PathBuf;
use std::rc::Rc;

/// A private directory under the temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("cp-stats-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct App {
    state: AppState,
    wired: Wired,
    /// The tag every photo but the last carries ("Places/Oslo").
    tag: i64,
}

/// A catalog of `n` photos: all but the last tagged `Places/Oslo`; photo 0 picked, photo 1
/// rejected, photo 0 rated 5★.
fn open_catalog(dir: &TempDir, name: &str, n: usize) -> (Catalog, PathBuf, i64) {
    let db = dir.0.join(format!("{name}.chairphoto"));
    let root = dir.0.join(format!("{name}-photos"));
    let catalog = Catalog::open(&db, &root).unwrap();
    let tag = catalog.create_tag("Places/Oslo").unwrap();
    let ids: Vec<i64> =
        (0..n).map(|i| catalog.upsert_photo(&root.join(format!("2026/p{i}.ARW")), None, 0, 1).unwrap().id).collect();
    for &id in &ids[..n - 1] {
        catalog.assign_tag(id, tag).unwrap();
    }
    catalog.set_culling(ids[0], Some(5), None, Some(PickState::Pick)).unwrap();
    catalog.set_culling(ids[1], None, None, Some(PickState::Reject)).unwrap();
    (catalog, db, tag)
}

/// `run`'s wiring over a catalog of `n` photos, opened as `catalog:switched` does, with the
/// Statistics module enabled.
fn app(dir: &TempDir, n: usize, cx: &mut TestAppContext) -> App {
    let (state, events_rx, ()) = start_core(|_| ());
    let wired = cx.update(|cx| {
        wire(
            cx,
            state.clone(),
            events_rx,
            None,
            &SystemThemeResult::unavailable(),
            WireOptions { on_exit: Rc::new(|| {}), open_default_catalog: false, unthrottled: false },
        )
    });
    let (catalog, db, tag) = open_catalog(dir, "a", n);
    *state.catalog.lock().unwrap() = Some(catalog);
    state.send(CoreEvent::CatalogSwitched(db.to_string_lossy().to_string()));
    cx.run_until_parked();
    cx.update(|cx| ModuleRegistry::enable(&wired.modules, STATISTICS_ID, cx));
    cx.run_until_parked();
    App { state, wired, tag }
}

impl App {
    fn window(&self) -> AnyWindowHandle {
        *self.wired.main_window.as_ref().unwrap()
    }

    fn present(&self, id: impl Into<SharedString>, cx: &mut TestAppContext) -> bool {
        let id = id.into();
        cx.update_window(self.window(), |_, window, cx| {
            window.render_frame(cx);
            window.try_find(id).is_some()
        })
        .unwrap()
    }

    /// Click `id` and let what it started run.
    fn click(&self, id: impl Into<SharedString>, cx: &mut TestAppContext) {
        self.click_unparked(id, cx);
        cx.run_until_parked();
    }

    /// Click `id` without running the work it spawned: a read stays in flight.
    fn click_unparked(&self, id: impl Into<SharedString>, cx: &mut TestAppContext) {
        let id = id.into();
        cx.update_window(self.window(), |_, window, cx| {
            window.render_frame(cx);
            window.click(id, cx);
        })
        .unwrap();
    }

    /// Scroll the dashboard until `id` is on screen (the lower cards are below the fold).
    fn scroll_to(&self, id: &'static str, cx: &mut TestAppContext) {
        for _ in 0..40 {
            let visible = cx
                .update_window(self.window(), |_, window, cx| {
                    window.render_frame(cx);
                    window.try_find(id).is_some_and(|s| s.visible())
                })
                .unwrap();
            if visible {
                return;
            }
            cx.update_window(self.window(), |_, window, cx| {
                window.scroll("stats-root", ScrollDelta::Pixels(point(px(0.), px(-200.))), cx)
            })
            .unwrap();
        }
        panic!("{id} never scrolled into view");
    }

    fn surface(&self, cx: &mut TestAppContext) -> Surface {
        self.wired.shell.read_with(cx, |s, _| s.surface.clone())
    }

    /// The module's state, through the view the registry built for the main window.
    fn stats(&self, cx: &mut TestAppContext) -> Entity<Statistics> {
        let modules = self.wired.modules.clone();
        cx.update_window(self.window(), |_, window, cx| {
            let view = ModuleRegistry::main_view(&modules, STATISTICS_VIEW_ID, window, cx).expect("the Stats view");
            view.view.downcast::<StatisticsView>().expect("a StatisticsView").read(cx).state().clone()
        })
        .unwrap()
    }

    fn total(&self, cx: &mut TestAppContext) -> Option<i64> {
        let stats = self.stats(cx);
        stats.read_with(cx, |s, _| s.session().stats().map(|s| s.total_photos))
    }
}

/// The module ships in `bundled()`, and enabling it puts "Stats" on the rail.
#[gpui_kit::test]
fn enabling_puts_stats_on_the_rail(cx: &mut TestAppContext) {
    let dir = TempDir::new("rail");
    let app = app(&dir, 3, cx);
    assert!(crate::modules::bundled().iter().any(|m| m.meta().id.as_ref() == STATISTICS_ID));
    assert!(app.present("rail-view-statistics", cx));
    app.click("rail-view-statistics", cx);
    assert_eq!(app.surface(cx), Surface::Module(STATISTICS_VIEW_ID.into()));
    assert!(app.present("stats-root", cx));
}

/// vitest: "shows the skeleton on a cold load, then repaints instantly from cache on
/// remount". The first frame after opening the view is the skeleton (the read has not
/// run); once it lands the figures show; back in the Library and on to Stats again, the
/// very first frame already has them — before the fresh read has run.
#[gpui_kit::test]
fn a_cold_load_shows_the_skeleton_and_a_return_repaints_from_the_cache(cx: &mut TestAppContext) {
    let dir = TempDir::new("cold");
    let app = app(&dir, 3, cx);
    app.click_unparked("rail-view-statistics", cx);
    assert!(app.present("stats-skeleton", cx), "a cold load shows the skeleton");
    assert!(!app.present("stats-photos", cx));
    let stats = app.stats(cx);
    assert!(stats.read_with(cx, |s, _| s.session().loading()));

    cx.run_until_parked();
    assert!(app.present("stats-photos", cx));
    assert!(!app.present("stats-skeleton", cx));
    assert_eq!(app.total(cx), Some(3));

    app.wired.shell.update(cx, |s, cx| s.show_library(cx));
    cx.run_until_parked();
    app.click_unparked("rail-view-statistics", cx);
    assert!(stats.read_with(cx, |s, _| s.session().loading()), "a fresh read runs on every open");
    assert!(app.present("stats-photos", cx), "the cache repaints at once");
    assert!(!app.present("stats-skeleton", cx));
    cx.run_until_parked();
}

/// vitest: "shows the scope chip after setFilterContext()". Narrowing the Library to a tag
/// while Stats shows puts up the chip (named from the shell's scope info) and re-reads the
/// figures for that tag.
#[gpui_kit::test]
fn the_scope_chip_follows_the_library_scope(cx: &mut TestAppContext) {
    let dir = TempDir::new("scope");
    let app = app(&dir, 4, cx);
    app.click("rail-view-statistics", cx);
    assert_eq!(app.total(cx), Some(4));
    assert!(!app.present("stats-scope-chip", cx));

    let tag = app.tag;
    app.wired.shell.update(cx, |s, cx| s.update_scope(cx, |l| l.select_tag(Some(tag))));
    cx.run_until_parked();
    assert!(app.present("stats-scope-chip", cx));
    let stats = app.stats(cx);
    let label = cx.update(|cx| stats.read(cx).scope_label(cx));
    assert_eq!(label.as_deref(), Some("Scoped to tag ‹Oslo›"), "named as React did: the tag's name");
    assert_eq!(app.total(cx), Some(3), "the figures describe the tag's photos");

    app.wired.shell.update(cx, |s, cx| s.update_scope(cx, |l| l.clear_scope()));
    cx.run_until_parked();
    assert!(!app.present("stats-scope-chip", cx));
    assert_eq!(app.total(cx), Some(4));
}

/// vitest: "renders the keeper-analysis sections from the stats payload" — over a real
/// catalog: 1 picked of 2 decided is 50 %, and 1 of 1 rated photo is ≥ 4★.
#[gpui_kit::test]
fn the_keeper_sections_render_from_the_catalog(cx: &mut TestAppContext) {
    let dir = TempDir::new("keeper");
    let app = app(&dir, 3, cx);
    app.click("rail-view-statistics", cx);
    assert!(app.present("stats-cull-survival", cx));
    assert!(app.present("stats-hit-rate", cx));
    let stats = app.stats(cx);
    let d = stats.update(cx, |s, _| s.dashboard()).unwrap();
    assert_eq!(d.cull_survival.value, "50%");
    assert_eq!(d.cull_survival.footnote.as_deref(), Some("1 undecided photos not counted"));
    assert_eq!(d.hit_rate.value, "100%");
    assert_eq!(d.hit_rate.detail, "1 of 1 rated ≥4★");
    assert!(app.present("stats-ratings-bars", cx), "the ratings card has bars");
    assert!(!app.present("stats-shutter-bars", cx), "no shutter data: the card says No data");
}

/// The keeper-analysis switch changes what the crossings rate.
#[gpui_kit::test]
fn the_metric_switch_rates_hits_instead_of_keeps(cx: &mut TestAppContext) {
    let dir = TempDir::new("metric");
    let app = app(&dir, 3, cx);
    app.click("rail-view-statistics", cx);
    let stats = app.stats(cx);
    assert_eq!(stats.read_with(cx, |s, _| s.metric()), RateMetric::Keep);
    app.scroll_to("stats-metric-hit", cx);
    app.click("stats-metric-hit", cx);
    assert_eq!(stats.read_with(cx, |s, _| s.metric()), RateMetric::Hit);
    let d = stats.update(cx, |s, _| s.dashboard()).unwrap();
    assert_eq!(d.metric, RateMetric::Hit, "the dashboard was re-derived for the new metric");
}

/// A top-tag row filters the Library by that tag and goes back to the grid
/// (`api.filterByTag`).
#[gpui_kit::test]
fn a_top_tag_row_filters_the_library(cx: &mut TestAppContext) {
    let dir = TempDir::new("filter");
    let app = app(&dir, 3, cx);
    app.click("rail-view-statistics", cx);
    let row: &'static str = Box::leak(format!("stats-tag-row-{}", app.tag).into_boxed_str());
    app.scroll_to(row, cx);
    app.click(row, cx);
    assert_eq!(app.surface(cx), Surface::Library);
    assert_eq!(app.wired.shell.read_with(cx, |s, _| s.library.scope().tag_id), Some(app.tag));
}

/// A read still in flight when the catalog switches must not land: neither on screen nor in
/// the cache. The interleaving is forced: the read is started and not run until after the
/// switch has been delivered.
#[gpui_kit::test]
fn a_catalog_switch_drops_a_read_in_flight(cx: &mut TestAppContext) {
    let dir = TempDir::new("switch");
    let app = app(&dir, 3, cx);
    app.click_unparked("rail-view-statistics", cx);
    let stats = app.stats(cx);
    assert!(stats.read_with(cx, |s, _| s.session().loading()), "the read is in flight");

    let (catalog, db, _) = open_catalog(&dir, "b", 5);
    *app.state.catalog.lock().unwrap() = Some(catalog);
    app.state.send(CoreEvent::CatalogSwitched(db.to_string_lossy().to_string()));
    cx.run_until_parked();
    stats.read_with(cx, |s, _| {
        assert!(s.session().stats().is_none(), "the old read's answer is not shown");
        assert_eq!(s.session().cache_len(), 0, "nor cached");
        assert!(!s.session().loading());
    });
    assert_eq!(app.surface(cx), Surface::Library, "the switch sent the stage back to the Library");

    app.click("rail-view-statistics", cx);
    assert_eq!(app.total(cx), Some(5), "the new catalog's figures");
}

/// Disabling the module with a read in flight unloads it cleanly: the view goes, the stage
/// falls back to the Library, and the late answer has nowhere to land.
#[gpui_kit::test]
fn disabling_mid_read_unloads_cleanly(cx: &mut TestAppContext) {
    let dir = TempDir::new("unload");
    let app = app(&dir, 3, cx);
    app.click_unparked("rail-view-statistics", cx);
    let stats = app.stats(cx);
    let weak = stats.downgrade();
    drop(stats);
    cx.update(|cx| ModuleRegistry::disable(&app.wired.modules, STATISTICS_ID, cx));
    cx.run_until_parked();
    assert_eq!(app.surface(cx), Surface::Library);
    assert!(!app.present("rail-view-statistics", cx));
    assert!(!app.present("stats-root", cx));
    assert!(weak.upgrade().is_none(), "the module's state was dropped with its instance");
}
