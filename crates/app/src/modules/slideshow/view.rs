//! "Make slideshow" (`SlideshowDialog.tsx`): the selection as a thumbnail strip (drag to
//! reorder, index badges), per-photo duration, orientation × resolution with the output size,
//! crossfade and its length, Ken Burns, frame rate, the output folder (Browse…), Render with
//! progress, Cancel, Reveal, errors. A click on the backdrop closes it.
//!
//! **The render** is core's slideshow job in two worker steps, never on the UI thread:
//! [`SlideshowDialog::render_movie`] queues the claim (ffmpeg lookup, identity check,
//! resolve); when it lands the dialog keeps the job's id and its own abort flag and queues the
//! run. So Cancel reaches exactly this render: after the claim it trips the job's flag (ffmpeg
//! is killed); before, the claimed job is dropped unrun. A newer Render supersedes the older
//! (core trips it) and the older one's answers are dropped.
//!
//! **Progress** is `slideshow:progress` for this dialog's job id only; the label is
//! "Preparing frames…" until the first event, then "Encoding… N%". The terminal result is the
//! job's return value: the movie's path, or the error (missing ffmpeg, a changed catalog,
//! "Slideshow cancelled").
//!
//! **Closing the dialog does not stop a render** (React's didn't either); its answer then goes
//! to the status line. A catalog switch closes the dialog, and core's switch kills the render.

use super::SlideshowBackend;
use crate::image_store::ImageState;
use crate::model::AppModelEvent;
use crate::modules::dialog::{self, DialogHost, Picked, SelectionSnapshot, NO_ROWS};
use crate::shell::style::Colors;
use crate::storage::{ui, Runner};
use crate::tags::toggle;
use chairphoto_core::app::slideshow::{claim_slideshow, SlideshowJob, SlideshowOptions, SLIDESHOW_CANCELLED};
use chairphoto_core::app::CoreEvent;
use chairphoto_core::image_pool::ImageKind;
use chairphoto_model::slideshow::{
    progress_label, reorder, snap, video_dims, Orientation, Resolution, DEFAULT_DEST, DURATION_RANGE, FPS_CHOICES,
    TRANSITION_RANGE,
};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::prelude::*;
use gpui_kit::{div, img, px, AnyElement, Context, Entity, ObjectFit, SharedString, Subscription, TestSupportExt as _, Window};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// What a landed claim does next.
enum Go {
    Run,
    Cancelled,
    Superseded,
}

/// What the dialog says when a worker step died (a panic) without an answer.
const STOPPED: &str = "The slideshow render stopped unexpectedly";

/// The running render: its claim, once it has landed.
#[derive(Default)]
struct Running {
    /// Cancel was pressed (before or after the claim landed).
    cancelled: bool,
    /// The claimed job's id.
    job: Option<u64>,
    /// The claimed job's own abort flag.
    abort: Option<Arc<AtomicBool>>,
}

/// Dragged strip tile: where it came from.
#[derive(Clone)]
struct DraggedSlide {
    from: usize,
    name: SharedString,
}

struct DragBadge(SharedString);

impl Render for DragBadge {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        div().px(px(8.)).py(px(4.)).rounded(px(4.)).bg(colors.elev).text_size(px(11.)).text_color(colors.txt).child(self.0.clone())
    }
}

pub struct SlideshowDialog {
    host: DialogHost,
    backend: SlideshowBackend,
    /// The selection when the dialog opened, in play order (reordered by drag).
    pub order: Vec<Picked>,
    snapshot: SelectionSnapshot,
    pub duration: f64,
    pub transition: bool,
    pub transition_duration: f64,
    pub ken_burns: bool,
    pub fps: u32,
    pub orientation: Orientation,
    pub resolution: Resolution,
    pub dest: Entity<InputState>,
    duration_slider: Entity<SliderState>,
    transition_slider: Entity<SliderState>,
    pub busy: bool,
    /// The newest `slideshow:progress` of this dialog's job.
    pub progress: Option<(u32, u32)>,
    pub output: Option<PathBuf>,
    pub error: Option<String>,
    /// The running render; a new Render replaces it.
    running: Option<Running>,
    /// Bumped by every Render; a superseded render's answer is dropped.
    attempt: u64,
    _subscriptions: Vec<Subscription>,
}

impl SlideshowDialog {
    pub fn new(host: DialogHost, backend: SlideshowBackend, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let snapshot = SelectionSnapshot::take(&host.shell, cx);
        let dest = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder(DEFAULT_DEST);
            s.set_value(DEFAULT_DEST, window, cx);
            s
        });
        let slider = |(min, max, step): (f64, f64, f64), value: f64, cx: &mut Context<Self>| {
            // `max` before `min`: `min` clamps against the default max (100).
            cx.new(|_| SliderState::new().max(max as f32).min(min as f32).step(step as f32).default_value(value as f32))
        };
        let duration_slider = slider(DURATION_RANGE, 4.0, cx);
        let transition_slider = slider(TRANSITION_RANGE, 1.0, cx);
        let mut subs = vec![
            dialog::close_on_switch(&host.model, window, cx),
            cx.subscribe(&host.model, |this: &mut Self, _, event: &AppModelEvent, cx| {
                if let AppModelEvent::Core(CoreEvent::SlideshowProgress(p)) = event {
                    this.on_progress(p.job, p.done, p.total, cx);
                }
            }),
            cx.subscribe_in(&duration_slider, window, |this: &mut Self, _, e: &SliderEvent, _, cx| {
                if let SliderEvent::Change(v) = e {
                    this.duration = snap(v.start() as f64, DURATION_RANGE);
                    cx.notify();
                }
            }),
            cx.subscribe_in(&transition_slider, window, |this: &mut Self, _, e: &SliderEvent, _, cx| {
                if let SliderEvent::Change(v) = e {
                    this.transition_duration = snap(v.start() as f64, TRANSITION_RANGE);
                    cx.notify();
                }
            }),
        ];
        if let Some(images) = &host.images {
            subs.push(cx.observe(images, |_, _, cx| cx.notify()));
        }
        SlideshowDialog {
            order: snapshot.photos.clone(),
            snapshot,
            host,
            backend,
            duration: 4.0,
            transition: true,
            transition_duration: 1.0,
            ken_burns: false,
            fps: 30,
            orientation: Orientation::Landscape,
            resolution: Resolution::Hd,
            dest,
            duration_slider,
            transition_slider,
            busy: false,
            progress: None,
            output: None,
            error: None,
            running: None,
            attempt: 0,
            _subscriptions: subs,
        }
    }

    /// The options the dialog would render with.
    pub fn options(&self) -> SlideshowOptions {
        let (width, height) = video_dims(self.orientation, self.resolution);
        SlideshowOptions {
            duration_per_photo: self.duration,
            transition: self.transition,
            transition_duration: self.transition_duration,
            ken_burns: self.ken_burns,
            fps: self.fps,
            width,
            height,
        }
    }

    /// Move strip tile `from` to `to`.
    pub fn reorder(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        reorder(&mut self.order, from, to);
        cx.notify();
    }


    /// The running render's job id, once its claim has landed.
    pub fn job(&self) -> Option<u64> {
        self.running.as_ref().and_then(|r| r.job)
    }

    fn on_progress(&mut self, job: u64, done: u32, total: u32, cx: &mut Context<Self>) {
        if self.busy && self.job() == Some(job) {
            self.progress = Some((done, total));
            cx.notify();
        }
    }

    /// Render: claim core's slideshow job on a worker, then run it there.
    pub fn render_movie(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        self.output = None;
        self.progress = None;
        let dest = self.dest.read(cx).value().trim().to_string();
        if dest.is_empty() {
            self.error = Some("Choose an output folder.".into());
            cx.notify();
            return;
        }
        let Some(catalog) = self.snapshot.catalog else {
            self.error = Some(NO_ROWS.into());
            cx.notify();
            return;
        };
        self.attempt += 1;
        let attempt = self.attempt;
        self.running = Some(Running::default());
        self.busy = true;
        let (state, ids, opts, backend) = (self.host.app.clone(), self.order.iter().map(|p| p.id).collect::<Vec<_>>(), self.options(), self.backend.clone());
        let ffmpeg = backend.ffmpeg.clone();
        let claim = Runner::get(cx).run(move || claim_slideshow(&state, Some(catalog), &ids, opts, &dest, ffmpeg()));
        let host = self.host.clone();
        cx.spawn(async move |this, cx| {
            let gone = |result: &Result<PathBuf, String>| match result {
                Ok(path) => format!("Slideshow saved to {}", path.display()),
                Err(e) => format!("Slideshow failed: {e}"),
            };
            let job: SlideshowJob = match claim.await.unwrap_or_else(|_| Err(STOPPED.into())) {
                Ok(job) => job,
                Err(e) => {
                    let result = Err(e);
                    if this.update(cx, |d, cx| d.land(attempt, result.clone(), cx)).is_err() {
                        cx.update(|cx| host.status(gone(&result), cx));
                    }
                    return;
                }
            };
            // A closed dialog does not stop the render (its answer goes to the status line).
            let go = this.update(cx, |d, cx| d.claimed(attempt, &job, cx)).unwrap_or(Go::Run);
            match go {
                Go::Run => {}
                Go::Cancelled => {
                    this.update(cx, |d, cx| d.land(attempt, Err(SLIDESHOW_CANCELLED.into()), cx)).ok();
                    return;
                }
                Go::Superseded => return,
            }
            let frames = backend.frames.clone();
            let run = cx.update(|cx| Runner::get(cx).run(move || job.run_with(&frames)));
            let result = run.await.unwrap_or_else(|_| Err(STOPPED.into()));
            if this.update(cx, |d, cx| d.land(attempt, result.clone(), cx)).is_err() {
                cx.update(|cx| host.status(gone(&result), cx));
            }
        })
        .detach();
        cx.notify();
    }

    /// The claim of render `attempt` landed: keep its id and abort flag, unless Cancel came
    /// first (then it never runs) or a newer Render superseded it.
    fn claimed(&mut self, attempt: u64, job: &SlideshowJob, cx: &mut Context<Self>) -> Go {
        if attempt != self.attempt {
            return Go::Superseded;
        }
        let Some(running) = self.running.as_mut() else { return Go::Superseded };
        running.job = Some(job.job);
        running.abort = Some(job.abort_handle());
        cx.notify();
        if running.cancelled {
            Go::Cancelled
        } else {
            Go::Run
        }
    }

    fn land(&mut self, attempt: u64, result: Result<PathBuf, String>, cx: &mut Context<Self>) {
        if attempt != self.attempt {
            return; // a superseded render
        }
        self.busy = false;
        self.running = None;
        match result {
            Ok(path) => self.output = Some(path),
            Err(e) => self.error = Some(e),
        }
        cx.notify();
    }

    /// Cancel the running render: trip its job once claimed, else keep it from running.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(r) = self.running.as_mut() {
            r.cancelled = true;
            if let Some(abort) = &r.abort {
                abort.store(true, Ordering::Relaxed);
            }
        }
        cx.notify();
    }

    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        dialog::pick_folder(
            "Choose the output folder",
            window,
            cx,
            |d, path, window, cx| d.dest.update(cx, |i, cx| i.set_value(path, window, cx)),
            |d, e, cx| {
                d.error = Some(e);
                cx.notify();
            },
        );
    }

    fn strip(&self, colors: Colors, cx: &mut Context<Self>) -> AnyElement {
        let images: Vec<ImageState> = match &self.host.images {
            Some(images) => images.update(cx, |store, _| {
                let wanted: Vec<_> = self.order.iter().map(|p| (p.id, ImageKind::Thumb)).collect();
                store.request_batch(&wanted);
                self.order.iter().map(|p| store.get(p.id, ImageKind::Thumb)).collect()
            }),
            None => self.order.iter().map(|_| ImageState::Absent).collect(),
        };
        let tiles = self.order.iter().zip(images).enumerate().map(|(i, (p, image))| {
            let tile = div()
                .id(("slideshow-tile", i))
                .relative()
                .flex_none()
                .w(px(72.))
                .h(px(54.))
                .rounded(px(4.))
                .overflow_hidden()
                .bg(colors.panel)
                .border_1()
                .border_color(colors.border)
                .cursor_grab()
                .tooltip(crate::tags::tip(p.name.clone()))
                .on_drag(DraggedSlide { from: i, name: p.name.clone() }, |d, _, _, cx| cx.new(|_| DragBadge(d.name.clone())))
                .drag_over::<DraggedSlide>(move |s, _, _, _| s.border_color(colors.accent))
                .on_drop(cx.listener(move |d, dragged: &DraggedSlide, _, cx| d.reorder(dragged.from, i, cx)));
            let tile = match image {
                ImageState::Ready(l) => tile.child(img(l.image).size_full().object_fit(ObjectFit::Cover)),
                _ => tile,
            };
            tile.child(
                div()
                    .absolute()
                    .left(px(3.))
                    .top(px(3.))
                    .px(px(4.))
                    .rounded(px(3.))
                    .bg(colors.scrim)
                    .text_size(px(10.))
                    .text_color(colors.txt)
                    .child(format!("{}", i + 1)),
            )
            .test_support()
            .into_any_element()
        });
        ui::row().gap(px(6.)).children(tiles).into_any_element()
    }

    fn choice(&self, id: SharedString, label: impl Into<SharedString>, on: bool, colors: Colors, cx: &mut Context<Self>, f: impl Fn(&mut Self) + 'static) -> AnyElement {
        let chip = ui::chip(id, label, true, colors);
        let chip = if on { chip.border_color(colors.accent).text_color(colors.txt) } else { chip };
        ui::clickable(chip, true, cx.listener(move |d, _, _, cx| {
            f(d);
            cx.notify();
        }))
    }
}

impl Render for SlideshowDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let body = ui::body().id("slideshow-dialog");
        if self.order.len() < 2 {
            return body
                .child(div().id("slideshow-hint").child(ui::sub("Select at least 2 photos in the library, then open Make slideshow again.", colors)).test_support())
                .test_support();
        }
        let (w, h) = video_dims(self.orientation, self.resolution);
        let mut body = body
            .child(ui::label(format!("Photos ({}) — drag to reorder", self.order.len()), colors))
            .child(self.strip(colors, cx))
            .child(ui::label(format!("Duration per photo {}s", self.duration), colors))
            .child(div().w(px(260.)).child(Slider::new(&self.duration_slider)))
            .child(ui::label("Orientation & resolution", colors));
        let mut orient = ui::row();
        for o in Orientation::ALL {
            orient = orient.child(self.choice(format!("slideshow-orient-{o:?}").to_lowercase().into(), o.label(), self.orientation == o, colors, cx, move |d| d.orientation = o));
        }
        for r in Resolution::ALL {
            orient = orient.child(self.choice(format!("slideshow-res-{r:?}").to_lowercase().into(), r.label(), self.resolution == r, colors, cx, move |d| d.resolution = r));
        }
        body = body.child(orient).child(ui::sub(
            format!("Output {w}×{h}. Portrait is for mobile (Snapchat/Stories). Photos are rotated upright and letterboxed to fit (never cropped)."),
            colors,
        ));
        body = body.child(toggle("slideshow-crossfade", self.transition, "Crossfade transitions", colors, cx.listener(|d, _, _, cx| {
            d.transition = !d.transition;
            cx.notify();
        })));
        if self.transition {
            body = body
                .child(ui::label(format!("Transition duration {}s", self.transition_duration), colors))
                .child(div().w(px(260.)).child(Slider::new(&self.transition_slider)));
        }
        body = body.child(toggle("slideshow-kenburns", self.ken_burns, "Ken Burns (slow pan/zoom)", colors, cx.listener(|d, _, _, cx| {
            d.ken_burns = !d.ken_burns;
            cx.notify();
        })));
        let mut fps = ui::row().child(ui::label("Frame rate", colors));
        for f in FPS_CHOICES {
            fps = fps.child(self.choice(format!("slideshow-fps-{f}").into(), format!("{f} fps"), self.fps == f, colors, cx, move |d| d.fps = f));
        }
        body = body.child(fps).child(ui::label("Output folder", colors)).child(
            ui::row()
                .child(div().w(px(380.)).child(Input::new(&self.dest)))
                .child(ui::clickable(ui::chip("slideshow-browse", "Browse…", true, colors), true, cx.listener(|d, _, window, cx| d.browse(window, cx)))),
        );
        let busy = self.busy;
        let mut actions = ui::row().child(ui::clickable(
            ui::primary("slideshow-render", if busy { "Rendering…" } else { "Render" }, !busy, colors),
            !busy,
            cx.listener(|d, _, _, cx| d.render_movie(cx)),
        ));
        if busy {
            actions = actions.child(ui::clickable(ui::chip("slideshow-cancel", "Cancel", true, colors), true, cx.listener(|d, _, _, cx| d.cancel(cx))));
        }
        body = body.child(actions);
        if busy {
            let pct = self.progress.map_or(0, |(d, t)| chairphoto_model::slideshow::percent(d, t));
            body = body
                .child(
                    div()
                        .h(px(4.))
                        .w(px(260.))
                        .rounded(px(2.))
                        .bg(colors.well)
                        .child(div().h_full().rounded(px(2.)).bg(colors.accent).w(px(260. * pct as f32 / 100.))),
                )
                .child(div().id("slideshow-progress").child(ui::sub(progress_label(true, self.progress), colors)).test_support());
        }
        if let Some(path) = self.output.clone() {
            body = body.child(
                ui::row()
                    .id("slideshow-output")
                    .child(ui::sub(format!("Saved to {}", path.display()), colors))
                    .child(ui::clickable(ui::chip("slideshow-reveal", "Reveal", true, colors), true, move |_, _, cx| cx.reveal_path(&path)))
                    .test_support(),
            );
        }
        if let Some(e) = &self.error {
            body = body.child(ui::error("slideshow-error", e.clone(), colors));
        }
        body.test_support()
    }
}
