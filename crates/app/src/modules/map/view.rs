//! [`MapView`]: the Map module's main view (`MapView` in map.tsx) — one canvas that paints
//! tiles, fence polygons and markers, with the overlays around it.
//!
//! ```text
//! ┌ fences ┐                                   [+][−]
//! │ list   │      canvas: tiles → fences → draft → vertex handles → markers
//! └────────┘            (consent card / empty / loading / error over it)
//! ┌ filmstrip: "N photos at this location"  [Show in Library] [×] ─────┐
//! status: "N photos with GPS" · drawing hint · tile host · © OpenStreetMap contributors
//! ```
//!
//! **Input** (registered in the canvas's paint, against its own hitbox, so the overlays —
//! which occlude — keep their clicks): drag pans (or moves a fence vertex, saved on release);
//! the wheel zooms about the cursor ([`logic::wheel_zoom`]), a pinch too; a click hits a
//! marker (filmstrip of its photos, the first selected quietly so a pop-out loupe follows),
//! else a fence (selected in the list); a double-click zooms in one level. While drawing, a
//! click adds a vertex, a double-click or a click on the first vertex closes the polygon and
//! opens the fence editor. Escape closes the editor, cancels drawing, or closes the
//! filmstrip, in that order.
//!
//! **The filmstrip** lists every photo at the marker, as React's did (a large cluster is
//! one long strip). It is a horizontal virtual list: only the frames on screen are built,
//! and only they — plus [`STRIP_OVERSCAN`] each side — are asked for, nearest the active
//! photo first, under the view's own image claim. A frame that scrolls out of that window
//! is let go (its pending render released, unless another view holds it). The strip is
//! bound to the catalog its photos were read from: it closes when that is no longer the
//! map's, and it never shows a thumbnail rendered while another catalog was open.
//!
//! **Tiles** load only for a host the user allowed (decision #118). Until the settings say,
//! nothing is fetched; when the host was never asked, a card asks. With no tiles the map is
//! a plain background with a graticule, and markers and fences work as before.

use super::logic::{self, Consent, Draft, DraftStep};
use super::state::{Load, MapState};
use super::tiles::{release, MapTiles, TileDone, TileLayer, TILE_BUDGET};
use crate::image_store::{ClaimId, ImageState, ImageStore};
use crate::shell::style::Colors;
use crate::shell::ShellState;
use crate::storage::ui;
use crate::loupe::zoom::fitted;
use chairphoto_core::app::CatalogIdentity;
use chairphoto_core::image_pool::ImageKind;
use chairphoto_core::plugins::map::cluster::{cluster, Cluster, ProjectedPoint, CLUSTER_RADIUS_PX};
use chairphoto_core::plugins::map::tiles::math::{fit_bounds, unproject, TileKey, Viewport};
use chairphoto_core::plugins::map::tiles::source::{MAX_TILE_ZOOM, OSM_ATTRIBUTION};
use chairphoto_core::plugins::map::{point_in_polygon, Fence, LatLng};
use futures::StreamExt as _;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{h_virtual_list, VirtualListScrollHandle};
use gpui_kit::prelude::*;
use gpui_kit::{
    canvas, div, fill, point, px, rgb, size, AnyElement, App, Bounds, Context, CursorStyle, DispatchPhase, Entity,
    FocusHandle, FontWeight, Hitbox, HitboxBehavior, Hsla, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ObjectFit, PathBuilder, PinchEvent, Pixels, Point, RenderImage, ScrollDelta, ScrollWheelEvent,
    SharedString, Subscription, Task, TestSupportExt as _, Window,
};
use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

/// Leaflet's initial view (map.tsx): centre `[20, 0]`, zoom 2.
pub const INITIAL_CENTER: LatLng = (20.0, 0.0);
pub const INITIAL_ZOOM: f64 = 2.0;
/// Fit-to-data on load: `fitBounds(bounds.pad(0.05), { maxZoom: 12 })` (map.tsx).
pub const FIT_PAD: f64 = 0.05;
pub const FIT_MAX_ZOOM: f64 = 12.0;
/// A press that moves less than this is a click, not a drag.
const CLICK_SLOP_PX: f64 = 4.0;
/// A fence vertex handle's hit radius (its dot is 10 px).
const VERTEX_HIT_PX: f64 = 8.0;
/// At most this many cluster count labels are laid out as elements per frame.
const MAX_LABELS: usize = 400;
/// The marker colour (map.tsx's pin, `#3b82f6`).
const MARKER: u32 = 0x3b82f6;
/// A filmstrip frame's size and the gap between frames.
const FRAME_W: f32 = 96.;
const FRAME_H: f32 = 72.;
const FRAME_GAP: f32 = 6.;
/// Frames asked for on each side of the ones on screen, so a short scroll finds them ready
/// (React's `loading="lazy"` let the browser fetch a little past the viewport).
pub const STRIP_OVERSCAN: usize = 8;

/// The bottom filmstrip (`FilmstripState` in map.tsx).
#[derive(Debug, Clone, PartialEq)]
pub struct Filmstrip {
    pub ids: Arc<Vec<i64>>,
    pub active: i64,
    /// The catalog `ids` were read from: the clicked marker's ([`MapView::clusters_from`]).
    pub from: Option<CatalogIdentity>,
}

/// What a filmstrip frame shows for its thumbnail tier in `state`: only pixels rendered
/// while the strip's catalog was open. A switch whose `catalog:switched` has not arrived yet
/// renders the new catalog's photo under a colliding id; that is not this frame's photo.
/// (Without an identity probe nothing records where a thumbnail was rendered.)
pub fn frame_image(strip: &Filmstrip, state: ImageState) -> Option<Arc<RenderImage>> {
    match state {
        ImageState::Ready(l) if l.rendered_in.is_none() || l.rendered_in == strip.from => Some(l.image),
        _ => None,
    }
}

/// The thumbnails the filmstrip wants when `visible` (indices into the strip's `len`
/// photos) is on screen and `active` is the active photo's index: the visible frames
/// nearest the active photo first (it is clamped into the visible range; ahead before
/// behind at the same distance), then [`STRIP_OVERSCAN`] each side, nearest the visible
/// range first.
pub fn strip_wanted(visible: Range<usize>, len: usize, active: Option<usize>) -> Vec<usize> {
    let visible = visible.start.min(len)..visible.end.min(len);
    if visible.is_empty() {
        return Vec::new();
    }
    let anchor = active.unwrap_or(visible.start).clamp(visible.start, visible.end - 1);
    let mut out: Vec<usize> = visible.clone().collect();
    out.sort_by_key(|&i| (i.abs_diff(anchor), i < anchor));
    let before = visible.start.saturating_sub(STRIP_OVERSCAN)..visible.start;
    let after = visible.end..(visible.end + STRIP_OVERSCAN).min(len);
    let mut rest: Vec<usize> = before.chain(after).collect();
    rest.sort_by_key(|&i| (if i < visible.start { visible.start - i } else { i + 1 - visible.end }, i < visible.start));
    out.extend(rest);
    out
}

/// The fence editor dialog: a new fence (`polygon` drawn) or an existing one's name and tag.
pub struct FenceEditor {
    pub existing: Option<Fence>,
    pub polygon: Vec<LatLng>,
    pub name: Entity<InputState>,
    pub tag_path: Entity<InputState>,
    pub error: Option<&'static str>,
    _enter: [Subscription; 2],
}

#[derive(Debug, Clone)]
enum Drag {
    /// Panning, or a click if it never moves past the slop.
    Pan { start: (f64, f64), last: (f64, f64), moved: bool, clicks: usize },
    /// Moving vertex `index` of fence `fence`; `polygon` is the live shape.
    Vertex { fence: i64, index: usize, polygon: Vec<LatLng>, moved: bool },
}

/// A marker on screen: where, its hit radius, and which cluster.
#[derive(Debug, Clone, Copy)]
struct OnScreen {
    x: f64,
    y: f64,
    r: f64,
    cluster: usize,
}

/// What one frame paints, in canvas-local pixels.
#[derive(Default)]
struct Scene {
    /// `(clip, image placement, texture)`: an ancestor stands in with a larger placement.
    tiles: Vec<(Bounds<Pixels>, Bounds<Pixels>, Arc<RenderImage>)>,
    graticule: Vec<(Point<Pixels>, Point<Pixels>)>,
    fences: Vec<(Vec<Point<Pixels>>, Hsla, bool)>,
    draft: Option<Vec<Point<Pixels>>>,
    handles: Vec<(Point<Pixels>, Hsla)>,
    markers: Vec<(Point<Pixels>, f32, usize)>,
}

pub struct MapView {
    pub state: Entity<MapState>,
    shell: Entity<ShellState>,
    images: Option<Entity<ImageStore>>,
    pub viewport: Viewport,
    /// The `points_revision` the view last fitted to.
    fitted: Option<u64>,
    /// The canvas's window bounds at the last paint.
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    pub tiles: TileLayer,
    pub clusters: Arc<Vec<Cluster>>,
    /// The catalog the clusters' photo ids are from: the one their points were read from
    /// (`MapState::catalog`, taken with the points). A marker's click is bound to it, not to
    /// whatever catalog the state has read by the time the click lands (#199).
    clusters_from: Option<CatalogIdentity>,
    /// `(points revision, zoom)` the clusters are for, and the one being computed.
    cluster_key: Option<(u64, u8)>,
    clustering: Option<((u64, u8), Task<()>)>,
    drag: Option<Drag>,
    pub draft: Option<Draft>,
    pub selected_fence: Option<i64>,
    pub filmstrip: Option<Filmstrip>,
    /// The filmstrip's scroll position, and the frames its list last laid out on screen.
    pub strip_scroll: VirtualListScrollHandle,
    strip_range: Rc<Cell<Option<Range<usize>>>>,
    /// The frames' sizes, kept while the strip's length is the same (a big cluster's list).
    strip_sizes: Rc<Vec<gpui_kit::Size<Pixels>>>,
    /// The filmstrip's hold on its thumbnails (see the module docs); made on first use.
    pub strip_claim: Option<ClaimId>,
    pub editor: Option<FenceEditor>,
    /// A polygon closed by a click (which has no window): the editor opens on the next render.
    pending_editor: Option<(Option<Fence>, Vec<LatLng>)>,
    pub focus: FocusHandle,
    /// Frame timing, when `CHAIRPHOTO_MAP_TIMING` is set (see [`FrameTimes`]).
    timing: Option<Rc<std::cell::RefCell<FrameTimes>>>,
    _drain: Task<()>,
    _observe: Subscription,
}

fn color(c: u32) -> Hsla {
    rgb(c).into()
}

fn pt(x: f64, y: f64) -> Point<Pixels> {
    point(px(x as f32), px(y as f32))
}

impl MapView {
    pub fn new(
        state: Entity<MapState>,
        shell: Entity<ShellState>,
        images: Option<Entity<ImageStore>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let backend = MapTiles::get(cx);
        let (tiles, mut rx) = TileLayer::new(backend, TILE_BUDGET);
        // Tile results come from the core runtime; this task lands them on the UI thread. It
        // ends with the view (the update fails), dropping whatever is still on its way.
        let _drain = cx.spawn(async move |this, cx| {
            while let Some(done) = rx.next().await {
                let landed = this.update(cx, |v, cx| v.tile_done(done, cx));
                if landed.is_err() {
                    break;
                }
            }
        });
        // A change of consent, host or catalog takes effect at once, not at the next paint:
        // pending loads for a host no longer allowed are cancelled, their late results dropped.
        let _observe = cx.observe(&state, |this: &mut MapView, _, cx| {
            this.sync_source(cx);
            this.sync_strip_catalog(cx);
            cx.notify();
        });
        // Every texture this view still holds leaves the atlas with it.
        cx.on_release(|this: &mut MapView, cx: &mut App| {
            release(this.tiles.clear(), cx);
            if let (Some(images), Some(claim)) = (this.images.as_ref(), this.strip_claim.take()) {
                images.update(cx, |store, _| store.drop_claim(claim));
            }
        })
        .detach();
        MapView {
            state,
            shell,
            images,
            // Sized by the canvas's first layout (`laid_out`); nothing is fitted or fetched
            // before it.
            viewport: Viewport::new(INITIAL_CENTER, INITIAL_ZOOM, 0.0, 0.0)
                .with_max_zoom(MAX_TILE_ZOOM as f64),
            fitted: None,
            bounds: Rc::new(Cell::new(None)),
            tiles,
            clusters: Arc::new(Vec::new()),
            clusters_from: None,
            cluster_key: None,
            clustering: None,
            drag: None,
            draft: None,
            selected_fence: None,
            filmstrip: None,
            strip_scroll: VirtualListScrollHandle::new(),
            strip_range: Rc::new(Cell::new(None)),
            strip_sizes: Rc::new(Vec::new()),
            strip_claim: None,
            editor: None,
            pending_editor: None,
            focus: cx.focus_handle(),
            timing: std::env::var_os("CHAIRPHOTO_MAP_TIMING").map(|_| Rc::default()),
            _drain,
            _observe,
        }
    }

    fn tile_done(&mut self, done: TileDone, cx: &mut Context<Self>) {
        let key = done.key;
        let gone = self.tiles.complete(done);
        release(gone, cx);
        // A failed tile is retried once its backoff ends: repaint then (`want` asks again if
        // it is still visible).
        if let Some(wait) = self.tiles.retry_in(&key) {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(wait).await;
                this.update(cx, |_, cx| cx.notify()).ok();
            })
            .detach();
        }
        cx.notify();
    }

    /// The consent card is up: the host was never asked and the settings are known.
    pub fn asking_consent(&self, cx: &App) -> bool {
        let s = self.state.read(cx);
        s.settings_known() && s.consent() == Consent::Unknown
    }

    /// Answer the consent card (or change the answer from the map's chip).
    pub fn answer_consent(&mut self, allowed: bool, cx: &mut Context<Self>) {
        let host = self.state.read(cx).source.host().to_string();
        self.state.update(cx, |s, cx| s.set_consent(&host, Some(allowed), cx));
        cx.notify();
    }

    // --- per-frame bookkeeping ---------------------------------------------------------

    /// Tiles only from a host the user allowed (decision #118). A change of host, answer or
    /// catalog drops everything held and cancels everything pending.
    fn sync_source(&mut self, cx: &mut Context<Self>) {
        let s = self.state.read(cx);
        let source = (s.consent() == Consent::Allowed).then(|| s.source.clone());
        let others: Vec<String> =
            s.host_consent().hosts().filter(|&(h, allowed)| allowed && h != s.source.host()).map(|(h, _)| h.to_string()).collect();
        self.tiles.set_redirect_hosts(others);
        let gone = self.tiles.set_source(source);
        release(gone, cx);
    }

    /// Bring the tiles, the fit and the clusters up to date with the state. Called by render.
    fn sync(&mut self, cx: &mut Context<Self>) {
        self.sync_source(cx);
        self.sync_strip_catalog(cx);
        let (points, revision, from) = {
            let s = self.state.read(cx);
            (s.points.clone(), s.points_revision, s.catalog())
        };

        let (w, h) = self.viewport.size();
        if let Load::Ready(points) = &points {
            if self.fitted != Some(revision) && w > 0.0 && h > 0.0 {
                self.fitted = Some(revision);
                let lls: Vec<LatLng> = points.iter().map(|p| unproject(p.u, p.v)).collect();
                if let Some((center, zoom)) = fit_bounds(&lls, w, h, FIT_PAD, FIT_MAX_ZOOM) {
                    self.viewport.set_zoom(zoom);
                    self.viewport.set_center(center);
                }
            }
        }
        self.recluster(points, revision, from, cx);
        if self.tiles.source().is_some() {
            let keys: Vec<TileKey> = self.viewport.visible_tiles().iter().map(|t| t.key).collect();
            self.tiles.want(&keys);
        }
    }

    /// Cluster for the current integer zoom, off the UI thread. The old clusters stay up
    /// until the new ones land, unless the points themselves changed. `from` is the catalog
    /// `points` were read from; the clusters keep it ([`Self::clusters_from`]).
    fn recluster(
        &mut self,
        points: Load<Arc<Vec<ProjectedPoint>>>,
        revision: u64,
        from: Option<CatalogIdentity>,
        cx: &mut Context<Self>,
    ) {
        let Load::Ready(points) = points else {
            if self.cluster_key.is_some_and(|(r, _)| r != revision) || !self.clusters.is_empty() {
                self.clusters = Arc::new(Vec::new());
                self.clusters_from = None;
                self.cluster_key = None;
            }
            return;
        };
        let key = (revision, self.viewport.zoom().round() as u8);
        if self.cluster_key == Some(key) || self.clustering.as_ref().is_some_and(|(k, _)| *k == key) {
            return;
        }
        if self.cluster_key.is_some_and(|(r, _)| r != revision) {
            self.clusters = Arc::new(Vec::new());
            self.clusters_from = None;
        }
        let task = cx.background_executor().spawn(async move { cluster(&points, key.1, CLUSTER_RADIUS_PX) });
        let task = cx.spawn(async move |this, cx| {
            let clusters = task.await;
            this.update(cx, |v, cx| {
                if v.clustering.as_ref().is_some_and(|(k, _)| *k == key) {
                    v.clustering = None;
                    v.clusters = Arc::new(clusters);
                    v.clusters_from = from;
                    v.cluster_key = Some(key);
                    cx.notify();
                }
            })
            .ok();
        });
        self.clustering = Some((key, task));
    }

    fn markers_on_screen(&self) -> Vec<OnScreen> {
        let (w, h) = self.viewport.size();
        let mut out = Vec::new();
        for (i, c) in self.clusters.iter().enumerate() {
            let r = logic::marker_radius(c.len());
            for (x, y) in self.viewport.screen_copies(c.u, c.v) {
                if x >= -r && y >= -r && x <= w + r && y <= h + r {
                    out.push(OnScreen { x, y, r, cluster: i });
                }
            }
        }
        out
    }

    /// The fences as drawn now: a vertex being dragged moves its fence's live shape.
    fn live_fences(&self, cx: &App) -> Vec<(usize, Fence)> {
        let fences = &self.state.read(cx).fences;
        fences
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let mut f = f.clone();
                if let Some(Drag::Vertex { fence, polygon, .. }) = &self.drag {
                    if *fence == f.id {
                        f.polygon = polygon.clone();
                    }
                }
                (i, f)
            })
            .collect()
    }

    fn scene(&mut self, colors: Colors, cx: &App) -> Scene {
        let mut scene = Scene::default();
        let vp = self.viewport;
        if self.tiles.source().is_some() {
            for t in vp.visible_tiles() {
                let r = t.rect;
                let clip = Bounds::new(pt(r.x, r.y), size(px(r.w as f32), px(r.h as f32)));
                if let Some(img) = self.tiles.get(&t.key) {
                    scene.tiles.push((clip, clip, img));
                } else if let Some((anc, img)) = self.tiles.ancestor(t.key, 4) {
                    let n = 1u32 << (t.key.z - anc.z);
                    let (ox, oy) = ((t.key.x % n) as f64, (t.key.y % n) as f64);
                    let (aw, ah) = (r.w * n as f64, r.h * n as f64);
                    let placed = Bounds::new(pt(r.x - ox * r.w, r.y - oy * r.h), size(px(aw as f32), px(ah as f32)));
                    scene.tiles.push((clip, placed, img));
                }
            }
        } else {
            // A graticule every 30° (every 10° when zoomed in), so panning is visible.
            let (w, h) = vp.size();
            let step = if vp.zoom() >= 4.0 { 10.0 } else { 30.0 };
            let mut lng = -180.0;
            while lng <= 180.0 {
                let (x, _) = vp.to_screen((0.0, lng));
                if (0.0..=w).contains(&x) {
                    scene.graticule.push((pt(x, 0.0), pt(x, h)));
                }
                lng += step;
            }
            let mut lat = -80.0;
            while lat <= 80.0 {
                let (_, y) = vp.to_screen((lat, 0.0));
                if (0.0..=h).contains(&y) {
                    scene.graticule.push((pt(0.0, y), pt(w, y)));
                }
                lat += step;
            }
        }
        for (i, f) in self.live_fences(cx) {
            let c = color(logic::fence_color(i));
            let poly: Vec<Point<Pixels>> = f.polygon.iter().map(|&ll| { let (x, y) = vp.to_screen(ll); pt(x, y) }).collect();
            for p in &poly {
                scene.handles.push((*p, c));
            }
            scene.fences.push((poly, c, self.selected_fence == Some(f.id)));
        }
        if let Some(d) = &self.draft {
            let poly: Vec<Point<Pixels>> = d.vertices.iter().map(|&ll| { let (x, y) = vp.to_screen(ll); pt(x, y) }).collect();
            for p in &poly {
                scene.handles.push((*p, colors.accent));
            }
            scene.draft = Some(poly);
        }
        for m in self.markers_on_screen() {
            scene.markers.push((pt(m.x, m.y), m.r as f32, self.clusters[m.cluster].len()));
        }
        scene
    }

    // --- input -------------------------------------------------------------------------

    /// The canvas was laid out at `bounds`: follow its size.
    fn laid_out(&mut self, bounds: Bounds<Pixels>, cx: &mut Context<Self>) {
        let (w, h) = (f64::from(bounds.size.width), f64::from(bounds.size.height));
        if self.viewport.size() != (w, h) {
            self.viewport.set_size(w, h);
            cx.notify();
        }
    }

    pub fn pointer_down(&mut self, at: (f64, f64), clicks: usize, cx: &mut Context<Self>) {
        if self.editor.is_some() || self.asking_consent(cx) {
            return;
        }
        let (x, y) = at;
        if let Some(draft) = &mut self.draft {
            let step = if clicks >= 2 {
                draft.finish()
            } else {
                let first = draft.vertices.first().map(|&ll| {
                    let (fx, fy) = self.viewport.to_screen(ll);
                    ((fx - x).powi(2) + (fy - y).powi(2)).sqrt()
                });
                draft.click(self.viewport.screen_to_latlng(x, y), first)
            };
            match step {
                DraftStep::Added => {}
                DraftStep::Cancelled => self.draft = None,
                DraftStep::Closed(polygon) => {
                    self.draft = None;
                    self.pending_editor = Some((None, polygon));
                }
            }
            cx.notify();
            return;
        }
        // A fence's vertex handle, topmost (last drawn) first.
        let fences = self.live_fences(cx);
        let mut handles = Vec::new();
        for (_, f) in &fences {
            for (i, &ll) in f.polygon.iter().enumerate() {
                let (hx, hy) = self.viewport.to_screen(ll);
                handles.push((hx, hy, VERTEX_HIT_PX, f.id, i, f.polygon.clone()));
            }
        }
        let targets: Vec<(f64, f64, f64)> = handles.iter().map(|h| (h.0, h.1, h.2)).collect();
        if let Some(i) = logic::hit_nearest(&targets, x, y) {
            let (_, _, _, fence, index, polygon) = handles.swap_remove(i);
            self.drag = Some(Drag::Vertex { fence, index, polygon, moved: false });
            cx.notify();
            return;
        }
        self.drag = Some(Drag::Pan { start: at, last: at, moved: false, clicks });
        cx.notify();
    }

    pub fn pointer_move(&mut self, at: (f64, f64), cx: &mut Context<Self>) {
        let vp = self.viewport;
        match &mut self.drag {
            Some(Drag::Pan { start, last, moved, .. }) => {
                if !*moved && ((at.0 - start.0).powi(2) + (at.1 - start.1).powi(2)).sqrt() < CLICK_SLOP_PX {
                    return;
                }
                *moved = true;
                let (dx, dy) = (at.0 - last.0, at.1 - last.1);
                *last = at;
                self.viewport.pan_by(dx, dy);
                cx.notify();
            }
            Some(Drag::Vertex { index, polygon, moved, .. }) => {
                polygon[*index] = vp.screen_to_latlng(at.0, at.1);
                *moved = true;
                cx.notify();
            }
            None => {}
        }
    }

    pub fn pointer_up(&mut self, at: (f64, f64), cx: &mut Context<Self>) {
        match self.drag.take() {
            Some(Drag::Pan { moved: false, clicks, .. }) => self.click(at, clicks, cx),
            Some(Drag::Vertex { fence, polygon, moved: true, .. }) => {
                let current = self.state.read(cx).fences.iter().find(|f| f.id == fence).cloned();
                if let Some(mut f) = current {
                    f.polygon = polygon;
                    self.state.update(cx, |s, cx| s.update_fence(f, cx));
                }
            }
            _ => {}
        }
        cx.notify();
    }

    /// A click on the map (not a drag): a marker, else a fence; a double-click zooms in.
    fn click(&mut self, (x, y): (f64, f64), clicks: usize, cx: &mut Context<Self>) {
        if clicks >= 2 {
            let z = self.viewport.zoom().round() + 1.0;
            self.viewport.zoom_around(x, y, z);
            return;
        }
        let markers = self.markers_on_screen();
        let targets: Vec<(f64, f64, f64)> = markers.iter().map(|m| (m.x, m.y, m.r)).collect();
        if let Some(i) = logic::hit_nearest(&targets, x, y) {
            let ids = self.clusters[markers[i].cluster].ids.clone();
            self.open_filmstrip(ids, self.clusters_from, cx);
            return;
        }
        let ll = self.viewport.screen_to_latlng(x, y);
        let hit = self.live_fences(cx).into_iter().rev().find(|(_, f)| f.polygon.len() >= 3 && point_in_polygon(ll, &f.polygon));
        if let Some((_, f)) = hit {
            self.selected_fence = Some(f.id);
        }
    }

    pub fn wheel(&mut self, at: (f64, f64), delta: ScrollDelta, cx: &mut Context<Self>) {
        let dz = match delta {
            ScrollDelta::Lines(d) => logic::wheel_zoom(Some(d.y as f64), None),
            ScrollDelta::Pixels(d) => logic::wheel_zoom(None, Some(f64::from(d.y))),
        };
        if dz != 0.0 {
            self.viewport.zoom_around(at.0, at.1, self.viewport.zoom() + dz);
            cx.notify();
        }
    }

    pub fn zoom_by(&mut self, dz: f64, cx: &mut Context<Self>) {
        let (w, h) = self.viewport.size();
        self.viewport.zoom_around(w / 2.0, h / 2.0, self.viewport.zoom().round() + dz);
        cx.notify();
    }

    /// Open the strip on a marker's photos, bound to `from`, the catalog the marker was
    /// computed from (AGENTS.md: a row's action captures its catalog when drawn). When the
    /// state has since read another catalog (a switch whose `catalog:switched` has not
    /// arrived, then a re-read), those ids are not that catalog's photos: nothing opens and
    /// nothing is selected; the next frame draws the new catalog's markers.
    fn open_filmstrip(&mut self, ids: Vec<i64>, from: Option<CatalogIdentity>, cx: &mut Context<Self>) {
        let Some(&first) = ids.first() else { return };
        if from.is_none() || from != self.state.read(cx).catalog() {
            return;
        }
        self.select_quietly(first, cx);
        self.filmstrip = Some(Filmstrip { ids: Arc::new(ids), active: first, from });
        self.strip_range.set(None);
        self.strip_scroll.scroll_to_item(0, gpui_kit::ScrollStrategy::Top);
    }

    /// The strip's ids are the catalog's it was opened in: once the map's photos are not
    /// (a switch, seen when the state hears of it — the view need not be on screen), it
    /// closes.
    fn sync_strip_catalog(&mut self, cx: &mut Context<Self>) {
        if self.filmstrip.as_ref().is_some_and(|f| f.from != self.state.read(cx).catalog()) {
            self.close_filmstrip(cx);
        }
    }

    /// Close the filmstrip and let go of every thumbnail it held.
    pub fn close_filmstrip(&mut self, cx: &mut Context<Self>) {
        self.filmstrip = None;
        self.strip_range.set(None);
        if let (Some(images), Some(claim)) = (&self.images, self.strip_claim) {
            images.update(cx, |store, _| store.set_claim(claim, []));
        }
    }

    /// The frames on screen, once per frame, from the strip's prepaint (after its list laid
    /// them out): claim and ask for [`strip_wanted`]'s thumbnails; what left the window is
    /// released by the claim.
    ///
    /// Not done while building the frames: the list also builds one frame every layout to
    /// measure it, and a claim keyed on that would release and re-ask the visible frames
    /// every frame.
    fn on_strip_visible(&mut self, visible: Range<usize>, cx: &mut Context<Self>) {
        let (Some(images), Some(strip)) = (self.images.clone(), self.filmstrip.as_ref()) else { return };
        let active = strip.ids.iter().position(|&id| id == strip.active);
        let wanted: Vec<(i64, ImageKind)> =
            strip_wanted(visible, strip.ids.len(), active).into_iter().map(|i| (strip.ids[i], ImageKind::Thumb)).collect();
        let claim = *self.strip_claim.get_or_insert_with(|| images.update(cx, |store, _| store.new_claim()));
        images.update(cx, |store, _| {
            store.set_claim(claim, wanted.iter().copied());
            store.request_batch(&wanted);
        });
    }

    /// Select a photo without leaving the map (`selectPhotoSilent`): the pop-out loupe follows.
    fn select_quietly(&mut self, id: i64, cx: &mut Context<Self>) {
        self.shell.update(cx, |s, cx| {
            s.library.select_quiet(id);
            cx.notify();
        });
    }

    pub fn filmstrip_select(&mut self, id: i64, cx: &mut Context<Self>) {
        if let Some(f) = &mut self.filmstrip {
            f.active = id;
        }
        self.select_quietly(id, cx);
        cx.notify();
    }

    /// "Show in Library": select the active photo and switch to the Library.
    pub fn show_in_library(&mut self, cx: &mut Context<Self>) {
        if let Some(f) = self.filmstrip.clone() {
            self.close_filmstrip(cx);
            self.shell.update(cx, |s, cx| {
                s.library.select_quiet(f.active);
                s.show_library(cx);
            });
        }
        cx.notify();
    }

    /// Escape: the editor, then drawing, then the filmstrip.
    pub fn escape(&mut self, cx: &mut Context<Self>) {
        if self.editor.take().is_some() {
        } else if self.draft.take().is_some() {
        } else {
            self.close_filmstrip(cx);
        }
        cx.notify();
    }

    /// "+ Draw" / "Cancel".
    pub fn toggle_drawing(&mut self, cx: &mut Context<Self>) {
        self.draft = if self.draft.is_some() { None } else { Some(Draft::default()) };
        cx.notify();
    }

    // --- the fence editor --------------------------------------------------------------

    pub fn open_editor(&mut self, existing: Option<Fence>, polygon: Vec<LatLng>, window: &mut Window, cx: &mut Context<Self>) {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("e.g. Aker Brygge"));
        let tag_path = cx.new(|cx| InputState::new(window, cx).placeholder("e.g. Places/Oslo/Aker Brygge"));
        if let Some(f) = &existing {
            let (n, t) = (f.name.clone(), f.tag_path.clone());
            name.update(cx, |i, cx| i.set_value(n, window, cx));
            tag_path.update(cx, |i, cx| i.set_value(t, window, cx));
        }
        let enter = |input: &Entity<InputState>, window: &mut Window, cx: &mut Context<Self>| {
            cx.subscribe_in(input, window, |this: &mut Self, _, e: &InputEvent, window, cx| {
                if matches!(e, InputEvent::PressEnter { .. }) {
                    this.save_editor(window, cx);
                }
            })
        };
        let _enter = [enter(&name, window, cx), enter(&tag_path, window, cx)];
        name.update(cx, |i, cx| i.focus(window, cx));
        self.editor = Some(FenceEditor { existing, polygon, name, tag_path, error: None, _enter });
        cx.notify();
    }

    pub fn save_editor(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let Some(ed) = &mut self.editor else { return };
        let (name, tag) = (ed.name.read(cx).value().to_string(), ed.tag_path.read(cx).value().to_string());
        match logic::check_fence_fields(&name, &tag) {
            Err(e) => {
                ed.error = Some(e);
                cx.notify();
            }
            Ok((name, tag_path)) => {
                let ed = self.editor.take().expect("checked above");
                self.state.update(cx, |s, cx| match ed.existing {
                    Some(f) => s.update_fence(Fence { name, tag_path, ..f }, cx),
                    None => s.create_fence(name, tag_path, ed.polygon, cx),
                });
                cx.notify();
            }
        }
    }

    fn delete_fence(&mut self, fence: Fence, window: &mut Window, cx: &mut Context<Self>) {
        let answer = ui::confirm(
            window,
            cx,
            "Delete fence".into(),
            format!("Delete fence \u{201c}{}\u{201d}? Existing photo tags are kept.", fence.name).into(),
            "Delete",
        );
        cx.spawn(async move |this, cx| {
            if answer.await.unwrap_or(false) {
                this.update(cx, |v, cx| {
                    if v.selected_fence == Some(fence.id) {
                        v.selected_fence = None;
                    }
                    v.state.update(cx, |s, cx| s.delete_fence(fence.id, cx));
                })
                .ok();
            }
        })
        .detach();
    }
}

// --- render -------------------------------------------------------------------------------

impl Render for MapView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let started = std::time::Instant::now();
        if let Some((existing, polygon)) = self.pending_editor.take() {
            self.open_editor(existing, polygon, window, cx);
        }
        self.sync(cx);
        let colors = Colors::get(cx);
        let scene = self.scene(colors, cx);
        let labels = self.cluster_labels(&scene, colors);
        let canvas = self.render_canvas(scene, colors, cx);
        let overlays = self.render_overlays(colors, window, cx);
        if let Some(t) = &self.timing {
            t.borrow_mut().render.push(started.elapsed());
        }
        div()
            .id("map-view")
            .key_context("Map")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| {
                if e.keystroke.key == "escape" {
                    this.escape(cx);
                    cx.stop_propagation();
                }
            }))
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(colors.well)
            .child(
                div()
                    .id("map-surface")
                    .absolute()
                    .inset_0()
                    .child(canvas)
                    .children(labels)
                    .test_support(),
            )
            .children(overlays)
            .test_support()
    }
}

impl MapView {
    fn cluster_labels(&self, scene: &Scene, colors: Colors) -> Vec<AnyElement> {
        scene
            .markers
            .iter()
            .filter(|(_, _, n)| *n > 1)
            .take(MAX_LABELS)
            .map(|(p, r, n)| {
                let r = *r;
                div()
                    .absolute()
                    .left(p.x - px(r))
                    .top(p.y - px(8.))
                    .w(px(2. * r))
                    .h(px(16.))
                    .flex()
                    .justify_center()
                    .items_center()
                    .text_size(px(11.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(colors.onaccent)
                    .child(n.to_string())
                    .into_any_element()
            })
            .collect()
    }

    fn render_canvas(&self, scene: Scene, colors: Colors, cx: &mut Context<Self>) -> AnyElement {
        let bounds_cell = self.bounds.clone();
        let this = cx.weak_entity();
        let focus = self.focus.clone();
        let drawing = self.draft.is_some();
        let dragging = self.drag.is_some();
        let timing = self.timing.clone();
        canvas(
            move |bounds, window, _| {
                bounds_cell.set(Some(bounds));
                (bounds, window.insert_hitbox(bounds, HitboxBehavior::Normal))
            },
            move |_, (bounds, hitbox): (Bounds<Pixels>, Hitbox), window, cx| {
                // Follow the canvas's size (a resize, the first layout).
                if let Some(view) = this.upgrade() {
                    let laid = view.read(cx).viewport.size();
                    let now = (f64::from(bounds.size.width), f64::from(bounds.size.height));
                    if laid != now {
                        let view = view.clone();
                        cx.defer(move |cx| view.update(cx, |v, cx| v.laid_out(bounds, cx)));
                    }
                }
                let painting = std::time::Instant::now();
                let o = bounds.origin;
                window.with_content_mask(Some(gpui_kit::ContentMask { bounds }), |window| {
                    for (clip, placed, image) in scene.tiles {
                        let clip = Bounds::new(clip.origin + o, clip.size);
                        let placed = Bounds::new(placed.origin + o, placed.size);
                        let _ = window.paint_image(clip, placed, (0.).into(), image, 0, false);
                    }
                    for (a, b) in scene.graticule {
                        let mut path = PathBuilder::stroke(px(1.));
                        path.move_to(a + o);
                        path.line_to(b + o);
                        if let Ok(path) = path.build() {
                            window.paint_path(path, colors.line);
                        }
                    }
                    for (poly, c, selected) in &scene.fences {
                        if poly.len() < 2 {
                            continue;
                        }
                        let pts: Vec<Point<Pixels>> = poly.iter().map(|p| *p + o).collect();
                        if pts.len() >= 3 {
                            let mut fill_path = PathBuilder::fill();
                            fill_path.add_polygon(&pts, true);
                            if let Ok(p) = fill_path.build() {
                                window.paint_path(p, c.opacity(if *selected { 0.3 } else { 0.15 }));
                            }
                        }
                        let mut stroke = PathBuilder::stroke(px(if *selected { 3. } else { 2. }));
                        stroke.add_polygon(&pts, true);
                        if let Ok(p) = stroke.build() {
                            window.paint_path(p, *c);
                        }
                    }
                    if let Some(poly) = &scene.draft {
                        let pts: Vec<Point<Pixels>> = poly.iter().map(|p| *p + o).collect();
                        if pts.len() >= 3 {
                            let mut fill_path = PathBuilder::fill();
                            fill_path.add_polygon(&pts, true);
                            if let Ok(p) = fill_path.build() {
                                window.paint_path(p, colors.accent.opacity(0.15));
                            }
                        }
                        if pts.len() >= 2 {
                            let mut stroke = PathBuilder::stroke(px(2.)).dash_array(&[px(6.), px(4.)]);
                            stroke.add_polygon(&pts, pts.len() >= 3);
                            if let Ok(p) = stroke.build() {
                                window.paint_path(p, colors.accent);
                            }
                        }
                    }
                    for (p, c) in &scene.handles {
                        let b = Bounds::new(*p + o - point(px(6.), px(6.)), size(px(12.), px(12.)));
                        window.paint_quad(fill(b, gpui_kit::white()).corner_radii(px(6.)));
                        let b = Bounds::new(*p + o - point(px(4.), px(4.)), size(px(8.), px(8.)));
                        window.paint_quad(fill(b, *c).corner_radii(px(4.)));
                    }
                    for (p, r, n) in &scene.markers {
                        let r = *r;
                        let outer = Bounds::new(*p + o - point(px(r), px(r)), size(px(2. * r), px(2. * r)));
                        window.paint_quad(fill(outer, gpui_kit::white()).corner_radii(px(r)));
                        let inner_r = r - 2.;
                        let inner =
                            Bounds::new(*p + o - point(px(inner_r), px(inner_r)), size(px(2. * inner_r), px(2. * inner_r)));
                        window.paint_quad(fill(inner, color(MARKER)).corner_radii(px(inner_r)));
                        if *n <= 1 {
                            let dot = Bounds::new(*p + o - point(px(3.), px(3.)), size(px(6.), px(6.)));
                            window.paint_quad(fill(dot, gpui_kit::white()).corner_radii(px(3.)));
                        }
                    }
                });
                if let Some(t) = &timing {
                    t.borrow_mut().paint(painting.elapsed());
                }
                window.set_cursor_style(
                    if drawing { CursorStyle::Crosshair } else if dragging { CursorStyle::ClosedHand } else { CursorStyle::OpenHand },
                    &hitbox,
                );
                // Input against this canvas's own hitbox: overlays above it occlude.
                let local = move |p: Point<Pixels>| (f64::from(p.x - o.x), f64::from(p.y - o.y));
                let (h, this2) = (hitbox.clone(), this.clone());
                window.on_mouse_event(move |e: &MouseDownEvent, phase, window, cx| {
                    if phase == DispatchPhase::Bubble && e.button == MouseButton::Left && h.is_hovered(window) {
                        window.focus(&focus, cx);
                        this2.update(cx, |v, cx| v.pointer_down(local(e.position), e.click_count, cx)).ok();
                    }
                });
                let this2 = this.clone();
                window.on_mouse_event(move |e: &MouseMoveEvent, phase, _, cx| {
                    if phase == DispatchPhase::Bubble {
                        this2.update(cx, |v, cx| v.pointer_move(local(e.position), cx)).ok();
                    }
                });
                let this2 = this.clone();
                window.on_mouse_event(move |e: &MouseUpEvent, phase, _, cx| {
                    if phase == DispatchPhase::Bubble && e.button == MouseButton::Left {
                        this2.update(cx, |v, cx| v.pointer_up(local(e.position), cx)).ok();
                    }
                });
                let (h, this2) = (hitbox.clone(), this.clone());
                window.on_mouse_event(move |e: &ScrollWheelEvent, phase, window, cx| {
                    if phase == DispatchPhase::Bubble && h.is_hovered(window) {
                        this2.update(cx, |v, cx| v.wheel(local(e.position), e.delta, cx)).ok();
                    }
                });
                let (h, this2) = (hitbox, this);
                window.on_mouse_event(move |e: &PinchEvent, phase, window, cx| {
                    if phase == DispatchPhase::Bubble && h.is_hovered(window) {
                        let at = local(e.position);
                        this2
                            .update(cx, |v, cx| {
                                let z = v.viewport.zoom() + logic::pinch_zoom(e.delta as f64);
                                v.viewport.zoom_around(at.0, at.1, z);
                                cx.notify();
                            })
                            .ok();
                    }
                });
            },
        )
        .size_full()
        .into_any_element()
    }

    fn render_overlays(&mut self, colors: Colors, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        out.push(self.render_fence_panel(colors, cx));
        out.push(self.render_zoom_buttons(colors, cx));
        let s = self.state.read(cx);
        let (points, fence_error, source_error) = (s.points.clone(), s.fence_error.clone(), s.source_error.clone());
        match &points {
            Load::Loading => out.push(center_note("map-loading", "Loading GPS points…", colors)),
            Load::Failed(e) => out.push(center_note("map-error", e.clone(), colors)),
            Load::Ready(p) if p.is_empty() => out.push(
                div()
                    .id("map-empty")
                    .occlude()
                    .absolute()
                    .top(px(40.))
                    .left_0()
                    .right_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(4.))
                    .text_color(colors.dim)
                    .text_size(px(12.))
                    .child("No photos with GPS data in this catalog.")
                    .child(div().text_size(px(11.)).child("GPS is read from EXIF during scan."))
                    .test_support()
                    .into_any_element(),
            ),
            Load::Ready(_) => {}
        }
        if let Some(e) = fence_error.or(source_error) {
            out.push(
                div()
                    .id("map-warning")
                    .absolute()
                    .top(px(8.))
                    .left(px(260.))
                    .right(px(60.))
                    .text_size(px(11.5))
                    .text_color(colors.danger)
                    .child(e)
                    .test_support()
                    .into_any_element(),
            );
        }
        if self.asking_consent(cx) {
            out.push(self.render_consent(colors, cx));
        }
        if let Some(f) = self.filmstrip.clone() {
            out.push(self.render_filmstrip(f, colors, cx));
        }
        out.push(self.render_status(colors, cx));
        if self.editor.is_some() {
            out.push(self.render_editor(colors, window, cx));
        }
        out
    }

    fn render_consent(&self, colors: Colors, cx: &mut Context<Self>) -> AnyElement {
        let host = self.state.read(cx).source.host().to_string();
        let allow = cx.listener(|this, _, _, cx| this.answer_consent(true, cx));
        let deny = cx.listener(|this, _, _, cx| this.answer_consent(false, cx));
        div()
            .id("map-consent")
            .occlude()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(colors.scrim)
            .child(
                ui::body()
                    .w(px(420.))
                    .p(px(18.))
                    .rounded(crate::shell::style::RADIUS)
                    .bg(colors.elev)
                    .border_1()
                    .border_color(colors.border)
                    .child(div().font_weight(FontWeight::SEMIBOLD).text_color(colors.txt).child(format!("Load map tiles from {host}?")))
                    .child(ui::sub(
                        "The map's background is downloaded from this tile server. Each request tells it your IP \
                         address and which part of the map you are looking at — roughly where your photos were \
                         taken. No photos are sent. Until you allow it, the map shows your photos and fences on \
                         a plain background, and nothing is downloaded.",
                        colors,
                    ))
                    .child(ui::sub("You can change this later in the Map module's settings.", colors))
                    .child(
                        ui::row()
                            .child(ui::clickable(ui::primary("map-consent-allow", "Load tiles", true, colors), true, allow))
                            .child(ui::clickable(ui::chip("map-consent-deny", "Not now", true, colors), true, deny)),
                    ),
            )
            .test_support()
            .into_any_element()
    }

    fn render_zoom_buttons(&self, colors: Colors, cx: &mut Context<Self>) -> AnyElement {
        let button = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .w(px(28.))
                .h(px(28.))
                .flex()
                .items_center()
                .justify_center()
                .bg(colors.elev)
                .border_1()
                .border_color(colors.border)
                .rounded(px(6.))
                .text_color(colors.txt)
                .cursor_pointer()
                .child(label)
        };
        div()
            .occlude()
            .absolute()
            .top(px(10.))
            .right(px(10.))
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(button("map-zoom-in", "+").on_click(cx.listener(|this, _, _, cx| this.zoom_by(1.0, cx))).test_support())
            .child(button("map-zoom-out", "−").on_click(cx.listener(|this, _, _, cx| this.zoom_by(-1.0, cx))).test_support())
            .into_any_element()
    }

    fn render_fence_panel(&self, colors: Colors, cx: &mut Context<Self>) -> AnyElement {
        let s = self.state.read(cx);
        let fences = s.fences.clone();
        let applying = s.applying;
        let busy = applying.is_some();
        let drawing = self.draft.is_some();
        let mut panel = ui::body()
            .id("map-fences")
            .occlude()
            .absolute()
            .top(px(10.))
            .left(px(10.))
            .w(px(240.))
            .max_h(px(420.))
            .overflow_y_scroll()
            .p(px(10.))
            .gap(px(8.))
            .rounded(crate::shell::style::RADIUS)
            .bg(colors.elev)
            .border_1()
            .border_color(colors.border)
            .text_size(px(12.))
            .child(
                ui::row()
                    .justify_between()
                    .child(div().font_weight(FontWeight::SEMIBOLD).text_color(colors.txt).child("Fences"))
                    .child(ui::clickable(
                        ui::chip("map-draw", if drawing { "Cancel" } else { "+ Draw" }, true, colors),
                        true,
                        cx.listener(|this, _, _, cx| this.toggle_drawing(cx)),
                    )),
            );
        if fences.is_empty() && !drawing {
            panel = panel.child(ui::sub("No fences yet. Click + Draw to draw a polygon on the map.", colors));
        }
        if drawing {
            panel = panel.child(ui::sub(
                "Click the map to add vertices. Double-click (or click the first vertex) to close the polygon.",
                colors,
            ));
        }
        for (i, f) in fences.iter().enumerate() {
            let selected = self.selected_fence == Some(f.id);
            let id = f.id;
            let applying_this = applying == Some(Some(id));
            let (edit_f, del_f) = (f.clone(), f.clone());
            let row = div()
                .id(SharedString::from(format!("map-fence-{id}")))
                .flex()
                .flex_row()
                .gap(px(8.))
                .p(px(6.))
                .rounded(px(6.))
                .when(selected, |d| d.bg(colors.sel))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.selected_fence = Some(id);
                    cx.notify();
                }))
                .child(div().flex_none().w(px(4.)).rounded(px(2.)).bg(color(logic::fence_color(i))))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w_0()
                        .gap(px(2.))
                        .child(div().text_color(colors.txt).child(f.name.clone()))
                        .child(div().text_size(px(11.)).text_color(colors.dim).child(f.tag_path.clone()))
                        .child(
                            ui::row()
                                .child(ui::clickable(
                                    ui::chip(
                                        SharedString::from(format!("map-fence-apply-{id}")),
                                        if applying_this { "Applying…" } else { "Apply" },
                                        !busy,
                                        colors,
                                    ),
                                    !busy,
                                    cx.listener(move |this, _, _, cx| this.state.update(cx, |s, cx| s.apply(Some(id), cx))),
                                ))
                                .child(ui::clickable(
                                    ui::chip(SharedString::from(format!("map-fence-edit-{id}")), "Edit", !busy, colors),
                                    !busy,
                                    cx.listener(move |this, _, window, cx| {
                                        this.open_editor(Some(edit_f.clone()), edit_f.polygon.clone(), window, cx)
                                    }),
                                ))
                                .child(ui::clickable(
                                    ui::danger_chip(SharedString::from(format!("map-fence-delete-{id}")), "Delete", !busy, colors),
                                    !busy,
                                    cx.listener(move |this, _, window, cx| this.delete_fence(del_f.clone(), window, cx)),
                                )),
                        ),
                )
                .test_support();
            panel = panel.child(row);
        }
        if !fences.is_empty() {
            panel = panel.child(ui::clickable(
                ui::primary("map-apply-all", if applying == Some(None) { "Applying all…" } else { "Apply all fences" }, !busy, colors),
                !busy,
                cx.listener(|this, _, _, cx| this.state.update(cx, |s, cx| s.apply(None, cx))),
            ));
        }
        panel.test_support().into_any_element()
    }

    /// Frames `range` of the strip, as its virtual list asks for them (to lay out the ones on
    /// screen, and one to measure). Builds elements from what the image store holds; asks
    /// for nothing (see [`Self::on_strip_visible`]).
    fn render_frames(&mut self, range: Range<usize>, colors: Colors, cx: &mut Context<Self>) -> Vec<AnyElement> {
        self.strip_range.set(Some(range.clone()));
        let Some(strip) = self.filmstrip.clone() else { return Vec::new() };
        let range = range.start.min(strip.ids.len())..range.end.min(strip.ids.len());
        let ids = &strip.ids[range];
        let images: Vec<Option<Arc<RenderImage>>> = match &self.images {
            Some(images) => images.update(cx, |store, _| ids.iter().map(|&id| frame_image(&strip, store.get(id, ImageKind::Thumb))).collect()),
            None => vec![None; ids.len()],
        };
        ids.iter()
            .zip(images)
            .map(|(&id, image)| {
                let active = id == strip.active;
                let cell = div()
                    .id(("map-thumb", id as u64))
                    .flex_none()
                    .w(px(FRAME_W))
                    .h(px(FRAME_H))
                    .rounded(px(4.))
                    .overflow_hidden()
                    .bg(colors.panel)
                    .border_2()
                    .border_color(if active { colors.accent } else { colors.border })
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.filmstrip_select(id, cx)));
                let cell = match image {
                    Some(image) => cell.child(fitted(("map-thumb-picture", id as u64), image, ObjectFit::Cover)),
                    None => cell,
                };
                cell.test_support().into_any_element()
            })
            .collect()
    }

    fn render_filmstrip(&mut self, strip: Filmstrip, colors: Colors, cx: &mut Context<Self>) -> AnyElement {
        let n = strip.ids.len();
        if self.strip_sizes.len() != n {
            self.strip_sizes = Rc::new(vec![size(px(FRAME_W), px(FRAME_H)); n]);
        }
        let list = h_virtual_list(
            cx.entity(),
            "map-filmstrip-row",
            self.strip_sizes.clone(),
            move |this: &mut MapView, range, _window, cx| this.render_frames(range, colors, cx),
        )
        .track_scroll(&self.strip_scroll)
        .gap(px(FRAME_GAP))
        .w_full()
        .h(px(FRAME_H));
        // Laid out after the list, so its prepaint sees the range the list just put on
        // screen (the last one it built: the measuring call comes first).
        let visible = {
            let range = self.strip_range.clone();
            let view = cx.entity().downgrade();
            canvas(
                move |_, _, cx| {
                    if let Some(range) = range.take() {
                        view.update(cx, |v, cx| v.on_strip_visible(range, cx)).ok();
                    }
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_0()
        };
        div()
            .id("map-filmstrip")
            .occlude()
            .absolute()
            .left_0()
            .right_0()
            .bottom(px(26.))
            .flex()
            .flex_col()
            .gap(px(6.))
            .p(px(8.))
            .bg(colors.elev)
            .border_t_1()
            .border_color(colors.border)
            .child(
                ui::row()
                    .justify_between()
                    .child(
                        div()
                            .id("map-filmstrip-count")
                            .text_size(px(12.))
                            .text_color(colors.txt)
                            .child(format!("{} at this location", ui::plural(n, "photo", "photos")))
                            .test_support(),
                    )
                    .child(
                        ui::row()
                            .child(ui::clickable(
                                ui::primary("map-show-in-library", "Show in Library", true, colors),
                                true,
                                cx.listener(|this, _, _, cx| this.show_in_library(cx)),
                            ))
                            .child(ui::clickable(
                                ui::chip("map-filmstrip-close", "×", true, colors),
                                true,
                                cx.listener(|this, _, _, cx| {
                                    this.close_filmstrip(cx);
                                    cx.notify();
                                }),
                            )),
                    ),
            )
            .child(div().relative().w_full().h(px(FRAME_H)).child(list).child(visible))
            .test_support()
            .into_any_element()
    }

    fn render_status(&self, colors: Colors, cx: &mut Context<Self>) -> AnyElement {
        let s = self.state.read(cx);
        let count = match &s.points {
            Load::Ready(p) => ui::plural(p.len(), "photo with GPS", "photos with GPS"),
            Load::Loading => "Loading…".into(),
            Load::Failed(_) => "GPS points unavailable".into(),
        };
        let consent = s.consent();
        let host = s.source.host().to_string();
        let tiles_shown = self.tiles.source().is_some();
        let tile_error = self.tiles.last_error().map(str::to_string);
        let mut bar = div()
            .id("map-status")
            .occlude()
            .absolute()
            .left_0()
            .right_0()
            .bottom_0()
            .h(px(26.))
            .px(px(10.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.))
            .bg(colors.panel)
            .border_t_1()
            .border_color(colors.line)
            .text_size(px(11.))
            .text_color(colors.dim)
            .child(div().id("map-count").child(count).test_support());
        if self.draft.is_some() {
            bar = bar.child(
                div().text_color(colors.accent).child("Drawing fence — click to add vertices, double-click to close"),
            );
        }
        if let Some(e) = tile_error {
            bar = bar.child(div().id("map-tile-error").text_color(colors.danger).child(format!("Tiles: {e}")).test_support());
        }
        bar = bar.child(div().flex_1());
        if consent == Consent::Denied {
            bar = bar.child(ui::clickable(
                ui::chip("map-tiles-off", format!("Map tiles off · load from {host}"), true, colors),
                true,
                cx.listener(|this, _, _, cx| this.answer_consent(true, cx)),
            ));
        }
        if tiles_shown {
            // The policy's attribution, always visible while tiles are.
            bar = bar.child(div().id("map-attribution").opacity(0.8).child(format!("Tiles {OSM_ATTRIBUTION}")).test_support());
        }
        bar.into_any_element()
    }

    fn render_editor(&self, colors: Colors, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let ed = self.editor.as_ref().expect("checked by the caller");
        let title = if ed.existing.is_some() { "Edit fence" } else { "New fence" };
        div()
            .id("map-editor")
            .occlude()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(colors.scrim)
            .child(
                ui::body()
                    .w(px(400.))
                    .p(px(18.))
                    .rounded(crate::shell::style::RADIUS)
                    .bg(colors.elev)
                    .border_1()
                    .border_color(colors.border)
                    .child(div().font_weight(FontWeight::SEMIBOLD).text_color(colors.txt).child(title))
                    .child(ui::label("Name", colors))
                    .child(Input::new(&ed.name))
                    .child(ui::label("Tag path", colors))
                    .child(Input::new(&ed.tag_path))
                    .child(ui::sub(
                        "Use forward slashes for hierarchy, e.g. Places/Norway/Oslo/Sentrum. The tag and its \
                         ancestors are created automatically.",
                        colors,
                    ))
                    .when_some(ed.error, |d, e| d.child(ui::error("map-editor-error", e, colors)))
                    .child(
                        ui::row()
                            .child(ui::clickable(
                                ui::primary("map-editor-save", "Save", true, colors),
                                true,
                                cx.listener(|this, _, window, cx| this.save_editor(window, cx)),
                            ))
                            .child(ui::clickable(
                                ui::chip("map-editor-cancel", "Cancel", true, colors),
                                true,
                                cx.listener(|this, _, _, cx| {
                                    this.editor = None;
                                    cx.notify();
                                }),
                            )),
                    ),
            )
            .test_support()
            .into_any_element()
    }
}

/// CPU time per frame, logged to stderr every [`FrameTimes::EVERY`] frames when the app runs
/// with `CHAIRPHOTO_MAP_TIMING=1`: the view's render (tile bookkeeping, scene building,
/// overlays) and the canvas's paint (recording tiles, paths and quads). GPU time and frame
/// pacing are not in it: measure those on the display (the EIZO's budget is ~33 ms).
#[derive(Default)]
pub struct FrameTimes {
    render: Vec<std::time::Duration>,
    paint: Vec<std::time::Duration>,
}

impl FrameTimes {
    pub const EVERY: usize = 120;

    fn paint(&mut self, d: std::time::Duration) {
        self.paint.push(d);
        if self.paint.len() >= Self::EVERY {
            let q = |v: &mut Vec<std::time::Duration>, p: f64| {
                v.sort();
                v.get(((v.len().max(1) - 1) as f64 * p).round() as usize).map_or(0.0, |d| d.as_secs_f64() * 1e3)
            };
            let (mut r, mut p) = (std::mem::take(&mut self.render), std::mem::take(&mut self.paint));
            eprintln!(
                "map timing: {} frames · render p50 {:.2} ms p95 {:.2} ms max {:.2} ms · paint p50 {:.2} ms p95 {:.2} ms max {:.2} ms",
                p.len(),
                q(&mut r, 0.5),
                q(&mut r, 0.95),
                q(&mut r, 1.0),
                q(&mut p, 0.5),
                q(&mut p, 0.95),
                q(&mut p, 1.0)
            );
        }
    }
}

fn center_note(id: &'static str, text: impl Into<SharedString>, colors: Colors) -> AnyElement {
    div()
        .id(id)
        .absolute()
        .top(px(40.))
        .left_0()
        .right_0()
        .flex()
        .justify_center()
        .text_size(px(12.))
        .text_color(colors.dim)
        .child(text.into())
        .test_support()
        .into_any_element()
}
