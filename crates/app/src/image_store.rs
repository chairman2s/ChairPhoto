//! The image layer (#101): photo tiers as GPU-ready textures.
//!
//! ```text
//! view ──request──▶ ImageStore ──submit_batch──▶ core ImagePool<Loaded> (LIFO, merged keys)
//!                      ▲                              │ worker: media::render_image
//!                      │                              │         → BGRA RenderImage (to_bgra)
//!                      └──── Done {key, generation} ◀─┘ unbounded channel → main thread
//! ```
//!
//! - **Decoding never happens on the UI thread.** The pool's worker runs
//!   `media::render_image` (the on-disk thumb/preview/zoom JPEG caches, decoded once, no
//!   re-encode) and converts the pixels to the BGRA `RenderImage` GPUI uploads, then sends
//!   the `Arc` home. Nothing crosses as encoded bytes.
//! - **A byte-budgeted LRU** ([`ImageLru`]) keyed by [`ImageKey`] — photo, tier and a
//!   per-photo *version* that [`ImageStore::invalidate`] bumps (a rotation, a new cover) —
//!   holds the decoded images. An evicted image's texture is removed from every window's
//!   sprite atlas (`App::drop_image`, deferred to outside any window update), so neither
//!   RAM nor GPU memory grows past the budget with churn.
//! - **Stale results are dropped by generation.** Every submission records a generation in
//!   `pending`; a result lands only if its key is still pending with that generation. A key
//!   released by navigation ([`ImageStore::navigate`], [`ImageStore::release_pending`]),
//!   invalidated, or cleared by a catalog switch no longer matches, so its late pixels are
//!   dropped — never shown, never cached. Released keys still queued in the pool are
//!   cancelled there; one already rendering cannot be interrupted and is dropped on arrival.
//! - **A render already running when its photo changes** (an invalidate, a catalog switch)
//!   cannot be cancelled, and the pool would merge a new request for the same tier into it —
//!   handing the old pixels to the new version. So the store remembers every submission it
//!   has not heard back from (`unanswered`: generation, version, catalog epoch), whether or
//!   not anyone still wants it. A request for a tier with an unanswered submission from an
//!   older version or epoch waits (`deferred`), and goes out when the last such submission's
//!   own answer arrives — not on any other answer for the tier.
//! - **Navigation** asks for the current photo first, then N+1, then N−1 as one pool batch,
//!   so the current photo is on top of the LIFO stack before any worker can pop
//!   (AGENTS.md § Performance). A preload that became the current photo is re-sent to move
//!   it up; the re-send is an `unanswered` submission like any other, because the pool may
//!   already have finished that job and start a fresh render for it.
//!
//! The pool is behind [`Submit`] so tests can hold responders and deliver them in any order.

use chairphoto_core::image_pool::{ImageKind, ImagePool, JobKey, Respond};
use chairphoto_core::media::DecodedImage;
use futures::channel::mpsc::{unbounded, UnboundedSender};
use futures::StreamExt as _;
use gpui_kit::{App, Context, RenderImage, SharedString, Task};
use image::{DynamicImage, RgbaImage};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

/// Default decoded-image budget: ~1000 grid thumbnails, or ~70 loupe previews, or two
/// full-resolution zoom tiers. Textures in the atlas are bounded by the same number.
pub const DEFAULT_BUDGET_BYTES: usize = 768 * 1024 * 1024;

/// A decoded image ready for GPUI: BGRA pixels in a `RenderImage`.
#[derive(Clone)]
pub struct Loaded {
    pub image: Arc<RenderImage>,
    /// A video with no poster frame, shown as the core's generic video tile.
    pub video_tile: bool,
}

impl Loaded {
    /// Convert on the worker thread that decoded it.
    pub fn from_decoded(decoded: DecodedImage) -> Self {
        Self { image: to_bgra(decoded.image), video_tile: decoded.video_tile }
    }

    /// Decoded size in bytes (what the LRU budgets).
    pub fn bytes(&self) -> usize {
        image_bytes(&self.image)
    }
}

/// Bytes of every frame of `image`.
pub fn image_bytes(image: &RenderImage) -> usize {
    (0..image.frame_count()).filter_map(|i| image.as_bytes(i)).map(<[u8]>::len).sum()
}

/// Convert a decoded image to the BGRA frame `RenderImage` holds, in one pass for the
/// common `Rgb8` JPEG decode. Blocking and O(pixels): call it off the UI thread.
pub fn to_bgra(image: DynamicImage) -> Arc<RenderImage> {
    let bgra: RgbaImage = match image {
        DynamicImage::ImageRgb8(rgb) => {
            let (w, h) = rgb.dimensions();
            let mut out = Vec::with_capacity(w as usize * h as usize * 4);
            for px in rgb.as_raw().chunks_exact(3) {
                out.extend_from_slice(&[px[2], px[1], px[0], 0xff]);
            }
            RgbaImage::from_raw(w, h, out).expect("w*h*4 bytes")
        }
        other => {
            let mut rgba = other.into_rgba8();
            for px in rgba.pixels_mut() {
                px.0.swap(0, 2);
            }
            rgba
        }
    };
    Arc::new(RenderImage::new(vec![image::Frame::new(bgra)]))
}

/// What the store caches an image under.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ImageKey {
    pub photo: i64,
    pub kind: ImageKind,
    /// The photo's version when requested ([`ImageStore::invalidate`] bumps it).
    pub version: u64,
}

// --- LRU ---------------------------------------------------------------------------------

/// A byte-budgeted least-recently-used map of decoded images. Pure data: it only says which
/// images left, and the caller releases their textures.
pub struct ImageLru {
    budget: usize,
    bytes: usize,
    tick: u64,
    entries: HashMap<ImageKey, (Loaded, u64)>,
    /// Last-use tick → key, oldest first.
    order: BTreeMap<u64, ImageKey>,
}

impl ImageLru {
    pub fn new(budget: usize) -> Self {
        Self { budget, bytes: 0, tick: 0, entries: HashMap::new(), order: BTreeMap::new() }
    }

    pub fn budget(&self) -> usize {
        self.budget
    }

    /// Decoded bytes held.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The image under `key`, marked as just used.
    pub fn get(&mut self, key: &ImageKey) -> Option<&Loaded> {
        self.tick += 1;
        let tick = self.tick;
        let (loaded, used) = self.entries.get_mut(key)?;
        self.order.remove(used);
        *used = tick;
        self.order.insert(tick, *key);
        Some(loaded)
    }

    /// The image under `key`, without marking it used.
    pub fn peek(&self, key: &ImageKey) -> Option<&Loaded> {
        self.entries.get(key).map(|(l, _)| l)
    }

    /// Insert as the most recently used, then evict least-recently-used images until the
    /// total fits the budget. The image just inserted is never evicted, so one image larger
    /// than the whole budget is still held (alone). Returns every image that left, including
    /// one `key` replaced.
    pub fn insert(&mut self, key: ImageKey, loaded: Loaded) -> Vec<Arc<RenderImage>> {
        let mut gone = self.remove(&key).into_iter().collect::<Vec<_>>();
        self.tick += 1;
        self.bytes += loaded.bytes();
        self.entries.insert(key, (loaded, self.tick));
        self.order.insert(self.tick, key);
        while self.bytes > self.budget && self.entries.len() > 1 {
            let Some((_, oldest)) = self.order.pop_first() else { break };
            if let Some((l, _)) = self.entries.remove(&oldest) {
                self.bytes -= l.bytes();
                gone.push(l.image);
            }
        }
        gone
    }

    pub fn remove(&mut self, key: &ImageKey) -> Option<Arc<RenderImage>> {
        let (loaded, used) = self.entries.remove(key)?;
        self.order.remove(&used);
        self.bytes -= loaded.bytes();
        Some(loaded.image)
    }

    /// Remove every image whose key matches.
    pub fn remove_where(&mut self, mut matches: impl FnMut(&ImageKey) -> bool) -> Vec<Arc<RenderImage>> {
        let keys: Vec<ImageKey> = self.entries.keys().filter(|k| matches(k)).copied().collect();
        keys.iter().filter_map(|k| self.remove(k)).collect()
    }
}

// --- the pool seam -----------------------------------------------------------------------

/// What the store needs from the decode pool. [`ImagePool<Loaded>`] in the app; a
/// hand-driven fake in tests.
pub trait Submit: Send + Sync {
    /// Submit in priority order, most urgent first (`ImagePool::submit_batch`).
    fn submit_batch(&self, batch: Vec<(JobKey, Respond<Loaded>)>);
    /// Take a queued job off the pool (`ImagePool::cancel`). A rendering one stays.
    fn cancel(&self, key: &JobKey) -> bool;
}

impl Submit for ImagePool<Loaded> {
    fn submit_batch(&self, batch: Vec<(JobKey, Respond<Loaded>)>) {
        ImagePool::submit_batch(self, batch)
    }

    fn cancel(&self, key: &JobKey) -> bool {
        ImagePool::cancel(self, key)
    }
}

/// The pool's runner for the app: `media::render_image`, converted to BGRA on the worker.
pub fn runner(state: chairphoto_core::app::AppState) -> chairphoto_core::image_pool::Runner<Loaded> {
    Arc::new(move |key| chairphoto_core::media::render_image(&state, key).map(Loaded::from_decoded))
}

// --- navigation --------------------------------------------------------------------------

/// The indices navigation loads, in load order: `index`, then `index + 1`, then `index − 1`
/// (the ones that exist). AGENTS.md: the requested photo first, then its neighbours.
pub fn neighbours(len: usize, index: usize) -> Vec<usize> {
    neighbour_window(len, index, 1, 1)
}

/// [`neighbours`] with a wider preload: `index`, then N+1 and N−1 (the AGENTS.md order),
/// then the rest of the window ahead (N+2 … N+`ahead`), then the rest behind
/// (N−2 … N−`behind`). Culling moves forward, so the loupe and the cull session preload
/// further ahead than behind (React prefetched +1..+5, −1, −2 and +1..+5, −1).
pub fn neighbour_window(len: usize, index: usize, ahead: usize, behind: usize) -> Vec<usize> {
    if index >= len {
        return Vec::new();
    }
    let mut out = vec![index];
    if ahead >= 1 && index + 1 < len {
        out.push(index + 1);
    }
    if behind >= 1 && index >= 1 {
        out.push(index - 1);
    }
    out.extend((2..=ahead).map(|d| index + d).take_while(|&i| i < len));
    out.extend((2..=behind).map_while(|d| index.checked_sub(d)));
    out
}

// --- the store ---------------------------------------------------------------------------

/// What a view gets for one image.
#[derive(Clone)]
pub enum ImageState {
    Ready(Loaded),
    Loading,
    Failed(SharedString),
    /// Not requested (or released).
    Absent,
}

/// Counters for tests and the bench.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoreStats {
    /// Jobs handed to the pool (a promotion of a pending key is not counted).
    pub submitted: u64,
    /// Results that arrived for a key no longer pending under their generation.
    pub stale_dropped: u64,
    /// Textures handed to `drop_image` (evicted, replaced, invalidated, cleared).
    pub released: u64,
}

/// One submission to the pool, remembered until it answers.
#[derive(Clone, Copy, Debug)]
struct Submission {
    generation: u64,
    version: u64,
    epoch: u64,
}

struct Done {
    key: ImageKey,
    generation: u64,
    /// The answer to a navigation promotion: never the one a pending key waits for, so
    /// dropping it is not counted as stale.
    promotion: bool,
    result: Result<Loaded, String>,
}

/// The app's image cache and request broker. One per app; views read it and request
/// through it.
pub struct ImageStore {
    pool: Arc<dyn Submit>,
    lru: ImageLru,
    /// Requested and not yet answered: key → the generation of its newest submission.
    pending: HashMap<ImageKey, u64>,
    failed: HashMap<ImageKey, SharedString>,
    versions: HashMap<i64, u64>,
    /// Every submission not yet answered, per tier: (generation, version, epoch). Released
    /// or abandoned ones stay until their answer arrives; see the module docs.
    unanswered: HashMap<(i64, ImageKind), Vec<Submission>>,
    /// Requests held back by an outdated unanswered submission, sent once none is left.
    deferred: HashSet<(i64, ImageKind)>,
    /// Bumped by [`clear`](Self::clear): photo ids from an older epoch are other photos.
    epoch: u64,
    generation: u64,
    done: UnboundedSender<Done>,
    stats: StoreStats,
    _drain: Task<()>,
}

impl ImageStore {
    pub fn new(pool: Arc<dyn Submit>, budget: usize, cx: &mut Context<Self>) -> Self {
        let (done, mut rx) = unbounded::<Done>();
        // Results come from pool workers; this task lands them on the main thread. It ends
        // with the store (the update fails) or when every sender is gone.
        let _drain = cx.spawn(async move |this, cx| {
            while let Some(d) = rx.next().await {
                if this.update(cx, |store, cx| store.complete(d, cx)).is_err() {
                    break;
                }
            }
        });
        Self {
            pool,
            lru: ImageLru::new(budget),
            pending: HashMap::new(),
            failed: HashMap::new(),
            versions: HashMap::new(),
            unanswered: HashMap::new(),
            deferred: HashSet::new(),
            epoch: 0,
            generation: 0,
            done,
            stats: StoreStats::default(),
            _drain,
        }
    }

    pub fn stats(&self) -> StoreStats {
        self.stats
    }

    /// The decode pool this store submits to: edit renders (the loupe's version render, the
    /// Duel's and the Proof sheet's variants) go to the same workers.
    pub fn pool(&self) -> Arc<dyn Submit> {
        self.pool.clone()
    }

    pub fn lru(&self) -> &ImageLru {
        &self.lru
    }

    /// The key a request for `photo`'s `kind` uses now.
    pub fn key(&self, photo: i64, kind: ImageKind) -> ImageKey {
        ImageKey { photo, kind, version: self.versions.get(&photo).copied().unwrap_or(0) }
    }

    pub fn is_pending(&self, photo: i64, kind: ImageKind) -> bool {
        self.pending.contains_key(&self.key(photo, kind))
    }

    /// What there is for `photo`'s `kind`, marking a cached image as used.
    pub fn get(&mut self, photo: i64, kind: ImageKind) -> ImageState {
        let key = self.key(photo, kind);
        if let Some(loaded) = self.lru.get(&key) {
            return ImageState::Ready(loaded.clone());
        }
        self.state_uncached(&key)
    }

    /// [`get`](Self::get) without marking it used.
    pub fn peek(&self, photo: i64, kind: ImageKind) -> ImageState {
        let key = self.key(photo, kind);
        match self.lru.peek(&key) {
            Some(loaded) => ImageState::Ready(loaded.clone()),
            None => self.state_uncached(&key),
        }
    }

    fn state_uncached(&self, key: &ImageKey) -> ImageState {
        if self.pending.contains_key(key) {
            ImageState::Loading
        } else if let Some(e) = self.failed.get(key) {
            ImageState::Failed(e.clone())
        } else {
            ImageState::Absent
        }
    }

    /// Request one image (a no-op when cached, pending or failed).
    pub fn request(&mut self, photo: i64, kind: ImageKind) {
        self.request_batch(&[(photo, kind)]);
    }

    /// Request several images, most urgent first, as one pool batch: the first ends on top of
    /// the pool's LIFO stack. Cached, failed and already pending keys are skipped, so a view
    /// may call this on every render: once everything it wants is pending, nothing is sent.
    pub fn request_batch(&mut self, wanted: &[(i64, ImageKind)]) {
        self.submit(wanted, false);
    }

    /// [`request_batch`](Self::request_batch), and with `promote` a pending key is re-sent so
    /// the pool moves it up to its place in this batch — what navigation needs when a preload
    /// becomes the current photo. The re-send is tracked in `unanswered` under its own
    /// generation; its answer only ends that record (the pending key's own answer lands).
    fn submit(&mut self, wanted: &[(i64, ImageKind)], promote: bool) {
        let mut batch: Vec<(JobKey, Respond<Loaded>)> = Vec::with_capacity(wanted.len());
        let mut seen = HashSet::new();
        for &(photo, kind) in wanted {
            let key = self.key(photo, kind);
            if !seen.insert(key) || self.lru.peek(&key).is_some() || self.failed.contains_key(&key) {
                continue;
            }
            if self.outdated_in_flight((photo, kind), key.version) {
                self.deferred.insert((photo, kind));
                continue;
            }
            let job = JobKey::photo(photo, kind);
            let promotion = self.pending.contains_key(&key);
            if promotion && !promote {
                continue;
            }
            // A promotion is a submission like any other. The pool merges it into the
            // pending job if it still holds that job; if the job already finished (its answer
            // not yet drained), the pool starts a fresh render, which `unanswered` must know
            // about so that a later version or catalog does not merge into it.
            self.generation += 1;
            let generation = self.generation;
            if !promotion {
                self.pending.insert(key, generation);
                self.stats.submitted += 1;
            }
            self.unanswered.entry((photo, kind)).or_default().push(Submission {
                generation,
                version: key.version,
                epoch: self.epoch,
            });
            let done = self.done.clone();
            batch.push((
                job,
                Box::new(move |result| {
                    // Fails only when the store is gone; the result is then unwanted.
                    let _ = done.unbounded_send(Done { key, generation, promotion, result });
                }),
            ));
        }
        if !batch.is_empty() {
            self.pool.submit_batch(batch);
        }
    }

    /// Stop wanting every pending request `keep` rejects: each is cancelled in the pool if it
    /// is still queued, and whatever it produces later is dropped by generation. A request
    /// held back in `deferred` that `keep` rejects (judged under the photo's current version,
    /// which is what it would be sent as) is forgotten, so it is not sent when the render it
    /// waits for answers.
    pub fn release_pending(&mut self, mut keep: impl FnMut(&ImageKey) -> bool) {
        let released: Vec<ImageKey> = self.pending.keys().filter(|k| !keep(k)).copied().collect();
        for key in released {
            self.pending.remove(&key);
            self.pool.cancel(&JobKey::photo(key.photo, key.kind));
        }
        let versions = &self.versions;
        self.deferred.retain(|&(photo, kind)| {
            keep(&ImageKey { photo, kind, version: versions.get(&photo).copied().unwrap_or(0) })
        });
    }

    /// The loupe moved to `photos[index]`: supersede this tier's other pending requests, then
    /// request the current photo, N+1 and N−1, in that order, as one batch.
    pub fn navigate(&mut self, photos: &[i64], index: usize, kind: ImageKind) {
        self.navigate_window(photos, index, kind, 1, 1);
    }

    /// [`navigate`](Self::navigate) over a wider window ([`neighbour_window`]): the current
    /// photo first, then N+1, N−1, then the rest ahead and behind — one batch, so the current
    /// photo is on top of the pool's stack. This tier's pending requests outside the window
    /// are superseded.
    pub fn navigate_window(&mut self, photos: &[i64], index: usize, kind: ImageKind, ahead: usize, behind: usize) {
        let wanted: Vec<(i64, ImageKind)> = neighbour_window(photos.len(), index, ahead, behind)
            .into_iter()
            .map(|i| (photos[i], kind))
            .collect();
        let keep: HashSet<i64> = wanted.iter().map(|&(p, _)| p).collect();
        self.release_pending(|k| k.kind != kind || keep.contains(&k.photo));
        self.submit(&wanted, true);
    }

    /// Drop the cached images `matches` selects and release their textures. For what a view
    /// is done with and the LRU would otherwise keep: a full-resolution tier (180–245 MB for a
    /// typical RAW) costs hundreds of thumbnails while it waits for eviction.
    pub fn evict(&mut self, matches: impl FnMut(&ImageKey) -> bool, cx: &mut Context<Self>) {
        let gone = self.lru.remove_where(matches);
        if !gone.is_empty() {
            self.release(gone, cx);
            cx.notify();
        }
    }

    /// A photo's pixels changed (rotation, cover version): drop what is cached for it, and
    /// make its pending requests stale. The next request renders it again.
    pub fn invalidate(&mut self, photo: i64, cx: &mut Context<Self>) {
        *self.versions.entry(photo).or_insert(0) += 1;
        let gone = self.lru.remove_where(|k| k.photo == photo);
        self.release(gone, cx);
        self.abandon(|k| k.photo == photo);
        self.failed.retain(|k, _| k.photo != photo);
        cx.notify();
    }

    /// Forget everything — the catalog changed, so every photo id means something else.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.epoch += 1;
        self.deferred.clear();
        let gone = self.lru.remove_where(|_| true);
        self.release(gone, cx);
        self.abandon(|_| true);
        self.failed.clear();
        self.versions.clear();
        cx.notify();
    }

    /// Drop the pending requests `matches` selects because what they would render changed,
    /// cancelling the queued ones. A running one stays in `unanswered` under its old version
    /// or epoch, which holds back the next request for its tier.
    fn abandon(&mut self, mut matches: impl FnMut(&ImageKey) -> bool) {
        self.release_pending(|k| !matches(k));
    }

    /// Whether `tier` has a submission not yet answered that rendered for another version of
    /// the photo or another catalog — one a new request must not merge into.
    fn outdated_in_flight(&self, tier: (i64, ImageKind), version: u64) -> bool {
        self.unanswered
            .get(&tier)
            .is_some_and(|subs| subs.iter().any(|s| s.version != version || s.epoch != self.epoch))
    }

    fn complete(&mut self, done: Done, cx: &mut Context<Self>) {
        let tier = (done.key.photo, done.key.kind);
        // This submission is answered, whatever becomes of its result.
        if let Some(subs) = self.unanswered.get_mut(&tier) {
            subs.retain(|s| s.generation != done.generation);
            if subs.is_empty() {
                self.unanswered.remove(&tier);
            }
        }
        // Retry a held-back request: `request_batch` sends it only if no outdated submission
        // for the tier is left unanswered, and holds it back again otherwise.
        if self.deferred.remove(&tier) {
            self.request_batch(&[tier]);
        }
        if done.promotion {
            return;
        }
        if self.pending.get(&done.key) != Some(&done.generation) {
            self.stats.stale_dropped += 1;
            return;
        }
        self.pending.remove(&done.key);
        match done.result {
            Ok(loaded) => {
                let gone = self.lru.insert(done.key, loaded);
                self.release(gone, cx);
            }
            Err(e) => {
                eprintln!("image: photo {} {:?}: {e}", done.key.photo, done.key.kind);
                self.failed.insert(done.key, e.into());
            }
        }
        cx.notify();
    }

    /// Remove `images`' textures from every window's atlas. Deferred: `App::drop_image`
    /// cannot reach a window that is mid-update, and this may run inside one (a render or a
    /// listener requesting images).
    fn release(&mut self, images: Vec<Arc<RenderImage>>, cx: &mut Context<Self>) {
        if images.is_empty() {
            return;
        }
        self.stats.released += images.len() as u64;
        cx.defer(move |cx: &mut App| {
            for image in images {
                cx.drop_image(image, None);
            }
        });
    }
}
