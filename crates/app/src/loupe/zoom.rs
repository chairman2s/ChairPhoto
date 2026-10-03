//! [`ZoomImage`]: the loupe image with zoom and pan (`ZoomableImage.tsx`), and the
//! transform it draws with ([`ZoomView`], pure maths).
//!
//! - **Fit → zoom tier swap.** At fit it shows the photo's preview tier; the first zoom-in
//!   asks the image layer for the full-resolution zoom tier and swaps to it once it is ready,
//!   so detail is real, not an upscaled preview. While the preview itself is still on its way
//!   the photo's own grid thumbnail stands in — the same photo, never another one.
//! - **Controls.** The wheel zooms toward the cursor (×1.15 a notch); dragging pans while
//!   zoomed; a double-click toggles fit ↔ 100 % (one image pixel per logical pixel) at the
//!   cursor, waiting for the zoom tier when it is not in yet; "Fit N%" returns to fit.
//!   The ceiling is 100 % once the full-resolution image is in, 8× until then; an override
//!   source (an edited version's render) never caps below 4×.
//! - **Shared view.** The transform lives in a [`ZoomShared`] entity. The loupe gives each
//!   image its own ([`ZoomImage::new`]) and resets it when the photo changes; Compare gives
//!   every pane the same one ([`ZoomImage::shared`]) and resets it itself when the set
//!   changes, so the panes pan and zoom as one.
//! - **Override.** An edited version's render ([`Override`]) replaces the photo tiers; its
//!   hi-res render is asked for on the first zoom-in, by whoever owns the render
//!   ([`ZoomImage::wants_hi`]).
//!
//! Nothing here decodes: the image layer and the edit renders hand over ready textures.

use crate::image_store::{ImageState, ImageStore};
use crate::shell::actions::{RelocatePhoto, RemoveFromCatalog, RetrieveFromNas};
use crate::shell::style::Colors;
use crate::storage::ui;
use chairphoto_core::image_pool::ImageKind;
use gpui_kit::prelude::*;
use gpui_kit::{
    canvas, div, img, px, Bounds, Context, CursorStyle, Div, ElementId, Entity, ImageSource, MouseButton,
    MouseDownEvent, MouseMoveEvent, ObjectFit, Pixels, Point, RenderImage, ScrollWheelEvent, SharedString,
    Subscription, TestSupportExt as _, Window,
};
use std::sync::Arc;

/// One wheel notch's zoom factor.
pub const WHEEL_STEP: f32 = 1.15;
/// The ceiling until the full-resolution image (and so its true size) is in.
pub const FALLBACK_MAX: f32 = 8.;
/// An override source never caps below this: a tightly cropped version can have fewer
/// pixels than the window, and some magnification beats none.
pub const OVERRIDE_FLOOR: f32 = 4.;
/// Above this the view counts as zoomed (React's 1.0001 / 1.001 tolerances).
const ZOOMED: f32 = 1.0001;

/// The pan/zoom transform, as a value: the image at fit, scaled by `scale` about the
/// container's centre, then moved by (`tx`, `ty`) logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ZoomView {
    /// 1 = fit to the container.
    pub scale: f32,
    pub tx: f32,
    pub ty: f32,
}

impl ZoomView {
    pub const FIT: ZoomView = ZoomView { scale: 1., tx: 0., ty: 0. };

    pub fn zoomed(&self) -> bool {
        self.scale > ZOOMED
    }

    /// The "Fit N%" label's N.
    pub fn percent(&self) -> i32 {
        (self.scale * 100.).round() as i32
    }

    /// Zoom by `factor` toward `at` (relative to the container's centre), clamped to
    /// 1..=`max`. `None` when the scale would not change. Reaching fit snaps the pan back.
    pub fn zoom_toward(&self, at: (f32, f32), factor: f32, max: f32) -> Option<ZoomView> {
        let next = (self.scale * factor).min(max).max(1.);
        if next == self.scale {
            return None;
        }
        if next <= ZOOMED {
            return Some(ZoomView::FIT);
        }
        let k = next / self.scale;
        Some(ZoomView { scale: next, tx: at.0 - k * (at.0 - self.tx), ty: at.1 - k * (at.1 - self.ty) })
    }

    /// The view at `scale` (never below fit) that keeps the point under `at` (relative to
    /// the container's centre, at fit) where it is.
    pub fn at_scale(scale: f32, at: (f32, f32)) -> ZoomView {
        let s = scale.max(1.);
        ZoomView { scale: s, tx: at.0 * (1. - s), ty: at.1 * (1. - s) }
    }

    /// The view a drag that started at `self` reaches after moving (`dx`, `dy`).
    pub fn panned(&self, dx: f32, dy: f32) -> ZoomView {
        ZoomView { scale: self.scale, tx: self.tx + dx, ty: self.ty + dy }
    }

    /// Where an image of `natural` size lands in a `container`: (left, top, width, height),
    /// relative to the container's top-left. At fit the image is contained and centred.
    pub fn placement(&self, natural: (f32, f32), container: (f32, f32)) -> (f32, f32, f32, f32) {
        let fit = fit_factor(natural, container);
        let (w, h) = (natural.0 * fit * self.scale, natural.1 * fit * self.scale);
        ((container.0 - w) / 2. + self.tx, (container.1 - h) / 2. + self.ty, w, h)
    }
}

/// The factor that contains `natural` in `container` (CSS `object-fit: contain`).
pub fn fit_factor(natural: (f32, f32), container: (f32, f32)) -> f32 {
    if natural.0 <= 0. || natural.1 <= 0. {
        return 1.;
    }
    (container.0 / natural.0).min(container.1 / natural.1)
}

/// `image` fitted to the box its parent gives it with `fit` — `Contain` (whole, centred,
/// letterboxed) or `Cover` (filling it, centred, cropped) — as the picture element `id`. The
/// box fills its parent (`size_full`). The cull stage, the duel, the proof sheet and preset
/// cards draw their pictures with it.
///
/// `img(..).size_full()` placed in the flow is not enough. GPUI's `img` gives its element
/// the image's aspect ratio, and in a block parent the layout then takes the element's height
/// from its width through that ratio instead of from the parent: a portrait frame — or any
/// frame narrower than the box's shape — comes out taller than the box, and Contain inside
/// that taller element fills the width and runs off the bottom (#174), over whatever sits
/// below it (#178). Positioned absolutely, the element takes both sizes from its containing
/// box and the ratio is only used to paint.
pub fn fitted(id: impl Into<ElementId>, image: impl Into<ImageSource>, fit: ObjectFit) -> Div {
    div()
        .relative()
        .size_full()
        .child(img(image).id(id).absolute().top_0().left_0().size_full().object_fit(fit).test_support())
}

/// The scale at which one image pixel covers one logical pixel: the inverse of the fit
/// factor. Below 1 for an image smaller than the container.
pub fn hundred_percent(natural: (f32, f32), container: (f32, f32)) -> f32 {
    let fit = fit_factor(natural, container);
    if fit > 0. {
        1. / fit
    } else {
        1.
    }
}

/// The zoom ceiling: 100 % when the full-resolution size is known, else [`FALLBACK_MAX`];
/// never below [`OVERRIDE_FLOOR`] for an override source, never below fit.
pub fn max_scale(hires: Option<(f32, f32)>, container: (f32, f32), is_override: bool) -> f32 {
    let max = match hires {
        Some(natural) => hundred_percent(natural, container),
        None => FALLBACK_MAX,
    };
    max.max(if is_override { OVERRIDE_FLOOR } else { 1. })
}

/// The transform panes share. See the module docs.
pub struct ZoomShared {
    pub view: ZoomView,
}

impl ZoomShared {
    pub fn new() -> Self {
        ZoomShared { view: ZoomView::FIT }
    }

    pub fn set(&mut self, view: ZoomView, cx: &mut Context<Self>) {
        if self.view != view {
            self.view = view;
            cx.notify();
        }
    }
}

impl Default for ZoomShared {
    fn default() -> Self {
        Self::new()
    }
}

/// An edited version's render, shown instead of the photo's tiers (`srcOverride`).
#[derive(Clone, Default)]
pub struct Override {
    /// The fit render; `None` while it renders.
    pub lo: Option<Arc<RenderImage>>,
    /// The hi-res render, once in.
    pub hi: Option<Arc<RenderImage>>,
    /// No hi-res render is coming (none offered, or it failed): the fit render is the
    /// full-resolution image.
    pub hi_settled: bool,
    /// The fit render failed.
    pub failed: Option<SharedString>,
}

impl PartialEq for Override {
    /// The same textures (by identity) and the same states.
    fn eq(&self, other: &Self) -> bool {
        let same = |a: &Option<Arc<RenderImage>>, b: &Option<Arc<RenderImage>>| match (a, b) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        same(&self.lo, &other.lo)
            && same(&self.hi, &other.hi)
            && self.hi_settled == other.hi_settled
            && self.failed == other.failed
    }
}

/// What the image shows this frame.
struct Shown {
    image: Option<Arc<RenderImage>>,
    /// Which source `image` is.
    drawn: Option<Drawn>,
    /// The full-resolution image's size, once it is in: the 100 % reference.
    hires: Option<(f32, f32)>,
    failed: bool,
}

/// Which source the image drew in its last frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drawn {
    /// The photo's grid thumbnail, standing in while its preview loads.
    Thumb,
    Preview,
    /// The full-resolution tier.
    Zoom,
    /// An override's fit render.
    OverrideLo,
    /// An override's hi-res render.
    OverrideHi,
}

/// The zoomable image. See the module docs.
pub struct ZoomImage {
    images: Entity<ImageStore>,
    shared: Entity<ZoomShared>,
    /// Whether this image resets the shared view itself when its photo changes (the loupe);
    /// a Compare pane leaves that to Compare.
    owns_view: bool,
    photo: Option<i64>,
    over: Option<Override>,
    /// Zoomed in at least once since the photo changed: use the full-resolution source.
    hi: bool,
    /// A double-click at fit asked for 100 % before the full-resolution image was in.
    want_hundred: bool,
    /// Whether the unavailable state offers Relocate / Retrieve / Remove.
    unavailable_actions: bool,
    bounds: Option<Bounds<Pixels>>,
    drag: Option<(Point<Pixels>, ZoomView)>,
    id: SharedString,
    /// What the last frame drew: the photo and the source (tests, the latency bench).
    last_drawn: Option<(i64, Drawn)>,
    _observers: [Subscription; 2],
}

impl ZoomImage {
    /// An image with its own transform (the loupe).
    pub fn new(images: Entity<ImageStore>, id: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        let shared = cx.new(|_| ZoomShared::new());
        Self::build(images, shared, true, id.into(), cx)
    }

    /// An image driven by a transform shared with others (a Compare pane).
    pub fn shared(
        images: Entity<ImageStore>,
        shared: Entity<ZoomShared>,
        id: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(images, shared, false, id.into(), cx)
    }

    fn build(
        images: Entity<ImageStore>,
        shared: Entity<ZoomShared>,
        owns_view: bool,
        id: SharedString,
        cx: &mut Context<Self>,
    ) -> Self {
        let _observers = [cx.observe(&images, |_, _, cx| cx.notify()), cx.observe(&shared, |_, _, cx| cx.notify())];
        ZoomImage {
            images,
            shared,
            owns_view,
            photo: None,
            over: None,
            hi: false,
            want_hundred: false,
            unavailable_actions: false,
            bounds: None,
            drag: None,
            id,
            last_drawn: None,
            _observers,
        }
    }

    pub fn photo(&self) -> Option<i64> {
        self.photo
    }

    pub fn view(&self, cx: &gpui_kit::App) -> ZoomView {
        self.shared.read(cx).view
    }

    pub fn shared_view(&self) -> &Entity<ZoomShared> {
        &self.shared
    }

    /// Whether the full-resolution source is wanted (zoomed in since the photo changed).
    pub fn wants_hi(&self) -> bool {
        self.hi
    }

    /// The container as the last layout measured it.
    pub fn bounds(&self) -> Option<Bounds<Pixels>> {
        self.bounds
    }

    /// The photo and source the last frame drew, if it drew a picture.
    pub fn drawn(&self) -> Option<(i64, Drawn)> {
        self.last_drawn
    }

    pub fn set_unavailable_actions(&mut self, on: bool) {
        self.unavailable_actions = on;
    }

    /// Show `photo`. A change resets the tier to the preview, and — when this image owns its
    /// view — the transform to fit; the override goes with the photo it belonged to.
    pub fn set_photo(&mut self, photo: Option<i64>, cx: &mut Context<Self>) {
        if self.photo == photo {
            return;
        }
        self.photo = photo;
        self.over = None;
        self.reset_tier();
        if self.owns_view {
            self.shared.update(cx, |s, cx| s.set(ZoomView::FIT, cx));
        }
        cx.notify();
    }

    /// Show an edited version's render instead of the photo's tiers (`None`: the tiers).
    /// A different override (another version) starts at its fit render again.
    pub fn set_override(&mut self, over: Option<Override>, cx: &mut Context<Self>) {
        if self.over == over {
            return;
        }
        let was = self.over.is_some();
        if was != over.is_some() {
            self.reset_tier();
            if self.owns_view {
                self.shared.update(cx, |s, cx| s.set(ZoomView::FIT, cx));
            }
        }
        self.over = over;
        cx.notify();
    }

    fn reset_tier(&mut self) {
        self.hi = false;
        self.want_hundred = false;
        self.drag = None;
    }

    /// Back to fit (the "Fit N%" button): the preview tier again.
    pub fn fit(&mut self, cx: &mut Context<Self>) {
        self.hi = false;
        self.want_hundred = false;
        self.shared.update(cx, |s, cx| s.set(ZoomView::FIT, cx));
        cx.notify();
    }

    fn container(&self) -> Option<(f32, f32)> {
        self.bounds.map(|b| (f32::from(b.size.width), f32::from(b.size.height)))
    }

    /// `position` (window coordinates) relative to the container's centre.
    fn from_centre(&self, position: Point<Pixels>) -> (f32, f32) {
        match self.bounds {
            Some(b) => {
                let c = b.center();
                (f32::from(position.x - c.x), f32::from(position.y - c.y))
            }
            None => (0., 0.),
        }
    }

    fn shown(&self, cx: &mut Context<Self>) -> Shown {
        if let Some(over) = &self.over {
            let hires = match (&over.hi, over.hi_settled, &over.lo) {
                (Some(hi), _, _) => Some(natural(hi)),
                (None, true, Some(lo)) => Some(natural(lo)),
                _ => None,
            };
            let (image, drawn) = match (self.hi, &over.hi, &over.lo) {
                (true, Some(hi), _) => (Some(hi.clone()), Some(Drawn::OverrideHi)),
                (_, _, Some(lo)) => (Some(lo.clone()), Some(Drawn::OverrideLo)),
                _ => (None, None),
            };
            return Shown { failed: image.is_none() && over.failed.is_some(), image, drawn, hires };
        }
        let Some(photo) = self.photo else { return Shown { image: None, drawn: None, hires: None, failed: false } };
        self.images.update(cx, |store, _| {
            let mut wanted = vec![(photo, ImageKind::Preview)];
            if self.hi {
                wanted.push((photo, ImageKind::Zoom));
            }
            store.request_batch(&wanted);
            let zoom = match store.get(photo, ImageKind::Zoom) {
                ImageState::Ready(l) if !l.video_tile => Some(l.image),
                _ => None,
            };
            let preview = store.get(photo, ImageKind::Preview);
            let hires = zoom.as_ref().map(|i| natural(i));
            let failed = matches!(preview, ImageState::Failed(_));
            let (image, drawn) = match (self.hi, zoom, preview) {
                (true, Some(z), _) => (Some(z), Some(Drawn::Zoom)),
                (_, _, ImageState::Ready(l)) => (Some(l.image), Some(Drawn::Preview)),
                // The same photo's grid thumbnail while its preview is on the way.
                _ => match store.peek(photo, ImageKind::Thumb) {
                    ImageState::Ready(l) => (Some(l.image), Some(Drawn::Thumb)),
                    _ => (None, None),
                },
            };
            Shown { image, drawn, hires, failed }
        })
    }

    fn max(&self, hires: Option<(f32, f32)>) -> f32 {
        let container = self.container().unwrap_or((1., 1.));
        max_scale(hires, container, self.over.is_some())
    }

    /// One wheel step toward `position`; `zoom_in` from the wheel's direction.
    pub fn wheel(&mut self, position: Point<Pixels>, zoom_in: bool, cx: &mut Context<Self>) {
        let hires = self.shown(cx).hires;
        let max = self.max(hires);
        let at = self.from_centre(position);
        let factor = if zoom_in { WHEEL_STEP } else { 1. / WHEEL_STEP };
        let view = self.view(cx);
        if let Some(next) = view.zoom_toward(at, factor, max) {
            if next.zoomed() {
                self.hi = true;
            }
            self.shared.update(cx, |s, cx| s.set(next, cx));
            cx.notify();
        }
    }

    /// Fit ↔ 100 % at `position`. At fit without the full-resolution image, ask for it and
    /// apply 100 % (centred) once it is in.
    pub fn double_click(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if self.view(cx).zoomed() {
            self.shared.update(cx, |s, cx| s.set(ZoomView::FIT, cx));
            cx.notify();
            return;
        }
        let shown = self.shown(cx);
        match (shown.hires, self.container()) {
            (Some(natural), Some(container)) => {
                self.want_hundred = false;
                let view = ZoomView::at_scale(hundred_percent(natural, container), self.from_centre(position));
                self.shared.update(cx, |s, cx| s.set(view, cx));
            }
            _ => {
                self.want_hundred = true;
                self.hi = true;
            }
        }
        cx.notify();
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        if event.click_count >= 2 {
            self.drag = None;
            self.double_click(event.position, cx);
            return;
        }
        let view = self.view(cx);
        if view.zoomed() {
            self.drag = Some((event.position, view));
        }
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some((start, view)) = self.drag else { return };
        if event.pressed_button != Some(MouseButton::Left) {
            self.drag = None;
            return;
        }
        let next = view.panned(f32::from(event.position.x - start.x), f32::from(event.position.y - start.y));
        self.shared.update(cx, |s, cx| s.set(next, cx));
    }

    fn set_bounds(&mut self, bounds: Bounds<Pixels>, cx: &mut Context<Self>) {
        if self.bounds != Some(bounds) {
            self.bounds = Some(bounds);
            cx.notify();
        }
    }
}

fn natural(image: &RenderImage) -> (f32, f32) {
    let s = image.size(0);
    (s.width.0 as f32, s.height.0 as f32)
}

impl Render for ZoomImage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let shown = self.shown(cx);
        self.last_drawn = self.photo.zip(shown.drawn);
        // A 100 % asked for before the full-resolution image was in, applied once it is.
        if self.want_hundred {
            if let (Some(natural), Some(container)) = (shown.hires, self.container()) {
                self.want_hundred = false;
                let view = ZoomView::at_scale(hundred_percent(natural, container), (0., 0.));
                self.shared.update(cx, |s, cx| s.set(view, cx));
            }
        }
        let view = self.view(cx);
        let id = self.id.clone();
        if shown.failed {
            return div()
                .id(id)
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.))
                .child(div().text_size(px(15.)).text_color(colors.txt).child("This photo isn’t available right now"))
                .child(div().text_size(px(12.)).text_color(colors.mute).child(
                    "Its file couldn’t be loaded — it may be on the NAS (connect it), moved, or gone.",
                ))
                .when(self.unavailable_actions, |d| {
                    d.child(
                        div()
                            .flex()
                            .gap(px(6.))
                            .child(ui::clickable(ui::chip("loupe-relocate", "Relocate…", true, colors), true, |_, w, cx| {
                                w.dispatch_action(Box::new(RelocatePhoto), cx)
                            }))
                            .child(ui::clickable(
                                ui::chip("loupe-retrieve", "Retrieve from NAS", true, colors),
                                true,
                                |_, w, cx| w.dispatch_action(Box::new(RetrieveFromNas), cx),
                            ))
                            .child(ui::clickable(
                                ui::danger_chip("loupe-remove", "Remove from catalog", true, colors),
                                true,
                                |_, w, cx| w.dispatch_action(Box::new(RemoveFromCatalog), cx),
                            )),
                    )
                })
                .test_support()
                .into_any_element();
        }

        let picture = match (&shown.image, self.container()) {
            (Some(image), Some(container)) => {
                let (l, t, w, h) = view.placement(natural(image), container);
                img(image.clone())
                    .absolute()
                    .left(px(l))
                    .top(px(t))
                    .w(px(w))
                    .h(px(h))
                    .object_fit(ObjectFit::Fill)
                    .into_any_element()
            }
            // Before the first layout: contained, centred — what fit looks like.
            (Some(image), None) => {
                fitted(SharedString::from(format!("{}-picture", self.id)), image.clone(), ObjectFit::Contain)
                    .into_any_element()
            }
            (None, _) => div().into_any_element(),
        };
        let this = cx.entity().downgrade();
        let measure = canvas(
            move |bounds, _, cx| {
                this.update(cx, |z, cx| z.set_bounds(bounds, cx)).ok();
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        let fit_button = view.zoomed().then(|| {
            div()
                .id(SharedString::from(format!("{}-fit", self.id)))
                .absolute()
                .bottom(px(10.))
                .right(px(10.))
                .px(px(10.))
                .h(px(24.))
                .flex()
                .items_center()
                .rounded_full()
                .bg(colors.canvas.opacity(0.78))
                .border_1()
                .border_color(colors.border)
                .text_size(px(11.5))
                .text_color(colors.txt)
                .cursor_pointer()
                .child(format!("Fit {}%", view.percent()))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(|this, _, _, cx| {
                    cx.stop_propagation();
                    this.fit(cx);
                }))
                .test_support()
        });
        div()
            .id(id)
            .relative()
            .size_full()
            .overflow_hidden()
            .cursor(if view.zoomed() { CursorStyle::OpenHand } else { CursorStyle::Arrow })
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                let dy = f32::from(event.delta.pixel_delta(px(16.)).y);
                if dy != 0. {
                    // GPUI's wheel-up is a positive delta: zoom in, as the browser's
                    // negative `deltaY` did.
                    this.wheel(event.position, dy > 0., cx);
                }
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, e: &MouseDownEvent, _, cx| this.on_mouse_down(e, cx)))
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| this.on_mouse_move(e, cx)))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, _| this.drag = None))
            .child(measure)
            .child(picture)
            .children(fit_button)
            .test_support()
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn zooming_toward_a_point_keeps_it_under_the_cursor() {
        let v = ZoomView::FIT;
        let at = (100., -50.);
        let z = v.zoom_toward(at, 2., 8.).unwrap();
        assert_eq!(z.scale, 2.);
        // The image point under the cursor at fit was `at`; after zooming it maps to
        // tx + scale·at, which must still be `at`.
        assert!(close(z.tx + z.scale * at.0, at.0) && close(z.ty + z.scale * at.1, at.1), "{z:?}");
        // And again from a zoomed, panned view.
        let z2 = z.zoom_toward((-30., 20.), 1.5, 8.).unwrap();
        let image_point = ((-30. - z.tx) / z.scale, (20. - z.ty) / z.scale);
        assert!(close(z2.tx + z2.scale * image_point.0, -30.) && close(z2.ty + z2.scale * image_point.1, 20.));
    }

    #[test]
    fn zoom_clamps_to_fit_and_the_ceiling() {
        let z = ZoomView { scale: 1.1, tx: 40., ty: 40. };
        assert_eq!(z.zoom_toward((0., 0.), 1. / 1.15, 8.), Some(ZoomView::FIT), "below fit snaps back, pan too");
        assert_eq!(ZoomView::FIT.zoom_toward((0., 0.), 1. / 1.15, 8.), None, "already at fit");
        let top = ZoomView { scale: 3., tx: 0., ty: 0. };
        assert_eq!(top.zoom_toward((0., 0.), 1.15, 3.), None, "at the ceiling");
        assert_eq!(ZoomView { scale: 2.9, ..top }.zoom_toward((0., 0.), 1.15, 3.).unwrap().scale, 3.);
    }

    #[test]
    fn hundred_percent_is_one_image_pixel_per_logical_pixel() {
        // 6000×4000 in 1500×1000: fit is 0.25, 100 % is 4×.
        assert_eq!(hundred_percent((6000., 4000.), (1500., 1000.)), 4.);
        // A portrait frame is limited by the height.
        assert_eq!(hundred_percent((4000., 6000.), (1500., 1000.)), 6.);
        let v = ZoomView::FIT;
        let (_, _, w, h) = ZoomView { scale: 4., ..v }.placement((6000., 4000.), (1500., 1000.));
        assert_eq!((w, h), (6000., 4000.));
    }

    #[test]
    fn the_ceiling_is_100_percent_once_known_8x_before_and_4x_for_overrides() {
        assert_eq!(max_scale(None, (1500., 1000.), false), FALLBACK_MAX);
        assert_eq!(max_scale(Some((6000., 4000.)), (1500., 1000.), false), 4.);
        // A small image's 100 % is below fit: never below fit…
        assert_eq!(max_scale(Some((300., 200.)), (1500., 1000.), false), 1.);
        // …and an override keeps some magnification.
        assert_eq!(max_scale(Some((300., 200.)), (1500., 1000.), true), OVERRIDE_FLOOR);
    }

    #[test]
    fn at_fit_the_image_is_contained_and_centred_and_pan_moves_it() {
        let (l, t, w, h) = ZoomView::FIT.placement((3000., 2000.), (1000., 1000.));
        assert_eq!((w, h), (1000., 2000. / 3.));
        assert!(close(l, 0.) && close(t, (1000. - h) / 2.));
        let moved = ZoomView { scale: 2., tx: 0., ty: 0. }.panned(30., -10.);
        let (l2, t2, ..) = moved.placement((3000., 2000.), (1000., 1000.));
        let (l0, t0, ..) = ZoomView { scale: 2., tx: 0., ty: 0. }.placement((3000., 2000.), (1000., 1000.));
        assert!(close(l2 - l0, 30.) && close(t2 - t0, -10.));
    }

    #[test]
    fn a_hundred_percent_at_a_point_keeps_that_point() {
        let v = ZoomView::at_scale(4., (200., 100.));
        assert!(close(v.tx + 4. * 200., 200.) && close(v.ty + 4. * 100., 100.));
        assert_eq!(ZoomView::at_scale(0.5, (10., 10.)).scale, 1., "never below fit");
    }
}
