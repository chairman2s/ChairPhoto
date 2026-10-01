//! Map tiles as GPU textures: the [`TileBackend`] seam (the network and decode, off the UI
//! thread) and the view's [`TileLayer`] (what is wanted, what is held, what is released).
//!
//! ```text
//! MapView ──want(visible keys)──▶ TileLayer ──load──▶ TileBackend (core runtime:
//!                                    ▲                  TileFetcher::load → PNG/JPEG decode
//!                                    │                  → BGRA RenderImage, on a blocking worker)
//!                                    └── TileDone {key, generation} ◀── unbounded channel
//! ```
//!
//! - **Consent first.** The layer is only ever asked for tiles of a host the user allowed
//!   (the view checks; `tests.rs` proves no load happens otherwise).
//! - **Only what is visible.** `want` cancels every pending load that left the view — a load
//!   still waiting for one of the fetcher's four permits never reaches the network.
//! - **Stale results are dropped** by generation: a result lands only if its key is still
//!   pending under the generation it was asked with. A source change, a consent change, a
//!   catalog switch ([`TileLayer::clear`]) or the view closing (the channel is gone) makes
//!   every outstanding result stale.
//! - **Bounded GPU memory.** At most [`TILE_BUDGET`] decoded tiles are held (LRU) — or the
//!   visible set, when that is larger: a tile the view shows now is **pinned** and never
//!   evicted, so a 4K canvas near a half zoom (≈ 300 visible tiles) does not evict and reload
//!   its own tiles in a loop. An evicted or cleared tile is released from every window's
//!   sprite atlas with `drop_image` (deferred: it may be evicted inside a render).

use chairphoto_core::plugins::map::tiles::fetch::TileFetcher;
use chairphoto_core::plugins::map::tiles::{TileKey, TileSource};
use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use gpui_kit::{App, Global, RenderImage};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

/// Decoded tiles held beyond the visible set: 256 × 256 × 4 bytes each, so about 64 MiB of
/// textures. A 4K viewport just above a half zoom (the next zoom's tiles drawn at ≈ 0.7×) shows about 300,
/// more than this: visible tiles are pinned ([`TileLayer::want`]), so the layer then holds
/// exactly the visible set until the view shrinks or zooms.
pub const TILE_BUDGET: usize = 256;

/// Answers one tile load: the texture, or why not.
pub type TileRespond = Box<dyn FnOnce(Result<Arc<RenderImage>, String>) + Send>;

/// A load in progress: cancelling it drops the work (and the network request, if it has
/// not started).
pub trait TileTicket {
    fn cancel(&self);
}

/// Where tiles come from. [`NetTiles`] in the app; a recording fake in tests.
pub trait TileBackend: Send + Sync {
    /// Load `key` from `source`. A redirect may lead only to the source's own host or to
    /// one of `redirect_hosts` (the other hosts the user allowed).
    fn load(&self, source: &TileSource, redirect_hosts: &Arc<[String]>, key: TileKey, respond: TileRespond) -> Box<dyn TileTicket>;
}

/// The installed backend, a GPUI global; absent means [`NetTiles`] (made on first use).
#[derive(Clone)]
pub struct MapTiles(pub Arc<dyn TileBackend>);

impl Global for MapTiles {}

impl MapTiles {
    pub fn get(cx: &mut App) -> Arc<dyn TileBackend> {
        if let Some(t) = cx.try_global::<MapTiles>() {
            return t.0.clone();
        }
        let backend: Arc<dyn TileBackend> = match TileFetcher::production() {
            Ok(fetcher) => Arc::new(NetTiles { fetcher }),
            Err(e) => Arc::new(Unavailable(e)),
        };
        cx.set_global(MapTiles(backend.clone()));
        backend
    }
}

/// The real backend: [`TileFetcher`] on the core runtime, decoded on a blocking worker.
pub struct NetTiles {
    pub fetcher: TileFetcher,
}

struct Abort(tokio::task::AbortHandle);

impl TileTicket for Abort {
    fn cancel(&self) {
        self.0.abort();
    }
}

impl TileBackend for NetTiles {
    fn load(&self, source: &TileSource, redirect_hosts: &Arc<[String]>, key: TileKey, respond: TileRespond) -> Box<dyn TileTicket> {
        let (fetcher, source, redirect_hosts) = (self.fetcher.clone(), source.clone(), redirect_hosts.clone());
        let task = chairphoto_core::app::runtime().spawn(async move {
            let result = match fetcher.load_allowing(&source, key, &redirect_hosts).await {
                Ok(tile) => tokio::task::spawn_blocking(move || decode(&tile.bytes))
                    .await
                    .unwrap_or_else(|e| Err(e.to_string())),
                Err(e) => Err(e),
            };
            respond(result);
        });
        Box::new(Abort(task.abort_handle()))
    }
}

/// No HTTP client could be built: every load fails with why.
struct Unavailable(String);

struct NoTicket;

impl TileTicket for NoTicket {
    fn cancel(&self) {}
}

impl TileBackend for Unavailable {
    fn load(&self, _: &TileSource, _: &Arc<[String]>, _: TileKey, respond: TileRespond) -> Box<dyn TileTicket> {
        respond(Err(self.0.clone()));
        Box::new(NoTicket)
    }
}

/// PNG/JPEG bytes → the BGRA texture GPUI uploads. Blocking: a worker thread only.
pub fn decode(bytes: &[u8]) -> Result<Arc<RenderImage>, String> {
    let image = image::load_from_memory(bytes).map_err(|e| format!("tile decode failed: {e}"))?;
    Ok(crate::image_store::to_bgra(image))
}

/// A finished load, on its way to the UI thread.
pub struct TileDone {
    pub key: TileKey,
    pub generation: u64,
    pub result: Result<Arc<RenderImage>, String>,
}

/// Counters for tests and the bench.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TileStats {
    /// Loads handed to the backend.
    pub requested: u64,
    /// Pending loads cancelled because their tile left the view (or the layer was cleared).
    pub cancelled: u64,
    /// Results that arrived for a key no longer pending under their generation.
    pub stale_dropped: u64,
    /// Textures handed to `drop_image`.
    pub released: u64,
}

/// The view's tiles: an LRU of textures, the pending loads, and failures (not retried
/// until the source changes).
pub struct TileLayer {
    backend: Arc<dyn TileBackend>,
    source: Option<TileSource>,
    /// The other hosts the user allowed: where a redirect may also lead.
    redirect_hosts: Arc<[String]>,
    held: HashMap<TileKey, (Arc<RenderImage>, u64)>,
    order: BTreeMap<u64, TileKey>,
    tick: u64,
    pending: HashMap<TileKey, (u64, Box<dyn TileTicket>)>,
    /// The keys of the last [`want`](Self::want): pinned, never evicted.
    visible: HashSet<TileKey>,
    failed: HashSet<TileKey>,
    last_error: Option<String>,
    generation: u64,
    done: UnboundedSender<TileDone>,
    budget: usize,
    pub stats: TileStats,
}

impl TileLayer {
    /// A layer and the receiver its results arrive on (the view drains it on the UI thread).
    pub fn new(backend: Arc<dyn TileBackend>, budget: usize) -> (Self, UnboundedReceiver<TileDone>) {
        let (done, rx) = unbounded();
        let layer = TileLayer {
            backend,
            source: None,
            redirect_hosts: Arc::from(Vec::new()),
            held: HashMap::new(),
            order: BTreeMap::new(),
            tick: 0,
            pending: HashMap::new(),
            visible: HashSet::new(),
            failed: HashSet::new(),
            last_error: None,
            generation: 0,
            done,
            budget: budget.max(1),
            stats: TileStats::default(),
        };
        (layer, rx)
    }

    pub fn source(&self) -> Option<&TileSource> {
        self.source.as_ref()
    }

    pub fn held(&self) -> usize {
        self.held.len()
    }

    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    pub fn is_pending(&self, key: &TileKey) -> bool {
        self.pending.contains_key(key)
    }

    /// The newest load error, for the status line.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Show tiles of `source` from now on; a different source drops everything held and
    /// pending (returned, to be released). `None` = no tiles (no consent).
    /// The other hosts the user allowed (a tile server's redirect may lead there, and
    /// nowhere else but its own host). Applies to loads started from now on.
    pub fn set_redirect_hosts(&mut self, hosts: Vec<String>) {
        if *self.redirect_hosts != *hosts {
            self.redirect_hosts = Arc::from(hosts);
        }
    }

    pub fn set_source(&mut self, source: Option<TileSource>) -> Vec<Arc<RenderImage>> {
        if self.source == source {
            return Vec::new();
        }
        self.source = source;
        self.clear()
    }

    /// Forget everything: cancel every pending load (its late result is stale) and hand
    /// back every held texture to release.
    pub fn clear(&mut self) -> Vec<Arc<RenderImage>> {
        self.generation += 1;
        for (_, (_, ticket)) in self.pending.drain() {
            ticket.cancel();
            self.stats.cancelled += 1;
        }
        self.failed.clear();
        self.last_error = None;
        self.visible.clear();
        self.order.clear();
        let gone: Vec<Arc<RenderImage>> = self.held.drain().map(|(_, (img, _))| img).collect();
        self.stats.released += gone.len() as u64;
        gone
    }

    /// The tiles the view shows now, most urgent first: cancel pending loads not among
    /// them, then load the missing ones. Nothing without a source. These keys stay pinned
    /// (never evicted) until the next call.
    pub fn want(&mut self, keys: &[TileKey]) {
        let Some(source) = self.source.clone() else { return };
        let wanted: HashSet<TileKey> = keys.iter().copied().collect();
        self.visible = wanted.clone();
        let gone: Vec<TileKey> = self.pending.keys().filter(|k| !wanted.contains(k)).copied().collect();
        for key in gone {
            if let Some((_, ticket)) = self.pending.remove(&key) {
                ticket.cancel();
                self.stats.cancelled += 1;
            }
        }
        let mut seen = HashSet::new();
        for &key in keys {
            if !seen.insert(key) || self.held.contains_key(&key) || self.pending.contains_key(&key) || self.failed.contains(&key) {
                continue;
            }
            self.generation += 1;
            let generation = self.generation;
            let done = self.done.clone();
            self.stats.requested += 1;
            // Register before loading: a backend may answer synchronously.
            let respond: TileRespond = Box::new(move |result| {
                // Fails only when the view is gone: the tile is then unwanted.
                let _ = done.unbounded_send(TileDone { key, generation, result });
            });
            let ticket = self.backend.load(&source, &self.redirect_hosts, key, respond);
            self.pending.insert(key, (generation, ticket));
        }
    }

    /// A result arrived. Returns the textures to release (evicted, or a stale result's).
    pub fn complete(&mut self, done: TileDone) -> Vec<Arc<RenderImage>> {
        if self.pending.get(&done.key).map(|(g, _)| *g) != Some(done.generation) {
            self.stats.stale_dropped += 1;
            // Never painted, so never in an atlas: dropping the Arc is enough.
            return Vec::new();
        }
        self.pending.remove(&done.key);
        match done.result {
            Ok(image) => self.insert(done.key, image),
            Err(e) => {
                eprintln!("map: tile {:?}: {e}", done.key);
                self.failed.insert(done.key);
                self.last_error = Some(e);
                Vec::new()
            }
        }
    }

    fn insert(&mut self, key: TileKey, image: Arc<RenderImage>) -> Vec<Arc<RenderImage>> {
        self.tick += 1;
        let mut gone = Vec::new();
        if let Some((old, used)) = self.held.insert(key, (image, self.tick)) {
            self.order.remove(&used);
            gone.push(old);
        }
        self.order.insert(self.tick, key);
        while self.held.len() > self.budget {
            // The least recently used tile that is not on screen. None: everything held is
            // visible, so the layer holds the visible set (bounded by the viewport) for now.
            let victim = self.order.iter().find(|(_, k)| !self.visible.contains(k)).map(|(t, k)| (*t, *k));
            let Some((used, oldest)) = victim else { break };
            self.order.remove(&used);
            if let Some((img, _)) = self.held.remove(&oldest) {
                gone.push(img);
            }
        }
        self.stats.released += gone.len() as u64;
        gone
    }

    /// The texture for `key`, marked as used.
    pub fn get(&mut self, key: &TileKey) -> Option<Arc<RenderImage>> {
        self.tick += 1;
        let tick = self.tick;
        let (img, used) = self.held.get_mut(key)?;
        self.order.remove(used);
        *used = tick;
        self.order.insert(tick, *key);
        Some(img.clone())
    }

    /// The nearest held ancestor of `key` (up to `levels` zooms out), to stretch over a tile
    /// still loading so zooming never flashes blank: the ancestor and its key.
    pub fn ancestor(&mut self, key: TileKey, levels: u8) -> Option<(TileKey, Arc<RenderImage>)> {
        let mut k = key;
        for _ in 0..levels {
            k = k.parent()?;
            if let Some(img) = self.get(&k) {
                return Some((k, img));
            }
        }
        None
    }
}

impl Drop for TileLayer {
    fn drop(&mut self) {
        for (_, (_, ticket)) in self.pending.drain() {
            ticket.cancel();
        }
    }
}

/// Release `images` from every window's atlas, after the current update.
pub fn release(images: Vec<Arc<RenderImage>>, cx: &mut App) {
    if images.is_empty() {
        return;
    }
    cx.defer(move |cx| {
        for image in images {
            cx.drop_image(image, None);
        }
    });
}

#[cfg(test)]
pub(crate) mod fake {
    //! A tile backend that only records: the test answers each load when it chooses.
    use super::*;
    use std::sync::Mutex;

    pub struct Load {
        pub host: String,
        pub redirect_hosts: Vec<String>,
        pub key: TileKey,
        pub respond: Option<TileRespond>,
        pub cancelled: Arc<std::sync::atomic::AtomicBool>,
    }

    #[derive(Default)]
    pub struct FakeTiles {
        pub loads: Mutex<Vec<Load>>,
    }

    struct Ticket(Arc<std::sync::atomic::AtomicBool>);

    impl TileTicket for Ticket {
        fn cancel(&self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl TileBackend for FakeTiles {
        fn load(&self, source: &TileSource, redirect_hosts: &Arc<[String]>, key: TileKey, respond: TileRespond) -> Box<dyn TileTicket> {
            let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
            self.loads.lock().unwrap().push(Load {
                host: source.host().to_string(),
                redirect_hosts: redirect_hosts.to_vec(),
                key,
                respond: Some(respond),
                cancelled: cancelled.clone(),
            });
            Box::new(Ticket(cancelled))
        }
    }

    impl FakeTiles {
        pub fn count(&self) -> usize {
            self.loads.lock().unwrap().len()
        }

        /// Answer every load not yet answered (and not cancelled) with a 1×1 tile.
        pub fn answer_all(&self) -> usize {
            let mut n = 0;
            for load in self.loads.lock().unwrap().iter_mut() {
                if load.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
                    continue;
                }
                if let Some(respond) = load.respond.take() {
                    respond(Ok(tiny()));
                    n += 1;
                }
            }
            n
        }
    }

    pub fn tiny() -> Arc<RenderImage> {
        Arc::new(RenderImage::new(vec![image::Frame::new(image::RgbaImage::new(1, 1))]))
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{tiny, FakeTiles};
    use super::*;

    fn key(x: u32) -> TileKey {
        TileKey { z: 3, x, y: 1 }
    }

    fn layer(budget: usize) -> (TileLayer, Arc<FakeTiles>, UnboundedReceiver<TileDone>) {
        let fake = Arc::new(FakeTiles::default());
        let (mut l, rx) = TileLayer::new(fake.clone(), budget);
        l.set_source(Some(TileSource::default()));
        (l, fake, rx)
    }

    fn drain(l: &mut TileLayer, rx: &mut UnboundedReceiver<TileDone>) -> Vec<Arc<RenderImage>> {
        let mut gone = Vec::new();
        while let Ok(d) = rx.try_recv() {
            gone.extend(l.complete(d));
        }
        gone
    }

    #[test]
    fn nothing_loads_without_a_source() {
        let fake = Arc::new(FakeTiles::default());
        let (mut l, _rx) = TileLayer::new(fake.clone(), 8);
        l.want(&[key(0), key(1)]);
        assert_eq!(fake.count(), 0);
    }

    #[test]
    fn wanted_tiles_load_once_and_leaving_the_view_cancels() {
        let (mut l, fake, mut rx) = layer(8);
        l.want(&[key(0), key(1), key(0)]);
        l.want(&[key(0), key(1)]);
        assert_eq!(fake.count(), 2, "one load per tile");
        l.want(&[key(1), key(2)]);
        assert!(fake.loads.lock().unwrap()[0].cancelled.load(std::sync::atomic::Ordering::SeqCst), "tile 0 left the view");
        assert_eq!(fake.answer_all(), 2);
        drain(&mut l, &mut rx);
        assert!(l.get(&key(1)).is_some() && l.get(&key(2)).is_some());
        assert_eq!((l.held(), l.pending()), (2, 0));
    }

    /// A result for a tile whose load was superseded (cancelled, cleared, new source) is
    /// dropped, never shown.
    #[test]
    fn stale_results_are_dropped() {
        let (mut l, fake, mut rx) = layer(8);
        l.want(&[key(0)]);
        let respond = fake.loads.lock().unwrap()[0].respond.take().unwrap();
        let cleared = l.set_source(Some(TileSource::parse("https://example.org/{z}/{x}/{y}.png").unwrap()));
        assert!(cleared.is_empty());
        respond(Ok(tiny()));
        drain(&mut l, &mut rx);
        assert_eq!((l.held(), l.stats.stale_dropped), (0, 1));
    }

    /// Past the budget the least recently used tile goes, and every texture that leaves is
    /// handed back for `drop_image`.
    #[test]
    fn eviction_and_clearing_hand_back_textures_to_release() {
        let (mut l, fake, mut rx) = layer(2);
        l.want(&[key(0), key(1)]);
        fake.answer_all();
        assert!(drain(&mut l, &mut rx).is_empty());
        l.get(&key(0)); // 0 is now newer than 1
        l.want(&[key(0), key(2)]);
        fake.answer_all();
        let gone = drain(&mut l, &mut rx);
        assert_eq!(gone.len(), 1);
        assert!(l.get(&key(1)).is_none() && l.get(&key(0)).is_some());
        assert_eq!(l.clear().len(), 2);
        assert_eq!(l.stats.released, 3);
    }

    /// A 3840×2160 canvas just above a half zoom (tiles of the next zoom drawn at ≈ 0.7×) shows more tiles than [`TILE_BUDGET`]. Every
    /// one of them stays held after loading, and asking for the same view again loads
    /// nothing: no visible tile is evicted and re-requested (the review's reload loop).
    #[test]
    fn visible_tiles_beyond_the_budget_are_pinned_not_reloaded() {
        use chairphoto_core::plugins::map::tiles::Viewport;
        let vp = Viewport::new((48.85, 2.35), 10.51, 3840.0, 2160.0);
        let keys: Vec<TileKey> = vp.visible_tiles().iter().map(|t| t.key).collect();
        let unique: HashSet<TileKey> = keys.iter().copied().collect();
        assert!(unique.len() > TILE_BUDGET, "{} visible tiles: the case needs more than the budget", unique.len());
        let (mut l, fake, mut rx) = layer(TILE_BUDGET);
        l.want(&keys);
        fake.answer_all();
        assert!(drain(&mut l, &mut rx).is_empty(), "nothing visible was released");
        assert_eq!(l.held(), unique.len());
        for _ in 0..3 {
            l.want(&keys); // the view repaints
            fake.answer_all();
            drain(&mut l, &mut rx);
        }
        assert_eq!(fake.count(), unique.len(), "a visible tile was evicted and loaded again");
        // Panning away: the old tiles are no longer pinned and the LRU trims back to budget.
        let moved = Viewport::new((40.0, -74.0), 10.51, 3840.0, 2160.0);
        let next: Vec<TileKey> = moved.visible_tiles().iter().map(|t| t.key).collect();
        l.want(&next);
        fake.answer_all();
        let released = drain(&mut l, &mut rx);
        let next_unique: HashSet<TileKey> = next.iter().copied().collect();
        assert_eq!(l.held(), next_unique.len().max(TILE_BUDGET));
        assert_eq!(released.len(), unique.len() + next_unique.len() - l.held());
    }

    #[test]
    fn a_failed_tile_is_not_retried_until_the_source_changes() {
        let (mut l, fake, mut rx) = layer(4);
        l.want(&[key(0)]);
        let respond = fake.loads.lock().unwrap()[0].respond.take().unwrap();
        respond(Err("HTTP 404".into()));
        drain(&mut l, &mut rx);
        l.want(&[key(0)]);
        assert_eq!(fake.count(), 1);
        assert_eq!(l.last_error(), Some("HTTP 404"));
    }

    #[test]
    fn an_ancestor_stands_in_for_a_loading_tile() {
        let (mut l, fake, mut rx) = layer(4);
        let parent = TileKey { z: 2, x: 0, y: 0 };
        l.want(&[parent]);
        fake.answer_all();
        drain(&mut l, &mut rx);
        let child = TileKey { z: 4, x: 1, y: 2 };
        assert_eq!(l.ancestor(child, 3).map(|(k, _)| k), Some(parent));
        assert_eq!(l.ancestor(child, 1), None);
    }

    #[test]
    fn tiles_decode_to_bgra() {
        let mut png = Vec::new();
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([10, 20, 30, 255]));
        image::DynamicImage::ImageRgba8(img).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        let tex = decode(&png).unwrap();
        assert_eq!(&tex.as_bytes(0).unwrap()[..4], &[30, 20, 10, 255]);
        assert!(decode(b"not an image").is_err());
    }
}
