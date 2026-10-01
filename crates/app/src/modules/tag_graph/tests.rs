//! Headless tests of the Tag graph view: the load → scene → raster pipeline, input through the
//! canvas (hover, select, wheel, Escape), the stale-result rules (catalog switch, superseded
//! rasters, a closed view), and the module through the real shell wiring.

use super::view::{TagGraphStats, TagGraphView};
use super::{GraphSource, TAG_GRAPH_ID, VIEW_ID};
use crate::model::AppModel;
use crate::modules::ModuleRegistry;
use crate::shell::state::Surface;
use crate::shell::ShellState;
use crate::{start_core, wire, WireOptions};
use chairphoto_core::app::{AppState, CoreEvent, EventSink as _};
use chairphoto_core::appearance::SystemThemeResult;
use chairphoto_core::catalog::Catalog;
use chairphoto_model::tag_graph::graph::{LibraryGraph, NodeId};
use chairphoto_model::tag_graph::labels::slot_point;
use chairphoto_model::tag_graph::synthetic::{library, Scale};
use chairphoto_model::tag_graph::view::View;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    point, px, AppContext as _, Entity, InputEvent as _, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ScrollDelta, TestAppContext, WindowHandle,
};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct Fixture {
    model: Entity<AppModel>,
    shell: Entity<ShellState>,
    window: WindowHandle<TagGraphView>,
    view: Entity<TagGraphView>,
    loads: Arc<AtomicUsize>,
}

/// A window holding only the Tag graph view over `graph` (counting its loads).
fn fixture(graph: LibraryGraph, cx: &mut TestAppContext) -> Fixture {
    cx.update(|cx| {
        gpui_kit::init(cx);
        crate::theme::apply_system_theme(&SystemThemeResult::unavailable(), cx);
        cx.bind_keys(crate::keymap::bindings());
    });
    let model = cx.new(|_| AppModel::new(AppState::default(), None));
    let shell = cx.new(|cx| ShellState::new(&model, cx));
    let loads = Arc::new(AtomicUsize::new(0));
    let source: GraphSource = {
        let loads = loads.clone();
        Arc::new(move || {
            loads.fetch_add(1, Ordering::SeqCst);
            Ok(graph.clone())
        })
    };
    let (m, s) = (model.clone(), shell.clone());
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), move |window, cx| {
            cx.new(|cx| TagGraphView::new(&m, s, None, source, window, cx))
        })
        .unwrap()
    });
    let view = window.root(cx).unwrap();
    let f = Fixture { model, shell, window, view, loads };
    f.settle(cx);
    f
}

impl Fixture {
    fn any(&self) -> gpui_kit::AnyWindowHandle {
        self.window.into()
    }

    /// Render, let the background work and deferred resizes land, render again.
    fn settle(&self, cx: &mut TestAppContext) {
        for _ in 0..3 {
            cx.update_window(self.any(), |_, window, cx| window.render_frame(cx)).unwrap();
            cx.run_until_parked();
        }
    }

    fn stats(&self, cx: &mut TestAppContext) -> TagGraphStats {
        *self.view.read_with(cx, |v, _| v.stats()).borrow()
    }

    fn click(&self, id: &'static str, cx: &mut TestAppContext) {
        cx.update_window(self.any(), |_, window, cx| window.click(id, cx)).unwrap();
        self.settle(cx);
    }

    fn present(&self, id: &'static str, cx: &mut TestAppContext) -> bool {
        cx.update_window(self.any(), |_, window, cx| {
            window.render_frame(cx);
            window.try_find(id).is_some()
        })
        .unwrap()
    }

    fn status(&self, cx: &mut TestAppContext) -> String {
        self.view.read_with(cx, |v, _| v.session().status())
    }

    /// The window position of a ring slot, `extra` px outside the ring.
    fn slot(&self, id: NodeId, extra: f64, cx: &mut TestAppContext) -> gpui_kit::Point<gpui_kit::Pixels> {
        self.view.read_with(cx, |v, _| {
            let s = v.session();
            let scene = s.scene().unwrap();
            let angle = scene.layout.placement[&id].angle;
            let p = slot_point(s.view(), s.size().unwrap(), angle, extra);
            v.paint_cache().borrow().origin() + point(px(p.0 as f32), px(p.1 as f32))
        })
    }

    fn mouse_move(&self, at: gpui_kit::Point<gpui_kit::Pixels>, pressed: Option<MouseButton>, cx: &mut TestAppContext) {
        cx.update_window(self.any(), |_, window, cx| {
                window.dispatch_event(
                    MouseMoveEvent { position: at, pressed_button: pressed, modifiers: Modifiers::default() }.to_platform_input(),
                    cx,
                );
            })
            .unwrap();
        self.settle(cx);
    }

    fn press_at(&self, at: gpui_kit::Point<gpui_kit::Pixels>, cx: &mut TestAppContext) {
        cx.update_window(self.any(), |_, window, cx| {
                window.dispatch_event(
                    MouseDownEvent { button: MouseButton::Left, position: at, modifiers: Modifiers::default(), click_count: 1, first_mouse: false }
                        .to_platform_input(),
                    cx,
                );
                window.dispatch_event(
                    MouseUpEvent { button: MouseButton::Left, position: at, modifiers: Modifiers::default(), click_count: 1 }
                        .to_platform_input(),
                    cx,
                );
            })
            .unwrap();
        self.settle(cx);
    }

    fn wheel(&self, lines: f32, cx: &mut TestAppContext) {
        cx.update_window(self.any(), |_, window, cx| window.scroll("tg-canvas", ScrollDelta::Lines(point(0., lines)), cx))
            .unwrap();
        cx.run_until_parked();
    }

    fn view_state(&self, cx: &mut TestAppContext) -> View {
        self.view.read_with(cx, |v, _| v.session().view())
    }

    fn raster_view(&self, cx: &mut TestAppContext) -> Option<View> {
        self.view.read_with(cx, |v, _| v.raster().map(|r| r.view))
    }
}

fn small() -> LibraryGraph {
    library(Scale { tags: 120, edges: 400, cameras: 2 }, 11)
}

/// Opening the view loads the graph, but draws nothing until a node type is on; turning
/// Tags on lays the ring out, fits it, and rasters its edges at the fitted view.
#[gpui_kit::test]
fn tags_on_lays_out_fits_and_rasters_the_ring(cx: &mut TestAppContext) {
    let f = fixture(small(), cx);
    assert_eq!(f.loads.load(Ordering::SeqCst), 1);
    assert!(f.present("tg-hint", cx), "both node types start off");
    assert!(f.status(cx).starts_with("0 nodes"));

    f.click("tg-type-tags", cx);
    assert!(!f.present("tg-hint", cx));
    let (ring, size, view) = f.view.read_with(cx, |v, _| {
        let s = v.session();
        (s.scene().unwrap().layout.order.len(), s.size().unwrap(), s.view())
    });
    assert!(ring > 100, "every tag with photos takes a slot: {ring}");
    assert_eq!(view, View::fit(size), "fitted on first nodes");
    assert_eq!(f.raster_view(cx), Some(view), "the base edges are rastered at the fitted view");
    let st = f.stats(cx);
    assert_eq!((st.scenes_dropped, st.rasters_dropped), (0, 0));
    assert!(f.view.read_with(cx, |v, _| !v.paint_cache().borrow().labels.is_empty()), "labels placed");
}

/// Hovering a slot lights it (focus = hover); pressing it selects it and fills the inspector;
/// Escape (the view's key context) deselects.
#[gpui_kit::test]
fn hover_select_and_escape_through_the_canvas(cx: &mut TestAppContext) {
    let f = fixture(small(), cx);
    f.click("tg-type-tags", cx);
    let id = f.view.read_with(cx, |v, _| v.session().scene().unwrap().layout.label_order[0]);
    let at = f.slot(id, 0., cx);

    f.mouse_move(at, None, cx);
    assert_eq!(f.view.read_with(cx, |v, _| v.session().hover()), Some(id));
    assert_eq!(f.view.read_with(cx, |v, _| v.session().focus()), Some([id].into()));

    assert!(f.present("tg-empty", cx));
    f.press_at(at, cx);
    assert_eq!(f.view.read_with(cx, |v, _| v.session().selected()), Some(id));
    assert!(f.present("tg-filter", cx) && f.present("tg-isolate", cx));

    // An empty spot inside the ring deselects (and starts a pan).
    let inside = f.slot(id, -150., cx);
    f.press_at(inside, cx);
    assert_eq!(f.view.read_with(cx, |v, _| v.session().selected()), None);

    f.press_at(at, cx);
    cx.update_window(f.any(), |_, window, cx| window.press("escape", cx)).unwrap();
    f.settle(cx);
    assert_eq!(f.view.read_with(cx, |v, _| v.session().selected()), None, "Escape deselects");
}

/// "Filter library" scopes the Library to the tag and shows it (`api.filterByTag`).
#[gpui_kit::test]
fn filter_library_scopes_the_library_to_the_tag(cx: &mut TestAppContext) {
    let f = fixture(small(), cx);
    f.click("tg-type-tags", cx);
    let id = f.view.read_with(cx, |v, _| v.session().scene().unwrap().layout.label_order[0]);
    f.view.update(cx, |v, cx| v.update_session(cx, |s| s.select(Some(id))));
    f.shell.update(cx, |s, cx| s.show_module_view(VIEW_ID, cx));
    f.click("tg-filter", cx);
    let NodeId::Tag(tag) = id else { panic!("a tag") };
    assert_eq!(f.shell.read_with(cx, |s, _| s.library.scope().tag_id), Some(tag));
    assert_eq!(f.shell.read_with(cx, |s, _| s.surface.clone()), Surface::Library);
}

/// A wheel gesture zooms about the cursor at once and only reprojects the old raster; one
/// crisp raster follows once the gesture has been quiet for SETTLE, and the replaced texture
/// is released.
#[gpui_kit::test]
fn wheel_zoom_reprojects_then_rasters_once_settled(cx: &mut TestAppContext) {
    let f = fixture(small(), cx);
    f.click("tg-type-tags", cx);
    let before = f.stats(cx);
    let fitted = f.view_state(cx);

    f.wheel(3., cx);
    f.wheel(3., cx);
    let zoomed = f.view_state(cx);
    assert!((zoomed.k / fitted.k - 1.15 * 1.15).abs() < 1e-9, "two notches: {} → {}", fitted.k, zoomed.k);
    assert_eq!(f.raster_view(cx), Some(fitted), "while the gesture runs, the old raster is reprojected");

    cx.executor().advance_clock(super::raster::SETTLE);
    cx.run_until_parked();
    assert_eq!(f.raster_view(cx), Some(zoomed), "the crisp raster follows once quiet");
    let after = f.stats(cx);
    assert_eq!(after.rasters_applied - before.rasters_applied, 1, "one raster for the whole gesture");
    assert_eq!(after.images_released - before.images_released, 1, "the replaced texture left the atlas");
}

/// Two raster requests in a row: only the newer one lands.
#[gpui_kit::test]
fn a_superseded_raster_never_lands(cx: &mut TestAppContext) {
    let f = fixture(small(), cx);
    f.click("tg-type-tags", cx);
    let before = f.stats(cx);
    f.view.update(cx, |v, cx| {
        v.request_raster(false, cx);
        v.update_session(cx, |s| s.zoom_step(true));
        v.request_raster(false, cx);
    });
    cx.run_until_parked();
    let after = f.stats(cx);
    assert_eq!(after.rasters_applied - before.rasters_applied, 1);
    assert_eq!(f.raster_view(cx), Some(f.view_state(cx)), "the newer view's raster");
}

/// A catalog switch while a reload is in flight: the old catalog's graph never lands, the
/// raster is released, and the next render loads afresh.
#[gpui_kit::test]
fn a_catalog_switch_drops_the_load_in_flight(cx: &mut TestAppContext) {
    let f = fixture(small(), cx);
    f.click("tg-type-tags", cx);
    let before = f.stats(cx);
    // A catalog read marks the graph stale; the next render starts the reload …
    f.model.update(cx, |_, cx| cx.emit(crate::model::AppModelEvent::CatalogRead));
    cx.update_window(f.any(), |_, window, cx| window.render_frame(cx)).unwrap();
    assert_eq!(f.loads.load(Ordering::SeqCst), 1, "not run yet: still in flight");
    // … and the catalog switches before it answers.
    f.model.update(cx, |_, cx| cx.emit(crate::model::AppModelEvent::Core(CoreEvent::CatalogSwitched("other".into()))));
    f.view.read_with(cx, |v, _| {
        let s = v.session();
        assert!(s.graph().is_none() && s.scene().is_none() && v.raster().is_none(), "the old catalog's state is gone");
        assert!(s.visible().tags, "the node-type choice survives the switch");
    });
    assert_eq!(f.stats(cx).images_released - before.images_released, 1, "the old raster left the atlas");
    cx.run_until_parked();
    let after = f.stats(cx);
    assert_eq!(after.loads_dropped - before.loads_dropped, 1, "the old catalog's load was dropped");
    f.settle(cx);
    let after = f.stats(cx);
    assert_eq!(after.loads_applied - before.loads_applied, 1, "one fresh load for the new catalog");
    assert!(f.view.read_with(cx, |v, _| v.session().scene().is_some_and(|s| !s.layout.order.is_empty())));
}

/// Closing the window drops the view: its last raster leaves the atlas, and work still in
/// flight has nowhere to land.
#[gpui_kit::test]
fn a_closed_view_releases_its_raster(cx: &mut TestAppContext) {
    let f = fixture(small(), cx);
    f.click("tg-type-tags", cx);
    let stats: Rc<RefCell<TagGraphStats>> = f.view.read_with(cx, |v, _| v.stats());
    let released = stats.borrow().images_released;
    // A raster in flight when the window goes.
    f.view.update(cx, |v, cx| v.request_raster(false, cx));
    let weak = f.view.downgrade();
    cx.update_window(f.any(), |_, window, _| window.remove_window()).unwrap();
    drop(f);
    // Dropped entities are released at the next effect flush.
    cx.update(|_| {});
    cx.run_until_parked();
    assert!(weak.upgrade().is_none(), "the view is gone");
    let st = *stats.borrow();
    assert_eq!(st.images_released - released, 1, "released with the view");
}

// --- through the shell ----------------------------------------------------------------------

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The module in the real wiring: enabled from the registry, its rail item shows the view,
/// which reads `library_graph` from the open catalog; disabling it takes the view away.
#[gpui_kit::test]
fn the_module_reads_the_open_catalog_through_the_shell(cx: &mut TestAppContext) {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = TempDir(std::env::temp_dir().join(format!("cp-tg-{}-{nanos}", std::process::id())));
    std::fs::create_dir_all(&dir.0).unwrap();
    let (state, events_rx, ()) = start_core(|_| ());
    let wired = cx.update(|cx| {
        wire(
            cx,
            state.clone(),
            events_rx,
            None,
            &SystemThemeResult::unavailable(),
            WireOptions { on_exit: Rc::new(|| {}), open_default_catalog: false },
        )
    });
    let db = dir.0.join("t.chairphoto");
    let root = dir.0.join("photos");
    let catalog = Catalog::open(&db, &root).unwrap();
    let bird = catalog.create_tag("Animals/Bird").unwrap();
    let dog = catalog.create_tag("Animals/Dog").unwrap();
    for i in 0..3 {
        let p = catalog.upsert_photo(&root.join(format!("p{i}.ARW")), None, 0, 1).unwrap().id;
        catalog.assign_tag(p, bird).unwrap();
        if i < 2 {
            catalog.assign_tag(p, dog).unwrap();
        }
    }
    *state.catalog.lock().unwrap() = Some(catalog);
    state.send(CoreEvent::CatalogSwitched(db.to_string_lossy().to_string()));
    cx.run_until_parked();
    let window = *wired.main_window.as_ref().unwrap();
    let render = |cx: &mut TestAppContext| {
        cx.update_window(window, |_, w, cx| w.render_frame(cx)).unwrap();
        cx.run_until_parked();
    };

    cx.update(|cx| ModuleRegistry::enable(&wired.modules, TAG_GRAPH_ID, cx));
    render(cx);
    cx.update_window(window, |_, w, cx| w.click("rail-view-tag-graph", cx)).unwrap();
    for _ in 0..3 {
        render(cx);
    }
    assert_eq!(wired.shell.read_with(cx, |s, _| s.surface.clone()), Surface::Module(VIEW_ID.into()));
    cx.update_window(window, |_, w, cx| w.click("tg-type-tags", cx)).unwrap();
    for _ in 0..3 {
        render(cx);
    }
    let status = cx
        .update_window(window, |_, w, cx| {
            let v = ModuleRegistry::main_view(&wired.modules, VIEW_ID, w, cx).unwrap();
            v.view.downcast::<TagGraphView>().unwrap().read(cx).session().status()
        })
        .unwrap();
    // Two tags, one co-occurrence edge (2 shared photos), one community.
    assert_eq!(status, "2 nodes · 1 links · 1 communities");

    cx.update(|cx| ModuleRegistry::disable(&wired.modules, TAG_GRAPH_ID, cx));
    render(cx);
    assert_eq!(wired.shell.read_with(cx, |s, _| s.surface.clone()), Surface::Library);
    assert!(cx.update_window(window, |_, w, cx| {
        w.render_frame(cx);
        w.try_find("tag-graph").is_none()
    })
    .unwrap());
}
