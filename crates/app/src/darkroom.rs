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
//! A replaced frame's texture is removed from the atlas. A request also cancels the stage's
//! older requests still queued in the pool — they could only ever be stale — unless the
//! pool merged the new request into one of them (the same record and size again).
//!
//! The Darkroom view itself is a later ticket; this is its frame source.

use crate::image_store::{Loaded, Submit};
use chairphoto_core::image_pool::{EditJob, JobKey, CANCELLED};
use chairphoto_core::plugins::edit::SourceToken;
use futures::channel::mpsc::{unbounded, UnboundedSender};
use futures::StreamExt as _;
use gpui_kit::{App, Context, RenderImage, Task};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
}

/// A frame on the stage.
#[derive(Clone)]
pub struct StageFrame {
    pub image: Arc<RenderImage>,
    pub generation: u64,
    pub tier: FrameTier,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameStats {
    pub fast_requested: u64,
    pub full_requested: u64,
    pub shown: u64,
    pub stale_dropped: u64,
    pub failed: u64,
}

struct FrameDone {
    generation: u64,
    tier: FrameTier,
    result: Result<Loaded, String>,
}

/// One photo's Darkroom stage.
pub struct DarkroomStage {
    pool: Arc<dyn Submit>,
    photo_id: i64,
    source: SourceToken,
    edit_json: String,
    generation: u64,
    frame: Option<StageFrame>,
    last_fast: Option<Instant>,
    fast_timer: Option<Task<()>>,
    settle_timer: Option<Task<()>>,
    done: UnboundedSender<FrameDone>,
    stats: FrameStats,
    /// Set by [`close`](Self::close): frames of earlier generations are stale.
    closed_at: Option<u64>,
    /// Requests not yet answered: generation and pool key.
    outstanding: Vec<(u64, JobKey)>,
    _drain: Task<()>,
}

impl DarkroomStage {
    /// A stage for `photo_id` rendering from `source` (the camera preview, or a resident
    /// RAW working image by token) with the record `edit_json`. Renders nothing until asked.
    pub fn new(
        pool: Arc<dyn Submit>,
        photo_id: i64,
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
            source,
            edit_json,
            generation: 0,
            frame: None,
            last_fast: None,
            fast_timer: None,
            settle_timer: None,
            done,
            stats: FrameStats::default(),
            closed_at: None,
            outstanding: Vec::new(),
            _drain,
        }
    }

    pub fn frame(&self) -> Option<&StageFrame> {
        self.frame.as_ref()
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
        let now = cx.background_executor().now();
        let wait = match self.last_fast {
            Some(at) => FAST_INTERVAL.saturating_sub(now.saturating_duration_since(at)),
            None => Duration::ZERO,
        };
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
            clip: false,
        });
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
        if matches!(&done.result, Err(e) if e == CANCELLED) {
            self.stats.stale_dropped += 1;
            return;
        }
        if self.closed_at.is_some_and(|g| done.generation < g) {
            self.stats.stale_dropped += 1;
            return;
        }
        let newer = match &self.frame {
            None => true,
            Some(shown) => (done.generation, done.tier) > (shown.generation, shown.tier),
        };
        if !newer {
            self.stats.stale_dropped += 1;
            return;
        }
        match done.result {
            Ok(loaded) => {
                self.stats.shown += 1;
                let old = self.frame.replace(StageFrame {
                    image: loaded.image,
                    generation: done.generation,
                    tier: done.tier,
                });
                if let Some(old) = old {
                    cx.defer(move |cx: &mut App| cx.drop_image(old.image, None));
                }
                cx.notify();
            }
            Err(e) => {
                self.stats.failed += 1;
                eprintln!("darkroom: photo {} frame {}: {e}", self.photo_id, done.generation);
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
    use crate::image_tests::{pixels, FakePool};
    use chairphoto_core::image_pool::JobKey;
    use gpui_kit::{AppContext as _, Entity, TestAppContext};
    use std::sync::Arc;
    use chairphoto_core::plugins::edit::SourceToken;
    use std::time::Duration;

    fn stage(cx: &mut TestAppContext) -> (Arc<FakePool>, Entity<DarkroomStage>) {
        let pool = Arc::new(FakePool::default());
        let stage = cx.update(|cx| {
            let pool: Arc<dyn Submit> = pool.clone();
            cx.new(|cx| DarkroomStage::new(pool, 42, SourceToken::Preview, "{}".into(), cx))
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
