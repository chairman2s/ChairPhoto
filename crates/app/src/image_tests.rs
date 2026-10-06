//! Headless tests of the image layer (#101). The decode pool is [`FakePool`]: it records every
//! batch and holds every responder until the test answers it, so each interleaving — a result
//! arriving after its request was superseded, a render running through an invalidate — is
//! forced, not hoped for.

use crate::image_store::{
    image_bytes, neighbour_window, neighbours, to_bgra, ImageKey, ImageLru, ImageState, ImageStore, Loaded, Submit,
};
use chairphoto_core::image_pool::{ImageKind, JobKey, Respond, CANCELLED};
use gpui_kit::{
    img, point, px, size, AppContext as _, Entity, IntoElement as _, Styled as _, TestAppContext,
};
use image::DynamicImage;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// A pool that renders nothing by itself. `running` stands in for a worker having popped a
/// job: such a job cannot be cancelled, like the real pool's.
#[derive(Default)]
pub struct FakePool {
    /// Every responder ever submitted, in submission order; `None` once answered.
    responders: Mutex<Vec<Option<(JobKey, Respond<Loaded>)>>>,
    /// Each batch's keys, in the order given.
    pub batches: Mutex<Vec<Vec<JobKey>>>,
    pub cancelled: Mutex<Vec<JobKey>>,
    running: Mutex<HashSet<JobKey>>,
}

impl Submit for FakePool {
    fn submit_batch(&self, batch: Vec<(JobKey, Respond<Loaded>)>) {
        self.batches.lock().unwrap().push(batch.iter().map(|(k, _)| k.clone()).collect());
        self.responders.lock().unwrap().extend(batch.into_iter().map(Some));
    }

    fn cancel(&self, key: &JobKey) -> bool {
        if self.running.lock().unwrap().contains(key) {
            return false;
        }
        let taken = self.take(|k| k == key);
        if taken.is_empty() {
            return false;
        }
        self.cancelled.lock().unwrap().push(key.clone());
        for respond in taken {
            respond(Err(CANCELLED.into()));
        }
        true
    }
}

impl FakePool {
    fn take(&self, mut matches: impl FnMut(&JobKey) -> bool) -> Vec<Respond<Loaded>> {
        let mut all = self.responders.lock().unwrap();
        let mut out = Vec::new();
        for slot in all.iter_mut() {
            if slot.as_ref().is_some_and(|(k, _)| matches(k)) {
                out.push(slot.take().unwrap().1);
            }
        }
        out
    }

    /// A worker has started `key`.
    pub fn start(&self, key: JobKey) {
        self.running.lock().unwrap().insert(key);
    }

    /// The render of `key` finished: answer every responder attached to it, oldest first.
    pub fn finish(&self, key: &JobKey, result: Result<Loaded, String>) {
        self.running.lock().unwrap().remove(key);
        for respond in self.take(|k| k == key) {
            respond(result.clone());
        }
    }

    /// Answer the `n`th responder ever submitted (submission order).
    #[cfg_attr(not(feature = "edit"), allow(dead_code))]
    pub fn finish_nth(&self, n: usize, result: Result<Loaded, String>) {
        let (_, respond) = self.responders.lock().unwrap()[n].take().expect("already answered");
        respond(result);
    }

    pub fn submitted(&self) -> usize {
        self.responders.lock().unwrap().len()
    }

    pub fn last_batch(&self) -> Vec<JobKey> {
        self.batches.lock().unwrap().last().cloned().unwrap_or_default()
    }
}

/// A `w`×`h` black image: `w*h*4` bytes.
pub(crate) fn pixels(w: u32, h: u32) -> Loaded {
    Loaded { image: to_bgra(DynamicImage::new_rgb8(w, h)), video_tile: false, cover: false, rendered_in: None }
}

/// A catalog identity for a test with no catalog open (a stage or job built by hand): `n`
/// through the identity's opaque serialized form, the only way to make one outside the core.
/// Distinct `n` are distinct identities. Only the `edit` feature's tests build jobs by hand.
#[cfg(feature = "edit")]
pub(crate) fn identity(n: u64) -> chairphoto_core::app::CatalogIdentity {
    serde_json::from_value(serde_json::Value::String(n.to_string())).expect("an identity")
}

fn thumb(id: i64) -> JobKey {
    JobKey::photo(id, ImageKind::Thumb)
}

fn preview(id: i64) -> JobKey {
    JobKey::photo(id, ImageKind::Preview)
}

fn key(photo: i64) -> ImageKey {
    ImageKey { photo, kind: ImageKind::Thumb, version: 0 }
}

fn store(cx: &mut TestAppContext, budget: usize) -> (Arc<FakePool>, Entity<ImageStore>) {
    let pool = Arc::new(FakePool::default());
    let images = cx.update(|cx| {
        let pool: Arc<dyn Submit> = pool.clone();
        cx.new(|cx| ImageStore::new(pool, budget, cx))
    });
    (pool, images)
}

fn ready(images: &Entity<ImageStore>, photo: i64, kind: ImageKind, cx: &mut TestAppContext) -> Option<Loaded> {
    images.update(cx, |s, _| match s.get(photo, kind) {
        ImageState::Ready(l) => Some(l),
        _ => None,
    })
}

// --- pure ------------------------------------------------------------------------------------

#[test]
fn bgra_conversion_swaps_red_and_blue_and_is_opaque() {
    let img = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(2, 1, image::Rgb([10, 20, 30])));
    let out = to_bgra(img);
    assert_eq!(out.as_bytes(0).unwrap(), &[30, 20, 10, 255, 30, 20, 10, 255]);
    let rgba = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 4])));
    assert_eq!(to_bgra(rgba).as_bytes(0).unwrap(), &[3, 2, 1, 4]);
}

#[test]
fn the_lru_evicts_least_recently_used_first_and_get_counts_as_use() {
    let one = image_bytes(&pixels(5, 5).image); // 100 bytes
    assert_eq!(one, 100);
    let mut lru = ImageLru::new(3 * one);
    for id in 1..=3 {
        assert!(lru.insert(key(id), pixels(5, 5)).is_empty());
    }
    assert!(lru.get(&key(1)).is_some()); // 2 is now the oldest
    let gone = lru.insert(key(4), pixels(5, 5));
    assert_eq!(gone.len(), 1);
    assert!(lru.peek(&key(2)).is_none(), "the least recently used went");
    assert!(lru.peek(&key(1)).is_some() && lru.peek(&key(3)).is_some() && lru.peek(&key(4)).is_some());
    assert_eq!(lru.bytes(), 3 * one);
}

#[test]
fn the_lru_keeps_one_image_bigger_than_its_budget_alone() {
    let mut lru = ImageLru::new(100);
    lru.insert(key(1), pixels(5, 5));
    let gone = lru.insert(key(2), pixels(20, 20)); // 1600 bytes
    assert_eq!(gone.len(), 1, "the small one made room");
    assert_eq!(lru.len(), 1);
    assert!(lru.peek(&key(2)).is_some());
    // Replacing a key hands back the old image.
    let old = lru.peek(&key(2)).unwrap().image.clone();
    let gone = lru.insert(key(2), pixels(5, 5));
    assert!(gone.len() == 1 && Arc::ptr_eq(&gone[0], &old));
    assert_eq!(lru.bytes(), 100);
}

#[test]
fn the_lru_stays_within_budget_under_churn() {
    let one = image_bytes(&pixels(16, 16).image);
    let mut lru = ImageLru::new(10 * one);
    let mut evicted = 0;
    for id in 0..5_000 {
        evicted += lru.insert(key(id), pixels(16, 16)).len();
        assert!(lru.bytes() <= lru.budget());
        if id % 7 == 0 {
            lru.get(&key(id - 3)); // touch something older now and then
        }
    }
    assert_eq!(lru.len(), 10);
    assert_eq!(evicted, 5_000 - 10);
}

#[test]
fn neighbours_are_current_then_next_then_previous() {
    assert_eq!(neighbours(5, 2), vec![2, 3, 1]);
    assert_eq!(neighbours(5, 0), vec![0, 1]);
    assert_eq!(neighbours(5, 4), vec![4, 3]);
    assert_eq!(neighbours(1, 0), vec![0]);
    assert!(neighbours(0, 0).is_empty());
    assert!(neighbours(3, 3).is_empty());
}

/// The loupe's and the cull session's wider preload: still current, N+1, N−1 first.
#[test]
fn a_neighbour_window_loads_current_next_previous_then_the_rest() {
    assert_eq!(neighbour_window(20, 10, 5, 2), vec![10, 11, 9, 12, 13, 14, 15, 8]);
    assert_eq!(neighbour_window(20, 10, 5, 1), vec![10, 11, 9, 12, 13, 14, 15]);
    assert_eq!(neighbour_window(12, 10, 5, 2), vec![10, 11, 9, 8], "clipped at the end");
    assert_eq!(neighbour_window(20, 0, 5, 2), vec![0, 1, 2, 3, 4, 5], "clipped at the start");
    assert_eq!(neighbour_window(20, 1, 1, 3), vec![1, 2, 0]);
    assert_eq!(neighbour_window(5, 2, 1, 1), neighbours(5, 2));
    assert!(neighbour_window(3, 3, 5, 2).is_empty());
}

// --- the store -------------------------------------------------------------------------------

#[gpui_kit::test]
fn a_result_lands_in_the_cache_and_a_second_request_is_free(cx: &mut TestAppContext) {
    let (pool, images) = store(cx, 1 << 20);
    images.update(cx, |s, _| s.request(1, ImageKind::Thumb));
    images.update(cx, |s, _| s.request(1, ImageKind::Thumb)); // pending: no new job
    assert_eq!(images.update(cx, |s, _| s.stats().submitted), 1);
    assert!(matches!(images.update(cx, |s, _| s.get(1, ImageKind::Thumb)), ImageState::Loading));

    pool.finish(&thumb(1), Ok(pixels(4, 4)));
    cx.run_until_parked();
    assert!(ready(&images, 1, ImageKind::Thumb, cx).is_some());
    let before = pool.submitted();
    images.update(cx, |s, _| s.request(1, ImageKind::Thumb)); // cached: nothing sent at all
    assert_eq!(pool.submitted(), before);

    // A failure is remembered, not retried in a loop.
    images.update(cx, |s, _| s.request(2, ImageKind::Thumb));
    pool.finish(&thumb(2), Err("no reachable copy".into()));
    cx.run_until_parked();
    let before = pool.submitted();
    images.update(cx, |s, _| s.request(2, ImageKind::Thumb));
    assert_eq!(pool.submitted(), before);
    assert!(matches!(images.update(cx, |s, _| s.get(2, ImageKind::Thumb)), ImageState::Failed(_)));
}

/// AGENTS.md § Performance: the requested photo first, then N+1, then N−1 — as one batch, so
/// the pool's LIFO stack holds them in that order. Moving on supersedes the old neighbours:
/// queued ones are cancelled, and a preload that became the current photo is promoted.
#[gpui_kit::test]
fn navigation_requests_current_then_next_then_previous(cx: &mut TestAppContext) {
    let (pool, images) = store(cx, 1 << 30);
    let photos = [10, 11, 12, 13, 14, 15];
    images.update(cx, |s, _| s.request(99, ImageKind::Thumb)); // a grid thumb, another tier
    images.update(cx, |s, _| s.navigate(&photos, 2, ImageKind::Preview));
    assert_eq!(pool.last_batch(), vec![preview(12), preview(13), preview(11)]);

    images.update(cx, |s, _| s.navigate(&photos, 3, ImageKind::Preview));
    assert_eq!(*pool.cancelled.lock().unwrap(), vec![preview(11)], "only N−1 of the old spot left");
    assert_eq!(
        pool.last_batch(),
        vec![preview(13), preview(14), preview(12)],
        "13 was a preload: re-sent first so the pool promotes it"
    );
    images.update(cx, |s, _| {
        assert!(s.is_pending(99, ImageKind::Thumb), "another tier's request is untouched");
        assert!(!s.is_pending(11, ImageKind::Preview));
        assert_eq!(s.stats().submitted, 5, "99, 12, 13, 11, 14 — the promotion is not a new job");
    });

    // The cancelled job's answer (CANCELLED) arrived and was dropped by generation.
    cx.run_until_parked();
    images.update(cx, |s, _| {
        assert_eq!(s.stats().stale_dropped, 1);
        assert!(matches!(s.get(11, ImageKind::Preview), ImageState::Absent));
    });
}

/// Review finding 3: a view asks for its images on every render. Asking again for keys that
/// are already pending must not reach the pool — each re-send added a no-op responder and an
/// O(stack) move. (Navigation still re-sends on purpose, to promote; see the test above.)
#[gpui_kit::test]
fn asking_again_for_pending_images_sends_nothing(cx: &mut TestAppContext) {
    let (pool, images) = store(cx, 1 << 20);
    let wanted: Vec<(i64, ImageKind)> = (1..=8).map(|id| (id, ImageKind::Thumb)).collect();
    for _ in 0..100 {
        images.update(cx, |s, _| s.request_batch(&wanted));
    }
    assert_eq!(pool.submitted(), 8, "one responder per job");
    assert_eq!(pool.batches.lock().unwrap().len(), 1, "one batch");
}

/// A preload that was already rendering when the user moved away cannot be cancelled; its
/// pixels arrive later and must be dropped, not cached or shown.
#[gpui_kit::test]
fn a_released_render_that_was_already_running_is_dropped(cx: &mut TestAppContext) {
    let (pool, images) = store(cx, 1 << 30);
    let photos = [10, 11, 12, 13, 14, 15];
    images.update(cx, |s, _| s.navigate(&photos, 2, ImageKind::Preview));
    pool.start(preview(11));
    images.update(cx, |s, _| s.navigate(&photos, 4, ImageKind::Preview));
    assert_eq!(*pool.cancelled.lock().unwrap(), vec![preview(12)], "11 is running; 13 is still wanted");

    pool.finish(&preview(11), Ok(pixels(8, 8)));
    pool.finish(&preview(14), Ok(pixels(8, 8)));
    cx.run_until_parked();
    images.update(cx, |s, _| {
        assert!(matches!(s.get(11, ImageKind::Preview), ImageState::Absent), "stale pixels were cached");
        assert!(matches!(s.get(14, ImageKind::Preview), ImageState::Ready(_)));
        assert_eq!(s.stats().stale_dropped, 2, "12's cancellation and 11's late render");
    });
}

/// The same key, released and requested again while the first render runs: the pool merges
/// both into the one render, whose answer reaches both generations — the old one is dropped,
/// the new one lands once.
#[gpui_kit::test]
fn a_superseded_generation_is_dropped_for_the_same_key(cx: &mut TestAppContext) {
    let (pool, images) = store(cx, 1 << 20);
    images.update(cx, |s, _| s.request(7, ImageKind::Thumb));
    pool.start(thumb(7));
    images.update(cx, |s, _| {
        s.release_pending(|_| false);
        s.request(7, ImageKind::Thumb);
    });
    pool.finish(&thumb(7), Ok(pixels(4, 4)));
    cx.run_until_parked();
    images.update(cx, |s, _| {
        assert!(matches!(s.get(7, ImageKind::Thumb), ImageState::Ready(_)));
        assert_eq!(s.stats().stale_dropped, 1);
        assert_eq!(s.lru().len(), 1);
    });
}

/// A rotation lands while the thumbnail renders: the running render has the old pixels and
/// the pool would merge a new request into it. The new request waits for it, then goes out.
#[gpui_kit::test]
fn invalidate_waits_for_a_running_render_before_requesting_again(cx: &mut TestAppContext) {
    let (pool, images) = store(cx, 1 << 20);
    images.update(cx, |s, _| s.request(5, ImageKind::Thumb));
    pool.start(thumb(5));
    images.update(cx, |s, cx| {
        s.invalidate(5, cx);
        s.request(5, ImageKind::Thumb);
    });
    assert_eq!(pool.submitted(), 1, "nothing may merge into the running old-version render");

    let old = pixels(4, 4);
    pool.finish(&thumb(5), Ok(old.clone()));
    cx.run_until_parked();
    assert_eq!(pool.submitted(), 2, "the deferred request went out once the old render was back");
    assert!(ready(&images, 5, ImageKind::Thumb, cx).is_none(), "the old pixels were dropped");

    let new = pixels(4, 4);
    pool.finish(&thumb(5), Ok(new.clone()));
    cx.run_until_parked();
    let shown = ready(&images, 5, ImageKind::Thumb, cx).expect("the new render landed");
    assert!(Arc::ptr_eq(&shown.image, &new.image));
    images.update(cx, |s, _| assert_eq!(s.key(5, ImageKind::Thumb).version, 1));
}

/// Review finding 1: a render released while running (navigation moved on) and then made
/// stale by an invalidate must still hold back the new version's request — otherwise the pool
/// merges it into the old render and the pre-change pixels are cached under the new version.
#[gpui_kit::test]
fn a_released_running_render_still_blocks_the_next_version(cx: &mut TestAppContext) {
    let (pool, images) = store(cx, 1 << 20);
    images.update(cx, |s, _| s.request(5, ImageKind::Thumb));
    pool.start(thumb(5));
    images.update(cx, |s, cx| {
        s.release_pending(|_| false); // running: cannot be cancelled
        s.invalidate(5, cx);
        s.request(5, ImageKind::Thumb);
    });
    assert_eq!(pool.submitted(), 1, "the v1 request must not merge into the running v0 render");

    let old = pixels(4, 4);
    pool.finish(&thumb(5), Ok(old.clone()));
    cx.run_until_parked();
    if let Some(shown) = ready(&images, 5, ImageKind::Thumb, cx) {
        assert!(!Arc::ptr_eq(&shown.image, &old.image), "v0 pixels cached under v1");
    }
    assert_eq!(pool.submitted(), 2, "the v1 request goes out once the v0 render is back");
    let new = pixels(4, 4);
    pool.finish(&thumb(5), Ok(new.clone()));
    cx.run_until_parked();
    let shown = ready(&images, 5, ImageKind::Thumb, cx).expect("v1 landed");
    assert!(Arc::ptr_eq(&shown.image, &new.image));
}

/// The same after a catalog switch: an id means another photo now, so a render started for the
/// old catalog must never answer the new catalog's request.
#[gpui_kit::test]
fn a_released_running_render_still_blocks_after_a_catalog_switch(cx: &mut TestAppContext) {
    let (pool, images) = store(cx, 1 << 20);
    images.update(cx, |s, _| s.request(5, ImageKind::Thumb));
    pool.start(thumb(5));
    images.update(cx, |s, cx| {
        s.release_pending(|_| false);
        s.clear(cx);
        s.request(5, ImageKind::Thumb);
    });
    assert_eq!(pool.submitted(), 1, "the new catalog's request must wait");
    let old = pixels(4, 4);
    pool.finish(&thumb(5), Ok(old.clone()));
    cx.run_until_parked();
    if let Some(shown) = ready(&images, 5, ImageKind::Thumb, cx) {
        assert!(!Arc::ptr_eq(&shown.image, &old.image), "the old catalog's photo was cached");
    }
    assert_eq!(pool.submitted(), 2);
}

/// Review finding 2: the wait for a running old-version render ends only when *that* render
/// answers — not on an earlier stale answer (a cancelled request's CANCELLED) still in the
/// channel.
#[gpui_kit::test]
fn only_the_old_renders_own_answer_ends_the_wait(cx: &mut TestAppContext) {
    let (pool, images) = store(cx, 1 << 20);
    images.update(cx, |s, cx| {
        s.request(5, ImageKind::Thumb); // g1
        s.release_pending(|_| false); // g1 queued: cancelled, its CANCELLED answer queued
        s.request(5, ImageKind::Thumb); // g2
        pool.start(thumb(5)); // a worker takes g2
        s.invalidate(5, cx); // g2 is running: v1 must wait for it
        s.request(5, ImageKind::Thumb); // v1: deferred
    });
    cx.run_until_parked(); // delivers g1's CANCELLED
    assert_eq!(pool.submitted(), 2, "g1's CANCELLED must not release the v1 request");

    let old = pixels(4, 4);
    pool.finish(&thumb(5), Ok(old.clone())); // g2, v0 pixels
    cx.run_until_parked();
    if let Some(shown) = ready(&images, 5, ImageKind::Thumb, cx) {
        assert!(!Arc::ptr_eq(&shown.image, &old.image), "v0 pixels cached under v1");
    }
    assert_eq!(pool.submitted(), 3, "now the v1 request goes out");
    let new = pixels(4, 4);
    pool.finish(&thumb(5), Ok(new.clone()));
    cx.run_until_parked();
    let shown = ready(&images, 5, ImageKind::Thumb, cx).expect("v1 landed");
    assert!(Arc::ptr_eq(&shown.image, &new.image));
}

/// Codex gate finding 1: navigation promotes a pending preload by re-sending it. If the pool
/// already finished that job (its answer is still in the channel, not drained), the re-send
/// starts a fresh render. That render must be tracked like any submission: after the drain an
/// invalidate's request for the new version must wait for it, not merge into it and cache the
/// old pixels under the new version.
#[gpui_kit::test]
fn a_promotion_that_restarts_a_finished_job_still_blocks_the_next_version(cx: &mut TestAppContext) {
    let (pool, images) = store(cx, 1 << 30);
    let photos = [10, 11, 12];
    images.update(cx, |s, _| s.navigate(&photos, 0, ImageKind::Preview)); // 10, then 11
    // The preload of 11 finishes; its answer is sent but not drained (nothing has run yet).
    pool.finish(&preview(11), Ok(pixels(4, 4)));
    // 11 is still pending, so moving to it promotes it: the real pool no longer holds the
    // job and starts a fresh render of the v0 photo.
    images.update(cx, |s, _| s.navigate(&photos, 1, ImageKind::Preview));
    assert_eq!(pool.last_batch()[0], preview(11), "11 is re-sent first");
    pool.start(preview(11));
    cx.run_until_parked(); // the first answer lands: 11 v0 is cached
    assert!(ready(&images, 11, ImageKind::Preview, cx).is_some());

    let sent = pool.submitted();
    images.update(cx, |s, cx| {
        s.invalidate(11, cx);
        s.request(11, ImageKind::Preview);
    });
    assert_eq!(pool.submitted(), sent, "the v1 request must not merge into the running v0 render");

    let old = pixels(4, 4);
    pool.finish(&preview(11), Ok(old.clone())); // the promotion's render: v0 pixels
    cx.run_until_parked();
    if let Some(shown) = ready(&images, 11, ImageKind::Preview, cx) {
        assert!(!Arc::ptr_eq(&shown.image, &old.image), "v0 pixels cached under v1");
    }
    assert_eq!(pool.submitted(), sent + 1, "the v1 request goes out once the v0 render is back");
    let new = pixels(4, 4);
    pool.finish(&preview(11), Ok(new.clone()));
    cx.run_until_parked();
    let shown = ready(&images, 11, ImageKind::Preview, cx).expect("v1 landed");
    assert!(Arc::ptr_eq(&shown.image, &new.image));
}

/// Codex gate finding 4: a request held back behind an outdated render is a request too.
/// Navigating away must forget it like a pending one — not send it when that render answers.
#[gpui_kit::test]
fn navigating_away_forgets_a_held_back_request(cx: &mut TestAppContext) {
    let (pool, images) = store(cx, 1 << 30);
    let photos = [10, 11, 12, 13, 14, 15];
    images.update(cx, |s, _| s.navigate(&photos, 2, ImageKind::Preview)); // 12, 13, 11
    pool.start(preview(13));
    images.update(cx, |s, cx| {
        s.invalidate(13, cx); // 13 v0 is running: a v1 request must wait for it
        s.navigate(&photos, 2, ImageKind::Preview); // 13 v1: held back
    });
    let sent = pool.submitted();
    images.update(cx, |s, _| s.navigate(&photos, 5, ImageKind::Preview)); // 15, 14
    let after_move = pool.submitted();
    assert_eq!(pool.last_batch(), vec![preview(15), preview(14)]);

    pool.finish(&preview(13), Ok(pixels(4, 4))); // the v0 render answers
    cx.run_until_parked();
    assert!(after_move > sent);
    assert_eq!(pool.submitted(), after_move, "13 is no longer wanted: nothing more is sent");
    assert!(pool.last_batch() != vec![preview(13)]);
    images.update(cx, |s, _| {
        assert!(!s.is_pending(13, ImageKind::Preview));
        assert!(matches!(s.get(13, ImageKind::Preview), ImageState::Absent));
    });
}

#[gpui_kit::test]
fn clear_forgets_cache_pending_and_failures(cx: &mut TestAppContext) {
    let (pool, images) = store(cx, 1 << 20);
    images.update(cx, |s, _| s.request_batch(&[(1, ImageKind::Thumb), (2, ImageKind::Thumb)]));
    pool.finish(&thumb(1), Ok(pixels(4, 4)));
    cx.run_until_parked();
    images.update(cx, |s, cx| s.clear(cx));
    cx.run_until_parked();
    images.update(cx, |s, _| {
        assert!(s.lru().is_empty());
        assert!(!s.is_pending(2, ImageKind::Thumb));
        assert_eq!(s.stats().released, 1);
    });
    assert_eq!(*pool.cancelled.lock().unwrap(), vec![thumb(2)]);
}

/// Eviction removes the texture from the window's sprite atlas — including when the eviction
/// happens inside that window's own update (a render or listener requesting images), which
/// `App::drop_image` alone cannot reach; the store defers the drop past it.
#[gpui_kit::test]
fn eviction_removes_the_texture_from_the_atlas(cx: &mut TestAppContext) {
    let one = image_bytes(&pixels(10, 10).image);
    let (pool, images) = store(cx, one);
    images.update(cx, |s, _| s.request(1, ImageKind::Thumb));
    pool.finish(&thumb(1), Ok(pixels(10, 10)));
    cx.run_until_parked();
    let first = ready(&images, 1, ImageKind::Thumb, cx).unwrap().image;

    let window = cx.add_empty_window();
    let painted = first.clone();
    window.draw(point(px(0.), px(0.)), size(px(50.), px(50.)), move |_, _| {
        img(painted).size_full().into_any_element()
    });
    assert!(window.update(|w, _| w.has_image_atlas_entry(&first)), "painting uploads it");

    // Insert a second image from inside the window's update: the first must go.
    let second = pixels(10, 10);
    let images2 = images.clone();
    let pool2 = pool.clone();
    window.update(|_, cx| {
        images2.update(cx, |s, _| s.request(2, ImageKind::Thumb));
    });
    pool2.finish(&thumb(2), Ok(second));
    window.run_until_parked();
    assert!(!window.update(|w, _| w.has_image_atlas_entry(&first)), "the evicted texture stayed");

    // And an invalidate issued inside the window's update releases too.
    let second = window.update(|_, cx| match images.update(cx, |s, _| s.get(2, ImageKind::Thumb)) {
        ImageState::Ready(l) => l.image,
        _ => panic!("2 is cached"),
    });
    let painted = second.clone();
    window.draw(point(px(0.), px(0.)), size(px(50.), px(50.)), move |_, _| {
        img(painted).size_full().into_any_element()
    });
    assert!(window.update(|w, _| w.has_image_atlas_entry(&second)));
    let images3 = images.clone();
    window.update(|_, cx| images3.update(cx, |s, cx| s.invalidate(2, cx)));
    window.run_until_parked();
    assert!(!window.update(|w, _| w.has_image_atlas_entry(&second)));
}

/// A catalog switch clears the store (photo ids now mean other photos).
#[gpui_kit::test]
fn a_catalog_switch_clears_the_image_store(cx: &mut TestAppContext) {
    use chairphoto_core::app::{AppState, CoreEvent};
    let (pool, images) = store(cx, 1 << 20);
    let model = cx.update(|cx| {
        gpui_kit::init(cx);
        cx.new(|_| crate::model::AppModel::new(AppState::default(), None))
    });
    cx.update(|cx| crate::clear_images_on_catalog_switch(&model, &images, cx).detach());
    images.update(cx, |s, _| s.request(1, ImageKind::Thumb));
    pool.finish(&thumb(1), Ok(pixels(4, 4)));
    cx.run_until_parked();
    assert!(ready(&images, 1, ImageKind::Thumb, cx).is_some());

    model.update(cx, |m, cx| m.on_core_event(
            &CoreEvent::ThemeChanged(chairphoto_core::appearance::SystemThemeResult {
                available: false,
                theme_name: None,
                palette: None,
            }),
            cx,
        ));
    cx.run_until_parked();
    assert!(ready(&images, 1, ImageKind::Thumb, cx).is_some(), "other events leave it");

    model.update(cx, |m, cx| m.on_core_event(&CoreEvent::CatalogSwitched("/other".into()), cx));
    cx.run_until_parked();
    assert!(ready(&images, 1, ImageKind::Thumb, cx).is_none());
}

// --- claims (#110) ---------------------------------------------------------------------------

fn zoom(id: i64) -> JobKey {
    JobKey::photo(id, ImageKind::Zoom)
}

fn cancelled(pool: &FakePool) -> Vec<JobKey> {
    pool.cancelled.lock().unwrap().clone()
}

/// Two views navigate over one store: one stepping away releases only what the other does
/// not hold too, and what one holds survives everybody else's releases.
#[gpui_kit::test]
fn a_claim_releases_only_what_no_other_claim_holds(cx: &mut TestAppContext) {
    let (pool, images) = store(cx, 1 << 30);
    let photos = [10, 11, 12, 13, 14, 15, 16];
    let (a, b) = images.update(cx, |s, _| (s.new_claim(), s.new_claim()));
    images.update(cx, |s, _| {
        s.navigate_window_as(a, &photos, 1, ImageKind::Preview, 1, 1, &[(11, ImageKind::Zoom)]);
        s.request(11, ImageKind::Zoom);
        s.navigate_window_as(b, &photos, 2, ImageKind::Preview, 1, 1, &[]);
    });
    assert_eq!(images.read_with(cx, |s, _| s.claim(a).len()), 4, "10, 11, 12 and 11's zoom tier");

    // Another view's blanket release, and an unclaimed navigation, leave both claims alone.
    images.update(cx, |s, _| {
        s.release_pending(|_| false);
        s.navigate_window(&photos, 6, ImageKind::Preview, 0, 0);
    });
    assert!(cancelled(&pool).is_empty(), "{:?}", cancelled(&pool));

    // A steps to 15: 10 and 11's zoom tier were only its own; 11 and 12 are B's too.
    images.update(cx, |s, _| s.navigate_window_as(a, &photos, 5, ImageKind::Preview, 1, 1, &[]));
    let gone: HashSet<JobKey> = cancelled(&pool).into_iter().collect();
    assert_eq!(gone, HashSet::from([preview(10), zoom(11)]));
    assert!(images.read_with(cx, |s, _| s.is_pending(11, ImageKind::Preview) && s.is_pending(12, ImageKind::Preview)));

    // B lets go of 11, 12 and 13, which nobody else holds now.
    images.update(cx, |s, _| s.drop_claim(b));
    let gone = cancelled(&pool);
    for id in [11, 12, 13] {
        assert!(gone.contains(&preview(id)), "{id} released with B's claim: {gone:?}");
    }
    assert!(!gone.contains(&preview(14)), "14 is A's");
}

/// Eviction skips a tier a claim holds; a catalog switch empties every claim, and a changed
/// photo (an invalidate) drops its claimed requests — the pixels they would bring are stale.
#[gpui_kit::test]
fn claims_guard_eviction_but_not_a_change_of_pixels(cx: &mut TestAppContext) {
    let (pool, images) = store(cx, 1 << 30);
    let a = images.update(cx, |s, _| s.new_claim());
    images.update(cx, |s, _| {
        s.request(1, ImageKind::Zoom);
        s.set_claim(a, [(1, ImageKind::Zoom), (2, ImageKind::Preview)]);
        s.request(2, ImageKind::Preview);
    });
    pool.finish(&zoom(1), Ok(pixels(8, 8)));
    cx.run_until_parked();
    images.update(cx, |s, cx| s.evict(|k| k.kind == ImageKind::Zoom, cx));
    assert!(ready(&images, 1, ImageKind::Zoom, cx).is_some(), "claimed: kept");

    images.update(cx, |s, cx| s.invalidate(2, cx));
    assert_eq!(cancelled(&pool), vec![preview(2)], "an invalidate drops a claimed request");

    images.update(cx, |s, cx| s.clear(cx));
    assert!(images.read_with(cx, |s, _| s.claim(a).is_empty()), "a switch empties the claims");
    assert!(!images.read_with(cx, |s, _| s.is_claimed(1, ImageKind::Zoom)));
}
