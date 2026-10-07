//! Darkroom frames (#101): the stage's image while the user edits, rendered by the core's
//! `plugins::edit::render_proxy` through the image pool (`JobKey::Edit`, so identical
//! requests merge and the newest renders first) and handed to GPUI as a BGRA
//! `RenderImage` — no JPEG or PNG anywhere on this path (`media::render_edit_image`).
//!
//! The React Darkroom's timing, kept: while the record changes (a slider drag) a **fast**
//! frame at [`FAST_EDGE`] goes out at most every [`FAST_INTERVAL`] (leading edge); once the
//! record has been quiet for [`SETTLE`], one **full** frame at [`FULL_EDGE`] follows.
//!
//! Every change bumps a generation, and each frame carries the generation it rendered. A
//! frame is shown only if it is newer than the one on screen — a later generation, or the
//! full frame of the generation whose fast frame is showing. Anything else is stale and
//! dropped, so a slow full render of an old state can never paint over a newer fast frame.
//! A replaced frame's texture is removed from the atlas.
//!
//! A frame that fails is never silent: if it was rendering the newest record (the stage's
//! current generation), the stage holds a [`StageFailure`] until a frame of that generation
//! or a newer one is shown, so the view can mark the shown frame — older than the record, or
//! none at all — as not current. A failure of an older generation while a newer one is still
//! on its way is not reported: the newer frame is what the stage waits for.
//!
//! A request also cancels the stage's older requests still queued in the pool — they could
//! only ever be stale — unless the pool merged the new request into one of them (the same
//! record and size again).
//!
//! Every job carries the catalog the stage's photo was read from (`EditJob::catalog`, #251).
//! A render cannot be interrupted, and the pool merges identical keys; without it a stage
//! opened after a catalog switch for the same photo id, record and source would adopt a render
//! still running for the other catalog's photo. The worker renders a job only while its
//! catalog is the open one (checked under the catalog lock); in a switch's window, before
//! `catalog:switched` closes the stage, it answers `CATALOG_CHANGED` instead of the new
//! catalog's photo of that id, and the stage drops that answer like a cancellation: the frame
//! shown stays, and no failure is reported.
//!
//! **Frame timing.** Every request is stamped ([`FrameSample`], `chairphoto_model`'s
//! `render_timing`): requested, answered (the BGRA frame is back on the UI thread) and taken
//! as the stage's frame — or superseded (cancelled or stale). The last [`MAX_SAMPLES`] are
//! kept for [`DarkroomStage::timing_summary`]; with logging on, each finished frame prints
//! an `[edit-timing]` line (the Darkroom turns it on with `editor.renderTiming`).
//!
//! **Clipping layer.** A stage made [`clip_layer`](DarkroomStage::clip_layer) asks for the
//! sensor-clipping overlay (`EditJob::clip`) instead of the render; it is the same frame
//! source, so its frames are just as ordered and as catalog-bound.
//!
//! The Darkroom view (`super::view`) draws these frames; `super::session` owns the stage.

use crate::image_store::{Loaded, Submit};
use chairphoto_core::app::{CatalogIdentity, CATALOG_CHANGED};
use chairphoto_core::image_pool::{EditJob, JobKey, CANCELLED};
use chairphoto_core::plugins::edit::SourceToken;
use chairphoto_model::darkroom::render_timing::{format_sample, summarize, FrameSample, Tier, TimingSummary};
use futures::channel::mpsc::{unbounded, UnboundedSender};
use futures::StreamExt as _;
use gpui_kit::{App, Context, RenderImage, Task};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How many frame samples a stage keeps (the React log kept 200–400).
pub const MAX_SAMPLES: usize = 400;

/// The live-drag tier's longest edge (React `PREVIEW_FAST`).
pub const FAST_EDGE: u32 = 720;
/// The settled tier's longest edge (React `PREVIEW_MAX`).
pub const FULL_EDGE: u32 = 1400;
/// At most one fast frame per this interval while the record changes.
pub const FAST_INTERVAL: Duration = Duration::from_millis(90);
/// Quiet time after the last change before the full frame renders.
pub const SETTLE: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FrameTier {
    Fast,
    Full,
}

impl FrameTier {
    pub fn max_edge(self) -> u32 {
        match self {
            FrameTier::Fast => FAST_EDGE,
            FrameTier::Full => FULL_EDGE,
        }
    }

    /// The timing log's name for the tier (`fast`, `settled`).
    pub fn timing_tier(self) -> Tier {
        match self {
            FrameTier::Fast => Tier::Fast,
            FrameTier::Full => Tier::Settled,
        }
    }
}

/// A frame on the stage.
#[derive(Clone)]
pub struct StageFrame {
    pub image: Arc<RenderImage>,
    pub generation: u64,
    pub tier: FrameTier,
}

/// The newest record's render failed. The frame on the stage, if any, is older than the
/// record the user sees in the controls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageFailure {
    pub generation: u64,
    pub tier: FrameTier,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameStats {
    pub fast_requested: u64,
    pub full_requested: u64,
    pub shown: u64,
    pub stale_dropped: u64,
    pub failed: u64,
}

/// How a requested frame ended ([`frame_outcome`]), for its timing sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameOutcome {
    /// It became the stage's frame.
    Shown,
    /// Cancelled, refused for a catalog no longer open (`CATALOG_CHANGED`, #251), rendered for
    /// a closed stage, or not newer than the frame shown.
    Superseded,
    /// The newest-so-far frame's render failed.
    Failed,
}

/// How long a change waits for its fast frame: nothing on the leading edge, else the rest of
/// [`FAST_INTERVAL`] since the last fast frame went out. The stage's throttle — shared with
/// `examples/darkroom_bench.rs`, which plays a drag through it on real wall-clock time.
pub fn fast_wait(last_fast: Option<Instant>, now: Instant) -> Duration {
    match last_fast {
        Some(at) => FAST_INTERVAL.saturating_sub(now.saturating_duration_since(at)),
        None => Duration::ZERO,
    }
}

/// What a finished frame of `generation` at `tier` is to a stage showing `shown` (closed at
/// `closed_at`, if it was): cancelled, refused for another catalog, closed-over or not newer —
/// superseded; else shown, or failed. The stage's ordering rule (module docs) — shared with
/// `examples/darkroom_bench.rs`.
pub fn frame_outcome(
    generation: u64,
    tier: FrameTier,
    shown: Option<(u64, FrameTier)>,
    closed_at: Option<u64>,
    result: Result<(), &str>,
) -> FrameOutcome {
    let dropped = matches!(result, Err(e) if e == CANCELLED || e == CATALOG_CHANGED);
    if dropped || closed_at.is_some_and(|g| generation < g) {
        return FrameOutcome::Superseded;
    }
    if shown.is_some_and(|s| (generation, tier) <= s) {
        return FrameOutcome::Superseded;
    }
    match result {
        Ok(()) => FrameOutcome::Shown,
        Err(_) => FrameOutcome::Failed,
    }
}

struct FrameDone {
    generation: u64,
    tier: FrameTier,
    result: Result<Loaded, String>,
}

/// How many of `samples` failed: answered, but neither taken as the stage's frame nor
/// superseded ([`FrameOutcome::Failed`]). [`TimingSummary`] counts them in neither column,
/// so a report built from it must count them here.
pub fn failed_frames<'a>(samples: impl IntoIterator<Item = &'a FrameSample>) -> usize {
    samples.into_iter().filter(|s| s.resolved.is_some() && s.painted.is_none() && !s.superseded).count()
}

/// One photo's Darkroom stage.
pub struct DarkroomStage {
    pool: Arc<dyn Submit>,
    photo_id: i64,
    /// The catalog this stage's photo was read from (`OpenPhoto::from`).
    catalog: CatalogIdentity,
    source: SourceToken,
    edit_json: String,
    generation: u64,
    frame: Option<StageFrame>,
    /// Set when the current generation's render failed; see the module docs.
    failure: Option<StageFailure>,
    last_fast: Option<Instant>,
    fast_timer: Option<Task<()>>,
    settle_timer: Option<Task<()>>,
    done: UnboundedSender<FrameDone>,
    stats: FrameStats,
    /// Set by [`close`](Self::close): frames of earlier generations are stale.
    closed_at: Option<u64>,
    /// Requests not yet answered: generation and pool key.
    outstanding: Vec<(u64, JobKey)>,
    /// Ask for the sensor-clipping overlay instead of the render.
    clip: bool,
    /// The clock the samples are measured on, and the samples.
    clock0: Instant,
    samples: VecDeque<FrameSample>,
    log_timing: bool,
    _drain: Task<()>,
}

impl DarkroomStage {
    /// A stage for `photo_id` of the catalog `catalog` (the one its row was read from),
    /// rendering from `source` (the camera preview, or a resident RAW working image by token)
    /// with the record `edit_json`. Renders nothing until asked. A catalog switch needs a new
    /// stage: this one's frames are its catalog's.
    pub fn new(
        pool: Arc<dyn Submit>,
        photo_id: i64,
        catalog: CatalogIdentity,
        source: SourceToken,
        edit_json: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let (done, mut rx) = unbounded::<FrameDone>();
        let _drain = cx.spawn(async move |this, cx| {
            while let Some(d) = rx.next().await {
                if this.update(cx, |stage, cx| stage.frame_done(d, cx)).is_err() {
                    break;
                }
            }
        });
        Self {
            pool,
            photo_id,
            catalog,
            source,
            edit_json,
            generation: 0,
            frame: None,
            failure: None,
            last_fast: None,
            fast_timer: None,
            settle_timer: None,
            done,
            stats: FrameStats::default(),
            closed_at: None,
            outstanding: Vec::new(),
            clip: false,
            clock0: cx.background_executor().now(),
            samples: VecDeque::new(),
            log_timing: false,
            _drain,
        }
    }

    /// This stage renders the sensor-clipping overlay (`EditJob::clip`), not the picture.
    pub fn clip_layer(mut self) -> Self {
        self.clip = true;
        self
    }

    pub fn photo_id(&self) -> i64 {
        self.photo_id
    }

    pub fn source(&self) -> &SourceToken {
        &self.source
    }

    /// Print an `[edit-timing]` line per finished frame.
    pub fn set_timing_log(&mut self, on: bool) {
        self.log_timing = on;
    }

    /// The kept frame samples, oldest first.
    pub fn samples(&self) -> impl Iterator<Item = &FrameSample> {
        self.samples.iter()
    }

    /// The kept samples summarized (latency, answer, take, cadence, per tier).
    pub fn timing_summary(&self) -> TimingSummary {
        summarize(&self.samples.iter().cloned().collect::<Vec<_>>())
    }

    fn now_ms(&self, cx: &Context<Self>) -> f64 {
        cx.background_executor().now().saturating_duration_since(self.clock0).as_secs_f64() * 1e3
    }

    /// Stamp the answer of the newest unanswered sample of `generation` at `tier`.
    fn stamp(&mut self, generation: u64, tier: FrameTier, outcome: FrameOutcome, cx: &Context<Self>) {
        let now = self.now_ms(cx);
        let Some(s) = self
            .samples
            .iter_mut()
            .rev()
            .find(|s| s.seq == generation && s.tier == tier.timing_tier() && s.resolved.is_none() && !s.superseded)
        else {
            return;
        };
        s.resolved = Some(now);
        match outcome {
            FrameOutcome::Shown => s.painted = Some(now),
            FrameOutcome::Superseded => s.superseded = true,
            FrameOutcome::Failed => {}
        }
        if self.log_timing {
            eprintln!("{}", format_sample(s));
        }
    }

    pub fn frame(&self) -> Option<&StageFrame> {
        self.frame.as_ref()
    }

    /// The newest record's render failed, and no frame of that record (or a newer one) has
    /// been shown since: the stage must say so instead of showing [`frame`](Self::frame) as
    /// if it were current.
    pub fn failure(&self) -> Option<&StageFailure> {
        self.failure.as_ref()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn stats(&self) -> FrameStats {
        self.stats
    }

    /// The record changed (a slider moved): schedule a throttled fast frame and restart the
    /// settle timer for the full one. Both replace any still-waiting timer of their kind.
    pub fn edit_changed(&mut self, edit_json: String, cx: &mut Context<Self>) {
        self.edit_json = edit_json;
        self.generation += 1;
        let wait = fast_wait(self.last_fast, cx.background_executor().now());
        self.fast_timer = Some(self.after(wait, FrameTier::Fast, cx));
        self.settle_timer = Some(self.after(SETTLE, FrameTier::Full, cx));
    }

    fn after(&self, wait: Duration, tier: FrameTier, cx: &mut Context<Self>) -> Task<()> {
        let timer = cx.background_executor().timer(wait);
        cx.spawn(async move |this, cx| {
            timer.await;
            let _ = this.update(cx, |stage, cx| {
                stage.request(tier, cx);
            });
        })
    }

    /// Render the current record at `tier` now. Returns the generation the frame carries.
    pub fn request(&mut self, tier: FrameTier, cx: &mut Context<Self>) -> u64 {
        match tier {
            FrameTier::Fast => {
                self.last_fast = Some(cx.background_executor().now());
                self.stats.fast_requested += 1;
            }
            FrameTier::Full => self.stats.full_requested += 1,
        }
        let generation = self.generation;
        let job = JobKey::Edit(EditJob {
            photo_id: self.photo_id,
            edit_json: self.edit_json.clone(),
            max_edge: tier.max_edge(),
            hi_res: false,
            base_only: false,
            source: self.source.clone(),
            clip: self.clip,
            catalog: self.catalog,
        });
        let requested = self.now_ms(cx);
        if self.samples.len() >= MAX_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(FrameSample::new(generation, tier.timing_tier(), requested));
        self.cancel_older(generation, Some(&job));
        self.outstanding.push((generation, job.clone()));
        let done = self.done.clone();
        self.pool.submit_batch(vec![(
            job,
            Box::new(move |result| {
                let _ = done.unbounded_send(FrameDone { generation, tier, result });
            }),
        )]);
        generation
    }

    /// Cancel the queued requests older than `generation`, except one sharing `keep`'s key
    /// (the pool merged the newer request into it, and cancelling would answer both).
    fn cancel_older(&mut self, generation: u64, keep: Option<&JobKey>) {
        let pool = self.pool.clone();
        self.outstanding.retain(|(g, key)| {
            if *g >= generation || Some(key) == keep {
                return true;
            }
            // A running render cannot be cancelled; it stays outstanding and is dropped as
            // stale when it answers.
            !pool.cancel(key)
        });
    }

    /// Leaving the Darkroom: stop the timers, make every in-flight frame stale, and remove
    /// the shown frame's texture from the atlas. (A stage dropped without this leaves its
    /// last texture in the atlas: `drop_image` needs an `App`, which `Drop` does not have.)
    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.fast_timer = None;
        self.settle_timer = None;
        self.generation += 1;
        self.cancel_older(self.generation, None);
        if let Some(old) = self.frame.take() {
            cx.defer(move |cx: &mut App| cx.drop_image(old.image, None));
        }
        self.failure = None;
        self.closed_at = Some(self.generation);
        cx.notify();
    }

    fn frame_done(&mut self, done: FrameDone, cx: &mut Context<Self>) {
        if let Some(at) = self.outstanding.iter().position(|(g, key)| {
            *g == done.generation
                && matches!(key, JobKey::Edit(job) if job.max_edge == done.tier.max_edge())
        }) {
            self.outstanding.remove(at);
        }
        let shown = self.frame.as_ref().map(|f| (f.generation, f.tier));
        let result = done.result.as_ref().map(|_| ()).map_err(String::as_str);
        if frame_outcome(done.generation, done.tier, shown, self.closed_at, result) == FrameOutcome::Superseded {
            self.stats.stale_dropped += 1;
            self.stamp(done.generation, done.tier, FrameOutcome::Superseded, cx);
            return;
        }
        match done.result {
            Ok(loaded) => {
                self.stats.shown += 1;
                self.stamp(done.generation, done.tier, FrameOutcome::Shown, cx);
                let old = self.frame.replace(StageFrame {
                    image: loaded.image,
                    generation: done.generation,
                    tier: done.tier,
                });
                if let Some(old) = old {
                    cx.defer(move |cx: &mut App| cx.drop_image(old.image, None));
                }
                if self.failure.as_ref().is_some_and(|f| done.generation >= f.generation) {
                    self.failure = None;
                }
                cx.notify();
            }
            Err(e) => {
                self.stats.failed += 1;
                self.stamp(done.generation, done.tier, FrameOutcome::Failed, cx);
                eprintln!("darkroom: photo {} frame {}: {e}", self.photo_id, done.generation);
                // Only the current record's failure is the stage's state; an older one is
                // superseded by the newer frame already on its way.
                if done.generation == self.generation {
                    self.failure =
                        Some(StageFailure { generation: done.generation, tier: done.tier, message: e });
                    cx.notify();
                }
            }
        }
    }
}

/// Frames forced out of order through a hand-driven pool (`image_tests::FakePool`), and the
/// throttle/settle timers on GPUI's fake clock.
#[cfg(test)]
mod tests {
    use super::{DarkroomStage, FrameTier, FAST_EDGE, FAST_INTERVAL, FULL_EDGE, SETTLE};
    use crate::image_store::Submit;
    use crate::image_tests::{identity, pixels, FakePool};
    use chairphoto_core::image_pool::JobKey;
    use gpui_kit::{AppContext as _, Entity, TestAppContext};
    use std::sync::Arc;
    use chairphoto_core::plugins::edit::SourceToken;
    use std::time::Duration;

    fn stage(cx: &mut TestAppContext) -> (Arc<FakePool>, Entity<DarkroomStage>) {
        let pool = Arc::new(FakePool::default());
        let stage = cx.update(|cx| {
            let pool: Arc<dyn Submit> = pool.clone();
            cx.new(|cx| DarkroomStage::new(pool, 42, identity(1), SourceToken::Preview, "{}".into(), cx))
        });
        (pool, stage)
    }

    fn edge(key: &JobKey) -> u32 {
        match key {
            JobKey::Edit(job) => job.max_edge,
            other => panic!("not an edit job: {other:?}"),
        }
    }

    /// Frames come back out of order: a newer generation's fast frame first, then the older
    /// generation's. The older one is dropped; the full frame of the newest generation then
    /// replaces the fast one; a late full frame of an older generation is dropped too.
    #[gpui_kit::test]
    fn a_stale_frame_never_replaces_a_newer_one(cx: &mut TestAppContext) {
        let (pool, stage) = stage(cx);
        // Every job is taken by a worker at once, so none can be cancelled: each renders and
        // answers, in whatever order the test chooses.
        let running = |pool: &FakePool| pool.start(pool.last_batch()[0].clone());
        let g1 = stage.update(cx, |s, cx| {
            s.edit_changed(r#"{"tone":{"ev":0.1}}"#.into(), cx);
            s.request(FrameTier::Fast, cx)
        });
        running(&pool);
        let g2 = stage.update(cx, |s, cx| {
            s.edit_changed(r#"{"tone":{"ev":0.2}}"#.into(), cx);
            s.request(FrameTier::Fast, cx)
        });
        running(&pool);
        let g2_full = stage.update(cx, |s, cx| s.request(FrameTier::Full, cx));
        running(&pool);
        assert!(g1 < g2 && g2 == g2_full);
        // Submissions: [g1 fast, g2 fast, g2 full] (the throttle timers have not fired).
        let (a, b, c) = (pixels(4, 4), pixels(4, 4), pixels(8, 8));
        pool.finish_nth(1, Ok(b.clone())); // g2 fast
        cx.run_until_parked();
        pool.finish_nth(0, Ok(a)); // g1 fast, late
        cx.run_until_parked();
        stage.update(cx, |s, _| {
            let f = s.frame().unwrap();
            assert_eq!((f.generation, f.tier), (g2, FrameTier::Fast));
            assert!(Arc::ptr_eq(&f.image, &b.image));
            assert_eq!(s.stats().stale_dropped, 1);
        });
        pool.finish_nth(2, Ok(c.clone())); // g2 full
        cx.run_until_parked();
        stage.update(cx, |s, _| {
            let f = s.frame().unwrap();
            assert_eq!((f.generation, f.tier), (g2, FrameTier::Full));
            assert!(Arc::ptr_eq(&f.image, &c.image));
        });

        // A full frame of an older generation, arriving after a newer fast frame, is stale.
        let g3 = stage.update(cx, |s, cx| {
            s.edit_changed(r#"{"tone":{"ev":0.3}}"#.into(), cx);
            s.request(FrameTier::Full, cx)
        });
        running(&pool);
        let g4 = stage.update(cx, |s, cx| {
            s.edit_changed(r#"{"tone":{"ev":0.4}}"#.into(), cx);
            s.request(FrameTier::Fast, cx)
        });
        running(&pool);
        let n = pool.submitted();
        pool.finish_nth(n - 1, Ok(pixels(4, 4))); // g4 fast
        cx.run_until_parked();
        pool.finish_nth(n - 2, Ok(pixels(8, 8))); // g3 full, late
        cx.run_until_parked();
        stage.update(cx, |s, _| {
            let f = s.frame().unwrap();
            assert!(g3 < g4);
            assert_eq!((f.generation, f.tier), (g4, FrameTier::Fast));
            assert_eq!(s.stats().stale_dropped, 2);
        });
    }

    /// While the record keeps changing, fast frames go out at most every FAST_INTERVAL and no
    /// full frame; SETTLE after the last change, exactly one full frame of the last record.
    #[gpui_kit::test]
    fn fast_while_dragging_full_after_settle(cx: &mut TestAppContext) {
        let (pool, stage) = stage(cx);
        let step = Duration::from_millis(30);
        for i in 0..10 {
            stage.update(cx, |s, cx| s.edit_changed(format!(r#"{{"tone":{{"ev":{i}}}}}"#), cx));
            cx.run_until_parked();
            cx.executor().advance_clock(step);
            cx.run_until_parked();
        }
        // 10 changes over 300 ms: the leading edge at 0, then one per FAST_INTERVAL.
        let (fast, full) = stage.update(cx, |s, _| (s.stats().fast_requested, s.stats().full_requested));
        let most = 1 + (step * 10).as_millis() / FAST_INTERVAL.as_millis();
        assert!(fast >= 3 && fast as u128 <= most, "fast frames: {fast} (at most {most})");
        assert_eq!(full, 0, "no full frame while dragging");

        // The last change was 30 ms ago; the full frame is due SETTLE after it.
        cx.executor().advance_clock(SETTLE - step - Duration::from_millis(1));
        cx.run_until_parked();
        assert_eq!(stage.update(cx, |s, _| s.stats().full_requested), 0);
        cx.executor().advance_clock(Duration::from_millis(1));
        cx.run_until_parked();
        assert_eq!(stage.update(cx, |s, _| s.stats().full_requested), 1);

        let batches = pool.batches.lock().unwrap().clone();
        let last = batches.last().unwrap();
        assert_eq!(edge(&last[0]), FULL_EDGE);
        match &last[0] {
            JobKey::Edit(job) => assert_eq!(job.edit_json, r#"{"tone":{"ev":9}}"#),
            _ => unreachable!(),
        }
        assert!(batches[..batches.len() - 1].iter().all(|b| edge(&b[0]) == FAST_EDGE));
    }

    /// A newer request cancels the older ones still queued — but not one the pool merged the
    /// newer request into (the record went back to an earlier state), and not a running one.
    #[gpui_kit::test]
    fn a_newer_request_cancels_older_queued_frames(cx: &mut TestAppContext) {
        let (pool, stage) = stage(cx);
        let a = r#"{"tone":{"ev":0.1}}"#;
        let b = r#"{"tone":{"ev":0.2}}"#;
        let c = r#"{"tone":{"ev":0.3}}"#;
        stage.update(cx, |s, cx| {
            s.edit_changed(c.into(), cx);
            s.request(FrameTier::Fast, cx); // g1, C: a worker takes it
        });
        let running = pool.last_batch()[0].clone();
        pool.start(running.clone());
        stage.update(cx, |s, cx| {
            s.edit_changed(b.into(), cx);
            s.request(FrameTier::Fast, cx); // g2, B: queued
            s.edit_changed(a.into(), cx);
            s.request(FrameTier::Fast, cx); // g3, A: cancels g2; g1 is running
            s.edit_changed(a.into(), cx);
            s.request(FrameTier::Fast, cx); // g4, A again: the pool merges it into g3's job
        });
        let cancelled = pool.cancelled.lock().unwrap().clone();
        assert_eq!(cancelled.len(), 1, "{cancelled:?}");
        match &cancelled[0] {
            JobKey::Edit(job) => assert_eq!(job.edit_json, b),
            other => panic!("{other:?}"),
        }
        cx.run_until_parked(); // g2's CANCELLED answer: dropped quietly
        pool.finish(&running, Ok(pixels(4, 4))); // g1
        cx.run_until_parked();
        let a_key = pool.last_batch()[0].clone();
        pool.finish(&a_key, Ok(pixels(4, 4))); // answers g3 and g4
        cx.run_until_parked();
        stage.update(cx, |s, _| {
            let f = s.frame().expect("g4's frame");
            assert_eq!(f.generation, 4);
            assert_eq!(s.stats().failed, 0, "a cancellation is not a failure");
            assert_eq!(s.stats().stale_dropped, 1, "g2's cancellation");
        });
    }

    /// Codex gate finding 2: the newest record's render fails. The older frame stays on the
    /// stage, but the stage reports the failure instead of passing it off as current. A failure
    /// of an older generation while a newer one is on its way is not reported (progressive
    /// display while dragging), and a newer frame clears a reported failure.
    #[gpui_kit::test]
    fn a_failed_newest_frame_is_reported_not_hidden(cx: &mut TestAppContext) {
        let (pool, stage) = stage(cx);
        let g1 = stage.update(cx, |s, cx| {
            s.edit_changed(r#"{"tone":{"ev":0.1}}"#.into(), cx);
            s.request(FrameTier::Fast, cx)
        });
        pool.finish(&pool.last_batch()[0], Ok(pixels(4, 4)));
        cx.run_until_parked();
        let g2 = stage.update(cx, |s, cx| {
            s.edit_changed(r#"{"tone":{"ev":0.2}}"#.into(), cx);
            s.request(FrameTier::Fast, cx)
        });
        pool.finish(&pool.last_batch()[0], Err("decode failed".into()));
        cx.run_until_parked();
        stage.update(cx, |s, _| {
            assert_eq!(s.frame().map(|f| f.generation), Some(g1), "the older frame stays");
            let f = s.failure().expect("the newest record's failure is reported");
            assert_eq!((f.generation, f.tier, f.message.as_str()), (g2, FrameTier::Fast, "decode failed"));
        });

        // Dragging on: g3 and g4 go out; g3 fails while g4 is on its way — not reported (g4
        // is what the stage waits for) — then g4 lands and clears the g2 failure.
        let g3 = stage.update(cx, |s, cx| {
            s.edit_changed(r#"{"tone":{"ev":0.3}}"#.into(), cx);
            s.request(FrameTier::Fast, cx)
        });
        let k3 = pool.last_batch()[0].clone();
        pool.start(k3.clone()); // running: g4 cannot cancel it
        let g4 = stage.update(cx, |s, cx| {
            s.edit_changed(r#"{"tone":{"ev":0.4}}"#.into(), cx);
            s.request(FrameTier::Fast, cx)
        });
        let k4 = pool.last_batch()[0].clone();
        assert!(g2 < g3 && g3 < g4 && k3 != k4);
        pool.finish(&k3, Err("g3 failed".into()));
        cx.run_until_parked();
        stage.update(cx, |s, _| {
            assert_eq!(s.failure().map(|f| f.generation), Some(g2), "g3's failure is superseded by g4");
        });
        pool.finish(&k4, Ok(pixels(4, 4)));
        cx.run_until_parked();
        stage.update(cx, |s, _| {
            assert_eq!(s.frame().map(|f| f.generation), Some(g4));
            assert!(s.failure().is_none(), "a newer frame clears the failure");
            assert_eq!(s.stats().failed, 2);
            assert_eq!(super::failed_frames(s.samples()), 2, "the timing samples count them too");
        });
    }

    /// The policy functions the stage and `examples/darkroom_bench.rs` share: the throttle is
    /// leading-edge, then the rest of the interval; a frame is taken only if newer than the
    /// one shown (not the same frame again), and what is neither taken nor superseded failed.
    #[test]
    fn the_shared_throttle_and_ordering_rules() {
        use super::{fast_wait, frame_outcome, FrameOutcome};
        use chairphoto_core::image_pool::CANCELLED;
        let t = std::time::Instant::now();
        assert_eq!(fast_wait(None, t), Duration::ZERO);
        assert_eq!(fast_wait(Some(t), t + Duration::from_millis(30)), FAST_INTERVAL - Duration::from_millis(30));
        assert_eq!(fast_wait(Some(t), t + FAST_INTERVAL * 2), Duration::ZERO);

        let (fast, full) = (FrameTier::Fast, FrameTier::Full);
        assert_eq!(frame_outcome(1, fast, None, None, Ok(())), FrameOutcome::Shown);
        assert_eq!(frame_outcome(2, fast, Some((1, full)), None, Ok(())), FrameOutcome::Shown);
        assert_eq!(frame_outcome(1, full, Some((1, fast)), None, Ok(())), FrameOutcome::Shown);
        assert_eq!(frame_outcome(1, fast, Some((1, fast)), None, Ok(())), FrameOutcome::Superseded, "the same frame again");
        assert_eq!(frame_outcome(1, full, Some((2, fast)), None, Ok(())), FrameOutcome::Superseded);
        assert_eq!(frame_outcome(3, fast, None, Some(4), Ok(())), FrameOutcome::Superseded, "closed over");
        assert_eq!(frame_outcome(3, fast, None, None, Err(CANCELLED)), FrameOutcome::Superseded);
        // Refused for a catalog no longer open (#251): dropped, never a failure to report.
        let changed = chairphoto_core::app::CATALOG_CHANGED;
        assert_eq!(frame_outcome(3, fast, Some((2, full)), None, Err(changed)), FrameOutcome::Superseded);
        assert_eq!(frame_outcome(3, fast, Some((2, full)), None, Err("decode failed")), FrameOutcome::Failed);
        assert_eq!(frame_outcome(1, fast, Some((2, full)), None, Err("decode failed")), FrameOutcome::Superseded);
    }

    /// Codex gate finding 3: a stage opened after a catalog switch, for the same photo id,
    /// record and source, must not adopt a render still running for the old catalog. The pool
    /// merges equal keys (the fake answers every responder of a key at once), so the keys must
    /// differ.
    #[gpui_kit::test]
    fn a_new_catalogs_stage_never_adopts_the_old_catalogs_render(cx: &mut TestAppContext) {
        let pool = Arc::new(FakePool::default());
        let open = |catalog: u64, cx: &mut TestAppContext| {
            let pool: Arc<dyn Submit> = pool.clone();
            cx.update(|cx| {
                cx.new(|cx| DarkroomStage::new(pool, 42, identity(catalog), SourceToken::Preview, "{}".into(), cx))
            })
        };
        let old_stage = open(0, cx);
        old_stage.update(cx, |s, cx| s.request(FrameTier::Full, cx));
        let old_job = pool.last_batch()[0].clone();
        pool.start(old_job.clone()); // a worker renders the old catalog's photo 42
        old_stage.update(cx, |s, cx| s.close(cx)); // the catalog switches away

        let new_stage = open(1, cx);
        new_stage.update(cx, |s, cx| s.request(FrameTier::Full, cx));
        let new_job = pool.last_batch()[0].clone();
        assert_ne!(old_job, new_job, "the new catalog's request would merge into the old render");

        let old_pixels = pixels(8, 8);
        pool.finish(&old_job, Ok(old_pixels.clone()));
        cx.run_until_parked();
        new_stage.update(cx, |s, _| {
            if let Some(f) = s.frame() {
                assert!(!Arc::ptr_eq(&f.image, &old_pixels.image), "the old catalog's photo was shown");
            }
        });
        let new_pixels = pixels(8, 8);
        pool.finish(&new_job, Ok(new_pixels.clone()));
        cx.run_until_parked();
        new_stage.update(cx, |s, _| {
            assert!(Arc::ptr_eq(&s.frame().expect("its own frame").image, &new_pixels.image));
        });
    }

    /// Closing the stage cancels its queued frames and makes a running one stale.
    #[gpui_kit::test]
    fn frames_after_close_are_dropped(cx: &mut TestAppContext) {
        let (pool, stage) = stage(cx);
        stage.update(cx, |s, cx| {
            s.request(FrameTier::Full, cx);
        });
        pool.start(pool.last_batch()[0].clone());
        stage.update(cx, |s, cx| {
            s.edit_changed(r#"{"tone":{"ev":0.5}}"#.into(), cx);
            s.request(FrameTier::Fast, cx); // queued
            s.close(cx);
        });
        assert_eq!(pool.cancelled.lock().unwrap().len(), 1, "the queued frame was cancelled");
        pool.finish_nth(0, Ok(pixels(4, 4))); // the running one
        cx.run_until_parked();
        cx.executor().advance_clock(SETTLE * 2);
        cx.run_until_parked();
        stage.update(cx, |s, _| {
            assert!(s.frame().is_none());
            assert_eq!(s.stats().stale_dropped, 2);
            assert_eq!(s.stats().full_requested, 1, "close stopped the settle timer");
        });
    }
}
