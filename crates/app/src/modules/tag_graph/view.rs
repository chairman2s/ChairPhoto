//! [`TagGraphView`]: the Tag graph's main view (`GraphView` in `tagGraph.tsx`) — left panel,
//! canvas, inspector — over a [`GraphSession`].
//!
//! **Work off the UI thread, stale results dropped.** Three kinds of background work, each
//! tagged so only the newest result lands:
//!
//! - the **load** (`library_graph` + shaping) — by the session's load generation; a catalog
//!   switch resets the session, so a load from the old catalog is dropped;
//! - the **scene** (ring layout + every edge's curve) — by the session's scene generation;
//! - the **raster** of the base edges — by [`TagGraphView::raster_generation`] and the scene
//!   generation it was made from. A wheel/pan/resize asks for one after [`SETTLE`] of quiet
//!   (dropping the pending one), so a gesture reprojects the last raster and the crisp one
//!   follows when it stops.
//!
//! **One raster in flight.** A raster that has started cannot be cancelled (it strokes on
//! scoped threads), so at most one runs at a time: a request while one is running only marks
//! another as wanted, and when the running one finishes the wanted one starts from the state
//! *then* — however many requests came in between, the latest wins and at most one more
//! raster is made. A finished raster whose generation or scene is no longer current is
//! dropped (never painted, so never in the atlas). Peak raster memory is therefore bounded:
//! one raster being stroked — its BGRA output plus the strips' pixmaps, each at most
//! [`MAX_EDGE`](super::raster::MAX_EDGE)² × 4 B = 64 MiB, so ≈ 128 MiB — plus the raster on
//! screen and, until the next frame, the one it replaced (≤ 64 MiB each). Before this, a
//! link-strength slider sweep could stroke one full raster per scene at once.
//!
//! A result for a view that has been dropped (module disabled, window closed) has nowhere to
//! land: its `WeakEntity` update fails. A replaced raster's texture is released from the atlas
//! with `drop_image`, and so is the last one when the view is released.
//!
//! **Loads only while shown.** A catalog read marks the graph stale; the next render (which
//! happens only while the view is on the stage) starts the reload.

use super::paint::{self, Frame, PaintCache, RasterShown};
use super::raster::{region_for, rasterize, scale_for, strips_for, to_render_image, RasterJob, Region, SETTLE};
use super::{Back, GraphSource};
use crate::image_store::{ImageState, ImageStore};
use crate::keymap::contexts;
use crate::model::{AppModel, AppModelEvent};
use crate::shell::style::Colors;
use crate::shell::ShellState;
use chairphoto_core::app::{with_catalog, AppState, CoreEvent};
use chairphoto_core::catalog::{PhotoQuery, PhotoWindow};
use chairphoto_core::image_pool::ImageKind;
use chairphoto_model::tag_graph::graph::{Graph, NodeId};
use chairphoto_model::tag_graph::labels::hit_test;
use chairphoto_model::tag_graph::scene::Scene;
use chairphoto_model::tag_graph::session::{GraphSession, THRESHOLD_MAX};
use chairphoto_model::tag_graph::view::{wheel_factor_lines, wheel_factor_pixels};
use chairphoto_model::tag_graph::{CAMERA_COLOR, PALETTE};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::prelude::*;
use gpui_kit::{
    canvas, div, img, px, AnyElement, App, Context, CursorStyle, Entity, FocusHandle, FontWeight, MouseButton,
    MouseDownEvent, MouseMoveEvent, ObjectFit, PinchEvent, Pixels, Point, RenderImage, ScrollDelta, ScrollWheelEvent, SharedString,
    Subscription, Task, TestSupportExt as _, Window,
};
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

/// Counters of what happened to background results; tests and the bench read them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TagGraphStats {
    pub loads_applied: u64,
    pub loads_dropped: u64,
    pub scenes_applied: u64,
    pub scenes_dropped: u64,
    /// Rasters handed to the background (at most one at a time).
    pub rasters_started: u64,
    pub rasters_applied: u64,
    pub rasters_dropped: u64,
    /// Raster textures released with `drop_image` (counted once it has run).
    pub images_released: u64,
}

/// A pan in progress: where the pointer went down and the view then.
#[derive(Clone, Copy)]
struct Drag {
    start: Point<Pixels>,
    view: chairphoto_model::tag_graph::view::View,
}

pub struct TagGraphView {
    session: GraphSession,
    source: GraphSource,
    app: AppState,
    shell: Entity<ShellState>,
    images: Option<Entity<ImageStore>>,
    focus: FocusHandle,
    /// The graph needs (re)loading; the next render starts it.
    stale: bool,
    raster: Option<RasterShown>,
    raster_generation: u64,
    /// The quiet-time wait before a settled raster starts; replacing it drops the wait.
    raster_settle: Option<Task<()>>,
    /// A raster is being made (its task is detached: it always reports back).
    raster_in_flight: bool,
    /// Another raster was asked for while one was in flight; it starts when that one ends.
    raster_wanted: bool,
    paint: Rc<RefCell<PaintCache>>,
    drag: Option<Drag>,
    slider: Entity<SliderState>,
    /// The selected tag's first photos: `(tag id, photo ids)`.
    top_photos: Option<(i64, Vec<i64>)>,
    top_generation: u64,
    stats: Rc<RefCell<TagGraphStats>>,
    _subscriptions: Vec<Subscription>,
}

impl TagGraphView {
    pub fn new(
        model: &Entity<AppModel>,
        shell: Entity<ShellState>,
        images: Option<Entity<ImageStore>>,
        source: GraphSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let app = model.read(cx).state().clone();
        let slider = cx.new(|_| SliderState::new().max(THRESHOLD_MAX as f32).min(0.).step(1.).default_value(0.));
        let mut subs = vec![
            cx.subscribe(model, |this, _, event: &AppModelEvent, cx| match event {
                AppModelEvent::Core(CoreEvent::CatalogSwitched(_)) => this.catalog_switched(cx),
                AppModelEvent::CatalogRead => {
                    this.stale = true;
                    cx.notify();
                }
                _ => {}
            }),
            cx.subscribe_in(&slider, window, |this, _, event: &SliderEvent, _, cx| {
                if let SliderEvent::Change(v) = event {
                    this.session.set_link_threshold(v.start().round() as i64);
                    this.pump(cx);
                    cx.notify();
                }
            }),
        ];
        // The last raster's texture leaves the atlas with the view. Detached: a subscription
        // stored in the view itself is dropped with it before the release listeners run.
        cx.on_release(|this, cx| {
            if let Some(old) = this.raster.take() {
                release_raster(old, this.stats.clone(), cx);
            }
        })
        .detach();
        if let Some(images) = &images {
            subs.push(cx.observe(images, |_, _, cx| cx.notify()));
        }
        TagGraphView {
            session: GraphSession::new(),
            source,
            app,
            shell,
            images,
            focus: cx.focus_handle(),
            stale: true,
            raster: None,
            raster_generation: 0,
            raster_settle: None,
            raster_in_flight: false,
            raster_wanted: false,
            paint: Rc::default(),
            drag: None,
            slider,
            top_photos: None,
            top_generation: 0,
            stats: Rc::default(),
            _subscriptions: subs,
        }
    }

    pub fn session(&self) -> &GraphSession {
        &self.session
    }

    /// Change the session and run what follows (a scene request, a raster, top photos).
    pub fn update_session(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut GraphSession)) {
        let (selected, view) = (self.session.selected(), self.session.view());
        f(&mut self.session);
        if self.session.selected() != selected {
            self.load_top_photos(cx);
        }
        if self.session.view() != view {
            self.request_raster(true, cx);
        }
        self.pump(cx);
        cx.notify();
    }

    pub fn stats(&self) -> Rc<RefCell<TagGraphStats>> {
        self.stats.clone()
    }

    pub fn paint_cache(&self) -> Rc<RefCell<PaintCache>> {
        self.paint.clone()
    }

    pub fn raster(&self) -> Option<&RasterShown> {
        self.raster.as_ref()
    }

    pub fn raster_generation(&self) -> u64 {
        self.raster_generation
    }

    pub fn top_photos(&self) -> Option<&(i64, Vec<i64>)> {
        self.top_photos.as_ref()
    }

    // --- background work -------------------------------------------------------------------

    fn catalog_switched(&mut self, cx: &mut Context<Self>) {
        self.session.reset();
        self.stale = true;
        // A raster in flight finishes into a stale generation and is dropped.
        self.raster_generation += 1;
        self.raster_settle = None;
        self.raster_wanted = false;
        self.top_photos = None;
        self.top_generation += 1;
        if let Some(old) = self.raster.take() {
            self.release(old, cx);
        }
        cx.notify();
    }

    fn release(&mut self, old: RasterShown, cx: &mut Context<Self>) {
        release_raster(old, self.stats.clone(), cx);
    }

    fn start_load(&mut self, cx: &mut Context<Self>) {
        self.stale = false;
        let generation = self.session.begin_load();
        let source = self.source.clone();
        let read = cx.background_executor().spawn(async move { source().map(|g| Graph::build(&g)) });
        cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |v, cx| {
                if let Err(e) = &result {
                    eprintln!("tag graph: library_graph failed: {e}");
                }
                if v.session.apply_load(generation, result) {
                    v.stats.borrow_mut().loads_applied += 1;
                    v.load_top_photos(cx);
                    v.pump(cx);
                } else {
                    v.stats.borrow_mut().loads_dropped += 1;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Start a scene build if the session has a request.
    fn pump(&mut self, cx: &mut Context<Self>) {
        let Some(input) = self.session.take_scene_request() else { return };
        let build = cx.background_executor().spawn(async move { Scene::build(&input) });
        cx.spawn(async move |this, cx| {
            let scene = build.await;
            this.update(cx, |v, cx| {
                if v.session.apply_scene(scene) {
                    v.stats.borrow_mut().scenes_applied += 1;
                    v.request_raster(false, cx);
                } else {
                    v.stats.borrow_mut().scenes_dropped += 1;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Ask for a raster of the base edges: now, or after [`SETTLE`] of quiet (`settle`).
    /// Supersedes any pending request; while a raster is in flight it waits for that one
    /// (see the module docs).
    pub fn request_raster(&mut self, settle: bool, cx: &mut Context<Self>) {
        self.raster_generation += 1;
        if settle {
            self.raster_settle = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(SETTLE).await;
                this.update(cx, |v, cx| v.start_raster(cx)).ok();
            }));
        } else {
            self.raster_settle = None;
            self.start_raster(cx);
        }
    }

    /// Start a raster of the current scene and view, or, with one in flight, mark it wanted.
    fn start_raster(&mut self, cx: &mut Context<Self>) {
        if self.raster_in_flight {
            self.raster_wanted = true;
            return;
        }
        self.raster_wanted = false;
        let Some(job) = self.raster_job() else { return };
        let generation = self.raster_generation;
        let (scene_generation, view, region) = (job.scene.generation, job.view, job.region);
        self.raster_in_flight = true;
        self.stats.borrow_mut().rasters_started += 1;
        let made = cx.background_executor().spawn(async move { rasterize(&job).map(to_render_image) });
        // Detached, so it always reports back and clears `raster_in_flight` (unless the view is
        // gone, and then nothing is left to clear).
        cx.spawn(async move |this, cx| {
            let made = made.await;
            this.update(cx, |v, cx| v.finish_raster(generation, scene_generation, view, region, made, cx)).ok();
        })
        .detach();
    }

    /// A raster came back: show it if it is still current, then start the wanted one.
    fn finish_raster(
        &mut self,
        generation: u64,
        scene_generation: u64,
        view: chairphoto_model::tag_graph::view::View,
        region: Region,
        made: Option<Arc<RenderImage>>,
        cx: &mut Context<Self>,
    ) {
        self.raster_in_flight = false;
        if let Some(image) = made {
            let current = self.session.scene().map(|s| s.generation);
            if generation != self.raster_generation || current != Some(scene_generation) {
                self.stats.borrow_mut().rasters_dropped += 1; // never painted, so never in the atlas
            } else {
                self.stats.borrow_mut().rasters_applied += 1;
                let shown = RasterShown { image, region, view, scene_generation };
                if let Some(old) = self.raster.replace(shown) {
                    self.release(old, cx);
                }
                cx.notify();
            }
        }
        if self.raster_wanted {
            self.start_raster(cx);
        }
    }

    fn raster_job(&self) -> Option<RasterJob> {
        let scene = self.session.scene()?.clone();
        let size = self.session.size()?;
        let view = self.session.view();
        let region = region_for(view, size)?;
        let window_scale = self.paint.borrow().scale.max(1.) as f64;
        let scale = scale_for(region, window_scale);
        let strips = strips_for((region.h * scale).ceil() as u32);
        Some(RasterJob { scene, view, size, region, scale, strips })
    }

    /// (Re)read the selected tag's first photos: on a new selection, and after every applied
    /// load — a catalog read can change a tag's photos while the same tag stays selected.
    fn load_top_photos(&mut self, cx: &mut Context<Self>) {
        self.top_generation += 1;
        let generation = self.top_generation;
        let tag = match self.session.selected_node().map(|n| n.id) {
            Some(NodeId::Tag(id)) => id,
            _ => {
                self.top_photos = None;
                return;
            }
        };
        // The same tag's photos stay up while they are re-read; another tag's go at once.
        if self.top_photos.as_ref().is_some_and(|(t, _)| *t != tag) {
            self.top_photos = None;
        }
        let app = self.app.clone();
        let read = cx.background_executor().spawn(async move {
            let query = PhotoQuery { tag_id: Some(tag), window: Some(PhotoWindow::new(0, 6)), ..Default::default() };
            with_catalog(&app, |c| c.list_photos(&query)).map(|p| p.into_iter().map(|p| p.id).collect::<Vec<_>>())
        });
        cx.spawn(async move |this, cx| {
            let ids = read.await.unwrap_or_default();
            this.update(cx, |v, cx| {
                if v.top_generation == generation {
                    v.top_photos = Some((tag, ids));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    // --- input -----------------------------------------------------------------------------

    fn local(&self, p: Point<Pixels>) -> (f64, f64) {
        let o = self.paint.borrow().origin();
        (f64::from(p.x - o.x), f64::from(p.y - o.y))
    }

    /// The node under a window position.
    pub fn hit(&self, p: Point<Pixels>) -> Option<NodeId> {
        let scene = self.session.scene()?;
        let size = self.session.size()?;
        let cache = self.paint.borrow();
        hit_test(scene, self.session.view(), size, self.local(p), &cache.labels)
    }

    fn on_mouse_down(&mut self, e: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        match self.hit(e.position) {
            // Ring slots are fixed — a press is a select, not a drag.
            Some(id) => self.update_session(cx, |s| s.select(Some(id))),
            None => {
                self.drag = Some(Drag { start: e.position, view: self.session.view() });
                self.update_session(cx, |s| s.select(None));
            }
        }
    }

    fn on_mouse_move(&mut self, e: &MouseMoveEvent, cx: &mut Context<Self>) {
        if let Some(drag) = self.drag {
            if e.pressed_button == Some(MouseButton::Left) {
                let d = (f64::from(e.position.x - drag.start.x), f64::from(e.position.y - drag.start.y));
                self.update_session(cx, |s| s.set_view(s.view().panned(drag.view, d)));
                return;
            }
            self.drag = None; // released outside the window
        }
        let hit = self.hit(e.position);
        if self.session.hover() != hit {
            self.update_session(cx, |s| {
                s.set_hover(hit);
            });
        }
    }

    fn zoom(&mut self, factor: f64, at: Point<Pixels>, cx: &mut Context<Self>) {
        let at = self.local(at);
        self.update_session(cx, |s| s.set_view(s.view().zoom_at(factor, at)));
    }

    fn on_wheel(&mut self, e: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let factor = match e.delta {
            ScrollDelta::Lines(l) => wheel_factor_lines(l.y as f64),
            ScrollDelta::Pixels(p) => wheel_factor_pixels(f64::from(p.y)),
        };
        if factor != 1. {
            self.zoom(factor, e.position, cx);
        }
    }

    fn on_pinch(&mut self, e: &PinchEvent, cx: &mut Context<Self>) {
        self.zoom((1. + e.delta as f64).max(0.1), e.position, cx);
    }

    /// The canvas was laid out (size or scale changed): fit if waiting, re-raster.
    fn canvas_resized(&mut self, cx: &mut Context<Self>) {
        let Some(size) = self.paint.borrow().canvas_size() else { return };
        if self.session.set_size(size) {
            self.request_raster(self.raster.is_some(), cx);
            cx.notify();
        }
    }

    fn filter_library(&mut self, tag_id: i64, cx: &mut Context<Self>) {
        self.shell.update(cx, |s, cx| {
            s.update_scope(cx, |l| l.select_tag(Some(tag_id)));
            s.show_library(cx);
        });
    }

    // --- render ----------------------------------------------------------------------------

    fn frame(&self, colors: Colors) -> Option<Frame> {
        let scene = self.session.scene()?.clone();
        let scoped = self.session.scoped()?.clone();
        let emph = self.session.emphasised();
        let dimmed: HashSet<NodeId> = scene.layout.order.iter().copied().filter(|&id| self.session.dimmed(id)).collect();
        let groups = scene
            .layout
            .groups
            .iter()
            .map(|g| (g.name.clone(), self.session.group_color(&g.name), self.session.group_dimmed(&g.name)))
            .collect();
        Some(Frame {
            emph_neighbors: emph.map(|e| scoped.neighbors(e).to_vec()).unwrap_or_default(),
            scene,
            scoped,
            view: self.session.view(),
            hover: self.session.hover(),
            selected: self.session.selected(),
            emph,
            focus: self.session.focus(),
            dimmed,
            groups,
            raster: self.raster.clone(),
            colors,
        })
    }

    fn render_canvas(&mut self, colors: Colors, cx: &mut Context<Self>) -> AnyElement {
        let frame = self.frame(colors).map(Rc::new);
        let cache = self.paint.clone();
        let this = cx.entity().downgrade();
        let (cache_pre, frame_pre) = (cache.clone(), frame.clone());
        let surface = canvas(
            move |bounds, window, cx| {
                let (changed, scale) = {
                    let c = cache_pre.borrow();
                    (c.bounds.map(|b| b.size) != Some(bounds.size), c.scale)
                };
                let scale_changed = scale != window.scale_factor();
                match &frame_pre {
                    Some(f) => paint::prepaint(bounds, f, &mut cache_pre.borrow_mut(), window),
                    None => {
                        let mut c = cache_pre.borrow_mut();
                        c.bounds = Some(bounds);
                        c.scale = window.scale_factor();
                        c.labels.clear();
                    }
                }
                if changed || scale_changed {
                    let this = this.clone();
                    cx.defer(move |cx| {
                        this.update(cx, |v, cx| v.canvas_resized(cx)).ok();
                    });
                }
            },
            move |bounds, (), window, cx| {
                if let Some(f) = &frame {
                    paint::paint(bounds, f, &mut cache.borrow_mut(), window, cx);
                }
            },
        )
        .size_full();
        let cursor = if self.session.hover().is_some() {
            CursorStyle::PointingHand
        } else if self.drag.is_some() {
            CursorStyle::ClosedHand
        } else {
            CursorStyle::OpenHand
        };
        div()
            .id("tg-canvas")
            .absolute()
            .inset_0()
            .cursor(cursor)
            .on_mouse_down(MouseButton::Left, cx.listener(|this, e: &MouseDownEvent, window, cx| this.on_mouse_down(e, window, cx)))
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| this.on_mouse_move(e, cx)))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, cx| {
                this.drag = None;
                cx.notify();
            }))
            .on_scroll_wheel(cx.listener(|this, e: &ScrollWheelEvent, _, cx| this.on_wheel(e, cx)))
            .on_pinch(cx.listener(|this, e: &PinchEvent, _, cx| this.on_pinch(e, cx)))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !hovered && this.session.hover().is_some() {
                    this.update_session(cx, |s| {
                        s.set_hover(None);
                    });
                }
            }))
            .child(surface)
            .test_support()
            .into_any_element()
    }

    fn render_left(&self, colors: Colors, cx: &mut Context<Self>) -> AnyElement {
        let s = &self.session;
        let mut col = div()
            .id("tg-left")
            .flex()
            .flex_col()
            .flex_none()
            .w(px(230.))
            .h_full()
            .overflow_y_scroll()
            .gap(px(6.))
            .p(px(12.))
            .border_r_1()
            .border_color(colors.border)
            .bg(colors.canvas);
        if s.branch().is_some() {
            let mut crumbs = div().flex().flex_row().flex_wrap().items_center().gap(px(2.)).text_size(px(12.)).child(
                div()
                    .id("tg-crumb-all")
                    .px(px(6.))
                    .py(px(3.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_color(colors.dim)
                    .hover(|d| d.bg(colors.panel))
                    .child("All tags")
                    .on_click(cx.listener(|this, _, _, cx| this.update_session(cx, |s| s.focus_branch(None))))
                    .test_support(),
            );
            let crumbs_list = s.crumbs();
            let last = crumbs_list.len().saturating_sub(1);
            for (i, (seg, path)) in crumbs_list.into_iter().enumerate() {
                crumbs = crumbs.child(div().text_color(colors.mute).child("›"));
                if i == last {
                    crumbs = crumbs.child(div().px(px(6.)).font_weight(FontWeight::SEMIBOLD).child(seg));
                } else {
                    crumbs = crumbs.child(
                        div()
                            .id(("tg-crumb", i))
                            .px(px(6.))
                            .py(px(3.))
                            .rounded(px(6.))
                            .cursor_pointer()
                            .text_color(colors.dim)
                            .hover(|d| d.bg(colors.panel))
                            .child(seg)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let path = path.clone();
                                this.update_session(cx, |s| s.focus_branch(Some(path)))
                            }))
                            .test_support(),
                    );
                }
            }
            col = col.child(crumbs);
        }
        let head = |t: SharedString| {
            div().pt(px(8.)).text_size(px(10.)).font_weight(FontWeight::SEMIBOLD).text_color(colors.mute).child(t)
        };
        let row = |id: SharedString, dot: gpui_kit::Hsla, square: bool, label: SharedString, count: String| {
            div()
                .id(id)
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .px(px(8.))
                .py(px(6.))
                .rounded(px(6.))
                .cursor_pointer()
                .text_size(px(13.))
                .hover(|d| d.bg(colors.panel))
                .child(div().size(px(if square { 10. } else { 8. })).rounded(px(if square { 2. } else { 99. })).bg(dot))
                .child(div().flex_1().min_w_0().truncate().child(label))
                .child(div().text_size(px(12.)).text_color(colors.mute).child(count))
        };
        let (tags, cameras) = s.scoped().map(|sc| sc.kind_counts()).unwrap_or((0, 0));
        let vis = s.visible();
        col = col.child(head("NODE TYPES".into())).child(
            row("tg-type-tags".into(), paint::rgb(PALETTE[0], 1.), false, "Tags".into(), tags.to_string())
                .when(!vis.tags, |d| d.opacity(0.45))
                .on_click(cx.listener(|this, _, _, cx| this.update_session(cx, |s| s.toggle_tags())))
                .test_support(),
        );
        col = col.child(
            row("tg-type-cameras".into(), paint::rgb(CAMERA_COLOR, 1.), false, "Cameras".into(), cameras.to_string())
                .when(!vis.cameras, |d| d.opacity(0.45))
                .on_click(cx.listener(|this, _, _, cx| this.update_session(cx, |s| s.toggle_cameras())))
                .test_support(),
        );
        if let Some(sc) = s.scoped().filter(|sc| !sc.communities.is_empty()) {
            col = col.child(head(s.communities_heading().to_uppercase().into()));
            for (i, (name, count)) in sc.communities.iter().enumerate() {
                let on = s.active_community() == Some(name.as_str());
                let name_click = name.clone();
                col = col.child(
                    row(
                        format!("tg-comm-{i}").into(),
                        paint::rgb(sc.community_color(name), 1.),
                        true,
                        name.clone().into(),
                        count.to_string(),
                    )
                    .when(on, |d| d.bg(colors.panel).font_weight(FontWeight::SEMIBOLD))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let name = name_click.clone();
                        this.update_session(cx, |s| s.toggle_community(&name))
                    }))
                    .test_support(),
                );
            }
        }
        col.child(head("LINK STRENGTH".into()))
            .child(div().id("tg-threshold").px(px(4.)).child(Slider::new(&self.slider)).test_support())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .px(px(4.))
                    .text_size(px(10.))
                    .text_color(colors.mute)
                    .child("Loose")
                    .child("Tight"),
            )
            .into_any_element()
    }

    fn button(id: &'static str, label: impl Into<SharedString>, primary: bool, colors: Colors) -> gpui_kit::Stateful<gpui_kit::Div> {
        div()
            .id(id)
            .px(px(10.))
            .py(px(6.))
            .rounded(px(6.))
            .cursor_pointer()
            .text_size(px(12.))
            .text_center()
            .when(primary, |d| d.bg(colors.accent).text_color(colors.onaccent))
            .when(!primary, |d| d.border_1().border_color(colors.border).text_color(colors.txt).hover(|d| d.bg(colors.panel)))
            .child(label.into())
    }

    fn render_right(&mut self, colors: Colors, cx: &mut Context<Self>) -> AnyElement {
        let col = div()
            .id("tg-right")
            .flex()
            .flex_col()
            .flex_none()
            .w(px(280.))
            .h_full()
            .border_l_1()
            .border_color(colors.border)
            .bg(colors.canvas);
        let Some(scoped) = self.session.scoped().cloned() else {
            return col.into_any_element();
        };
        let chip = |t: String| {
            div().px(px(8.)).py(px(3.)).rounded(px(99.)).bg(colors.panel).border_1().border_color(colors.border).text_size(px(11.)).child(t)
        };
        let stat = |n: i64, l: &'static str| {
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .py(px(10.))
                .rounded(px(9.))
                .bg(colors.panel)
                .child(div().text_size(px(20.)).font_weight(FontWeight::SEMIBOLD).child(n.to_string()))
                .child(div().text_size(px(10.)).text_color(colors.mute).child(l))
        };
        let title = |color: gpui_kit::Hsla, t: String| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .child(div().size(px(8.)).rounded(px(99.)).bg(color))
                .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).child(t))
        };
        let conn_chip = |i: usize, color: gpui_kit::Hsla, label: String, detail: String, target: NodeId, cx: &mut Context<Self>| {
            div()
                .id(("tg-conn", i))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .px(px(8.))
                .py(px(4.))
                .rounded(px(99.))
                .bg(colors.panel)
                .border_1()
                .border_color(colors.border)
                .hover(|d| d.border_color(colors.accent))
                .cursor_pointer()
                .text_size(px(12.))
                .child(div().size(px(8.)).rounded(px(99.)).bg(color))
                .child(label)
                .child(div().text_color(colors.mute).child(detail))
                .on_click(cx.listener(move |this, _, _, cx| this.update_session(cx, |s| s.select(Some(target)))))
                .test_support()
        };
        let mut body = div().id("tg-inspector").flex().flex_col().flex_1().min_h_0().overflow_y_scroll().gap(px(12.)).p(px(14.));
        let mut actions = div().flex().flex_col().gap(px(6.)).p(px(14.));
        if let Some(n) = self.session.selected_node().cloned() {
            let is_tag = n.id.is_tag();
            let links = scoped.degree(n.id) as i64;
            body = body.child(title(paint::rgb(scoped.node_color(n.id), 1.), n.label.clone()));
            let mut chips = div().flex().flex_row().flex_wrap().gap(px(6.));
            chips = if is_tag {
                chips
                    .child(chip(format!("Tag{}", if scoped.is_hub(n.id) { " · hub" } else { "" })))
                    .child(chip(format!("Community: {}", n.community)))
            } else {
                chips.child(chip("Camera".into()))
            };
            body = body.child(chips);
            let mut stats = div().flex().flex_row().gap(px(8.)).child(stat(n.count, "Photos"));
            if is_tag {
                stats = stats.child(stat(scoped.children(n.id) as i64, "Children"));
            }
            body = body.child(stats.child(stat(links, "Links")));
            let connected = scoped.connected(n.id);
            if !connected.is_empty() {
                let mut wrap = div().flex().flex_row().flex_wrap().gap(px(6.));
                for (i, (id, w)) in connected.into_iter().take(8).enumerate() {
                    let Some(m) = scoped.node(id) else { continue };
                    wrap = wrap.child(conn_chip(i, paint::rgb(scoped.node_color(id), 1.), m.label.clone(), w.to_string(), id, cx));
                }
                body = body.child(div().text_size(px(10.)).font_weight(FontWeight::SEMIBOLD).text_color(colors.mute).child("CONNECTED")).child(wrap);
            }
            if let (true, Some((_, ids)), Some(images)) = (is_tag, self.top_photos.clone(), self.images.clone()) {
                if !ids.is_empty() {
                    let cells = images.update(cx, |store, _| {
                        let wanted: Vec<_> = ids.iter().map(|&id| (id, ImageKind::Thumb)).collect();
                        store.request_batch(&wanted);
                        ids.iter().map(|&id| store.get(id, ImageKind::Thumb)).collect::<Vec<_>>()
                    });
                    let mut grid = div().id("tg-top-photos").flex().flex_row().flex_wrap().gap(px(6.));
                    for state in cells {
                        let cell = div().w(px(76.)).h(px(76.)).rounded(px(6.)).overflow_hidden().bg(colors.panel);
                        grid = grid.child(match state {
                            ImageState::Ready(loaded) => cell.child(img(loaded.image).size_full().object_fit(ObjectFit::Cover)),
                            _ => cell,
                        });
                    }
                    body = body
                        .child(div().text_size(px(10.)).font_weight(FontWeight::SEMIBOLD).text_color(colors.mute).child("TOP PHOTOS"))
                        .child(grid.test_support());
                }
            }
            let can_focus = self.session.can_focus_selected();
            if can_focus {
                let path = n.full_path.clone();
                actions = actions.child(
                    Self::button("tg-focus-branch", "Focus on this branch", true, colors)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let path = path.clone();
                            this.update_session(cx, |s| s.focus_branch(Some(path)))
                        }))
                        .test_support(),
                );
            }
            let label = if is_tag { format!("Filter library to \"{}\"", n.label) } else { "Filter library (tags only)".into() };
            let mut filter = Self::button("tg-filter", label, !can_focus, colors);
            if let NodeId::Tag(tag) = n.id {
                filter = filter.on_click(cx.listener(move |this, _, _, cx| this.filter_library(tag, cx)));
            } else {
                filter = filter.opacity(0.5).cursor_default();
            }
            let isolated = self.session.isolated() == Some(n.id);
            actions = actions.child(filter.test_support()).child(
                Self::button("tg-isolate", if isolated { "Show all" } else { "Isolate neighbours" }, false, colors)
                    .on_click(cx.listener(move |this, _, _, cx| this.update_session(cx, |s| s.toggle_isolate(n.id))))
                    .test_support(),
            );
        } else if let Some(card) = self.session.community_card() {
            body = body.child(title(paint::rgb(card.color, 1.), card.name.clone()));
            let mut chips = div().flex().flex_row().flex_wrap().gap(px(6.)).child(chip(
                if self.session.branch().is_some() { "Branch" } else { "Community" }.into(),
            ));
            if card.path != card.name {
                chips = chips.child(chip(card.path.clone()));
            }
            body = body
                .child(chips)
                .child(div().flex().flex_row().gap(px(8.)).child(stat(card.photos, "Photos")).child(stat(card.members.len() as i64, "Tags")));
            let mut wrap = div().flex().flex_row().flex_wrap().gap(px(6.));
            for (i, id) in card.members.iter().take(8).enumerate() {
                let Some(m) = scoped.node(*id) else { continue };
                wrap = wrap.child(conn_chip(i, paint::rgb(scoped.node_color(*id), 1.), m.label.clone(), m.count.to_string(), *id, cx));
            }
            body = body.child(div().text_size(px(10.)).font_weight(FontWeight::SEMIBOLD).text_color(colors.mute).child("TOP TAGS")).child(wrap);
            if card.can_focus {
                let path = card.path.clone();
                actions = actions.child(
                    Self::button("tg-focus-branch", "Focus on this branch", true, colors)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let path = path.clone();
                            this.update_session(cx, |s| s.focus_branch(Some(path)))
                        }))
                        .test_support(),
                );
            }
            let mut filter = Self::button("tg-filter", format!("Filter library to \"{}\"", card.name), !card.can_focus, colors);
            filter = match card.tag_id {
                Some(tag) => filter.on_click(cx.listener(move |this, _, _, cx| this.filter_library(tag, cx))),
                None => filter.opacity(0.5).cursor_default(),
            };
            actions = actions.child(filter.test_support()).child(
                Self::button("tg-clear", "Clear", false, colors)
                    .on_click(cx.listener(|this, _, _, cx| this.update_session(cx, |s| s.clear_community())))
                    .test_support(),
            );
        } else {
            return col
                .child(
                    div()
                        .id("tg-empty")
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .p(px(20.))
                        .text_size(px(13.))
                        .text_color(colors.mute)
                        .text_center()
                        .child("Select a node, or a community on the left")
                        .test_support(),
                )
                .into_any_element();
        }
        col.child(body).child(actions.border_t_1().border_color(colors.border)).into_any_element()
    }
}

impl gpui_kit::Focusable for TagGraphView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// Take `old`'s texture out of every window's atlas. Deferred: while a window is being
/// updated it is out of `App::windows`, and `drop_image` would miss it.
fn release_raster(old: RasterShown, stats: Rc<RefCell<TagGraphStats>>, cx: &mut App) {
    cx.defer(move |cx| {
        cx.drop_image(old.image, None);
        stats.borrow_mut().images_released += 1;
    });
}

impl Render for TagGraphView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.stale {
            self.start_load(cx);
        }
        let colors = Colors::get(cx);
        let canvas = self.render_canvas(colors, cx);
        let left = self.render_left(colors, cx);
        let right = self.render_right(colors, cx);
        let s = &self.session;
        let pill_button = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .px(px(10.))
                .py(px(4.))
                .rounded(px(99.))
                .cursor_pointer()
                .text_size(px(13.))
                .hover(|d| d.bg(colors.txt.opacity(0.08)))
                .child(label)
        };
        let legend_row = |c: u32, t: &'static str| {
            div().flex().flex_row().items_center().gap(px(6.)).child(div().size(px(8.)).rounded(px(99.)).bg(paint::rgb(c, 1.))).child(t)
        };
        let center = div()
            .id("tg-center")
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(colors.canvas)
            .child(div().relative().flex_1().min_h_0().child(canvas).children(s.error().map(|e| {
                div()
                    .id("tg-error")
                    .absolute()
                    .top(px(12.))
                    .left(px(12.))
                    .px(px(12.))
                    .py(px(8.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(colors.danger)
                    .bg(colors.danger.opacity(0.15))
                    .text_size(px(12.))
                    .child(e.to_string())
                    .test_support()
            }))
            .when(s.scoped().is_some() && s.shown_stats().nodes == 0, |d| {
                d.child(
                    div()
                        .id("tg-hint")
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(13.))
                        .text_color(colors.mute)
                        .child("Turn on a node type on the left to draw the graph")
                        .test_support(),
                )
            })
            .child(
                div()
                    .absolute()
                    .top(px(12.))
                    .right(px(12.))
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .px(px(10.))
                    .py(px(8.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.panel.opacity(0.8))
                    .text_size(px(11.))
                    .child(legend_row(PALETTE[0], "Tag"))
                    .child(legend_row(CAMERA_COLOR, "Camera")),
            )
            .child(
                div().absolute().bottom(px(14.)).left_0().right_0().flex().justify_center().child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.))
                        .px(px(8.))
                        .py(px(4.))
                        .rounded(px(99.))
                        .border_1()
                        .border_color(colors.border)
                        .bg(colors.panel.opacity(0.85))
                        .child(pill_button("tg-zoom-out", "−").on_click(cx.listener(|this, _, _, cx| this.update_session(cx, |s| s.zoom_step(false)))).test_support())
                        .child(pill_button("tg-zoom-in", "＋").on_click(cx.listener(|this, _, _, cx| this.update_session(cx, |s| s.zoom_step(true)))).test_support())
                        .child(div().w(px(1.)).h(px(16.)).bg(colors.border))
                        .child(pill_button("tg-fit", "Fit").on_click(cx.listener(|this, _, _, cx| this.update_session(cx, |s| s.fit()))).test_support())
                        .child(pill_button("tg-recenter", "Re-center").on_click(cx.listener(|this, _, _, cx| this.update_session(cx, |s| s.recenter()))).test_support()),
                ),
            ))
            .child(
                div()
                    .id("tg-status")
                    .flex()
                    .flex_none()
                    .items_center()
                    .h(px(28.))
                    .px(px(12.))
                    .border_t_1()
                    .border_color(colors.border)
                    .text_size(px(11.))
                    .text_color(colors.mute)
                    .child(s.status())
                    .test_support(),
            );
        div()
            .id("tag-graph")
            .key_context(contexts::TAG_GRAPH)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Back, _, cx| this.update_session(cx, |s| s.escape())))
            .size_full()
            .flex()
            .flex_row()
            .bg(colors.canvas)
            .text_color(colors.txt)
            .child(left)
            .child(center)
            .child(right)
            .test_support()
    }
}
