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
//! - **Claims** say who still wants a tier, so one view's release cannot take another's
//!   images (#110: the inline loupe and the pop-out loupe navigate over the same store). A
//!   view that navigates holds a [`ClaimId`] and replaces its claimed set on every step
//!   ([`ImageStore::set_claim`]): what it held and holds no longer is released — unless
//!   another claim holds it. [`ImageStore::release_pending`] and [`ImageStore::evict`] never
//!   touch a claimed tier. Only a change of what the pixels are (an invalidate, a catalog
//!   switch) drops claimed requests, and a switch empties every claim.
//! - **Cover looks** (#134 the Darkroom filmstrip, #151 the Library grid): a thumbnail shows
//!   the photo's cover version's look (`media::render_image` renders the cover the catalog
//!   names when the worker runs). A view whose rows carry the cover token asks through
//!   [`ImageStore::request_look_batch`] (the grid) or [`ImageStore::request_looks`] (the
//!   strip, under its claim): the tier is keyed by the look the row names — cover version and
//!   revision, as `Thumbnail.tsx` put the cover token in the thumbnail URL — and by the
//!   catalog the row was read from. A different look, or a tier cached, pending, failed or
//!   refused with no look said, is invalidated and rendered again, so a result for the
//!   earlier look is dropped by generation; the same look asks nothing. `looks` says what the
//!   tier was last asked to show, whoever asked: the views that name looks read them from the
//!   same rows (the shell's), so they agree, and one view letting go never makes another's
//!   cached thumbnail unknown (which would render it again). Views without the token (stacks,
//!   the inspector's stack, cards, collage, the loupe's placeholder) ask plainly and share the
//!   tier: they show the look the grid or the strip last asked for.
//! - **A look's catalog.** A submission made for a look is bound to the catalog its row came
//!   from: after the render, on the worker, the open catalog's identity is checked
//!   ([`ImageStore::set_identity_probe`]); another catalog (a switch whose `catalog:switched`
//!   has not arrived, a re-root, which sends none) refuses the result, never pixels shown
//!   under the old row. Only look submissions are bound: a plain request is never checked
//!   against a catalog its view did not read (review rv134 M1). The catalog cannot have
//!   changed and changed back: identities never repeat. A refusal is not a failure: the tier
//!   is left empty and, while the photo's look is still the refused row's, not asked again
//!   (by any view: the catalog open is not that row's). Rows read from another catalog forget
//!   every earlier look and refusal, so the next ask renders again; a refusal that arrives
//!   for a look already superseded is not kept.
//!
//! The pool is behind [`Submit`] so tests can hold responders and deliver them in any order.

use chairphoto_core::app::{CatalogIdentity, CATALOG_CHANGED};
use chairphoto_core::image_pool::{ImageKind, ImagePool, JobKey, Respond};
use chairphoto_core::media::DecodedImage;
use chairphoto_model::darkroom::filmstrip::CoverLook;
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

/// `photos`' preload window around `index`, as `kind` requests, most urgent first.
fn window_of(photos: &[i64], index: usize, kind: ImageKind, ahead: usize, behind: usize) -> Vec<(i64, ImageKind)> {
    neighbour_window(photos.len(), index, ahead, behind).into_iter().map(|i| (photos[i], kind)).collect()
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
    /// Bound thumbnails rendered in another catalog than their row's, left empty.
    pub refused: u64,
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
    /// The catalog a look submission was bound to (see the module docs).
    bound: Option<CatalogIdentity>,
    /// Rendered in another catalog than the look's.
    refused: bool,
    result: Result<Loaded, String>,
}

/// A view's hold on the tiers it wants ([`ImageStore::new_claim`]); see the module docs.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ClaimId(u64);

/// The identity of the catalog open now, asked on a pool worker after a bound render (see
/// the module docs); `None` when none is open.
pub type IdentityProbe = Arc<dyn Fn() -> Option<CatalogIdentity> + Send + Sync>;

/// What a photo's thumbnail tier was asked to show ([`ImageStore::request_looks`]): the
/// cover look its row names, in the catalog that row was read from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Look {
    pub from: CatalogIdentity,
    pub cover: Option<CoverLook>,
}

/// The app's image cache and request broker. One per app; views read it and request
/// through it.
pub struct ImageStore {
    pool: Arc<dyn Submit>,
    lru: ImageLru,
    /// Requested and not yet answered: key → the generation of its newest submission.
    pending: HashMap<ImageKey, u64>,
    failed: HashMap<ImageKey, SharedString>,
    /// Per tier: bumped by [`invalidate`](Self::invalidate) (every tier of the photo) and by a
    /// new cover look (the thumbnail's).
    versions: HashMap<(i64, ImageKind), u64>,
    /// Every submission not yet answered, per tier: (generation, version, epoch). Released
    /// or abandoned ones stay until their answer arrives; see the module docs.
    unanswered: HashMap<(i64, ImageKind), Vec<Submission>>,
    /// Requests held back by an outdated unanswered submission, sent once none is left —
    /// with the catalog a look request bound them to (a plain request's merges into it).
    deferred: HashMap<(i64, ImageKind), Option<CatalogIdentity>>,
    /// Bumped by [`clear`](Self::clear): photo ids from an older epoch are other photos.
    epoch: u64,
    generation: u64,
    /// What each claim holds (see the module docs).
    claims: HashMap<ClaimId, HashSet<(i64, ImageKind)>>,
    next_claim: u64,
    /// The look each photo's thumbnail tier was last asked for, by any view (see the module
    /// docs). Forgotten when rows come from another catalog, and by a switch.
    looks: HashMap<i64, Look>,
    /// The catalog the looks in `looks` were read from.
    looks_from: Option<CatalogIdentity>,
    /// Tiers whose render for their look was refused: not asked again until that changes.
    refused: HashSet<ImageKey>,
    probe: Option<IdentityProbe>,
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
            deferred: HashMap::new(),
            epoch: 0,
            generation: 0,
            claims: HashMap::new(),
            next_claim: 0,
            looks: HashMap::new(),
            looks_from: None,
            refused: HashSet::new(),
            probe: None,
            done,
            stats: StoreStats::default(),
            _drain,
        }
    }

    pub fn stats(&self) -> StoreStats {
        self.stats
    }

    /// How a bound thumbnail render checks the catalog it was rendered from (`wire`: the
    /// core's `catalog_identity`). Without one, renders are not checked.
    pub fn set_identity_probe(&mut self, probe: IdentityProbe) {
        self.probe = Some(probe);
    }

    /// The look `photo`'s thumbnail tier was last asked for, if any.
    pub fn look(&self, photo: i64) -> Option<Look> {
        self.looks.get(&photo).copied()
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
        ImageKey { photo, kind, version: self.versions.get(&(photo, kind)).copied().unwrap_or(0) }
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
        self.submit(wanted, false, None);
    }

    /// [`request_batch`](Self::request_batch), and with `promote` a pending key is re-sent so
    /// the pool moves it up to its place in this batch — what navigation needs when a preload
    /// becomes the current photo. The re-send is tracked in `unanswered` under its own
    /// generation; its answer only ends that record (the pending key's own answer lands).
    /// With `look`, each thumbnail sent is bound to that catalog (see the module docs).
    fn submit(&mut self, wanted: &[(i64, ImageKind)], promote: bool, look: Option<CatalogIdentity>) {
        let mut batch: Vec<(JobKey, Respond<Loaded>)> = Vec::with_capacity(wanted.len());
        let mut seen = HashSet::new();
        for &(photo, kind) in wanted {
            let key = self.key(photo, kind);
            if !seen.insert(key) || self.lru.peek(&key).is_some() || self.failed.contains_key(&key) || self.refused.contains(&key) {
                continue;
            }
            if self.outdated_in_flight((photo, kind), key.version) {
                let bind = self.deferred.entry((photo, kind)).or_insert(None);
                if look.is_some() {
                    *bind = look;
                }
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
            // A thumbnail asked for a look is bound to the catalog its row came from.
            let check = match (kind, look, &self.probe) {
                (ImageKind::Thumb, Some(from), Some(probe)) => Some((from, probe.clone())),
                _ => None,
            };
            let bound = check.as_ref().map(|(from, _)| *from);
            batch.push((
                job,
                Box::new(move |result| {
                    // On the worker, after the render: pixels from another catalog are not
                    // this row's photo.
                    let refused = matches!(&check, Some((from, probe)) if result.is_ok() && probe() != Some(*from));
                    let result = if refused { Err(CATALOG_CHANGED.to_string()) } else { result };
                    // Fails only when the store is gone; the result is then unwanted.
                    let _ = done.unbounded_send(Done { key, generation, promotion, bound, refused, result });
                }),
            ));
        }
        if !batch.is_empty() {
            self.pool.submit_batch(batch);
        }
    }

    /// Stop wanting every pending request `keep` rejects and no claim holds: each is cancelled
    /// in the pool if it is still queued, and whatever it produces later is dropped by
    /// generation. A request held back in `deferred` that is released (judged under the
    /// photo's current version, which is what it would be sent as) is forgotten, so it is not
    /// sent when the render it waits for answers.
    pub fn release_pending(&mut self, mut keep: impl FnMut(&ImageKey) -> bool) {
        let claimed = self.claimed_tiers(None);
        self.drop_pending(|k| keep(k) || claimed.contains(&(k.photo, k.kind)));
    }

    /// A new, empty claim, for a view that navigates (see the module docs).
    pub fn new_claim(&mut self) -> ClaimId {
        self.next_claim += 1;
        let id = ClaimId(self.next_claim);
        self.claims.insert(id, HashSet::new());
        id
    }

    /// What `owner` holds now.
    pub fn claim(&self, owner: ClaimId) -> HashSet<(i64, ImageKind)> {
        self.claims.get(&owner).cloned().unwrap_or_default()
    }

    /// Whether any claim holds `photo`'s `kind`.
    pub fn is_claimed(&self, photo: i64, kind: ImageKind) -> bool {
        self.claims.values().any(|held| held.contains(&(photo, kind)))
    }

    /// Every tier a claim other than `except` holds.
    fn claimed_tiers(&self, except: Option<ClaimId>) -> HashSet<(i64, ImageKind)> {
        self.claims
            .iter()
            .filter(|(id, _)| Some(**id) != except)
            .flat_map(|(_, held)| held.iter().copied())
            .collect()
    }

    /// Make `owner` hold exactly `wanted`. What it held and holds no longer is released if
    /// pending — unless another claim holds it. Sends nothing.
    pub fn set_claim(&mut self, owner: ClaimId, wanted: impl IntoIterator<Item = (i64, ImageKind)>) {
        let wanted: HashSet<(i64, ImageKind)> = wanted.into_iter().collect();
        let before = self.claims.insert(owner, wanted.clone()).unwrap_or_default();
        let others = self.claimed_tiers(Some(owner));
        let gone: HashSet<(i64, ImageKind)> =
            before.into_iter().filter(|t| !wanted.contains(t) && !others.contains(t)).collect();
        if !gone.is_empty() {
            self.drop_pending(|k| !gone.contains(&(k.photo, k.kind)));
        }
    }

    /// Give up `owner`'s claim for good, releasing what only it held.
    pub fn drop_claim(&mut self, owner: ClaimId) {
        self.set_claim(owner, []);
        self.claims.remove(&owner);
    }

    /// [`release_pending`](Self::release_pending) whatever the claims hold: for what no claim
    /// survives (a change of what the pixels are, a superseded claim).
    fn drop_pending(&mut self, mut keep: impl FnMut(&ImageKey) -> bool) {
        let released: Vec<ImageKey> = self.pending.keys().filter(|k| !keep(k)).copied().collect();
        for key in released {
            self.pending.remove(&key);
            self.pool.cancel(&JobKey::photo(key.photo, key.kind));
        }
        let versions = &self.versions;
        self.deferred.retain(|&(photo, kind), _| {
            keep(&ImageKey { photo, kind, version: versions.get(&(photo, kind)).copied().unwrap_or(0) })
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
    /// are superseded, except those a claim holds.
    pub fn navigate_window(&mut self, photos: &[i64], index: usize, kind: ImageKind, ahead: usize, behind: usize) {
        let wanted = window_of(photos, index, kind, ahead, behind);
        let keep: HashSet<i64> = wanted.iter().map(|&(p, _)| p).collect();
        self.release_pending(|k| k.kind != kind || keep.contains(&k.photo));
        self.submit(&wanted, true, None);
    }

    /// [`navigate_window`](Self::navigate_window) for the view holding `owner`: its claim
    /// becomes the window plus `also` (tiers it may ask for later, such as the current
    /// photo's full-resolution one). What it superseded is released only if no other claim
    /// holds it, and what it wants is safe from other views' releases.
    #[allow(clippy::too_many_arguments)]
    pub fn navigate_window_as(
        &mut self,
        owner: ClaimId,
        photos: &[i64],
        index: usize,
        kind: ImageKind,
        ahead: usize,
        behind: usize,
        also: &[(i64, ImageKind)],
    ) {
        let wanted = window_of(photos, index, kind, ahead, behind);
        self.set_claim(owner, wanted.iter().chain(also).copied());
        self.submit(&wanted, true, None);
    }

    /// The Darkroom filmstrip's frames (#134): `owner` holds exactly `wanted`'s thumbnail
    /// tiers, asked for as [`request_look_batch`](Self::request_look_batch) asks (the strip's
    /// order: the open photo's frame, then outwards). Calling it again with the same looks
    /// sends nothing.
    pub fn request_looks(
        &mut self,
        owner: ClaimId,
        from: CatalogIdentity,
        wanted: &[(i64, Option<CoverLook>)],
        cx: &mut Context<Self>,
    ) {
        let tiers: Vec<(i64, ImageKind)> = wanted.iter().map(|&(photo, _)| (photo, ImageKind::Thumb)).collect();
        self.set_claim(owner, tiers.iter().copied());
        self.request_look_batch(from, wanted, cx);
    }

    /// Thumbnails for rows that carry their cover token (#151: the Library grid; the strip
    /// through [`request_looks`](Self::request_looks)), sent as one batch in `wanted`'s order
    /// — the caller's, most urgent first — each to show the cover look its row names, read
    /// from the catalog `from`. A tier asked for another look before — or cached, pending,
    /// failed or refused with no look said — is invalidated first, so what it held or will
    /// still answer for the earlier look is never shown; no other tier, and no other photo, is
    /// touched. What is sent is bound to `from` (see the module docs). Like
    /// [`request_batch`](Self::request_batch) it may be called every frame: once the looks
    /// are pending or cached, nothing is sent.
    pub fn request_look_batch(&mut self, from: CatalogIdentity, wanted: &[(i64, Option<CoverLook>)], cx: &mut Context<Self>) {
        if self.looks_from != Some(from) {
            // Rows from another catalog: the earlier rows' looks, and the refusals made for
            // them, say nothing about these.
            self.looks.clear();
            self.refused.clear();
            self.looks_from = Some(from);
        }
        for &(photo, cover) in wanted {
            let look = Look { from, cover };
            let stale = match self.looks.get(&photo) {
                Some(asked) => *asked != look,
                None => {
                    let key = self.key(photo, ImageKind::Thumb);
                    self.lru.peek(&key).is_some()
                        || self.pending.contains_key(&key)
                        || self.failed.contains_key(&key)
                        || self.refused.contains(&key)
                }
            };
            if stale {
                self.invalidate_tier(photo, ImageKind::Thumb, cx);
            }
            self.looks.insert(photo, look);
        }
        let tiers: Vec<(i64, ImageKind)> = wanted.iter().map(|&(photo, _)| (photo, ImageKind::Thumb)).collect();
        self.submit(&tiers, false, Some(from));
    }

    /// No strip: `owner` holds nothing. The looks stay: they say what the tiers show.
    pub fn release_looks(&mut self, owner: ClaimId) {
        self.set_claim(owner, []);
    }

    /// Drop the cached images `matches` selects and no claim holds, and release their
    /// textures. For what a view is done with and the LRU would otherwise keep: a
    /// full-resolution tier (180–245 MB for a typical RAW) costs hundreds of thumbnails while
    /// it waits for eviction.
    pub fn evict(&mut self, mut matches: impl FnMut(&ImageKey) -> bool, cx: &mut Context<Self>) {
        let claimed = self.claimed_tiers(None);
        let gone = self.lru.remove_where(|k| matches(k) && !claimed.contains(&(k.photo, k.kind)));
        if !gone.is_empty() {
            self.release(gone, cx);
            cx.notify();
        }
    }

    /// A photo's pixels changed (rotation, cover version): drop what is cached for it, and
    /// make its pending requests stale. The next request renders it again.
    pub fn invalidate(&mut self, photo: i64, cx: &mut Context<Self>) {
        for kind in [ImageKind::Thumb, ImageKind::Preview, ImageKind::Zoom] {
            self.invalidate_tier(photo, kind, cx);
        }
    }

    /// [`invalidate`](Self::invalidate) for one tier: only the thumbnail shows a cover's look,
    /// so a new look leaves the photo's preview and zoom tiers cached.
    fn invalidate_tier(&mut self, photo: i64, kind: ImageKind, cx: &mut Context<Self>) {
        *self.versions.entry((photo, kind)).or_insert(0) += 1;
        let tier = |k: &ImageKey| k.photo == photo && k.kind == kind;
        let gone = self.lru.remove_where(tier);
        self.release(gone, cx);
        self.abandon(tier);
        self.failed.retain(|k, _| !tier(k));
        self.refused.retain(|k| !tier(k));
        cx.notify();
    }

    /// Forget everything — the catalog changed, so every photo id means something else.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.epoch += 1;
        self.deferred.clear();
        for held in self.claims.values_mut() {
            held.clear();
        }
        let gone = self.lru.remove_where(|_| true);
        self.release(gone, cx);
        self.abandon(|_| true);
        self.failed.clear();
        self.versions.clear();
        self.looks.clear();
        self.looks_from = None;
        self.refused.clear();
        cx.notify();
    }

    /// Drop the pending requests `matches` selects because what they would render changed,
    /// cancelling the queued ones. A running one stays in `unanswered` under its old version
    /// or epoch, which holds back the next request for its tier.
    fn abandon(&mut self, mut matches: impl FnMut(&ImageKey) -> bool) {
        self.drop_pending(|k| !matches(k));
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
        // Retry a held-back request, bound as it was asked: `submit` sends it only if no
        // outdated submission for the tier is left unanswered, and holds it back again
        // otherwise.
        if let Some(bind) = self.deferred.remove(&tier) {
            self.submit(&[tier], false, bind);
        }
        if done.promotion {
            return;
        }
        if self.pending.get(&done.key) != Some(&done.generation) {
            self.stats.stale_dropped += 1;
            return;
        }
        self.pending.remove(&done.key);
        if done.refused {
            self.stats.refused += 1;
            // Kept only while the photo's look is still the refused row's: a look read since
            // from another catalog asks again.
            if done.bound.is_some() && self.looks.get(&done.key.photo).map(|l| l.from) == done.bound {
                self.refused.insert(done.key);
            }
            cx.notify();
            return;
        }
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
