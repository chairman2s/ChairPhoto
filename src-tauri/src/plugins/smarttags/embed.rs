//! CLIP image-embedding core for Smart Tagging (H7a).
//!
//! [`encode_jpeg`] takes JPEG (or any format the `image` crate reads) bytes, resizes to the
//! CLIP vision tower's fixed 224×224 input, normalizes with the standard CLIP mean/std, runs an
//! ONNX session against a CLIP vision encoder, and returns the **L2-normalized** embedding as a
//! `Vec<f32>` (a unit vector — so cosine similarity is a plain dot product downstream).
//!
//! This reuses the runtime patterns proven in `plugins/faces/engine.rs` (I7c/I7d):
//!
//! - A tiny lazily-built **session pool** ([`Pool`]/[`Checkout`]) so several embeds run in
//!   parallel without a per-model mutex; sized by [`configure`] off the indexer's
//!   [`crate::plugins::indexing::IndexingPlan`] (shared `indexing.speed` setting).
//! - A **CUDA execution provider with graceful CPU fallback** ([`try_register_cuda`]) — GPU is
//!   tried only in a `smarttags-cuda` build and never crashes when CUDA/cuDNN/a GPU is absent.
//! - The model path is **configurable** (`smarttags.model_path`) and a **missing model degrades
//!   gracefully**: [`encode_jpeg`] returns a typed [`EmbedError`], never panics.
//!
//! The pool is cached keyed by [`PoolKey`] — model path, pool size, intra-op threads,
//! force-CPU — through [`crate::plugins::onnx::KeyedCache`], not a plain `OnceLock` (issue
//! #18): the old cache froze at whatever the first call resolved, so a `smarttags.model_path`
//! change never took effect until restart. See [`clip_pool`].
//!
//! The pure preprocessing (resize + CLIP normalize into an NCHW tensor) and the L2-normalize are
//! model-free, so they are unit-tested offline. The full session path is exercised by a test
//! that builds a **tiny synthetic ONNX graph** in-process (no model download) — see the tests.

use image::RgbImage;

use super::models::{self, ModelError};

#[cfg(feature = "smarttags")]
use std::sync::{Arc, Condvar, Mutex};

/// CLIP's fixed square input side.
pub const CLIP_SIZE: u32 = 224;

/// Standard OpenAI CLIP preprocessing constants (per-channel mean/std over [0, 1] RGB). These
/// match every CLIP export (OpenAI, OpenCLIP, Transformers.js) so a swapped-in CLIP model still
/// gets the input distribution it was trained on.
const CLIP_MEAN: [f32; 3] = [0.481_454_67, 0.457_827_5, 0.408_210_73];
const CLIP_STD: [f32; 3] = [0.268_629_54, 0.261_302_6, 0.275_777_1];

/// The intra-op thread count each ONNX session is built with, and the number of sessions in the
/// pool. Set by [`configure`] (from the indexer, off the resolved
/// [`crate::plugins::indexing::IndexingPlan`] — Smart Tagging rides the same `indexing.speed`
/// setting). Read into a [`PoolKey`] on every pool fetch (see [`resolve_pool_key`]), so a
/// value change here rebuilds the pool the next time one is needed rather than being frozen at
/// whatever the first caller saw. A session built before `configure` ever runs (e.g. a single
/// inspector embed) uses the responsive defaults below.
#[cfg(feature = "smarttags")]
static POOL_SIZE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
#[cfg(feature = "smarttags")]
static INTRA_THREADS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(2);

/// When `true`, the CUDA EP is never registered even in a `smarttags-cuda` build — CPU only.
/// Set by [`configure_force_cpu`]. Like [`POOL_SIZE`]/[`INTRA_THREADS`], part of [`PoolKey`],
/// so flipping it rebuilds the pool on the next fetch. Inert in a non-CUDA build.
#[cfg(feature = "smarttags")]
static FORCE_CPU: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Which execution provider the built pool actually landed on. Set once at pool build.
#[cfg(feature = "smarttags")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveEp {
    /// No pool built yet (no inference has run).
    Unbuilt,
    /// Sessions run on CPU (non-CUDA build, forced CPU, or CUDA registration failed).
    Cpu,
    /// Sessions run on the CUDA execution provider (GPU).
    Cuda,
}

#[cfg(feature = "smarttags")]
static ACTIVE_EP: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

#[cfg(feature = "smarttags")]
fn set_active_ep(ep: ActiveEp) {
    use std::sync::atomic::Ordering;
    let v = match ep {
        ActiveEp::Unbuilt => 0,
        ActiveEp::Cpu => 1,
        ActiveEp::Cuda => 2,
    };
    ACTIVE_EP.store(v, Ordering::Relaxed);
}

/// The execution provider the (first) built pool landed on — [`ActiveEp::Unbuilt`] until any
/// session has been built. Lets the indexer report GPU-vs-CPU honestly after a run.
#[cfg(feature = "smarttags")]
pub fn active_ep() -> ActiveEp {
    use std::sync::atomic::Ordering;
    match ACTIVE_EP.load(Ordering::Relaxed) {
        2 => ActiveEp::Cuda,
        1 => ActiveEp::Cpu,
        _ => ActiveEp::Unbuilt,
    }
}

/// Configure the session-pool size and per-session intra-op thread count for the *next* pool
/// fetch. Call before an index run (from the indexer, off the resolved
/// [`crate::plugins::indexing::IndexingPlan`]). Unlike the old `OnceLock`-backed pool, this is
/// never a no-op: the pool is keyed by [`PoolKey`] (see [`clip_pool`]), so a value change here
/// rebuilds the pool the next time `encode_jpeg`/`encode_rgb` runs.
///
/// [`POOL_SIZE`]/[`INTRA_THREADS`] are process-global, so two Smart Tagging index runs
/// overlapping (a second `smarttags_index_photos` call trips the first job's abort flag — see
/// `commands::smarttags::begin_smarttags_job`) can move these out from under the first run's
/// still-in-flight chunk; see [`crate::plugins::faces::engine::configure`] for the full
/// reasoning, which applies identically here. A single run's own workers never see this,
/// because `configure`/[`configure_force_cpu`] are each called exactly once, before that run's
/// parallel loop starts.
#[cfg(feature = "smarttags")]
pub fn configure(pool_size: usize, intra_threads: usize) {
    use std::sync::atomic::Ordering;
    POOL_SIZE.store(pool_size.max(1), Ordering::Relaxed);
    INTRA_THREADS.store(intra_threads.max(1), Ordering::Relaxed);
}

/// Force every session onto CPU even in a `smarttags-cuda` build. Same lifecycle and
/// cross-job caveat as [`configure`]: rebuilds the pool on the next fetch rather than only
/// ever taking effect on the first one. Inert in a non-CUDA build.
#[cfg(feature = "smarttags")]
pub fn configure_force_cpu(force: bool) {
    use std::sync::atomic::Ordering;
    FORCE_CPU.store(force, Ordering::Relaxed);
}

/// Everything that determines what a built CLIP session pool looks like: the resolved model
/// path (`smarttags.model_path`, or the pinned default), whether that path is a user-supplied
/// custom model (needed by [`models::verify`], which only checksums the pinned default), pool
/// size, intra-op threads, and force-CPU. Two fetches with an equal key reuse the same pool;
/// any difference — including a changed model path — rebuilds (issue #18).
///
/// `force_cpu` doubles as "execution provider mode": today's engine has exactly two modes —
/// CPU-only and "try CUDA, fall back to CPU" — so the one boolean fully captures it, mirroring
/// `faces::engine::PoolKey`.
#[cfg(feature = "smarttags")]
#[derive(Debug, Clone, PartialEq, Eq)]
struct PoolKey {
    model_path: std::path::PathBuf,
    custom: bool,
    pool_size: usize,
    intra_threads: usize,
    force_cpu: bool,
}

/// Resolve `model_path_setting` and snapshot [`POOL_SIZE`]/[`INTRA_THREADS`]/[`FORCE_CPU`]
/// into a [`PoolKey`]. Called on every pool fetch (not just the first) — see [`clip_pool`] —
/// which is what makes both a `smarttags.model_path` change and a
/// `configure`/`configure_force_cpu` call take effect on the next fetch instead of being
/// ignored until restart.
#[cfg(feature = "smarttags")]
fn resolve_pool_key(model_path_setting: Option<&str>) -> Result<PoolKey, EmbedError> {
    use std::sync::atomic::Ordering;
    let (model_path, custom) = models::resolve_model_path(model_path_setting)?;
    Ok(PoolKey {
        model_path,
        custom,
        pool_size: POOL_SIZE.load(Ordering::Relaxed).max(1),
        intra_threads: INTRA_THREADS.load(Ordering::Relaxed).max(1),
        force_cpu: FORCE_CPU.load(Ordering::Relaxed),
    })
}

/// Failure modes of embedding. Model absence is surfaced as [`EmbedError::Model`] so the caller
/// can prompt a download rather than crashing.
#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error("image decode failed: {0}")]
    Decode(String),
    #[error("inference failed: {0}")]
    Inference(String),
}

// ── Public API ──────────────────────────────────────────────────────────────────

/// Encode a JPEG (or any format the `image` crate reads) into an L2-normalized CLIP image
/// embedding, using the model resolved from `model_path_setting` (the `smarttags.model_path`
/// value; `None`/blank → the pinned default under the app data dir).
///
/// The returned vector's length is the model's native embedding dimension (512 for CLIP
/// ViT-B/32) and is a **unit vector**. A missing/unreadable model returns [`EmbedError::Model`]
/// — it never panics.
#[cfg(feature = "smarttags")]
pub fn encode_jpeg(bytes: &[u8], model_path_setting: Option<&str>) -> Result<Vec<f32>, EmbedError> {
    let img = image::load_from_memory(bytes)
        .map_err(|e| EmbedError::Decode(e.to_string()))?
        .to_rgb8();
    encode_rgb(&img, model_path_setting)
}

/// As [`encode_jpeg`] but from an already-decoded RGB image (avoids a re-decode when the caller
/// already holds pixels, e.g. the index job's cached preview).
#[cfg(feature = "smarttags")]
pub fn encode_rgb(img: &RgbImage, model_path_setting: Option<&str>) -> Result<Vec<f32>, EmbedError> {
    let input = preprocess(img);
    let raw = run_clip(model_path_setting, &input)?;
    Ok(l2_normalize(&raw))
}

// ── Preprocessing (pure, model-free) ──────────────────────────────────────────────

/// Resize `img` to 224×224 and pack it into a CLIP-normalized NCHW f32 tensor (channel order
/// RGB, each channel `(px/255 - mean) / std`). Pure — unit-tested offline.
pub fn preprocess(img: &RgbImage) -> Vec<f32> {
    let resized = image::imageops::resize(
        img,
        CLIP_SIZE,
        CLIP_SIZE,
        image::imageops::FilterType::Triangle,
    );
    let n = (CLIP_SIZE * CLIP_SIZE) as usize;
    let mut input = vec![0f32; 3 * n];
    for (i, px) in resized.pixels().enumerate() {
        input[i] = (px[0] as f32 / 255.0 - CLIP_MEAN[0]) / CLIP_STD[0];
        input[n + i] = (px[1] as f32 / 255.0 - CLIP_MEAN[1]) / CLIP_STD[1];
        input[2 * n + i] = (px[2] as f32 / 255.0 - CLIP_MEAN[2]) / CLIP_STD[2];
    }
    input
}

/// L2-normalize into a unit vector (safe on a zero/empty vector — returns it unchanged).
pub fn l2_normalize(v: &[f32]) -> Vec<f32> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter().map(|x| x / norm).collect()
    } else {
        v.to_vec()
    }
}

/// Cosine similarity of two vectors (used by the kNN engine/tests).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

// ── Session pool + inference (feature-gated) ──────────────────────────────────────

/// A tiny fixed-size pool of interchangeable ONNX sessions. Workers **check out** a distinct
/// session, use it, and the [`Checkout`] guard returns it on drop — so N parallel embeds run on
/// N sessions with no per-model mutex bottleneck. (Same design as `faces::engine::Pool`.)
#[cfg(feature = "smarttags")]
struct Pool<T> {
    free: Mutex<Vec<T>>,
    available: Condvar,
}

#[cfg(feature = "smarttags")]
impl<T> Pool<T> {
    fn new(items: Vec<T>) -> Self {
        Pool {
            free: Mutex::new(items),
            available: Condvar::new(),
        }
    }

    /// Check out one item, blocking until one is free; the guard returns it on drop.
    fn checkout(&self) -> Result<Checkout<'_, T>, String> {
        let mut free = self.free.lock().map_err(|e| format!("pool poisoned: {e}"))?;
        while free.is_empty() {
            free = self
                .available
                .wait(free)
                .map_err(|e| format!("pool poisoned: {e}"))?;
        }
        let item = free.pop().expect("free is non-empty after the wait loop");
        Ok(Checkout {
            pool: self,
            item: Some(item),
        })
    }
}

/// RAII checkout of a pooled item; returns it to the pool (and wakes one waiter) on drop.
#[cfg(feature = "smarttags")]
struct Checkout<'a, T> {
    pool: &'a Pool<T>,
    item: Option<T>,
}

#[cfg(feature = "smarttags")]
impl<T> std::ops::Deref for Checkout<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.item.as_ref().expect("item present until drop")
    }
}

#[cfg(feature = "smarttags")]
impl<T> std::ops::DerefMut for Checkout<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.item.as_mut().expect("item present until drop")
    }
}

#[cfg(feature = "smarttags")]
impl<T> Drop for Checkout<'_, T> {
    fn drop(&mut self) {
        if let Some(item) = self.item.take() {
            if let Ok(mut free) = self.pool.free.lock() {
                free.push(item);
                self.pool.available.notify_one();
            }
        }
    }
}

/// Process-global keyed cache for the CLIP session pool — see [`PoolKey`] and
/// [`crate::plugins::onnx::KeyedCache`].
#[cfg(feature = "smarttags")]
static CLIP_CACHE: crate::plugins::onnx::KeyedCache<PoolKey, Pool<ort::session::Session>> =
    crate::plugins::onnx::KeyedCache::new();

/// Fetch (building or rebuilding as needed) the CLIP session pool for the model resolved from
/// the current `smarttags.model_path` setting and the current pool configuration. Equal
/// [`PoolKey`]s reuse the same pool, so the graph-optimization cost is paid once per distinct
/// configuration; a changed model path, pool size, intra-op thread count or force-CPU flag
/// rebuilds on the next call instead of being frozen at whatever the first caller saw
/// (issue #18) — switching models is a rare, explicit action, but it is no longer one that
/// needs a restart to take effect.
#[cfg(feature = "smarttags")]
fn clip_pool(
    model_path_setting: Option<&str>,
) -> Result<Arc<Pool<ort::session::Session>>, EmbedError> {
    let key = resolve_pool_key(model_path_setting)?;
    CLIP_CACHE
        .get_or_build(key.clone(), || build_pool(&key))
        .map_err(EmbedError::Inference)
}

/// Verify the model on disk, then build a [`Pool`] of `key.pool_size` sessions, each with
/// `key.intra_threads` intra-op threads and forced to CPU when `key.force_cpu`. Errors are
/// stringified so [`crate::plugins::onnx::KeyedCache`] can pass them through without requiring
/// `Clone`. A missing model surfaces its [`ModelError`] text so the caller can prompt a
/// download rather than crash. A build failure is not cached, so a later retry (e.g. once a
/// custom model path is fixed) gets a fresh attempt.
#[cfg(feature = "smarttags")]
fn build_pool(key: &PoolKey) -> Result<Pool<ort::session::Session>, String> {
    let path = models::verify(&key.model_path, key.custom).map_err(|e| e.to_string())?;
    let mut sessions = Vec::with_capacity(key.pool_size);
    let mut used_cuda = false;
    for _ in 0..key.pool_size {
        // `build_session` runs the ONNX Runtime preflight; a session must never be built
        // directly, or a missing runtime hangs instead of erroring. See plugins::onnx.
        // `key.force_cpu` — not the live `FORCE_CPU` atomic — decides CUDA registration, so
        // the built pool always matches the key it is cached under. See the equivalent note
        // in `faces::engine::build_pool`.
        let (session, cuda) = crate::plugins::onnx::build_session(&path, key.intra_threads, |b| {
            try_register_cuda(b, key.force_cpu)
        })?;
        used_cuda |= cuda;
        sessions.push(session);
    }
    set_active_ep(if used_cuda { ActiveEp::Cuda } else { ActiveEp::Cpu });
    if used_cuda {
        eprintln!(
            "smarttags engine: {} CLIP sessions built on the CUDA execution provider (GPU)",
            key.pool_size
        );
    }
    Ok(Pool::new(sessions))
}

/// Attempt to register the CUDA EP on `builder`; `true` only when it registered. GRACEFUL:
/// compiled out entirely in a non-CUDA build (always CPU), skipped when `force_cpu` is set,
/// and on a missing runtime/driver/GPU it logs and returns `false` so the build still commits
/// on CPU. Never panics, never propagates.
#[cfg(all(feature = "smarttags", feature = "smarttags-cuda"))]
fn try_register_cuda(builder: &mut ort::session::builder::SessionBuilder, force_cpu: bool) -> bool {
    if force_cpu {
        return false;
    }
    use ort::ep::ExecutionProvider;
    let cuda = ort::ep::CUDA::default();
    match cuda.register(builder) {
        Ok(()) => true,
        Err(e) => {
            eprintln!(
                "smarttags engine: CUDA execution provider unavailable ({e}); falling back to \
                 CPU inference. Ensure the CUDA runtime + cuDNN 9 are on the loader path, or set \
                 smarttags.force_cpu to silence this."
            );
            false
        }
    }
}

/// Non-CUDA build: no CUDA EP exists, so this is always CPU.
#[cfg(all(feature = "smarttags", not(feature = "smarttags-cuda")))]
fn try_register_cuda(_builder: &mut ort::session::builder::SessionBuilder, _force_cpu: bool) -> bool {
    false
}

/// Run the CLIP vision encoder on a preprocessed `[1, 3, 224, 224]` NCHW input and return the raw
/// (un-normalized) embedding. Binds the input **positionally** (a CLIP vision tower has one
/// image input, whatever its name) and reads output index 0.
#[cfg(feature = "smarttags")]
fn run_clip(model_path_setting: Option<&str>, input: &[f32]) -> Result<Vec<f32>, EmbedError> {
    use ort::value::Tensor;

    let pool = clip_pool(model_path_setting)?;
    let mut session = pool.checkout().map_err(EmbedError::Inference)?;

    let shape = [1i64, 3, CLIP_SIZE as i64, CLIP_SIZE as i64];
    let tensor = Tensor::from_array((shape, input.to_vec()))
        .map_err(|e| EmbedError::Inference(e.to_string()))?;
    // Positional single-input binding — CLIP vision exports name the input `pixel_values` or
    // `input`; binding by position sidesteps that variation.
    let outs = session
        .run(ort::inputs![tensor])
        .map_err(|e| EmbedError::Inference(e.to_string()))?;

    let (_, data) = outs[0]
        .try_extract_tensor::<f32>()
        .map_err(|e| EmbedError::Inference(e.to_string()))?;
    if data.is_empty() {
        return Err(EmbedError::Inference("model returned an empty embedding".into()));
    }
    Ok(data.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `true` when a usable ONNX Runtime is installed, otherwise `false` with a printed
    /// reason.
    ///
    /// ONNX Runtime is loaded at runtime rather than linked, so it is an optional dependency
    /// a developer — or a CI runner — may not have. Any test that reaches a session must skip
    /// without it, the same way the model-dependent tests already skip on absent models.
    /// Without this the suite fails on a machine that simply chose not to install inference.
    #[cfg(feature = "smarttags")]
    fn onnx_ready_or_skip(what: &str) -> bool {
        match crate::plugins::onnx::ensure_available() {
            Ok(_) => true,
            Err(e) => {
                eprintln!("SKIPPED: {what} — no usable ONNX Runtime ({e})");
                false
            }
        }
    }

    fn solid(w: u32, h: u32, rgb: [u8; 3]) -> RgbImage {
        let mut img = RgbImage::new(w, h);
        for p in img.pixels_mut() {
            *p = image::Rgb(rgb);
        }
        img
    }

    #[test]
    fn preprocess_dims_and_normalization() {
        // A solid mid-grey image: every channel value is deterministic post-normalization.
        let img = solid(300, 100, [128, 128, 128]);
        let t = preprocess(&img);
        let n = (CLIP_SIZE * CLIP_SIZE) as usize;
        assert_eq!(t.len(), 3 * n, "NCHW length for 224×224 RGB");
        // Channel 0, pixel 0: (128/255 - mean0) / std0.
        let expect0 = (128.0f32 / 255.0 - CLIP_MEAN[0]) / CLIP_STD[0];
        assert!((t[0] - expect0).abs() < 1e-5, "got {}, want {}", t[0], expect0);
        // Green channel starts at offset n.
        let expect1 = (128.0f32 / 255.0 - CLIP_MEAN[1]) / CLIP_STD[1];
        assert!((t[n] - expect1).abs() < 1e-5);
    }

    #[test]
    fn l2_normalize_is_unit() {
        let v = vec![3.0f32, 4.0, 0.0, -12.0]; // norm 13
        let u = l2_normalize(&v);
        let norm: f32 = u.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "norm={norm}");
        assert!((cosine(&u, &u) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn l2_normalize_zero_vector_is_safe() {
        let z = vec![0.0f32; 8];
        let u = l2_normalize(&z);
        assert_eq!(u, z, "zero vector normalizes to itself, no NaN");
    }

    /// A missing model file is a typed error, never a panic — the graceful-degradation contract.
    ///
    /// Asserted at the [`build_pool`] layer (not through [`encode_rgb`]) so it needs no pixel
    /// data and stays a pure "does resolving this key fail cleanly" check. This is no longer
    /// load-bearing for isolation from other tests the way it once was: `clip_pool` is now a
    /// [`crate::plugins::onnx::KeyedCache`] keyed on (among other things) the model path, so a
    /// call with a different/missing path builds its own entry rather than being shadowed by
    /// whichever test built the shared pool first.
    #[cfg(feature = "smarttags")]
    #[test]
    fn missing_model_is_graceful_error() {
        // Runs without an ONNX Runtime: `build_pool` resolves and verifies the model before
        // `build_session` performs the runtime preflight, so a missing model is reported on
        // its own terms. This briefly needed a skip guard, when the preflight ran first and
        // the runtime's error would have been asserted against instead of the model's.
        let key = resolve_pool_key(Some("/nonexistent/smarttags/model.onnx"))
            .expect("resolving a path never fails just because the file is missing");
        let err = match build_pool(&key) {
            Err(e) => e,
            Ok(_) => panic!("a missing model must fail cleanly, not build a pool"),
        };
        // The stringified error carries the model manager's "not available" text.
        assert!(
            err.to_lowercase().contains("not available") || err.to_lowercase().contains("no file"),
            "expected a graceful model-missing message, got: {err}"
        );
    }

    // ── PoolKey / configure (issue #18) ─────────────────────────────────────────
    //
    // `clip_pool` fetches through `crate::plugins::onnx::KeyedCache`, whose rebuild-on-key-
    // change policy is proven generically (no ONNX needed) by `plugins::onnx::tests`. What's
    // specific to this module is that `resolve_pool_key` actually reflects the model path
    // setting and `configure`/`configure_force_cpu` — that's what these tests cover.

    /// A different `smarttags.model_path` setting resolves to a different key, and an
    /// unset/blank setting resolves to the pinned default's path both times (so re-reading an
    /// unchanged blank setting does not spuriously invalidate the cache).
    #[cfg(feature = "smarttags")]
    #[test]
    fn resolve_pool_key_reflects_the_model_path_setting() {
        let default_a = resolve_pool_key(None).unwrap();
        let default_b = resolve_pool_key(Some("   ")).unwrap();
        assert_eq!(default_a, default_b, "unset and blank must resolve identically");
        assert!(!default_a.custom);

        let custom = resolve_pool_key(Some("/tmp/some-other-clip.onnx")).unwrap();
        assert_ne!(default_a, custom, "a custom path must change the key");
        assert!(custom.custom);
        assert_eq!(custom.model_path, std::path::PathBuf::from("/tmp/some-other-clip.onnx"));
    }

    /// `configure`/`configure_force_cpu` change the pool_size/intra_threads/force_cpu fields of
    /// the key `resolve_pool_key` produces — the mechanism that makes a settings change rebuild
    /// the pool instead of being silently ignored until restart. A single test, not several, so
    /// it does not race itself over the process-global atomics; no other test in this binary
    /// touches `POOL_SIZE`/`INTRA_THREADS`/`FORCE_CPU` (real pools are only ever built by the
    /// synthetic/real-model tests below, which don't call `configure`).
    #[cfg(feature = "smarttags")]
    #[test]
    fn configure_calls_change_the_resolved_pool_key() {
        configure(2, 3);
        configure_force_cpu(false);
        let a = resolve_pool_key(None).unwrap();
        assert_eq!(a.pool_size, 2);
        assert_eq!(a.intra_threads, 3);
        assert!(!a.force_cpu);

        configure(5, 1);
        configure_force_cpu(true);
        let b = resolve_pool_key(None).unwrap();
        assert_ne!(a, b, "changing pool_size/intra_threads/force_cpu must change the key");
        assert_eq!(b.pool_size, 5);
        assert_eq!(b.intra_threads, 1);
        assert!(b.force_cpu);

        // Zero is clamped to 1 — a degenerate resolved plan must still make progress.
        configure(0, 0);
        let c = resolve_pool_key(None).unwrap();
        assert_eq!(c.pool_size, 1);
        assert_eq!(c.intra_threads, 1);

        // Restore the responsive defaults: these atomics are process-global, so leaving
        // `force_cpu` at `true` would silently short-circuit CUDA registration for
        // `reconfiguring_between_calls_rebuilds_without_breaking_encoding` below if it runs
        // later in the same binary — confirmed by running this file's tests single-threaded
        // before this reset existed: the CUDA-unavailable fallback message never printed,
        // because this test's leftover `force_cpu = true` short-circuited it silently.
        configure(1, 2);
        configure_force_cpu(false);
    }

    /// `true` when the pinned default CLIP model is present and verified on disk (already
    /// downloaded, e.g. from an earlier in-app `smarttags_download_model` run), else `false`
    /// with a skip message. A model-presence gate layered on top of `onnx_ready_or_skip`'s
    /// runtime gate — mirrors `models_ready_or_skip` in `tests/faces_engine.rs`.
    #[cfg(feature = "smarttags")]
    fn default_model_ready_or_skip(what: &str) -> bool {
        if !onnx_ready_or_skip(what) {
            return false;
        }
        if models::status(None).ready {
            return true;
        }
        eprintln!("SKIPPED: {what} — pinned default CLIP model not downloaded");
        false
    }

    /// End-to-end proof that reconfiguring between calls (issue #18) rebuilds a real ONNX
    /// session pool against the pinned default model rather than silently keeping the old
    /// one. Mirrors
    /// `tests/faces_engine.rs::reconfiguring_between_calls_rebuilds_without_breaking_detection`
    /// — see that test for why this is a liveness/correctness smoke test (does reconfiguring
    /// more than once in one process still work?) rather than a timing or pool-identity
    /// assertion, neither of which the public API supports asserting on directly.
    #[cfg(feature = "smarttags")]
    #[test]
    fn reconfiguring_between_calls_rebuilds_without_breaking_encoding() {
        if !default_model_ready_or_skip(
            "reconfiguring_between_calls_rebuilds_without_breaking_encoding",
        ) {
            return;
        }
        let img = solid(256, 256, [90, 120, 200]);
        for (pool_size, intra_threads) in [(1, 1), (2, 2), (1, 4)] {
            configure(pool_size, intra_threads);
            eprintln!("reconfigure: pool_size={pool_size} intra_threads={intra_threads}");
            let emb = encode_rgb(&img, None).unwrap_or_else(|e| {
                panic!("encode after configure({pool_size},{intra_threads}): {e}")
            });
            let norm: f32 = emb.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((1.0 - norm).abs() < 1e-3, "embedding not unit after reconfigure: |{norm}|");
            assert_eq!(emb.len(), 512, "CLIP ViT-B/32 embedding must be 512-d");
        }
    }

    /// End-to-end session path over a **tiny synthetic ONNX graph** built in-process (no model
    /// download). The graph is `GlobalAveragePool` over the `[1,3,224,224]` input →
    /// `[1,3,1,1]`, so it exercises real preprocessing → `ort` session run → extract →
    /// L2-normalize, and asserts the result is a unit vector of the model's natural dimension.
    ///
    /// Gated behind an env var only for the *real* CLIP model; the synthetic-graph path here
    /// always runs so the production session code is covered without a network dependency.
    #[cfg(feature = "smarttags")]
    #[test]
    fn synthetic_onnx_roundtrip_is_unit_vector() {
        if !onnx_ready_or_skip("synthetic_onnx_roundtrip_is_unit_vector") {
            return;
        }
        let dir = std::env::temp_dir().join(format!("st-onnx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let model = dir.join("tiny.onnx");
        std::fs::write(&model, super::test_support::tiny_gap_onnx()).unwrap();

        // A non-solid image so the three channel averages differ → a meaningful direction.
        let mut img = RgbImage::new(64, 64);
        for (x, _y, p) in img.enumerate_pixels_mut() {
            *p = image::Rgb([(x * 3) as u8, (x * 5) as u8, (x * 7) as u8]);
        }
        let emb = encode_rgb(&img, Some(model.to_str().unwrap())).expect("synthetic embed");
        assert_eq!(emb.len(), 3, "GlobalAveragePool over 3 channels → 3-d embedding");
        let norm: f32 = emb.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "L2-normalized, norm={norm}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Opt-in test against a **real** CLIP model on disk, pointed to by `SMARTTAGS_TEST_MODEL`.
    /// Skipped (passes) when the env var is unset so CI needs no model download; when set it
    /// asserts a 512-d unit vector from the true production path.
    #[cfg(feature = "smarttags")]
    #[test]
    fn real_clip_model_when_env_set() {
        let Ok(path) = std::env::var("SMARTTAGS_TEST_MODEL") else {
            eprintln!(
                "SKIPPED: real_clip_model_when_env_set — SMARTTAGS_TEST_MODEL is unset, so no \
                 real CLIP model was exercised. Point it at a CLIP image-encoder .onnx to run \
                 this against the true production path."
            );
            return;
        };
        if !onnx_ready_or_skip("real_clip_model_when_env_set") {
            return;
        }
        let img = solid(256, 256, [90, 120, 200]);
        let emb = encode_rgb(&img, Some(&path)).expect("real CLIP embed");
        let norm: f32 = emb.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3, "unit vector, norm={norm}");
        assert!(emb.len() >= 128, "CLIP embeddings are ≥128-d, got {}", emb.len());
    }
}

// ── Synthetic-ONNX test support ───────────────────────────────────────────────────
//
// A hand-encoded minimal ONNX `ModelProto` (protobuf) with one `GlobalAveragePool` node mapping a
// `[1,3,224,224]` float input → `[1,3,1,1]` output. Enough for `ort` to load and run so the
// session code path is covered offline; kept in the crate (not behind `cfg(test)` on the mod) so
// the encoder helpers are reusable if a second synthetic test appears.
#[cfg(all(feature = "smarttags", test))]
mod test_support {
    /// Protobuf: write a varint.
    fn put_varint(buf: &mut Vec<u8>, mut v: u64) {
        loop {
            let mut b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                b |= 0x80;
            }
            buf.push(b);
            if v == 0 {
                break;
            }
        }
    }

    /// Protobuf: tag = (field_number << 3) | wire_type.
    fn tag(field: u32, wire: u32) -> u64 {
        ((field as u64) << 3) | wire as u64
    }

    /// Length-delimited field (wire type 2): tag, length, bytes.
    fn put_len_delim(buf: &mut Vec<u8>, field: u32, bytes: &[u8]) {
        put_varint(buf, tag(field, 2));
        put_varint(buf, bytes.len() as u64);
        buf.extend_from_slice(bytes);
    }

    /// Varint field (wire type 0).
    fn put_varint_field(buf: &mut Vec<u8>, field: u32, v: u64) {
        put_varint(buf, tag(field, 0));
        put_varint(buf, v);
    }

    /// A `TensorShapeProto` with the given dims (0 = unknown, encoded as a dim with a value).
    fn tensor_shape(dims: &[i64]) -> Vec<u8> {
        // TensorShapeProto { repeated Dimension dim = 1; } ; Dimension { int64 dim_value = 1; }
        let mut shape = Vec::new();
        for &d in dims {
            let mut dim = Vec::new();
            put_varint_field(&mut dim, 1, d as u64); // dim_value
            put_len_delim(&mut shape, 1, &dim);
        }
        shape
    }

    /// A `TypeProto` for a float tensor of the given shape.
    fn float_tensor_type(dims: &[i64]) -> Vec<u8> {
        // TypeProto { Tensor tensor_type = 1; }
        // Tensor { int32 elem_type = 1; TensorShapeProto shape = 2; }  elem_type FLOAT = 1
        let mut tensor = Vec::new();
        put_varint_field(&mut tensor, 1, 1); // elem_type = FLOAT
        put_len_delim(&mut tensor, 2, &tensor_shape(dims));
        let mut typ = Vec::new();
        put_len_delim(&mut typ, 1, &tensor); // TypeProto.tensor_type
        typ
    }

    /// A `ValueInfoProto` (name + type).
    fn value_info(name: &str, dims: &[i64]) -> Vec<u8> {
        // ValueInfoProto { string name = 1; TypeProto type = 2; }
        let mut vi = Vec::new();
        put_len_delim(&mut vi, 1, name.as_bytes());
        put_len_delim(&mut vi, 2, &float_tensor_type(dims));
        vi
    }

    /// The `NodeProto` for `GlobalAveragePool(input) -> output`.
    fn gap_node(input: &str, output: &str) -> Vec<u8> {
        // NodeProto { repeated string input = 1; repeated string output = 2; string op_type = 4; }
        let mut node = Vec::new();
        put_len_delim(&mut node, 1, input.as_bytes());
        put_len_delim(&mut node, 2, output.as_bytes());
        put_len_delim(&mut node, 4, b"GlobalAveragePool");
        node
    }

    /// Bytes of a minimal valid ONNX `ModelProto` runnable by ONNX Runtime.
    pub fn tiny_gap_onnx() -> Vec<u8> {
        let input_name = "input";
        let output_name = "output";

        // GraphProto {
        //   repeated NodeProto node = 1;
        //   string name = 2;
        //   repeated ValueInfoProto input = 11;
        //   repeated ValueInfoProto output = 12;
        // }
        let mut graph = Vec::new();
        put_len_delim(&mut graph, 1, &gap_node(input_name, output_name)); // node
        put_len_delim(&mut graph, 2, b"tiny"); // graph name
        put_len_delim(&mut graph, 11, &value_info(input_name, &[1, 3, 224, 224])); // input
        put_len_delim(&mut graph, 12, &value_info(output_name, &[1, 3, 1, 1])); // output

        // OperatorSetIdProto { string domain = 1; int64 version = 2; } — default domain, opset 13.
        let mut opset = Vec::new();
        put_len_delim(&mut opset, 1, b""); // domain "" (default)
        put_varint_field(&mut opset, 2, 13); // version

        // ModelProto {
        //   int64 ir_version = 1;
        //   repeated OperatorSetIdProto opset_import = 8;
        //   string producer_name = 2;
        //   GraphProto graph = 7;
        // }
        let mut model = Vec::new();
        put_varint_field(&mut model, 1, 7); // ir_version 7
        put_len_delim(&mut model, 2, b"chairphoto-test"); // producer_name
        put_len_delim(&mut model, 7, &graph); // graph
        put_len_delim(&mut model, 8, &opset); // opset_import
        model
    }
}
