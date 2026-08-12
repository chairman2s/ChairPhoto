//! Face detect + embed core (docs/face-tagging.md, model stack section).
//!
//! Two ONNX models, loaded lazily and cached keyed by the configuration that shapes them —
//! see [`yunet_pool`]/[`auraface_pool`] and [`PoolKey`] — so the graph optimization cost is
//! paid once per distinct configuration, not per face, but a change to pool size, intra-op
//! threads or `faces.force_cpu` still rebuilds rather than being silently ignored (issue #18):
//!
//! - **YuNet 2023mar** (detection): SCRFD-style multi-output ONNX, fixed 640×640 input.
//!   [`detect_faces`] resizes the JPEG to 640×640, runs the net, decodes the per-stride
//!   `cls/obj/bbox/kps` outputs (formula matches OpenCV's `FaceDetectorYN`), applies NMS,
//!   and returns each face's **bbox + 5 landmarks normalized 0–1** relative to the oriented
//!   image, plus a confidence.
//! - **AuraFace-v1** (embedding): [`embed_face`] aligns a face to the canonical ArcFace
//!   112×112 template via a 5-point similarity transform ([`umeyama_similarity`]), runs the
//!   net, and L2-normalizes the 512-d output into a unit vector.
//!
//! The similarity transform and preprocessing dims are pure math with no model dependency,
//! so they are unit-tested offline (see the `tests` module below).

use image::RgbImage;

use super::models::{self, ModelError};

#[cfg(feature = "faces")]
use std::sync::{Arc, Condvar, Mutex};

/// The intra-op thread count each ONNX session is configured with, and the number of sessions
/// per model in the pool. Set by [`configure`] (from the indexer, off the resolved
/// [`super::indexer::IndexingPlan`]). Read into a [`PoolKey`] on every pool fetch — see
/// [`current_pool_key`] — so a value change here rebuilds the pool the next time one is
/// needed rather than being frozen at whatever the first caller saw. A session built before
/// `configure` ever runs — e.g. a single loupe/inspector detect call — uses the responsive
/// defaults below (matching the old `background` throttle: 1 session, 2 intra-op threads).
#[cfg(feature = "faces")]
static POOL_SIZE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
#[cfg(feature = "faces")]
static INTRA_THREADS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(2);

/// When `true`, the CUDA execution provider is **never** registered even in a `faces-cuda`
/// build — every session runs on CPU. Set by [`configure_force_cpu`] (from the indexer, off
/// the `faces.force_cpu` setting). Like [`POOL_SIZE`]/[`INTRA_THREADS`], part of [`PoolKey`],
/// so flipping it rebuilds the pool on the next fetch instead of only ever taking effect on
/// the very first one. Default `false`: a `faces-cuda` build tries GPU first and falls back to
/// CPU on its own if CUDA is unavailable. In a plain `faces` (no-CUDA) build this flag has no
/// effect — there is no CUDA EP to register.
#[cfg(feature = "faces")]
static FORCE_CPU: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Which execution provider a built pool actually landed on. Set once when the pool is built
/// (CUDA if it registered, else CPU), so the indexer/UI can honestly report where inference
/// ran instead of guessing. `Unbuilt` until the first pool is constructed.
#[cfg(feature = "faces")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveEp {
    /// No pool built yet (no inference has run).
    Unbuilt,
    /// Sessions run on CPU (plain `faces` build, forced CPU, or CUDA registration failed).
    Cpu,
    /// Sessions run on the CUDA execution provider (GPU).
    Cuda,
}

#[cfg(feature = "faces")]
static ACTIVE_EP: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

#[cfg(feature = "faces")]
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
/// session has been built. Lets the indexer log/report GPU-vs-CPU honestly after a run.
#[cfg(feature = "faces")]
pub fn active_ep() -> ActiveEp {
    use std::sync::atomic::Ordering;
    match ACTIVE_EP.load(Ordering::Relaxed) {
        2 => ActiveEp::Cuda,
        1 => ActiveEp::Cpu,
        _ => ActiveEp::Unbuilt,
    }
}

/// Configure the session pool size and per-session intra-op thread count for the *next* pool
/// fetch. Call this before an index run (from the indexer, off the resolved
/// [`super::indexer::IndexingPlan`]). Unlike the old `OnceLock`-backed pool, this is never a
/// no-op: the pool is keyed by [`PoolKey`] (see [`yunet_pool`]/[`auraface_pool`]), so a value
/// change here rebuilds the pool the next time `detect_faces`/`embed_face` runs. Single-call
/// callers that never configure fall back to the responsive defaults.
///
/// `pool_size` = concurrent inference slots per model; `intra_threads` = ONNX intra-op
/// threads per session.
///
/// Because [`POOL_SIZE`]/[`INTRA_THREADS`] are process-global, two index runs of the *same*
/// family overlapping (a second `faces_index_photos` call trips the first job's abort flag —
/// see `commands::faces::begin_faces_index_job`) can move these values out from under the
/// first run's still-in-flight chunk. That chunk's checkout already holds its own `Arc` clone
/// of the pool it fetched (see [`onnx::KeyedCache`](crate::plugins::onnx::KeyedCache)), so it
/// finishes correctly on the pool it started with; only its *next* fetch — after it notices
/// the abort flag, at most one chunk later — could observe the newer job's configuration. A
/// single run's own workers never see this, because `configure`/[`configure_force_cpu`] are
/// each called exactly once, before that run's parallel loop starts.
#[cfg(feature = "faces")]
pub fn configure(pool_size: usize, intra_threads: usize) {
    use std::sync::atomic::Ordering;
    POOL_SIZE.store(pool_size.max(1), Ordering::Relaxed);
    INTRA_THREADS.store(intra_threads.max(1), Ordering::Relaxed);
}

/// Force every session onto CPU even in a `faces-cuda` build (setting `faces.force_cpu`).
/// Same lifecycle and cross-job caveat as [`configure`]: it rebuilds the pool on the next
/// fetch rather than only ever taking effect on the first one. In a non-CUDA build this is
/// inert. Useful when the GPU is needed for other work, when a driver is flaky, or to A/B
/// GPU-vs-CPU output.
#[cfg(feature = "faces")]
pub fn configure_force_cpu(force: bool) {
    use std::sync::atomic::Ordering;
    FORCE_CPU.store(force, Ordering::Relaxed);
}

/// Everything that determines what a built session pool looks like. Two fetches with an equal
/// key reuse the same pool; any difference rebuilds. Deliberately narrow: it captures exactly
/// the levers [`configure`]/[`configure_force_cpu`] expose (issue #18 — "key the pools by the
/// configuration that affects them"), not the model itself (the pool key is bundled together
/// with the model spec by [`yunet_pool`]/[`auraface_pool`] each owning a distinct
/// [`crate::plugins::onnx::KeyedCache`], so a key never needs to disambiguate YuNet vs
/// AuraFace).
///
/// `force_cpu` doubles as "execution provider mode" here: today's engine has exactly two
/// modes — CPU-only (`force_cpu = true`, or a plain non-CUDA build where it is moot) and
/// "try CUDA, fall back to CPU" (`force_cpu = false` in a `faces-cuda` build) — so the one
/// boolean fully captures it. A future explicit provider setting would extend this struct,
/// not replace it.
#[cfg(feature = "faces")]
#[derive(Debug, Clone, PartialEq, Eq)]
struct PoolKey {
    pool_size: usize,
    intra_threads: usize,
    force_cpu: bool,
}

/// Snapshot [`POOL_SIZE`]/[`INTRA_THREADS`]/[`FORCE_CPU`] into a [`PoolKey`]. Called on every
/// pool fetch (not just the first), which is what makes a `configure`/`configure_force_cpu`
/// call before the next fetch take effect instead of being ignored.
#[cfg(feature = "faces")]
fn current_pool_key() -> PoolKey {
    use std::sync::atomic::Ordering;
    PoolKey {
        pool_size: POOL_SIZE.load(Ordering::Relaxed).max(1),
        intra_threads: INTRA_THREADS.load(Ordering::Relaxed).max(1),
        force_cpu: FORCE_CPU.load(Ordering::Relaxed),
    }
}

/// The 512-d embedding AuraFace produces.
pub const EMBED_DIM: usize = 512;
/// YuNet's fixed square input side.
const DET_SIZE: u32 = 640;
/// AuraFace's fixed square input side.
const ALIGN_SIZE: u32 = 112;
/// Feature-map strides YuNet emits proposals at.
const STRIDES: [u32; 3] = [8, 16, 32];

/// Canonical ArcFace 5-point landmark template on the 112×112 aligned crop, in pixels:
/// right-eye, left-eye, nose, right-mouth, left-mouth. This is the standard InsightFace
/// `arcface_dst` template AuraFace was trained against.
pub const ARCFACE_TEMPLATE: [(f32, f32); 5] = [
    (38.2946, 51.6963),
    (73.5318, 51.5014),
    (56.0252, 71.7366),
    (41.5493, 92.3655),
    (70.7299, 92.2041),
];

/// A detected face with geometry normalized 0–1 relative to the oriented source image.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectedFace {
    /// `(x, y, w, h)` — top-left + size, each in 0–1 of the image dimensions.
    pub bbox: (f32, f32, f32, f32),
    /// 5 landmarks `(x, y)`, each in 0–1: right-eye, left-eye, nose, right-mouth, left-mouth.
    pub landmarks: [(f32, f32); 5],
    /// Detection confidence in 0–1 (`sqrt(cls * obj)`).
    pub confidence: f32,
}

/// Failure modes of detection/embedding. Model absence is surfaced as [`EngineError::Model`]
/// so the caller can prompt a download rather than crashing.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error("image decode failed: {0}")]
    Decode(String),
    #[error("inference failed: {0}")]
    Inference(String),
}

// ── Detection ─────────────────────────────────────────────────────────────────

/// Detect faces in a JPEG (or any format the `image` crate reads). Returns bbox + 5
/// landmarks + confidence, all geometry normalized to 0–1 of the image dimensions.
///
/// `conf_threshold` (≈0.6–0.7 works well) and `nms_iou` (≈0.3) tune the proposal filter.
#[cfg(feature = "faces")]
pub fn detect_faces(
    image_bytes: &[u8],
    conf_threshold: f32,
    nms_iou: f32,
) -> Result<Vec<DetectedFace>, EngineError> {
    let img = image::load_from_memory(image_bytes)
        .map_err(|e| EngineError::Decode(e.to_string()))?
        .to_rgb8();
    detect_faces_rgb(&img, conf_threshold, nms_iou)
}

/// As [`detect_faces`] but from an already-decoded RGB image (avoids a re-decode when the
/// caller already holds pixels, e.g. the indexing job's preview).
#[cfg(feature = "faces")]
pub fn detect_faces_rgb(
    img: &RgbImage,
    conf_threshold: f32,
    nms_iou: f32,
) -> Result<Vec<DetectedFace>, EngineError> {
    let (w, h) = (img.width() as f32, img.height() as f32);
    // YuNet has a fixed 640×640 input; OpenCV's FaceDetectorYN resizes directly (no
    // letterbox) and scales detections back by the independent x/y ratios, so we match that.
    let resized = image::imageops::resize(
        img,
        DET_SIZE,
        DET_SIZE,
        image::imageops::FilterType::Triangle,
    );
    let sx = w / DET_SIZE as f32;
    let sy = h / DET_SIZE as f32;

    // NCHW f32, raw 0–255 (YuNet ingests un-normalized pixels), channel order RGB.
    let n = (DET_SIZE * DET_SIZE) as usize;
    let mut input = vec![0f32; 3 * n];
    for (i, px) in resized.pixels().enumerate() {
        input[i] = px[0] as f32;
        input[n + i] = px[1] as f32;
        input[2 * n + i] = px[2] as f32;
    }

    let outputs = run_yunet(&input)?;
    let mut cands = decode_yunet(&outputs, conf_threshold, sx, sy, w, h);
    cands.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap());
    let kept = nms(&cands, nms_iou);
    Ok(kept)
}

/// The 12 raw YuNet outputs (cls/obj/bbox/kps × 3 strides), each as a flat f32 vec.
struct YunetOutputs {
    cls: [Vec<f32>; 3],
    obj: [Vec<f32>; 3],
    bbox: [Vec<f32>; 3],
    kps: [Vec<f32>; 3],
}

/// A small fixed-size pool of interchangeable resources (ONNX sessions). Workers **check
/// out** a distinct item, use it, and the [`Checkout`] guard returns it on drop. Under
/// contention each concurrent caller holds a different item — so N rayon workers run N
/// inferences in parallel with no per-model mutex bottleneck (the old single-flight).
///
/// A blocking `Condvar` free-list rather than a lock-free structure: pools are tiny (a
/// handful of sessions) and checkouts are held for the whole inference, so contention is
/// coarse and a parked thread is cheap. Generic over the item type so the checkout logic is
/// unit-testable without real ONNX sessions.
#[cfg(feature = "faces")]
struct Pool<T> {
    free: Mutex<Vec<T>>,
    available: Condvar,
}

#[cfg(feature = "faces")]
impl<T> Pool<T> {
    fn new(items: Vec<T>) -> Self {
        Pool {
            free: Mutex::new(items),
            available: Condvar::new(),
        }
    }

    /// Check out one item, blocking until one is free. The returned guard returns it to the
    /// pool on drop. Errors only if the internal mutex is poisoned.
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
#[cfg(feature = "faces")]
struct Checkout<'a, T> {
    pool: &'a Pool<T>,
    item: Option<T>,
}

#[cfg(feature = "faces")]
impl<T> std::ops::Deref for Checkout<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.item.as_ref().expect("item present until drop")
    }
}

#[cfg(feature = "faces")]
impl<T> std::ops::DerefMut for Checkout<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.item.as_mut().expect("item present until drop")
    }
}

#[cfg(feature = "faces")]
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

/// Process-global keyed caches, one per model — see [`PoolKey`] and
/// [`crate::plugins::onnx::KeyedCache`]. Separate slots (rather than one cache keyed by model
/// too) because each model has its own fixed spec; the pool *configuration* is what's shared.
#[cfg(feature = "faces")]
static YUNET_CACHE: crate::plugins::onnx::KeyedCache<PoolKey, Pool<ort::session::Session>> =
    crate::plugins::onnx::KeyedCache::new();
#[cfg(feature = "faces")]
static AURAFACE_CACHE: crate::plugins::onnx::KeyedCache<PoolKey, Pool<ort::session::Session>> =
    crate::plugins::onnx::KeyedCache::new();

/// Fetch (building or rebuilding as needed) the YuNet session pool for the current
/// [`current_pool_key`]. A `Session` is `Send` but its `run` takes `&mut self`, so several are
/// built and lent out from a [`Pool`]: up to `pool_size` inferences of this model run at once.
/// Each session gets `intra_threads` intra-op threads. Building streams a full SHA-256 verify
/// of the model before loading it, guaranteeing on-disk integrity before it reaches ONNX — paid
/// again on every rebuild, which is the cost of a config change actually taking effect.
#[cfg(feature = "faces")]
fn yunet_pool() -> Result<Arc<Pool<ort::session::Session>>, EngineError> {
    let key = current_pool_key();
    YUNET_CACHE
        .get_or_build(key.clone(), || build_pool(&models::YUNET, &key))
        .map_err(EngineError::Inference)
}

/// As [`yunet_pool`], for the AuraFace embedding model.
#[cfg(feature = "faces")]
fn auraface_pool() -> Result<Arc<Pool<ort::session::Session>>, EngineError> {
    let key = current_pool_key();
    AURAFACE_CACHE
        .get_or_build(key.clone(), || build_pool(&models::AURAFACE, &key))
        .map_err(EngineError::Inference)
}

/// Verify a model on disk (full checksum) and build a [`Pool`] of `key.pool_size` ONNX
/// sessions, each configured with `key.intra_threads` intra-op threads and forced to CPU when
/// `key.force_cpu`. Errors are stringified so [`crate::plugins::onnx::KeyedCache`] can pass
/// them through without requiring `Clone` (`ModelError`/session errors aren't `Clone`); the
/// caller re-wraps them into [`EngineError::Inference`]. A missing model surfaces its
/// `ModelError` text (e.g. "model yunet not downloaded"), so the caller can still prompt a
/// download rather than crash, and is reported before any session is built — so it needs no
/// ONNX Runtime to diagnose. A missing *runtime* surfaces from
/// [`crate::plugins::onnx::build_session`], which every session goes through precisely so that
/// absence is reported instead of hung on. A build failure is not cached by `KeyedCache`, so a
/// later retry (e.g. once the model finishes downloading) gets a fresh attempt.
#[cfg(feature = "faces")]
fn build_pool(spec: &models::ModelSpec, key: &PoolKey) -> Result<Pool<ort::session::Session>, String> {
    let path = models::verify(spec).map_err(|e| e.to_string())?;
    let mut sessions = Vec::with_capacity(key.pool_size);
    let mut used_cuda = false;
    for _ in 0..key.pool_size {
        // `build_session` runs the ONNX Runtime preflight; a session must never be built
        // directly, or a missing runtime hangs instead of erroring. See plugins::onnx.
        //
        // GPU is tried first (only in a `faces-cuda` build, and only if not forced to CPU).
        // On any failure `try_register_cuda` logs and leaves the builder on CPU — never an
        // error, never a crash. `key.force_cpu` — not the live `FORCE_CPU` atomic — decides
        // it, so the built pool always matches the key it was cached under even if a
        // concurrent `configure_force_cpu` call is landing at the same moment.
        let (session, cuda) = crate::plugins::onnx::build_session(&path, key.intra_threads, |b| {
            try_register_cuda(b, key.force_cpu)
        })?;
        used_cuda |= cuda;
        sessions.push(session);
    }
    // Record where this pool landed. `active_ep()` reflects whichever pool (yunet or
    // auraface, any key) was built most recently — a best-effort "where did we last run"
    // signal for the indexer/UI, not a per-call guarantee under concurrent rebuilds.
    set_active_ep(if used_cuda { ActiveEp::Cuda } else { ActiveEp::Cpu });
    if used_cuda {
        eprintln!(
            "faces engine: {} sessions built on the CUDA execution provider (GPU)",
            spec.key
        );
    }
    Ok(Pool::new(sessions))
}

/// Attempt to register the CUDA execution provider on `builder`, returning `true` only when it
/// registered successfully. GRACEFUL by construction:
///
/// - In a plain `faces` (non-CUDA) build this is compiled out entirely — always `false`, CPU.
/// - When `force_cpu` is set (the pool's [`PoolKey::force_cpu`], not the live `FORCE_CPU`
///   atomic — see the call site in [`build_pool`]) it skips registration — CPU.
/// - When the CUDA runtime/driver/cuDNN or a GPU is missing, `register` returns an `Err`; we
///   log it clearly and return `false`, so the caller's `commit_from_file` still succeeds on
///   CPU. Never panics, never propagates the error.
///
/// We register the EP *explicitly* (rather than `with_execution_providers`, which swallows the
/// outcome) precisely so we can detect failure and report which provider actually took effect.
#[cfg(all(feature = "faces", feature = "faces-cuda"))]
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
                "faces engine: CUDA execution provider unavailable ({e}); \
                 falling back to CPU inference. Ensure the CUDA runtime + cuDNN 9 are on the \
                 loader path, or set faces.force_cpu to silence this."
            );
            false
        }
    }
}

/// Non-CUDA build: no CUDA EP exists, so this is always CPU. Kept as a same-signature stub so
/// [`build_pool`] needs no `cfg` at the call site.
#[cfg(all(feature = "faces", not(feature = "faces-cuda")))]
fn try_register_cuda(_builder: &mut ort::session::builder::SessionBuilder, _force_cpu: bool) -> bool {
    false
}

#[cfg(feature = "faces")]
fn run_yunet(input: &[f32]) -> Result<YunetOutputs, EngineError> {
    use ort::inputs;
    use ort::value::Tensor;

    let pool = yunet_pool()?;
    let mut session = pool.checkout().map_err(EngineError::Inference)?;

    let shape = [1i64, 3, DET_SIZE as i64, DET_SIZE as i64];
    let tensor = Tensor::from_array((shape, input.to_vec()))
        .map_err(|e| EngineError::Inference(e.to_string()))?;
    let outs = session
        .run(inputs!["input" => tensor])
        .map_err(|e| EngineError::Inference(e.to_string()))?;

    let extract = |name: &str| -> Result<Vec<f32>, EngineError> {
        let (_, data) = outs[name]
            .try_extract_tensor::<f32>()
            .map_err(|e| EngineError::Inference(format!("{name}: {e}")))?;
        Ok(data.to_vec())
    };

    Ok(YunetOutputs {
        cls: [extract("cls_8")?, extract("cls_16")?, extract("cls_32")?],
        obj: [extract("obj_8")?, extract("obj_16")?, extract("obj_32")?],
        bbox: [extract("bbox_8")?, extract("bbox_16")?, extract("bbox_32")?],
        kps: [extract("kps_8")?, extract("kps_16")?, extract("kps_32")?],
    })
}

/// Decode YuNet's per-stride proposals into normalized [`DetectedFace`]s (pre-NMS).
///
/// Matches OpenCV's `FaceDetectorYN` decode exactly (verified against it at implementation
/// time): for anchor `(r, c)` at stride `s`,
/// `cx = (c + bbox[0]) * s`, `cy = (r + bbox[1]) * s`,
/// `w = exp(bbox[2]) * s`, `h = exp(bbox[3]) * s`; landmark
/// `x = (kps[2k] + c) * s`, `y = (kps[2k+1] + r) * s`; score `= sqrt(cls * obj)`.
/// Coordinates are then mapped from the 640×640 net space back to source pixels via
/// `(sx, sy)` and finally normalized by `(img_w, img_h)`.
#[cfg(feature = "faces")]
fn decode_yunet(
    o: &YunetOutputs,
    conf_threshold: f32,
    sx: f32,
    sy: f32,
    img_w: f32,
    img_h: f32,
) -> Vec<DetectedFace> {
    let mut out = Vec::new();
    for (si, &s) in STRIDES.iter().enumerate() {
        let grid = (DET_SIZE / s) as usize;
        let cls = &o.cls[si];
        let obj = &o.obj[si];
        let bbox = &o.bbox[si];
        let kps = &o.kps[si];
        for r in 0..grid {
            for c in 0..grid {
                let idx = r * grid + c;
                let cs = cls[idx].clamp(0.0, 1.0);
                let os = obj[idx].clamp(0.0, 1.0);
                let score = (cs * os).sqrt();
                if score < conf_threshold {
                    continue;
                }
                let sf = s as f32;
                let cx = (c as f32 + bbox[idx * 4]) * sf;
                let cy = (r as f32 + bbox[idx * 4 + 1]) * sf;
                let bw = bbox[idx * 4 + 2].exp() * sf;
                let bh = bbox[idx * 4 + 3].exp() * sf;
                let x1 = (cx - bw / 2.0) * sx;
                let y1 = (cy - bh / 2.0) * sy;
                let pw = bw * sx;
                let ph = bh * sy;

                let mut lms = [(0f32, 0f32); 5];
                for k in 0..5 {
                    let lx = (kps[idx * 10 + 2 * k] + c as f32) * sf * sx;
                    let ly = (kps[idx * 10 + 2 * k + 1] + r as f32) * sf * sy;
                    lms[k] = (lx / img_w, ly / img_h);
                }
                out.push(DetectedFace {
                    bbox: (x1 / img_w, y1 / img_h, pw / img_w, ph / img_h),
                    landmarks: lms,
                    confidence: score,
                });
            }
        }
    }
    out
}

/// Greedy non-maximum suppression over confidence-sorted candidates (bbox in normalized
/// coords; IoU threshold `iou_th`).
fn nms(sorted: &[DetectedFace], iou_th: f32) -> Vec<DetectedFace> {
    let mut keep: Vec<DetectedFace> = Vec::new();
    'cand: for f in sorted {
        for k in &keep {
            if bbox_iou(f.bbox, k.bbox) > iou_th {
                continue 'cand;
            }
        }
        keep.push(f.clone());
    }
    keep
}

/// IoU of two `(x, y, w, h)` boxes.
fn bbox_iou(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> f32 {
    let (ax1, ay1, aw, ah) = a;
    let (bx1, by1, bw, bh) = b;
    let (ax2, ay2) = (ax1 + aw, ay1 + ah);
    let (bx2, by2) = (bx1 + bw, by1 + bh);
    let ix1 = ax1.max(bx1);
    let iy1 = ay1.max(by1);
    let ix2 = ax2.min(bx2);
    let iy2 = ay2.min(by2);
    let iw = (ix2 - ix1).max(0.0);
    let ih = (iy2 - iy1).max(0.0);
    let inter = iw * ih;
    let uni = aw * ah + bw * bh - inter;
    if uni <= 0.0 {
        0.0
    } else {
        inter / uni
    }
}

// ── Embedding ─────────────────────────────────────────────────────────────────

/// Align a face and embed it into a 512-d L2-normalized vector.
///
/// `landmarks` are the 5 landmarks in **0–1** normalized coords (as [`DetectedFace`] carries
/// them). They are denormalized against `img`, a similarity transform is fit to the ArcFace
/// template ([`umeyama_similarity`]), the face is warped into a 112×112 crop, run through
/// AuraFace, and the output is unit-normalized.
#[cfg(feature = "faces")]
pub fn embed_face(
    img: &RgbImage,
    landmarks: &[(f32, f32); 5],
) -> Result<[f32; EMBED_DIM], EngineError> {
    let (w, h) = (img.width() as f32, img.height() as f32);
    let src: [(f32, f32); 5] = std::array::from_fn(|i| (landmarks[i].0 * w, landmarks[i].1 * h));
    let m = umeyama_similarity(&src, &ARCFACE_TEMPLATE);
    let aligned = warp_affine_112(img, &m);

    // AuraFace preprocessing: NCHW, RGB, scaled to [-1, 1] via (px - 127.5) / 127.5.
    let n = (ALIGN_SIZE * ALIGN_SIZE) as usize;
    let mut input = vec![0f32; 3 * n];
    for (i, px) in aligned.pixels().enumerate() {
        input[i] = (px[0] as f32 - 127.5) / 127.5;
        input[n + i] = (px[1] as f32 - 127.5) / 127.5;
        input[2 * n + i] = (px[2] as f32 - 127.5) / 127.5;
    }

    let raw = run_auraface(&input)?;
    Ok(l2_normalize(&raw))
}

#[cfg(feature = "faces")]
fn run_auraface(input: &[f32]) -> Result<[f32; EMBED_DIM], EngineError> {
    use ort::inputs;
    use ort::value::Tensor;

    let pool = auraface_pool()?;
    let mut session = pool.checkout().map_err(EngineError::Inference)?;

    let shape = [1i64, 3, ALIGN_SIZE as i64, ALIGN_SIZE as i64];
    let tensor = Tensor::from_array((shape, input.to_vec()))
        .map_err(|e| EngineError::Inference(e.to_string()))?;
    let outs = session
        .run(inputs!["data" => tensor])
        .map_err(|e| EngineError::Inference(e.to_string()))?;

    // The model has a single output ("1333") of shape [1, 512]; take it by index.
    let (_, data) = outs[0]
        .try_extract_tensor::<f32>()
        .map_err(|e| EngineError::Inference(e.to_string()))?;
    if data.len() != EMBED_DIM {
        return Err(EngineError::Inference(format!(
            "expected {EMBED_DIM}-d embedding, got {}",
            data.len()
        )));
    }
    let mut v = [0f32; EMBED_DIM];
    v.copy_from_slice(data);
    Ok(v)
}

/// L2-normalize into a unit vector (no-op-safe on a zero vector).
fn l2_normalize(v: &[f32; EMBED_DIM]) -> [f32; EMBED_DIM] {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    let inv = if norm > 0.0 { 1.0 / norm } else { 0.0 };
    std::array::from_fn(|i| v[i] * inv)
}

/// Cosine similarity of two unit (or arbitrary) vectors — used by the matching/tests.
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

// ── Similarity transform (Umeyama) ──────────────────────────────────────────────

/// A 2×3 affine matrix `[[a, b, tx], [c, d, ty]]` mapping source → destination:
/// `x' = a·x + b·y + tx`, `y' = c·x + d·y + ty`.
pub type Affine = [[f32; 3]; 2];

/// Least-squares similarity (rotation + uniform scale + translation, no shear/reflection)
/// mapping the `src` points onto the `dst` points — the Umeyama (1991) estimator, which is
/// exactly what InsightFace's `norm_crop` uses to align a face to the ArcFace template.
///
/// Pure math, no model dependency (unit-tested offline).
pub fn umeyama_similarity(src: &[(f32, f32); 5], dst: &[(f32, f32); 5]) -> Affine {
    let n = src.len() as f64;
    // Means.
    let (mut sxm, mut sym, mut dxm, mut dym) = (0.0f64, 0.0, 0.0, 0.0);
    for i in 0..src.len() {
        sxm += src[i].0 as f64;
        sym += src[i].1 as f64;
        dxm += dst[i].0 as f64;
        dym += dst[i].1 as f64;
    }
    sxm /= n;
    sym /= n;
    dxm /= n;
    dym /= n;

    // Variance of src, and 2×2 covariance Σ = (1/n) Σ (dst-μd)(src-μs)ᵀ.
    let mut var_src = 0.0f64;
    let (mut c00, mut c01, mut c10, mut c11) = (0.0f64, 0.0, 0.0, 0.0);
    for i in 0..src.len() {
        let sx = src[i].0 as f64 - sxm;
        let sy = src[i].1 as f64 - sym;
        let dx = dst[i].0 as f64 - dxm;
        let dy = dst[i].1 as f64 - dym;
        var_src += sx * sx + sy * sy;
        c00 += dx * sx;
        c01 += dx * sy;
        c10 += dy * sx;
        c11 += dy * sy;
    }
    var_src /= n;
    c00 /= n;
    c01 /= n;
    c10 /= n;
    c11 /= n;

    // SVD of the 2×2 covariance Σ = U·S·Vᵀ (closed form).
    let (u, s, vt) = svd2x2(c00, c01, c10, c11);

    // R = U·D·Vᵀ, with D correcting reflection so det(R) = +1.
    let det = (u[0][0] * u[1][1] - u[0][1] * u[1][0]) * (vt[0][0] * vt[1][1] - vt[0][1] * vt[1][0]);
    let d = if det < 0.0 { [1.0, -1.0] } else { [1.0, 1.0] };
    // R = U · diag(d) · Vᵀ
    let ud = [
        [u[0][0] * d[0], u[0][1] * d[1]],
        [u[1][0] * d[0], u[1][1] * d[1]],
    ];
    let r = [
        [
            ud[0][0] * vt[0][0] + ud[0][1] * vt[1][0],
            ud[0][0] * vt[0][1] + ud[0][1] * vt[1][1],
        ],
        [
            ud[1][0] * vt[0][0] + ud[1][1] * vt[1][0],
            ud[1][0] * vt[0][1] + ud[1][1] * vt[1][1],
        ],
    ];

    // Uniform scale c = trace(S · D) / var_src.
    let scale = if var_src > 0.0 {
        (s[0] * d[0] + s[1] * d[1]) / var_src
    } else {
        1.0
    };

    // Translation t = μd − c·R·μs.
    let a = scale * r[0][0];
    let b = scale * r[0][1];
    let cc = scale * r[1][0];
    let dd = scale * r[1][1];
    let tx = dxm - (a * sxm + b * sym);
    let ty = dym - (cc * sxm + dd * sym);

    [
        [a as f32, b as f32, tx as f32],
        [cc as f32, dd as f32, ty as f32],
    ]
}

/// Closed-form SVD of a 2×2 matrix `[[a, b], [c, d]] = U · diag(s) · Vᵀ`, returning
/// `(U, s, Vᵀ)`. Used only inside [`umeyama_similarity`].
fn svd2x2(a: f64, b: f64, c: f64, d: f64) -> ([[f64; 2]; 2], [f64; 2], [[f64; 2]; 2]) {
    // Eigen-decompose AᵀA for V and singular values.
    let ata00 = a * a + c * c;
    let ata01 = a * b + c * d;
    let ata11 = b * b + d * d;
    let (v, l) = sym_eig2x2(ata00, ata01, ata11);
    let s0 = l[0].max(0.0).sqrt();
    let s1 = l[1].max(0.0).sqrt();

    // U columns = A·v_i / s_i (fall back to an orthonormal completion when s≈0).
    let av = [
        [a * v[0][0] + b * v[1][0], a * v[0][1] + b * v[1][1]],
        [c * v[0][0] + d * v[1][0], c * v[0][1] + d * v[1][1]],
    ];
    let mut u = [[0.0f64; 2]; 2];
    if s0 > 1e-12 {
        u[0][0] = av[0][0] / s0;
        u[1][0] = av[1][0] / s0;
    } else {
        u[0][0] = 1.0;
        u[1][0] = 0.0;
    }
    if s1 > 1e-12 {
        u[0][1] = av[0][1] / s1;
        u[1][1] = av[1][1] / s1;
    } else {
        // Orthogonal to column 0.
        u[0][1] = -u[1][0];
        u[1][1] = u[0][0];
    }
    let s = [s0, s1];
    let vt = [[v[0][0], v[1][0]], [v[0][1], v[1][1]]];
    (u, s, vt)
}

/// Eigen-decomposition of a symmetric 2×2 `[[a, b], [b, c]]`, returning eigenvectors as
/// columns of `V` and eigenvalues `[λ0, λ1]` with `λ0 ≥ λ1`.
fn sym_eig2x2(a: f64, b: f64, c: f64) -> ([[f64; 2]; 2], [f64; 2]) {
    let tr = a + c;
    let det = a * c - b * b;
    let disc = ((tr * tr) / 4.0 - det).max(0.0).sqrt();
    let l0 = tr / 2.0 + disc;
    let l1 = tr / 2.0 - disc;
    // Eigenvector for l0.
    let (mut e0x, mut e0y);
    if b.abs() > 1e-12 {
        e0x = l0 - c;
        e0y = b;
    } else if a >= c {
        e0x = 1.0;
        e0y = 0.0;
    } else {
        e0x = 0.0;
        e0y = 1.0;
    }
    let n0 = (e0x * e0x + e0y * e0y).sqrt();
    if n0 > 0.0 {
        e0x /= n0;
        e0y /= n0;
    }
    // Second eigenvector is orthogonal.
    let (e1x, e1y) = (-e0y, e0x);
    ([[e0x, e1x], [e0y, e1y]], [l0, l1])
}

/// Warp `img` into a 112×112 RGB crop using the affine `m` (source → 112×112 dest), sampling
/// with bilinear interpolation via the inverse map. Out-of-bounds samples are black.
pub fn warp_affine_112(img: &RgbImage, m: &Affine) -> RgbImage {
    let inv = invert_affine(m);
    let (w, h) = (img.width() as i32, img.height() as i32);
    let mut out = RgbImage::new(ALIGN_SIZE, ALIGN_SIZE);
    for dy in 0..ALIGN_SIZE {
        for dx in 0..ALIGN_SIZE {
            // Inverse-map dest pixel → source coordinate.
            let sx = inv[0][0] * dx as f32 + inv[0][1] * dy as f32 + inv[0][2];
            let sy = inv[1][0] * dx as f32 + inv[1][1] * dy as f32 + inv[1][2];
            let px = bilinear(img, sx, sy, w, h);
            out.put_pixel(dx, dy, px);
        }
    }
    out
}

/// Bilinear sample of an RGB image at fractional `(x, y)`; black outside the image.
fn bilinear(img: &RgbImage, x: f32, y: f32, w: i32, h: i32) -> image::Rgb<u8> {
    if x < 0.0 || y < 0.0 || x > (w - 1) as f32 || y > (h - 1) as f32 {
        return image::Rgb([0, 0, 0]);
    }
    let x0 = x.floor() as i32;
    let y0 = y.floor() as i32;
    let x1 = (x0 + 1).min(w - 1);
    let y1 = (y0 + 1).min(h - 1);
    let fx = x - x0 as f32;
    let fy = y - y0 as f32;
    let p = |xx: i32, yy: i32| img.get_pixel(xx as u32, yy as u32);
    let p00 = p(x0, y0);
    let p10 = p(x1, y0);
    let p01 = p(x0, y1);
    let p11 = p(x1, y1);
    let mut out = [0u8; 3];
    for ch in 0..3 {
        let top = p00[ch] as f32 * (1.0 - fx) + p10[ch] as f32 * fx;
        let bot = p01[ch] as f32 * (1.0 - fx) + p11[ch] as f32 * fx;
        out[ch] = (top * (1.0 - fy) + bot * fy).round().clamp(0.0, 255.0) as u8;
    }
    image::Rgb(out)
}

/// Invert a 2×3 affine (assumes it is invertible — a similarity transform always is).
fn invert_affine(m: &Affine) -> Affine {
    let (a, b, tx) = (m[0][0], m[0][1], m[0][2]);
    let (c, d, ty) = (m[1][0], m[1][1], m[1][2]);
    let det = a * d - b * c;
    let inv_det = if det.abs() > 1e-12 { 1.0 / det } else { 0.0 };
    let ia = d * inv_det;
    let ib = -b * inv_det;
    let ic = -c * inv_det;
    let id = a * inv_det;
    let itx = -(ia * tx + ib * ty);
    let ity = -(ic * tx + id * ty);
    [[ia, ib, itx], [ic, id, ity]]
}

/// Apply a 2×3 affine to a point.
#[cfg(test)]
pub fn apply_affine(m: &Affine, p: (f32, f32)) -> (f32, f32) {
    (
        m[0][0] * p.0 + m[0][1] * p.1 + m[0][2],
        m[1][0] * p.0 + m[1][1] * p.1 + m[1][2],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The similarity transform fit to a template maps that template's points onto the
    /// targets. Here src is a rotated+scaled+translated copy of the ArcFace template, so
    /// the recovered transform should map src back onto the template near-exactly.
    #[test]
    fn similarity_maps_points_onto_targets() {
        let dst = ARCFACE_TEMPLATE;
        // Build src = R(θ)·scale·dst + t (an exact similarity), so it is perfectly fittable.
        let theta = 0.5f32;
        let (cs, sn) = (theta.cos(), theta.sin());
        let scale = 1.7f32;
        let (tx, ty) = (12.0f32, -5.0f32);
        let src: [(f32, f32); 5] = std::array::from_fn(|i| {
            let (x, y) = dst[i];
            (
                scale * (cs * x - sn * y) + tx,
                scale * (sn * x + cs * y) + ty,
            )
        });
        let m = umeyama_similarity(&src, &dst);
        for i in 0..5 {
            let (mx, my) = apply_affine(&m, src[i]);
            assert!(
                (mx - dst[i].0).abs() < 1e-2 && (my - dst[i].1).abs() < 1e-2,
                "point {i}: mapped ({mx},{my}) vs target {:?}",
                dst[i]
            );
        }
    }

    /// A pure translation is recovered with identity rotation and unit scale.
    #[test]
    fn similarity_pure_translation() {
        let dst = ARCFACE_TEMPLATE;
        let src: [(f32, f32); 5] = std::array::from_fn(|i| (dst[i].0 + 10.0, dst[i].1 - 7.0));
        let m = umeyama_similarity(&src, &dst);
        assert!((m[0][0] - 1.0).abs() < 1e-3, "a={}", m[0][0]);
        assert!((m[1][1] - 1.0).abs() < 1e-3, "d={}", m[1][1]);
        assert!(m[0][1].abs() < 1e-3 && m[1][0].abs() < 1e-3, "no rotation");
        // src = dst + (10, -7), so the src→dst map subtracts that: tx = -10, ty = +7.
        assert!((m[0][2] + 10.0).abs() < 1e-2, "tx={}", m[0][2]);
        assert!((m[1][2] - 7.0).abs() < 1e-2, "ty={}", m[1][2]);
    }

    /// Warping a solid-color image through any affine yields a 112×112 crop of that color
    /// (interior), confirming the preprocessing output dimensions and sampler wiring — no
    /// model needed.
    #[test]
    fn warp_output_dims_and_fill() {
        let mut img = RgbImage::new(200, 200);
        for p in img.pixels_mut() {
            *p = image::Rgb([40, 90, 160]);
        }
        // Identity-ish transform placing the image centered.
        let m: Affine = [[0.5, 0.0, 6.0], [0.0, 0.5, 6.0]];
        let out = warp_affine_112(&img, &m);
        assert_eq!(out.width(), ALIGN_SIZE);
        assert_eq!(out.height(), ALIGN_SIZE);
        let c = out.get_pixel(ALIGN_SIZE / 2, ALIGN_SIZE / 2);
        assert_eq!(c, &image::Rgb([40, 90, 160]));
    }

    /// The session pool hands out **distinct** items to concurrent checkouts — the property
    /// that replaces the old per-model single-flight (N workers ⇒ N sessions in flight, no
    /// two sharing one). Generic `Pool<T>` is exercised with plain integers so no ONNX
    /// session (or model download) is needed.
    #[cfg(feature = "faces")]
    #[test]
    fn pool_lends_distinct_items_under_contention() {
        use std::sync::{Arc, Barrier};

        const N: usize = 4;
        let pool = Arc::new(Pool::new((0..N).collect::<Vec<usize>>()));
        // All threads meet at the barrier while holding a checkout, so every item is out at
        // once — the set of held values must be exactly {0..N}, i.e. all distinct.
        let barrier = Arc::new(Barrier::new(N));
        let mut handles = Vec::new();
        for _ in 0..N {
            let pool = pool.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                let guard = pool.checkout().unwrap();
                let val = *guard;
                barrier.wait();
                val
            }));
        }
        let mut seen: Vec<usize> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..N).collect::<Vec<_>>(), "each concurrent checkout got a distinct item");

        // After all guards dropped, every item is back in the pool and reusable.
        for _ in 0..N {
            let _ = pool.checkout().unwrap();
        }
    }

    /// A single-item pool serializes checkouts: the guard returns the item on drop so the
    /// next checkout reuses it (no deadlock, no duplication).
    #[cfg(feature = "faces")]
    #[test]
    fn pool_of_one_serializes_and_reuses() {
        let pool = Pool::new(vec![42usize]);
        {
            let g = pool.checkout().unwrap();
            assert_eq!(*g, 42);
        }
        // Item returned; a second checkout gets the same item back.
        let g2 = pool.checkout().unwrap();
        assert_eq!(*g2, 42);
    }

    /// L2-normalize produces a unit vector; cosine of a vector with itself is 1.
    #[test]
    fn normalize_is_unit() {
        let mut v = [0f32; EMBED_DIM];
        for (i, x) in v.iter_mut().enumerate() {
            *x = (i as f32 % 7.0) - 3.0;
        }
        let u = l2_normalize(&v);
        let norm: f32 = u.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "norm={norm}");
        assert!((cosine(&u, &u) - 1.0).abs() < 1e-5);
    }

    // ── PoolKey / configure (issue #18) ─────────────────────────────────────────
    //
    // `yunet_pool`/`auraface_pool` fetch through `crate::plugins::onnx::KeyedCache`, whose
    // rebuild-on-key-change policy is proven generically (no ONNX needed) by
    // `plugins::onnx::tests`. What's specific to this module is that `configure`/
    // `configure_force_cpu` actually produce a different key — that's what this test covers.

    /// `configure`/`configure_force_cpu` update the atomics `current_pool_key` reads — the
    /// mechanism that makes a settings change rebuild the pool instead of being silently
    /// ignored until restart. Deliberately a single test rather than several: it would race
    /// itself over the process-global atomics if split up and run in parallel with copies of
    /// itself, and no *other* test in this binary touches `POOL_SIZE`/`INTRA_THREADS`/
    /// `FORCE_CPU` — real pools are only ever built through `tests/faces_engine.rs`, a
    /// separate process.
    #[cfg(feature = "faces")]
    #[test]
    fn configure_calls_change_the_current_pool_key() {
        configure(2, 3);
        configure_force_cpu(false);
        let a = current_pool_key();
        assert_eq!(a, PoolKey { pool_size: 2, intra_threads: 3, force_cpu: false });

        configure(5, 1);
        let b = current_pool_key();
        assert_ne!(a, b, "changing pool_size/intra_threads must change the key");
        assert_eq!(b, PoolKey { pool_size: 5, intra_threads: 1, force_cpu: false });

        configure_force_cpu(true);
        let c = current_pool_key();
        assert_ne!(b, c, "flipping force_cpu must change the key");
        assert_eq!(c, PoolKey { pool_size: 5, intra_threads: 1, force_cpu: true });

        // Reading it again without reconfiguring returns an equal key — this is what lets an
        // unchanged configuration reuse the cached pool instead of rebuilding on every call.
        assert_eq!(current_pool_key(), c);

        // Zero is clamped to 1 — a degenerate resolved plan must still make progress.
        configure(0, 0);
        assert_eq!(
            current_pool_key(),
            PoolKey { pool_size: 1, intra_threads: 1, force_cpu: true }
        );
    }
}
