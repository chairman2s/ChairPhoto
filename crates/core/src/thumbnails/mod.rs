//! Thumbnail and preview generation, with on-disk caching.
//!
//! Raster images are decoded directly by the `image` crate. RAW files have an
//! embedded preview JPEG extracted via `exiv2` — chosen because it is ~10x faster
//! to spawn than exiftool and exposes ALL embedded previews (Sony ARW embeds a
//! tiny, a medium ~1616px, and a full-resolution ~9984px preview). We pick the
//! smallest preview large enough for the requested size, so grid thumbnails decode
//! a small preview while the loupe gets the sharp full-resolution one.
//!
//! Results are cached on disk keyed by absolute path + mtime + size + target size,
//! so each version is generated at most once.

use crate::scanner::{is_raw, is_video};
use image::codecs::jpeg::JpegEncoder;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageReader, RgbImage};
use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

const THUMB_MAX: u32 = 512;
const PREVIEW_MAX: u32 = 2048;
/// Zoom uses the embedded preview at native resolution (no downscale below this);
/// Sony's full embedded preview is ~9984px, so this keeps it intact.
const ZOOM_MAX: u32 = 10000;

/// Bump when generation logic changes in a way that affects output (orientation,
/// preview selection, …), so stale cached images are regenerated, not reused. A bump orphans
/// the previous directories; [`cleanup_stale_caches`] removes the ones it names.
///
/// v5: single-decode downscale chain — a preview/zoom decode now opportunistically
/// derives (and caches) the smaller sizes from the one in-hand decode. Thumbnails
/// derived from a larger decode differ pixel-for-pixel from an independently
/// extracted small embedded preview, so old caches must not be reused.
///
/// v6 (#245): a tier never upscales (2497fa2, #168). Before it, `image`'s `thumbnail` fitted
/// every decode to the tier's box both ways, so an original smaller than a tier was enlarged
/// into it: a 1200×800 photo had a 2048×1365 preview and a 300×200 one a 512×341 thumbnail.
/// Those files are not told apart from a real downscale by their size (every v5 preview's
/// long edge is 2048), so the whole of `t512v5` and `p2048v5` is left behind and both tiers
/// regenerate lazily, on the image pool, at their native size. The face-region writer's
/// preview cross-check keeps the old previews' sizes meanwhile ([`cached_preview_size`]).
///
/// Shared by thumb and preview only — see [`ZOOM_VERSION`] for why zoom keeps its own.
const CACHE_VERSION: u32 = 6;

/// Zoom's own cache-directory version, independent of [`CACHE_VERSION`], so a change that
/// affects one tier's output does not regenerate the others. The no-upscale fix (#168) moved
/// zoom to v6 first, orphaning `z10000v5` (whose files were blown up to 10 000 px); thumb and
/// preview followed in #245, when `CACHE_VERSION` went to 6 for the same fix. The two numbers
/// being equal now is a coincidence: the directory names differ by tier tag (`z`, `p`, `t`).
const ZOOM_VERSION: u32 = 6;

/// One cache size: its longest-edge cap, on-disk tag, cache-directory version, and JPEG
/// quality.
#[derive(Clone, Copy)]
struct Size {
    max: u32,
    tag: &'static str,
    version: u32,
    quality: u8,
}

const THUMB: Size = Size { max: THUMB_MAX, tag: "t", version: CACHE_VERSION, quality: 80 };
const PREVIEW: Size = Size { max: PREVIEW_MAX, tag: "p", version: CACHE_VERSION, quality: 85 };
const ZOOM: Size = Size { max: ZOOM_MAX, tag: "z", version: ZOOM_VERSION, quality: 92 };

// --- analyzer hook ----------------------------------------------------------
// A tiny registry of callbacks invoked once, with the freshly decoded (full,
// oriented) image, every time a size is *generated* from an extraction/decode.
// This lets one decode feed many analyzers (H16 sharpness, H15a pHash) instead of
// each re-reading and re-decoding the file. Cache hits do not fire hooks — there is
// no decode to observe. Ships with a no-op default (empty registry).

/// An analyzer: given the decoded image and the file it came from, do its work
/// (compute a score/hash, stash it somewhere). Must be cheap-ish and must not panic —
/// it runs inside generation on worker threads. `Send + Sync + 'static` so it can be
/// stored in a `Vec` behind a mutex and cloned by `Arc` for lock-free dispatch.
pub type Analyzer = Arc<dyn Fn(&DynamicImage, &Path) + Send + Sync + 'static>;

fn analyzers() -> &'static Mutex<Vec<Analyzer>> {
    static REGISTRY: OnceLock<Mutex<Vec<Analyzer>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

/// Register an analyzer to be invoked with every freshly decoded image during
/// thumbnail/preview/zoom generation. Called once at startup by features that want to
/// piggyback on the decode (H16, H15a). No-op set by default.
pub fn register_analyzer(analyzer: Analyzer) {
    if let Ok(mut list) = analyzers().lock() {
        list.push(analyzer);
    }
}

/// Fire every registered analyzer once against a decoded image, but only when the
/// decoded size meets the minimum resolution required for accurate scoring.
///
/// `decoded_max` is the longest-edge cap used for this decode (i.e. `size.max` from
/// the `Size` that triggered the decode). Analyzers that depend on fine detail —
/// sharpness scoring, pHash — need at least `PREVIEW_MAX` (2048px) to see micro-blur;
/// firing them on a THUMB (512px) decode produces an inaccurate result that the
/// `sharpness IS NULL` guard then treats as canonical, preventing a later accurate score.
///
/// # Why we snapshot first
///
/// The global registry `Mutex` must **not** be held while executing callbacks:
/// callbacks can be CPU-heavy (sharpness scoring, pHash) and many decode workers run
/// in parallel — holding the lock during execution would serialize all of them on one
/// lock, defeating the parallelism (I7b review finding). Instead we snapshot the `Arc`
/// list under a brief lock and then call each analyzer with the lock released. The
/// `Arc` clones keep each callback alive for the duration; registration (the only
/// mutation) contends only with other registrations, not with ongoing callbacks.
fn run_analyzers(img: &DynamicImage, path: &Path, decoded_max: u32) {
    // Resolution gate: skip analyzers when the decoded size is below the preview tier.
    // A THUMB (512px) decode cannot show micro-blur; scoring on it produces a wrong
    // result that the IS NULL guard then permanently locks in, preventing an accurate
    // score from the batch indexer or a later preview/zoom decode.
    if decoded_max < PREVIEW_MAX {
        return;
    }
    // Snapshot: O(n) clone of Arc pointers, then release the lock immediately.
    let snapshot: Vec<Analyzer> = analyzers()
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default();
    // Execute with the lock NOT held.
    for a in &snapshot {
        a(img, path);
    }
}

/// JPEG bytes for a small grid thumbnail (cached).
pub fn thumbnail_bytes(path: &Path) -> Result<Vec<u8>, String> {
    cached(path, THUMB)
}

/// Apply a photo's non-destructive user rotation on top of the EXIF-oriented tier: clockwise
/// by `degrees`; anything but 90/180/270 (after normalising) leaves the image as it is.
/// Rotation by a multiple of 90° resamples nothing (a pure pixel permutation).
pub fn rotate_image(img: DynamicImage, degrees: i64) -> DynamicImage {
    match ((degrees % 360) + 360) % 360 {
        90 => img.rotate90(),
        180 => img.rotate180(),
        270 => img.rotate270(),
        _ => img,
    }
}

/// The persistent thumbnail of a rotated photo (`media::render_image`): JPEG quality 90, the
/// same file the Tauri shell's byte path wrote before #165.
pub(crate) fn encode_rotated_jpeg(img: &DynamicImage) -> Result<Vec<u8>, String> {
    let mut out = Cursor::new(Vec::new());
    img.write_with_encoder(JpegEncoder::new_with_quality(&mut out, 90))
        .map_err(|e| e.to_string())?;
    Ok(out.into_inner())
}

// --- persistent, photo-id-keyed thumbnails ---------------------------------
// The normal disk cache is keyed by path+mtime+size, so it can't be found once the
// original is unreachable (e.g. a photo offloaded to a NAS that's now unmounted). This
// second store is keyed only by photo id, so a NAS-only photo stays browsable offline.
// It's written whenever a thumbnail is served while the original IS reachable, and
// proactively at offload time.

/// Path of a photo's persistent (id-keyed) thumbnail.
pub fn persistent_thumb_path(photo_id: i64) -> PathBuf {
    cache_dir()
        .join("chairphoto")
        .join("persist")
        .join(format!("{photo_id}.jpg"))
}

/// Save (or refresh) a photo's persistent thumbnail. Best-effort.
pub fn save_persistent_thumb(photo_id: i64, bytes: &[u8]) {
    let p = persistent_thumb_path(photo_id);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&p, bytes).ok();
}

/// A photo's persistent thumbnail bytes, if one was kept.
pub fn read_persistent_thumb(photo_id: i64) -> Option<Vec<u8>> {
    std::fs::read(persistent_thumb_path(photo_id)).ok()
}

/// Generate a thumbnail from `path` and persist it under `photo_id` — called at offload
/// time so the grid keeps an image after the original leaves local disk.
pub fn ensure_persistent_thumb(photo_id: i64, path: &Path) -> Result<(), String> {
    let bytes = thumbnail_bytes(path)?;
    save_persistent_thumb(photo_id, &bytes);
    Ok(())
}

/// JPEG bytes for a large loupe preview (cached).
pub fn preview_bytes(path: &Path) -> Result<Vec<u8>, String> {
    cached(path, PREVIEW)
}

/// The pixel size of `path`'s cached 2048 px preview ([`preview_bytes`], the oriented image the
/// faces indexer detects on), read from the cached file's header. `None` when it is not
/// cached — nothing is generated — or its header cannot be read. The face-region writer
/// cross-checks the recorded frame against it (#154).
///
/// Until the current preview is generated, the pre-#245 one stands in: its file in the old
/// `p2048v5` directory if that is still there, else the size [`cleanup_stale_caches`] kept of
/// it before removing the directory. The cross-check compares aspects only, and an upscaled
/// preview has its decode's aspect (to a pixel of rounding on a 2048 px edge), so the old
/// size says what the new one will; it is also the frame the faces indexed before #245 were
/// found on. Without it the version bump would skip the cross-check for every photo whose
/// preview had not been regenerated yet.
pub fn cached_preview_size(path: &Path) -> Option<(u32, u32)> {
    let cache_path = cache_path_for(path, PREVIEW).ok()?;
    if let Some(size) = image_size(&cache_path) {
        return Some(size);
    }
    let name = cache_path.file_name()?.to_str()?;
    let root = cache_dir().join("chairphoto");
    let old_dir = root.join(STALE_PREVIEW_DIR);
    // Same check as `cleanup_stale_caches`: a symlinked old directory is never followed,
    // here either — read its own real directory only, never through a link (review Nit-1).
    is_own_dir(&old_dir)
        .then(|| regular_file_size(&old_dir.join(name)))
        .flatten()
        .or_else(|| stale_preview_sizes(&root)?.get(name).copied())
}

/// The pixel size in an image file's header, or `None`.
fn image_size(file: &Path) -> Option<(u32, u32)> {
    ImageReader::open(file).ok()?.with_guessed_format().ok()?.into_dimensions().ok()
}

/// [`image_size`] of a regular file only — never through a symlink, never a FIFO — for the
/// old cache files ChairPhoto no longer writes.
fn regular_file_size(file: &Path) -> Option<(u32, u32)> {
    std::fs::symlink_metadata(file).ok().filter(|m| m.file_type().is_file())?;
    image_size(file)
}

/// Whether a decoded JPEG (e.g. a cached thumbnail) is effectively grayscale (B&W).
/// Samples a grid of pixels and reports grayscale when almost none show meaningful
/// colour — robust to JPEG chroma noise and a few stray coloured pixels. This is the
/// reliable monochrome signal (camera "B&W" flags lie on some bodies).
pub fn is_grayscale_jpeg(jpeg: &[u8]) -> bool {
    let Ok(img) = image::load_from_memory(jpeg) else {
        return false;
    };
    let rgb = img.to_rgb8();
    let (w, h) = rgb.dimensions();
    if w == 0 || h == 0 {
        return false;
    }
    // Sample ~64×64 points regardless of size.
    let step_x = (w / 64).max(1);
    let step_y = (h / 64).max(1);
    let mut sampled = 0u32;
    let mut coloured = 0u32;
    let mut y = 0;
    while y < h {
        let mut x = 0;
        while x < w {
            let p = rgb.get_pixel(x, y).0;
            let chroma = p[0].max(p[1]).max(p[2]) - p[0].min(p[1]).min(p[2]);
            sampled += 1;
            if chroma > 18 {
                coloured += 1;
            }
            x += step_x;
        }
        y += step_y;
    }
    sampled > 0 && (coloured as f32 / sampled as f32) < 0.01
}

/// JPEG bytes for full-resolution zoom — the embedded preview at native size and
/// high quality, for pixel-peeping focus/sharpness in the loupe (cached).
pub fn zoom_bytes(path: &Path) -> Result<Vec<u8>, String> {
    cached(path, ZOOM)
}

/// Serve one cache size, generating it on a miss.
///
/// Grid-pool latency guard: a lone small-size request must not pay for a larger
/// decode. So on a miss we extract an embedded preview sized for exactly this size
/// (RAW: the smallest preview ≥ `size.max`) and decode once — no over-decode. But the
/// single decode is not wasted: because a larger tier's decode is a strict superset of
/// what the smaller tiers need, whenever we *do* decode for a size we opportunistically
/// derive and cache the smaller tiers too (see `generate_from_decode`). The next
/// smaller-size request is then a cache hit — free.
fn cached(path: &Path, size: Size) -> Result<Vec<u8>, String> {
    let cache_path = cache_path_for(path, size)?;
    if let Ok(bytes) = std::fs::read(&cache_path) {
        return Ok(bytes);
    }
    // Decode once for this size. The decode we just paid for is a strict superset of
    // every smaller tier, so derive and cache those too — a lone thumb request stays a
    // thumb decode (no over-decode), but a preview/zoom decode also fills the smaller
    // tiers so the next grid request is a free cache hit.
    let probe = probe_colour_space_beside(path);
    let img = extract_and_decode(path, size.max)?;
    if let Some(probe) = probe {
        let _ = probe.join();
    }
    run_analyzers(&img, path, size.max);
    let bytes = generate_from_decode(path, &img, size)?;
    for smaller in smaller_sizes(size.max) {
        let cp = cache_path_for(path, smaller)?;
        if !cp.exists() {
            // Best-effort: an opportunistic derive must never fail the request it rode in on.
            let _ = generate_from_decode(path, &img, smaller);
        }
    }
    Ok(bytes)
}

/// The cache sizes strictly smaller than `max`, largest first — the tiers a decode for
/// `max` can derive for free.
fn smaller_sizes(max: u32) -> Vec<Size> {
    [ZOOM, PREVIEW, THUMB]
        .into_iter()
        .filter(|s| s.max < max)
        .collect()
}

/// Generate every cache size for a photo in a single extraction + decode: extract the
/// largest embedded preview once, decode once, run analyzers once, then downscale that
/// one image into every requested tier (largest → smallest). This is the bulk path
/// (cache warming, import) where all sizes are wanted — it reads the RAW off the NAS
/// once instead of once per size. Missing tiers are (re)generated; existing ones are
/// left as-is. Returns nothing; the sizes land in the on-disk cache.
pub fn warm_all_sizes(path: &Path) -> Result<(), String> {
    let sizes = [ZOOM, PREVIEW, THUMB];
    // If every size is already cached, there is nothing to decode (and no hook to fire).
    let mut missing = Vec::new();
    for s in sizes {
        let cp = cache_path_for(path, s)?;
        if !cp.exists() {
            missing.push(s);
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    // Decode once at the largest needed size; the smaller tiers downscale from it.
    let largest = missing.iter().map(|s| s.max).max().unwrap();
    let probe = probe_colour_space_beside(path);
    let img = extract_and_decode(path, largest)?;
    if let Some(probe) = probe {
        let _ = probe.join();
    }
    run_analyzers(&img, path, largest);
    for s in missing {
        generate_from_decode(path, &img, s)?;
    }
    Ok(())
}

/// Extract an embedded preview / poster frame / decoded raster sized for `max`, decode
/// it, and apply orientation — producing the full oriented image (no downscale to the
/// target yet). This is the one expensive step (network read + decode) we want to do
/// once per photo when generating multiple sizes.
fn extract_and_decode(path: &Path, max: u32) -> Result<DynamicImage, String> {
    // RAW: extract an embedded preview sized for the target; its pixels are in
    // sensor (unrotated) orientation, so apply the RAW file's own EXIF orientation.
    // Raster: decode the file and use the orientation the decoder reports.
    let plain_raster = !is_raw(path) && !is_video(path) && !is_heic(path);
    let (source, raw_orientation) = if is_raw(path) {
        (extract_raw_preview(path, max)?, Some(exif_orientation(path)))
    } else if is_video(path) {
        // Video: grab a poster frame with ffmpeg; the rest of the pipeline resizes it.
        (extract_video_frame(path)?, None)
    } else if is_heic(path) {
        // HEIF/HEIC (iPhone): the `image` crate can't decode it, so convert to an upright
        // JPEG via ImageMagick's libheif delegate. -auto-orient bakes EXIF orientation into
        // the pixels, so downstream treats it as already-oriented (NoTransforms).
        (decode_via_magick(path, max)?, Some(Orientation::NoTransforms))
    } else {
        (std::fs::read(path).map_err(|e| e.to_string())?, None)
    };

    match decode_oriented(&source, raw_orientation) {
        Ok(img) => Ok(img),
        // The `image` crate gave up — a format it doesn't know (PSD, …) or a damaged
        // file (e.g. a JPEG with a corrupt SOF header). ImageMagick is both more
        // format-complete and more damage-tolerant, so give it one shot before failing.
        // Plain rasters only: RAW/video/HEIC sources already came from an external tool.
        Err(e) if plain_raster => {
            let rescued = decode_via_magick(path, max)
                .map_err(|magick_err| format!("{e}; magick fallback: {magick_err}"))?;
            decode_oriented(&rescued, Some(Orientation::NoTransforms))
        }
        Err(e) => Err(e),
    }
}

/// Downscale one already-decoded image into cache size `size`, encode it, and write the
/// cache file. Returns the encoded JPEG bytes. Shared by the single-size path and the
/// warm-all chain so both derive identically from one decode.
fn generate_from_decode(path: &Path, img: &DynamicImage, size: Size) -> Result<Vec<u8>, String> {
    let bytes = encode_size(path, img, size)?;
    let cache_path = cache_path_for(path, size)?;
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&cache_path, &bytes).ok();
    Ok(bytes)
}

/// Downscale a decoded image to `size` and JPEG-encode it (with Adobe-RGB→sRGB when the
/// source file is Adobe RGB). Pure — no disk writes.
fn encode_size(path: &Path, img: &DynamicImage, size: Size) -> Result<Vec<u8>, String> {
    // A tier only ever shrinks: an image that already fits is encoded at its own size. (image's
    // `thumbnail` fits the image to the box both ways, so the 10 000 px zoom tier used to
    // blow a 6000 px decode up to 10 000 px — a 67 MP JPEG that took seconds to encode and a
    // 267 MB texture, for no more detail, #168.) `downscale::thumbnail` is image's
    // `thumbnail`, byte for byte, without its per-pixel overhead.
    let fits = img.width() <= size.max && img.height() <= size.max;
    let resized = if fits { std::borrow::Cow::Borrowed(img) } else { std::borrow::Cow::Owned(downscale::thumbnail(img, size.max)) };
    let mut out = Cursor::new(Vec::new());
    // The webview shows untagged JPEGs as sRGB. Sony shoots Adobe RGB (wider gamut), so
    // an Adobe RGB preview displayed as-is looks dull/desaturated. Convert it to sRGB for
    // display. The original RAW is untouched; the edited export also renders in sRGB
    // (LibRaw `output_color = 1`), so the editor's proxy and the export share a colour
    // space — a prerequisite for the tone match (K1).
    if is_adobe_rgb(path) {
        let mut rgb = resized.to_rgb8();
        adobe_rgb_to_srgb(&mut rgb);
        DynamicImage::ImageRgb8(rgb)
            .write_with_encoder(JpegEncoder::new_with_quality(&mut out, size.quality))
            .map_err(|e| e.to_string())?;
    } else {
        resized
            .write_with_encoder(JpegEncoder::new_with_quality(&mut out, size.quality))
            .map_err(|e| e.to_string())?;
    }
    Ok(out.into_inner())
}

/// Decode encoded image bytes and apply orientation: the override when given (source
/// pixels whose orientation the decoder can't know — RAW previews, magick output),
/// otherwise whatever the decoder reports.
fn decode_oriented(
    source: &[u8],
    orientation_override: Option<Orientation>,
) -> Result<DynamicImage, String> {
    let mut decoder = ImageReader::new(Cursor::new(source))
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .into_decoder()
        .map_err(|e| e.to_string())?;
    let orientation = match orientation_override {
        Some(o) => o,
        None => decoder.orientation().unwrap_or(Orientation::NoTransforms),
    };
    let mut img = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    img.apply_orientation(orientation);
    Ok(img)
}

/// Start [`is_adobe_rgb`] for `path` on its own thread, for a caller about to decode: the
/// probe is an `exiftool` run (~75 ms, a quarter of a cold 24 MP preview) that the encode
/// after the decode needs, and the two need not wait for each other. Join it before encoding;
/// its answer is in `is_adobe_rgb`'s memo by then (#168). A probe that cannot start leaves
/// the encode to run it, as before.
fn probe_colour_space_beside(path: &Path) -> Option<std::thread::JoinHandle<()>> {
    let path = path.to_path_buf();
    std::thread::Builder::new()
        .name("colour-space-probe".into())
        .spawn(move || {
            is_adobe_rgb(&path);
        })
        .ok()
}

/// Whether a file's color space is Adobe RGB (Sony tags this as ColorSpace=Uncalibrated
/// + InteroperabilityIndex R03). Detected via exiftool and memoized per path, since
/// `generate` runs up to 3× per image (thumb/preview/zoom).
fn is_adobe_rgb(path: &Path) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(map) = cache.lock() {
        if let Some(&v) = map.get(path) {
            return v;
        }
    }
    let detected = detect_adobe_rgb(path);
    if let Ok(mut map) = cache.lock() {
        map.insert(path.to_path_buf(), detected);
    }
    detected
}

fn detect_adobe_rgb(path: &Path) -> bool {
    let output = Command::new("exiftool")
        .args(["-s3", "-ColorSpace", "-InteropIndex"])
        .arg(path)
        .output();
    if let Ok(out) = output {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            return s.contains("Adobe RGB") || s.contains("R03");
        }
    }
    false
}

/// Convert an Adobe RGB (1998) image to sRGB in place. Linearize with Adobe's gamma,
/// apply the Adobe-RGB→sRGB linear matrix, then re-apply the sRGB transfer curve.
fn adobe_rgb_to_srgb(img: &mut RgbImage) {
    const ADOBE_GAMMA: f32 = 2.199_218_8; // 2 + 51/256
    let srgb_encode = |c: f32| -> f32 {
        let c = c.clamp(0.0, 1.0);
        if c <= 0.003_130_8 {
            12.92 * c
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        }
    };
    for px in img.pixels_mut() {
        let ar = (px[0] as f32 / 255.0).powf(ADOBE_GAMMA);
        let ag = (px[1] as f32 / 255.0).powf(ADOBE_GAMMA);
        let ab = (px[2] as f32 / 255.0).powf(ADOBE_GAMMA);
        // Adobe RGB linear → sRGB linear (D65). Off-diagonals are ~0 for R←R, G←G.
        let sr = 1.398_25 * ar - 0.398_25 * ag;
        let sg = ag;
        let sb = -0.042_93 * ag + 1.042_93 * ab;
        px[0] = (srgb_encode(sr) * 255.0).round() as u8;
        px[1] = (srgb_encode(sg) * 255.0).round() as u8;
        px[2] = (srgb_encode(sb) * 255.0).round() as u8;
    }
}

/// Extract an embedded preview JPEG from a RAW file.
///
/// Fast path: exiv2 (excellent for Sony ARW — returns JPEG previews including the
/// full-resolution one). Adobe DNG, however, stores its previews as JPEG-compressed
/// TIFF that the image crate can't decode, so when exiv2 returns non-JPEG bytes we
/// fall back to exiftool, which yields clean JPEG for both formats (just slower).
/// Extract a poster frame (JPEG bytes) from a video with ffmpeg. Tries ~1s in (skips black
/// intros); falls back to the first frame for very short clips.
fn extract_video_frame(path: &Path) -> Result<Vec<u8>, String> {
    let grab = |seek: &str| -> Option<Vec<u8>> {
        let out = Command::new("ffmpeg")
            .args(["-v", "error", "-ss", seek, "-i"])
            .arg(path)
            .args(["-frames:v", "1", "-an", "-f", "mjpeg", "pipe:1"])
            .output()
            .ok()?;
        if out.status.success() && !out.stdout.is_empty() {
            Some(out.stdout)
        } else {
            None
        }
    };
    grab("1")
        .or_else(|| grab("0"))
        .ok_or_else(|| format!("ffmpeg could not extract a frame from {}", path.display()))
}

/// HEIF/HEIC container (iPhone photos). The `image` crate can't decode these; we route
/// them through ImageMagick's libheif delegate instead (see `decode_via_magick`), which turns
/// them by their container's `irot`/`imir` — the rule the face-region frame follows for the
/// same files (`metadata::heif`, #154), so it is one rule.
fn is_heic(path: &Path) -> bool {
    crate::metadata::heif::is_heif(path)
}

/// Decode an image file to JPEG bytes via ImageMagick (`magick`), upright (EXIF
/// orientation baked in) and downscaled to `max` on the long edge, converted to sRGB for
/// correct on-screen colour (iPhone HEIC is usually Display P3). The primary route for
/// HEIC/HEIF (needs ImageMagick built with the libheif delegate) and the rescue route for
/// rasters the `image` crate rejects — formats it doesn't know (PSD) or damaged files
/// magick's decoders tolerate. `[0]` limits multi-layer formats to their first frame
/// (a PSD's flattened composite) so layered files don't emit one JPEG per layer.
fn decode_via_magick(path: &Path, max: u32) -> Result<Vec<u8>, String> {
    let mut input = path.as_os_str().to_owned();
    input.push("[0]");
    let out = Command::new("magick")
        .arg(input)
        .arg("-auto-orient")
        .args(["-resize", &format!("{max}x{max}>")])
        .args(["-colorspace", "sRGB"])
        .args(["-quality", "92"])
        .arg("jpg:-")
        .output()
        .map_err(|e| format!("decode needs ImageMagick (`magick`): {e}"))?;
    if !out.status.success() || out.stdout.is_empty() {
        return Err(format!(
            "magick failed to decode {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(out.stdout)
}

fn extract_raw_preview(path: &Path, target_max: u32) -> Result<Vec<u8>, String> {
    if let Ok(bytes) = extract_via_exiv2(path, target_max) {
        if is_jpeg(&bytes) {
            return Ok(bytes);
        }
    }
    extract_via_exiftool(path, target_max)
}

/// exiv2 extraction: choose the smallest preview whose longest edge is >=
/// `target_max` (else the largest), extract it into a unique temp dir, read it back.
/// exiv2's exit status is NOT trusted — on some DNGs it prints a non-fatal maker-
/// note warning and exits non-zero yet still writes the file — so we rely on the
/// presence of an output file instead.
fn extract_via_exiv2(path: &Path, target_max: u32) -> Result<Vec<u8>, String> {
    let index = choose_preview_index(path, target_max)?;
    let tmp = unique_tmp_dir(path);
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;

    let result = (|| {
        let output = Command::new("exiv2")
            .arg(format!("-ep{index}"))
            .arg("-l")
            .arg(&tmp)
            .arg(path)
            .output()
            .map_err(|e| format!("exiv2 not available: {e}"))?;
        read_only_image_in(&tmp).map_err(|e| {
            let stderr = String::from_utf8_lossy(&output.stderr);
            format!("{e} (exiv2: {})", stderr.trim())
        })
    })();

    std::fs::remove_dir_all(&tmp).ok();
    result
}

/// exiftool fallback: pull a JPEG preview straight to stdout. Picks the full-size
/// `JpgFromRaw` for large targets and the smaller `PreviewImage` otherwise.
fn extract_via_exiftool(path: &Path, target_max: u32) -> Result<Vec<u8>, String> {
    let tags: &[&str] = if target_max > 1024 {
        &["-JpgFromRaw", "-PreviewImage", "-ThumbnailImage"]
    } else {
        &["-PreviewImage", "-JpgFromRaw", "-ThumbnailImage"]
    };
    for tag in tags {
        let output = Command::new("exiftool")
            .args(["-b", tag])
            .arg(path)
            .output()
            .map_err(|e| format!("exiftool not available: {e}"))?;
        if output.status.success() && is_jpeg(&output.stdout) {
            return Ok(output.stdout);
        }
    }
    Err(format!("No decodable preview found in {}", path.display()))
}

/// JPEG files start with the SOI marker 0xFFD8.
fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.len() >= 2 && bytes[0] == 0xff && bytes[1] == 0xd8
}

/// Parse `exiv2 -pp` and pick the preview index. Returns the smallest preview with
/// a long edge >= `target_max`, else the largest available.
fn choose_preview_index(path: &Path, target_max: u32) -> Result<u32, String> {
    let output = Command::new("exiv2")
        .arg("-pp")
        .arg(path)
        .output()
        .map_err(|e| format!("exiv2 not available: {e}"))?;
    let listing = String::from_utf8_lossy(&output.stdout);

    // Collect (index, long_edge) for each "Preview N: ..., WxH pixels, ..." line.
    let mut previews: Vec<(u32, u32)> = Vec::new();
    for line in listing.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("Preview ") else {
            continue;
        };
        let Some((idx_str, after)) = rest.split_once(':') else {
            continue;
        };
        let Ok(index) = idx_str.trim().parse::<u32>() else {
            continue;
        };
        if let Some(dims) = after.split_once(" pixels").and_then(|(d, _)| {
            d.rsplit(',').next().map(str::trim)
        }) {
            if let Some((w, h)) = dims.split_once('x') {
                if let (Ok(w), Ok(h)) = (w.trim().parse::<u32>(), h.trim().parse::<u32>()) {
                    previews.push((index, w.max(h)));
                }
            }
        }
    }

    if previews.is_empty() {
        return Err(format!("No embedded preview found in {}", path.display()));
    }
    previews.sort_by_key(|&(_, edge)| edge);
    let chosen = previews
        .iter()
        .find(|&&(_, edge)| edge >= target_max)
        .or_else(|| previews.last())
        .unwrap();
    Ok(chosen.0)
}

/// Read the single preview file exiv2 wrote into `dir`. Since we extract exactly
/// one preview index, there is at most one file; we read it regardless of its
/// extension (it may be .jpg or .tif depending on the RAW format).
fn read_only_image_in(dir: &Path) -> Result<Vec<u8>, String> {
    let entries = std::fs::read_dir(dir).map_err(|e| e.to_string())?;
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_file() {
            return std::fs::read(&p).map_err(|e| e.to_string());
        }
    }
    Err("exiv2 produced no preview file".into())
}

/// Read the EXIF Orientation (1–8) of a file via exiv2, mapped to the image
/// crate's `Orientation`. Defaults to no transform when unavailable.
pub(crate) fn exif_orientation(path: &Path) -> Orientation {
    let output = Command::new("exiv2")
        .args(["-g", "Exif.Image.Orientation", "-Pv"])
        .arg(path)
        .output();
    if let Ok(out) = output {
        if out.status.success() {
            if let Ok(n) = String::from_utf8_lossy(&out.stdout).trim().parse::<u8>() {
                if let Some(o) = Orientation::from_exif(n) {
                    return o;
                }
            }
        }
    }
    Orientation::NoTransforms
}

/// Cache file path: <cache_dir>/chairphoto/<tag><max>v<version>/<hash>.jpg, where
/// the hash covers path + mtime + size so edits invalidate the cache, and `version` is
/// `size`'s own cache-directory version (shared [`CACHE_VERSION`] for thumb/preview,
/// [`ZOOM_VERSION`] for zoom).
fn cache_path_for(path: &Path, size: Size) -> Result<PathBuf, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let key = format!("{}|{}|{}", path.display(), mtime, meta.len());
    let hash = fnv1a(&key);

    let base = cache_dir()
        .join("chairphoto")
        .join(format!("{}{}v{}", size.tag, size.max, size.version));
    Ok(base.join(format!("{hash:016x}.jpg")))
}

/// Cache directories older builds wrote and nothing reads any more, by their fixed names under
/// `<cache_dir>/chairphoto`, kept only so [`cleanup_stale_caches`] can find them.
///
/// `z10000v5`: zoom before `ZOOM_VERSION` split off from `CACHE_VERSION` (#168), upscaled to
/// 10 000 px for any original smaller than that.
const STALE_ZOOM_DIR: &str = "z10000v5";
/// `t512v5`: thumbnails before `CACHE_VERSION` 6 (#245), upscaled for originals under 512 px.
const STALE_THUMB_DIR: &str = "t512v5";
/// `p2048v5`: previews before `CACHE_VERSION` 6 (#245), upscaled for originals under 2048 px.
const STALE_PREVIEW_DIR: &str = "p2048v5";
/// `cover512v1`: cover thumbnails (`plugins::edit::cover`) before its `COVER_FORMAT` 2 (#245),
/// rendered from those upscaled previews, so enlarged for originals under 512 px.
const STALE_COVER_DIR: &str = "cover512v1";
/// What [`cleanup_stale_caches`] keeps of [`STALE_PREVIEW_DIR`] for [`cached_preview_size`]:
/// one `<file name> <width> <height>` line per old preview.
const STALE_PREVIEW_SIZES: &str = "p2048v5.sizes";

/// One-time, best-effort removal of the cache directories older builds left behind:
/// [`STALE_ZOOM_DIR`] (#168), [`STALE_THUMB_DIR`], [`STALE_PREVIEW_DIR`] and
/// [`STALE_COVER_DIR`] (#245). Nothing reads their images any more, so removing them only
/// reclaims disk space — except that the face-region writer's cross-check still wants the old
/// previews' pixel sizes until each photo's preview is regenerated ([`cached_preview_size`]).
/// Those are written first, to [`STALE_PREVIEW_SIZES`] (through a temporary file renamed into
/// place), and the preview directory is removed only once they have landed; if they cannot be
/// kept the directory stays for the next start.
///
/// Safe by construction: every path is this process's own `cache_dir()` joined with a fixed
/// literal, never anything caller-supplied, and a directory is removed only if that exact
/// path is a real directory — in particular never a symlink (checked with
/// [`std::fs::symlink_metadata`], which does not follow it), so a symlink planted at that
/// name is left untouched rather than followed. `fs::remove_dir_all` itself does not follow
/// symlinks it finds inside the tree either, and the size pass reads only regular files
/// named as cache files, so no entry under a directory can redirect anything elsewhere. A
/// missing directory or any I/O error is silently a no-op: this never panics or reports a
/// failure.
///
/// Call off the UI thread — it is disk I/O (a header read per old preview) and nothing waits
/// on it (`app::boot_with` spawns it on its own thread).
pub fn cleanup_stale_caches() {
    let root = cache_dir().join("chairphoto");
    remove_own_dir(&root.join(STALE_ZOOM_DIR));
    remove_own_dir(&root.join(STALE_THUMB_DIR));
    remove_own_dir(&root.join(STALE_COVER_DIR));
    let previews = root.join(STALE_PREVIEW_DIR);
    if is_own_dir(&previews) && keep_stale_preview_sizes(&root, &previews).is_ok() {
        remove_own_dir(&previews);
    }
}

/// Whether `dir` is a real directory, not a symlink to one.
fn is_own_dir(dir: &Path) -> bool {
    matches!(std::fs::symlink_metadata(dir), Ok(meta) if meta.file_type().is_dir())
}

/// Remove `dir` if it [`is_own_dir`]; errors are ignored.
fn remove_own_dir(dir: &Path) {
    if is_own_dir(dir) {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Whether `name` is a cache file's ([`cache_path_for`]): 16 lowercase hex digits and `.jpg`.
fn is_cache_file_name(name: &str) -> bool {
    name.strip_suffix(".jpg")
        .is_some_and(|hash| hash.len() == 16 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// Write the pixel size of every preview in `previews` to [`STALE_PREVIEW_SIZES`] under
/// `root`, merged with what an earlier, interrupted pass kept. A preview whose header cannot
/// be read is left out, as [`cached_preview_size`] could not read it either. `Err` when the
/// sizes did not land.
fn keep_stale_preview_sizes(root: &Path, previews: &Path) -> std::io::Result<()> {
    use std::io::Write;
    sweep_stale_tmp_sizes_files(root);
    let mut sizes = read_stale_preview_sizes(&root.join(STALE_PREVIEW_SIZES));
    for entry in std::fs::read_dir(previews)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().filter(|n| is_cache_file_name(n)).map(str::to_owned) else {
            continue;
        };
        if let Some(size) = regular_file_size(&entry.path()) {
            sizes.insert(name, size);
        }
    }
    let mut lines: Vec<String> = sizes.iter().map(|(name, (w, h))| format!("{name} {w} {h}\n")).collect();
    lines.sort();
    // A name unique to this attempt — this process's id plus a per-process nonce — so two
    // processes sharing one cache dir (`single_instance` is keyed per app *data* dir, not
    // cache dir, so two XDG_DATA_HOMEs with one default cache can run this at once, #245
    // review LOW-2) never share a tmp name and so can never interleave through it:
    // `create_new` claims a name nothing else has, and only this attempt writes to or renames
    // it. Another process's start-up sweep (`sweep_stale_tmp_sizes_files`) can unlink it
    // mid-write; then this rename fails, this attempt returns `Err`, and `p2048v5` is kept
    // for the next start — fail-safe, never a partial sizes file.
    static NONCE: AtomicU64 = AtomicU64::new(0);
    let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
    let tmp = root.join(format!("{STALE_PREVIEW_SIZES}.{}.{nonce}.tmp", std::process::id()));
    // Only a name this attempt created is ever removed: a `create_new` that fails left
    // whatever holds the name alone.
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    let write = (|| -> std::io::Result<()> {
        file.write_all(lines.concat().as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, root.join(STALE_PREVIEW_SIZES))
    })();
    drop(file);
    if write.is_err() {
        // Our own attempt's name: remove what we created rather than leave it for the next
        // sweep, best-effort (a failure here changes nothing — it is still only ever read
        // back as a `.tmp`-suffixed name no code treats as [`STALE_PREVIEW_SIZES`]).
        let _ = std::fs::remove_file(&tmp);
    }
    write
}

/// Best-effort removal of a previous attempt's leftover temporary sizes file under `root`:
/// the fixed name a pre-LOW-2 build used, or one of today's unique `<pid>.<nonce>` ones,
/// left behind by a process that crashed or was killed before its own rename landed.
/// Harmless either way — nothing ever reads a `.tmp`-suffixed name back as
/// [`STALE_PREVIEW_SIZES`] — this only keeps them from accumulating. It matches any
/// `<STALE_PREVIEW_SIZES>.*.tmp` name, not only the `<pid>.<nonce>` shape: it only ever
/// looks inside ChairPhoto's own cache directory, where nothing else writes such names. Never a directory: a
/// name it cannot remove (one planted there instead) is left alone, not traversed into or
/// removed recursively. Never a symlink's target: `remove_file` unlinks the name itself,
/// whatever it points to, never the pointed-to file's content.
fn sweep_stale_tmp_sizes_files(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    let prefix = format!("{STALE_PREVIEW_SIZES}.");
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
        if !(name.starts_with(&prefix) && name.ends_with(".tmp")) {
            continue;
        }
        if matches!(std::fs::symlink_metadata(entry.path()), Ok(meta) if meta.file_type().is_dir()) {
            continue;
        }
        let _ = std::fs::remove_file(entry.path());
    }
}

/// Parse [`STALE_PREVIEW_SIZES`]. Lines that are not `<cache file name> <w> <h>` are skipped.
/// The write is tmp + fsync + rename, so a complete file always ends in `\n`; one that
/// doesn't was truncated mid-write (external damage — disk full, a killed process without
/// our own tmp+rename, a copy cut short) and its last line may be only part of a write, so
/// that line is dropped rather than parsed — fail safe to no size (the cross-check is
/// skipped for that photo) rather than a wrong one. A missing or unreadable file, or one
/// that is not a regular file, is empty.
fn read_stale_preview_sizes(file: &Path) -> HashMap<String, (u32, u32)> {
    let regular = matches!(std::fs::symlink_metadata(file), Ok(meta) if meta.file_type().is_file());
    let text = if regular { std::fs::read_to_string(file).unwrap_or_default() } else { String::new() };
    let complete_lines = if text.is_empty() || text.ends_with('\n') {
        text.as_str()
    } else {
        text.rsplit_once('\n').map_or("", |(head, _)| head)
    };
    complete_lines
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_ascii_whitespace();
            let name = parts.next().filter(|n| is_cache_file_name(n))?;
            let w = parts.next()?.parse().ok()?;
            let h = parts.next()?.parse().ok()?;
            parts.next().is_none().then(|| (name.to_owned(), (w, h)))
        })
        .collect()
}

/// [`STALE_PREVIEW_SIZES`] under `root`, parsed once per version of the file: the faces
/// indexer asks for one photo after another, and the cleanup may write the file after the
/// first ask, so the memo is keyed by the file's path, length and mtime. `None` when there is
/// no such file.
fn stale_preview_sizes(root: &Path) -> Option<Arc<HashMap<String, (u32, u32)>>> {
    type Key = (PathBuf, u64, Option<std::time::SystemTime>);
    static MEMO: Mutex<Option<(Key, Arc<HashMap<String, (u32, u32)>>)>> = Mutex::new(None);
    let file = root.join(STALE_PREVIEW_SIZES);
    let meta = std::fs::symlink_metadata(&file).ok().filter(|m| m.file_type().is_file())?;
    let key = (file, meta.len(), meta.modified().ok());
    let mut memo = MEMO.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((memo_key, sizes)) = memo.as_ref() {
        if *memo_key == key {
            return Some(sizes.clone());
        }
    }
    let sizes = Arc::new(read_stale_preview_sizes(&key.0));
    *memo = Some((key, sizes.clone()));
    Some(sizes)
}

/// A unique temp dir for one extraction, avoiding collisions between concurrent
/// extractions and files that share a stem in different folders.
fn unique_tmp_dir(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let hash = fnv1a(&format!("{}|{n}", path.display()));
    std::env::temp_dir()
        .join("chairphoto-extract")
        .join(format!("{hash:016x}"))
}

/// Resolve the user cache dir (XDG_CACHE_HOME or ~/.cache), with a temp fallback.
pub(crate) fn cache_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(xdg);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".cache");
    }
    std::env::temp_dir()
}

/// Small, dependency-free 64-bit hash for cache file names (not cryptographic).
fn fnv1a(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in s.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

mod downscale;

#[cfg(test)]
mod bench;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn adobe_to_srgb_keeps_neutral_gray() {
        // A neutral gray must stay neutral (and roughly the same value).
        let mut img = RgbImage::from_pixel(2, 2, image::Rgb([128, 128, 128]));
        adobe_rgb_to_srgb(&mut img);
        let p = img.get_pixel(0, 0).0;
        assert_eq!(p[0], p[1], "stays neutral (R=G)");
        assert_eq!(p[1], p[2], "stays neutral (G=B)");
        assert!((p[0] as i32 - 128).abs() <= 4, "gray ~unchanged, got {}", p[0]);
    }

    #[test]
    fn adobe_to_srgb_boosts_saturated_red() {
        // An Adobe RGB red maps to a higher sRGB red value (sRGB needs a bigger number to
        // express the same colour) — i.e. it looks more saturated than the dull as-is view.
        let mut img = RgbImage::from_pixel(2, 2, image::Rgb([200, 100, 100]));
        adobe_rgb_to_srgb(&mut img);
        let p = img.get_pixel(0, 0).0;
        assert!(p[0] > 200, "red channel should increase, got {}", p[0]);
    }

    // --- tiers never upscale (#168) ------------------------------------------

    /// An image smaller than a tier is encoded at its own size — the zoom tier is the
    /// decode's native resolution — while a larger one still shrinks to fit.
    #[test]
    fn a_tier_never_upscales() {
        let path = Path::new("/nonexistent/upscale-check.jpg");
        let dims = |img: &DynamicImage, size: Size| {
            let bytes = encode_size(path, img, size).unwrap();
            let decoded = image::load_from_memory(&bytes).unwrap();
            (decoded.width(), decoded.height())
        };
        let small = DynamicImage::ImageRgb8(RgbImage::from_pixel(600, 400, image::Rgb([90, 120, 150])));
        assert_eq!(dims(&small, ZOOM), (600, 400), "zoom: native");
        assert_eq!(dims(&small, PREVIEW), (600, 400), "preview of a small image: native");
        assert_eq!(dims(&small, THUMB), (512, 341), "thumb: shrunk to fit");
        let tall = DynamicImage::ImageRgb8(RgbImage::from_pixel(300, 3000, image::Rgb([9, 9, 9])));
        assert_eq!(dims(&tall, PREVIEW), (205, 2048), "a long edge over the box still shrinks");
    }

    /// Each tier's cache directory: zoom under its own [`ZOOM_VERSION`], thumb and preview
    /// under the shared [`CACHE_VERSION`] — and none of them a directory an older build wrote
    /// upscaled files into (`z10000v5` before #168, `t512v5`/`p2048v5` before #245), which
    /// must never be read back as current.
    #[test]
    fn each_tier_caches_under_a_directory_no_upscaling_build_wrote() {
        let _guard = test_lock();
        let tmp_dir = TestTmpDir::new("zoom-version");
        let tmp = tmp_dir.path().to_path_buf();
        std::env::set_var("XDG_CACHE_HOME", tmp.join("cache"));
        let img = write_test_jpeg(&tmp, "zoomversion.jpg", 64, 64);

        let zoom_dir = cache_path_for(&img, ZOOM).unwrap().parent().unwrap().file_name().unwrap().to_owned();
        let preview_dir = cache_path_for(&img, PREVIEW).unwrap().parent().unwrap().file_name().unwrap().to_owned();
        let thumb_dir = cache_path_for(&img, THUMB).unwrap().parent().unwrap().file_name().unwrap().to_owned();

        assert_eq!(zoom_dir, format!("z{ZOOM_MAX}v{ZOOM_VERSION}").as_str());
        assert_eq!(preview_dir, format!("p{PREVIEW_MAX}v{CACHE_VERSION}").as_str());
        assert_eq!(thumb_dir, format!("t{THUMB_MAX}v{CACHE_VERSION}").as_str());
        assert_eq!(
            (zoom_dir.to_str().unwrap(), preview_dir.to_str().unwrap(), thumb_dir.to_str().unwrap()),
            ("z10000v6", "p2048v6", "t512v6")
        );
        for stale in [STALE_ZOOM_DIR, STALE_PREVIEW_DIR, STALE_THUMB_DIR] {
            assert!(![&zoom_dir, &preview_dir, &thumb_dir].contains(&&std::ffi::OsString::from(stale)), "{stale}");
        }
    }

    // --- pre-#245 thumbnails and previews regenerate at native size (#245) ---------------
    // Before 2497fa2 a tier fitted every decode to its box both ways, so a small original's
    // thumbnail and preview were enlarged. Those files sit in `t512v5`/`p2048v5` under the
    // same file names the current tiers use (the name hashes path, mtime and length only).

    /// A solid JPEG of `w`×`h` as an old build cached it, at `dir/name`.
    fn plant_old_tier(dir: &Path, name: &std::ffi::OsStr, w: u32, h: u32) {
        std::fs::create_dir_all(dir).unwrap();
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(RgbImage::from_pixel(w, h, image::Rgb([200, 10, 10])))
            .write_with_encoder(JpegEncoder::new_with_quality(&mut bytes, 80))
            .unwrap();
        std::fs::write(dir.join(name), bytes.into_inner()).unwrap();
    }

    /// [`plant_old_tier`] at `img`'s own preview cache name, under [`STALE_PREVIEW_DIR`] in
    /// `cache` (a `XDG_CACHE_HOME`) — for tests outside this module that exercise the real
    /// [`cached_preview_size`] path against a pre-#245 upscaled preview (review Nit-2).
    /// Its only caller today is behind `faces` (`plugins::faces::regions`), which `plugins`
    /// itself compiles out without that feature (`#[cfg(feature = "faces")] pub mod faces;`),
    /// so this is gated the same way rather than warning as dead code without it.
    #[cfg(feature = "faces")]
    pub(crate) fn plant_old_preview_tier(cache: &Path, img: &Path, w: u32, h: u32) {
        let name = cache_path_for(img, PREVIEW).unwrap().file_name().unwrap().to_owned();
        plant_old_tier(&cache.join("chairphoto").join(STALE_PREVIEW_DIR), &name, w, h);
    }

    /// The owner's decision on #245: a small original whose upscaled thumbnail and preview an
    /// old build cached gets them regenerated at its own size — the old files are never served.
    #[test]
    fn an_old_upscaled_thumbnail_and_preview_are_not_served() {
        let _guard = test_lock();
        let tmp_dir = TestTmpDir::new("old-upscaled");
        let cache = tmp_dir.path().join("cache");
        std::env::set_var("XDG_CACHE_HOME", &cache);
        let img = write_test_jpeg(tmp_dir.path(), "small.jpg", 300, 200);
        let name = cache_path_for(&img, PREVIEW).unwrap().file_name().unwrap().to_owned();
        assert_eq!(name, cache_path_for(&img, THUMB).unwrap().file_name().unwrap());
        // What the build before 2497fa2 cached for this 300x200 original.
        plant_old_tier(&cache.join("chairphoto").join(STALE_THUMB_DIR), &name, 512, 341);
        plant_old_tier(&cache.join("chairphoto").join(STALE_PREVIEW_DIR), &name, 2048, 1365);

        let dims = |bytes: Vec<u8>| image::load_from_memory(&bytes).map(|i| (i.width(), i.height())).unwrap();
        assert_eq!(dims(thumbnail_bytes(&img).unwrap()), (300, 200), "thumbnail");
        assert_eq!(dims(preview_bytes(&img).unwrap()), (300, 200), "preview");
    }

    /// The face-region writer's cross-check (#154) keeps the old preview's size until the
    /// photo's preview is regenerated: from the old file while it is there, then from what
    /// the cleanup kept of it, and once regenerated from the new preview — the same aspect
    /// throughout, so the cross-check answers as it did before the bump.
    #[test]
    fn the_preview_size_outlives_the_old_preview_directory() {
        let _guard = test_lock();
        let tmp_dir = TestTmpDir::new("old-preview-size");
        let cache = tmp_dir.path().join("cache");
        std::env::set_var("XDG_CACHE_HOME", &cache);
        let img = write_test_jpeg(tmp_dir.path(), "faces.jpg", 1200, 800);
        let other = write_test_jpeg(tmp_dir.path(), "other.jpg", 600, 400);
        let old_dir = cache.join("chairphoto").join(STALE_PREVIEW_DIR);
        let name = |p: &Path| cache_path_for(p, PREVIEW).unwrap().file_name().unwrap().to_owned();
        plant_old_tier(&old_dir, &name(&img), 2048, 1365);

        assert_eq!(cached_preview_size(&img), Some((2048, 1365)), "from the old file");
        assert_eq!(cached_preview_size(&other), None, "never cached is still unknown");

        cleanup_stale_caches();
        assert!(!old_dir.exists(), "the old preview directory is removed");
        assert_eq!(cached_preview_size(&img), Some((2048, 1365)), "from the kept sizes");
        assert_eq!(cached_preview_size(&other), None);
        assert!(!cache_path_for(&img, PREVIEW).unwrap().exists(), "asking generated nothing");

        preview_bytes(&img).unwrap();
        assert_eq!(cached_preview_size(&img), Some((1200, 800)), "the regenerated preview wins");
    }

    /// An interrupted cleanup (sizes kept, directory partly removed) loses nothing on the next
    /// start: the sizes already kept are merged with the files still there.
    #[test]
    fn a_second_cleanup_keeps_the_sizes_the_first_kept() {
        let _guard = test_lock();
        let tmp_dir = TestTmpDir::new("old-preview-merge");
        let cache = tmp_dir.path().join("cache");
        std::env::set_var("XDG_CACHE_HOME", &cache);
        let a = write_test_jpeg(tmp_dir.path(), "a.jpg", 64, 48);
        let b = write_test_jpeg(tmp_dir.path(), "b.jpg", 64, 48);
        let old_dir = cache.join("chairphoto").join(STALE_PREVIEW_DIR);
        let name = |p: &Path| cache_path_for(p, PREVIEW).unwrap().file_name().unwrap().to_owned();
        plant_old_tier(&old_dir, &name(&a), 2048, 1536);
        cleanup_stale_caches();
        plant_old_tier(&old_dir, &name(&b), 1536, 2048);
        cleanup_stale_caches();
        assert_eq!(cached_preview_size(&a), Some((2048, 1536)));
        assert_eq!(cached_preview_size(&b), Some((1536, 2048)));
    }

    // --- review fix245b: truncated sizes file, racing writers, symlinked old dir ---------

    /// LOW-1: a truncated last line — written by something other than
    /// [`keep_stale_preview_sizes`]'s own tmp+fsync+rename, which always ends the file in
    /// `\n` — parses as no entry for that line, not a wrong one. Dropping the whole line
    /// (not just letting its own parse fail) is what tells apart "the name is cut short
    /// too" (still three whitespace-separated tokens; the digits after a truncated name
    /// would mis-parse as a plausible but wrong size) from a line that is simply absent.
    #[test]
    fn read_stale_preview_sizes_drops_an_unterminated_last_line() {
        let dir = TestTmpDir::new("stale-sizes-truncated");
        let file = dir.path().join("p2048v5.sizes");

        // A complete file (trailing '\n'): every line is trusted.
        std::fs::write(&file, b"0123456789abcdef.jpg 2048 1365\n").unwrap();
        assert_eq!(
            read_stale_preview_sizes(&file),
            HashMap::from([("0123456789abcdef.jpg".to_string(), (2048, 1365))]),
        );

        // The review's own example: one line, truncated, no trailing newline at all.
        std::fs::write(&file, b"0123456789abcdef.jpg 2048 13").unwrap();
        assert_eq!(read_stale_preview_sizes(&file), HashMap::new(), "an unterminated line is never trusted");

        // A complete line followed by a truncated one: the undamaged line still lands,
        // only the damaged tail is dropped.
        std::fs::write(&file, b"0123456789abcdef.jpg 2048 1365\nfedcba9876543210.jpg 2048 13").unwrap();
        assert_eq!(
            read_stale_preview_sizes(&file),
            HashMap::from([("0123456789abcdef.jpg".to_string(), (2048, 1365))]),
            "only the undamaged line is kept"
        );
    }

    /// LOW-2: a leftover temporary sizes file — the fixed name a pre-fix build used, or one
    /// of this fix's own `<pid>.<nonce>` ones — left behind by a process that crashed or was
    /// killed before its rename landed is swept on the next cleanup. Harmless even without
    /// the sweep (nothing ever reads a `.tmp`-suffixed name back as the real sizes file),
    /// but a directory planted at such a name is left alone rather than removed, and a
    /// normal cleanup leaves no `.tmp` file behind at all.
    #[test]
    fn stray_tmp_sizes_files_are_swept_or_left_harmless() {
        let _guard = test_lock();
        let tmp_dir = TestTmpDir::new("stale-sizes-stray-tmp");
        let cache = tmp_dir.path().join("cache");
        std::env::set_var("XDG_CACHE_HOME", &cache);
        let img = write_test_jpeg(tmp_dir.path(), "stray.jpg", 64, 48);
        let root = cache.join("chairphoto");
        let old_dir = root.join(STALE_PREVIEW_DIR);
        let name = cache_path_for(&img, PREVIEW).unwrap().file_name().unwrap().to_owned();
        plant_old_tier(&old_dir, &name, 2048, 1536);

        std::fs::create_dir_all(&root).unwrap();
        // A pre-fix build's fixed name, and a stray unique one from an earlier crashed attempt.
        std::fs::write(root.join(format!("{STALE_PREVIEW_SIZES}.tmp")), b"leftover").unwrap();
        std::fs::write(root.join(format!("{STALE_PREVIEW_SIZES}.4242.7.tmp")), b"leftover too").unwrap();
        // A directory squatting a plausible tmp name: left alone, not traversed into.
        let dir_at_tmp_name = root.join(format!("{STALE_PREVIEW_SIZES}.9999.0.tmp"));
        std::fs::create_dir_all(&dir_at_tmp_name).unwrap();
        std::fs::write(dir_at_tmp_name.join("keep.txt"), b"not ours to remove").unwrap();

        cleanup_stale_caches();

        assert!(!root.join(format!("{STALE_PREVIEW_SIZES}.tmp")).exists(), "the pre-fix fixed name is swept");
        assert!(!root.join(format!("{STALE_PREVIEW_SIZES}.4242.7.tmp")).exists(), "a stray unique temp is swept");
        assert!(dir_at_tmp_name.join("keep.txt").exists(), "a directory at a tmp-shaped name is left alone");
        assert_eq!(cached_preview_size(&img), Some((2048, 1536)), "the real write still landed");
        let any_tmp_file_left = std::fs::read_dir(&root).unwrap().flatten().any(|e| {
            e.file_name().to_str().is_some_and(|n| n.ends_with(".tmp"))
                && !matches!(std::fs::symlink_metadata(e.path()), Ok(meta) if meta.file_type().is_dir())
        });
        assert!(!any_tmp_file_left, "no .tmp regular file remains after a successful cleanup");
    }

    /// Nit-1: [`cached_preview_size`]'s fallback refuses a symlinked old preview directory
    /// just as [`cleanup_stale_caches`] does — never following it to read a header.
    #[test]
    #[cfg(unix)]
    fn cached_preview_size_never_follows_a_symlinked_old_directory() {
        let _guard = test_lock();
        let tmp_dir = TestTmpDir::new("stale-preview-symlink-read");
        let cache = tmp_dir.path().join("cache");
        std::env::set_var("XDG_CACHE_HOME", &cache);
        let img = write_test_jpeg(tmp_dir.path(), "linked.jpg", 1200, 800);

        let elsewhere = tmp_dir.path().join("elsewhere");
        let name = cache_path_for(&img, PREVIEW).unwrap().file_name().unwrap().to_owned();
        plant_old_tier(&elsewhere, &name, 2048, 1365);

        let root = cache.join("chairphoto");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join(STALE_PREVIEW_DIR)).unwrap();

        assert_eq!(cached_preview_size(&img), None, "a symlinked old directory is never read");
    }

    // --- decode-once chain + analyzer hook -----------------------------------
    // These tests touch the global on-disk cache (via XDG_CACHE_HOME) and the global
    // analyzer registry, both process-wide. Serialize them behind one mutex so the env
    // var and the registry can't be mutated by two tests at once.
    use std::sync::atomic::AtomicUsize;

    /// A test's own temp directory, removed on drop.
    ///
    /// The name carries the process id. Keying only on a constant — as these tests did —
    /// gives every `cargo test` process on the machine the same path, and each test's
    /// cleanup then deletes a directory another process is still writing into, which
    /// surfaces as `No such file or directory` from `write_test_jpeg` rather than as
    /// anything to do with thumbnails. Two worktrees, or a targeted run beside a full one,
    /// is enough to trigger it.
    ///
    /// Dropping rather than calling `remove_dir_all` at the end of each test also means a
    /// panicking test cleans up after itself; the old placement left the directory behind,
    /// which is how 7.9 GB of fixtures accumulated in `/tmp`.
    pub(crate) struct TestTmpDir(PathBuf);

    impl TestTmpDir {
        pub(crate) fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("cp-thumb-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestTmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Clear the analyzer registry, then install a single counter that increments once per
    /// decode. Returns the counter. The registry is process-global, so callers hold
    /// `test_lock()` for the duration.
    fn install_decode_counter() -> std::sync::Arc<AtomicUsize> {
        if let Ok(mut list) = analyzers().lock() {
            list.clear();
        }
        let counter = std::sync::Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        register_analyzer(Arc::new(move |_img, _path| {
            c.fetch_add(1, Ordering::Relaxed);
        }));
        counter
    }

    /// Write a small solid-colour JPEG to a unique path under `dir` so each test has its
    /// own cache key (the key is path+mtime+size). A plain raster decodes the *same*
    /// source file for every size, so chain-derived and independently-generated caches
    /// downscale from an identical decode — giving byte-identical results.
    pub(crate) fn write_test_jpeg(dir: &Path, name: &str, w: u32, h: u32) -> PathBuf {
        let mut img = RgbImage::new(w, h);
        // A non-uniform gradient so downscaling is a real resample (not a trivial fill).
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8]);
        }
        let path = dir.join(name);
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(img)
            .write_with_encoder(JpegEncoder::new_with_quality(&mut bytes, 90))
            .unwrap();
        std::fs::write(&path, bytes.into_inner()).unwrap();
        path
    }

    // --- stale cache cleanup (#168 review, #245) ------------------------------
    // Shares the env-var lock with the tests above: XDG_CACHE_HOME is process-global.

    #[test]
    fn cleanup_stale_caches_removes_only_the_old_directories() {
        let _guard = test_lock();
        let tmp_dir = TestTmpDir::new("stale-dirs");
        let cache = tmp_dir.path().join("cache");
        std::env::set_var("XDG_CACHE_HOME", &cache);
        let root = cache.join("chairphoto");

        for stale in [STALE_ZOOM_DIR, STALE_THUMB_DIR, STALE_PREVIEW_DIR, STALE_COVER_DIR] {
            std::fs::create_dir_all(root.join(stale)).unwrap();
            std::fs::write(root.join(stale).join("deadbeefdeadbeef.jpg"), b"old upscaled tier").unwrap();
        }
        // The current tiers and the id-keyed persistent thumbnails must survive untouched.
        let keep = ["z10000v6", "p2048v6", "t512v6", "cover512v2", "persist"];
        for dir in keep {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            std::fs::write(root.join(dir).join("keep.jpg"), b"current").unwrap();
        }

        cleanup_stale_caches();

        for stale in [STALE_ZOOM_DIR, STALE_THUMB_DIR, STALE_PREVIEW_DIR, STALE_COVER_DIR] {
            assert!(!root.join(stale).exists(), "{stale} should be gone");
        }
        for dir in keep {
            assert!(root.join(dir).join("keep.jpg").exists(), "{dir} must be untouched");
        }
        // The unreadable old "preview" kept no size, but the sizes file was still written.
        assert!(root.join(STALE_PREVIEW_SIZES).is_file());
    }

    #[test]
    fn cleanup_stale_caches_is_a_silent_no_op_when_absent() {
        let _guard = test_lock();
        let tmp_dir = TestTmpDir::new("stale-absent");
        let cache = tmp_dir.path().join("cache");
        std::env::set_var("XDG_CACHE_HOME", &cache);
        // Nothing to remove; must not panic, and makes nothing.
        cleanup_stale_caches();
        assert!(!cache.exists());
    }

    /// A symlink planted at an exact stale name is never followed: neither it nor whatever
    /// it points at is touched. `symlink_metadata` sees the link, not a directory, so the
    /// removal refuses outright. A symlink inside the old preview directory is not read for
    /// a size, and one at the sizes file's temporary name is replaced, not written through.
    #[test]
    #[cfg(unix)]
    fn cleanup_stale_caches_never_follows_a_symlink() {
        let _guard = test_lock();
        let tmp_dir = TestTmpDir::new("stale-symlink");
        let cache = tmp_dir.path().join("cache");
        std::env::set_var("XDG_CACHE_HOME", &cache);

        let elsewhere = tmp_dir.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("precious.txt"), b"not ours to delete").unwrap();
        plant_old_tier(&elsewhere, std::ffi::OsStr::new("0123456789abcdef.jpg"), 40, 30);

        let root = cache.join("chairphoto");
        std::fs::create_dir_all(&root).unwrap();
        for stale in [STALE_ZOOM_DIR, STALE_THUMB_DIR, STALE_PREVIEW_DIR, STALE_COVER_DIR] {
            std::os::unix::fs::symlink(&elsewhere, root.join(stale)).unwrap();
        }

        cleanup_stale_caches();

        for stale in [STALE_ZOOM_DIR, STALE_THUMB_DIR, STALE_PREVIEW_DIR, STALE_COVER_DIR] {
            let meta = std::fs::symlink_metadata(root.join(stale)).expect("the symlink itself must still exist");
            assert!(meta.file_type().is_symlink(), "{stale} must remain a symlink, not be removed or replaced");
        }
        assert!(elsewhere.join("precious.txt").exists(), "the symlinks' target must be untouched");
        assert!(!root.join(STALE_PREVIEW_SIZES).exists(), "a symlinked preview directory is not read");

        // A real old preview directory holding a symlinked "preview", and a symlink at the
        // sizes file's temporary name.
        std::fs::remove_file(root.join(STALE_PREVIEW_DIR)).unwrap();
        std::fs::create_dir_all(root.join(STALE_PREVIEW_DIR)).unwrap();
        let inner = root.join(STALE_PREVIEW_DIR).join("0123456789abcdef.jpg");
        std::os::unix::fs::symlink(elsewhere.join("0123456789abcdef.jpg"), inner).unwrap();
        let target = elsewhere.join("tmp-target");
        std::fs::write(&target, b"not ours to write").unwrap();
        std::os::unix::fs::symlink(&target, root.join(format!("{STALE_PREVIEW_SIZES}.tmp"))).unwrap();

        cleanup_stale_caches();

        assert!(!root.join(STALE_PREVIEW_DIR).exists(), "the real directory is removed");
        assert_eq!(std::fs::read(&target).unwrap(), b"not ours to write", "the temporary name's target is untouched");
        assert!(elsewhere.join("0123456789abcdef.jpg").exists(), "a symlinked preview's target is untouched");
        let sizes = std::fs::read_to_string(root.join(STALE_PREVIEW_SIZES)).unwrap();
        assert_eq!(sizes, "", "a symlinked preview kept no size");
        assert!(std::fs::symlink_metadata(root.join(STALE_PREVIEW_SIZES)).unwrap().file_type().is_file());
    }

    #[test]
    fn analyzer_hook_fires_once_per_decode() {
        let _guard = test_lock();
        let tmp_dir = TestTmpDir::new("hook");
        let tmp = tmp_dir.path().to_path_buf();
        std::env::set_var("XDG_CACHE_HOME", tmp.join("cache"));
        let counter = install_decode_counter();

        // Use an image large enough that its longest edge exceeds PREVIEW_MAX (2048px), so
        // a preview-size decode doesn't down-sample it to a size where the hook is gated off.
        let img = write_test_jpeg(&tmp, "hook.jpg", 2200, 1800);

        // A thumbnail request must NOT fire the hook — THUMB (512px) is below the resolution
        // gate (PREVIEW_MAX = 2048). Scoring micro-blur at 512px is inaccurate.
        thumbnail_bytes(&img).unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 0, "thumb decode must not fire the hook");

        // A preview request (PREVIEW_MAX = 2048px) exceeds the gate → hook fires once.
        preview_bytes(&img).unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 1, "preview decode fires the hook once");

        // Second preview request is a cache hit: no decode, no hook.
        preview_bytes(&img).unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 1, "cache hit fires no hook");

        // Cleanup: drop the test analyzer so it doesn't leak into other tests.
        if let Ok(mut list) = analyzers().lock() {
            list.clear();
        }
    }

    /// #154: the face-region writer's preview size comes from the cached preview's header —
    /// nothing until the preview is cached (and asking generates nothing), then its size.
    #[test]
    fn cached_preview_size_reads_only_the_cache() {
        let _guard = test_lock();
        let tmp_dir = TestTmpDir::new("cachedsize");
        let tmp = tmp_dir.path().to_path_buf();
        std::env::set_var("XDG_CACHE_HOME", tmp.join("cache"));
        let img = write_test_jpeg(&tmp, "cachedsize.jpg", 1200, 900);

        assert_eq!(cached_preview_size(&img), None, "not cached yet");
        assert!(!cache_path_for(&img, PREVIEW).unwrap().exists(), "asking generated it");
        let preview = image::load_from_memory(&preview_bytes(&img).unwrap()).unwrap();
        assert_eq!(cached_preview_size(&img), Some((preview.width(), preview.height())));
    }

    #[test]
    fn single_small_request_does_not_over_decode() {
        let _guard = test_lock();
        let tmp_dir = TestTmpDir::new("nodecode");
        let tmp = tmp_dir.path().to_path_buf();
        std::env::set_var("XDG_CACHE_HOME", tmp.join("cache"));
        let counter = install_decode_counter();

        let img = write_test_jpeg(&tmp, "nodecode.jpg", 1200, 900);

        // A lone thumbnail request must decode exactly once — never a preview/zoom-size
        // decode on the grid pool's critical path. The THUMB decode (512px) is below the
        // resolution gate, so the hook must NOT fire.
        thumbnail_bytes(&img).unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 0, "lone thumb → no hook (below resolution gate)");

        // A preview request now needs its own (larger) decode: the thumb tier was derived
        // *down* from the thumb decode and can't serve a preview. The preview decode is at
        // PREVIEW_MAX (2048px) which meets the resolution gate → hook fires once.
        preview_bytes(&img).unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 1, "preview decode fires the hook once");

        // …but the preview decode opportunistically re-derived every smaller tier, so a
        // second thumbnail request is a pure cache hit — no further decode, no further hook.
        let n = counter.load(Ordering::Relaxed);
        thumbnail_bytes(&img).unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), n, "smaller tiers ride the larger decode");

        if let Ok(mut list) = analyzers().lock() {
            list.clear();
        }
    }

    #[test]
    fn chain_matches_independent_generation() {
        let _guard = test_lock();
        let tmp_dir = TestTmpDir::new("chain");
        let tmp = tmp_dir.path().to_path_buf();

        // Two identical rasters at distinct paths → distinct cache keys, no cross-talk.
        let chain_img = write_test_jpeg(&tmp, "chain.jpg", 2400, 1600);
        let indep_img = write_test_jpeg(&tmp, "indep.jpg", 2400, 1600);

        // (a) The chain: one decode fills every size.
        std::env::set_var("XDG_CACHE_HOME", tmp.join("cache-chain"));
        let counter = install_decode_counter();
        warm_all_sizes(&chain_img).unwrap();
        assert_eq!(
            counter.load(Ordering::Relaxed),
            1,
            "warm_all_sizes decodes exactly once for all three sizes"
        );
        let chain_thumb = thumbnail_bytes(&chain_img).unwrap();
        let chain_preview = preview_bytes(&chain_img).unwrap();
        let chain_zoom = zoom_bytes(&chain_img).unwrap();
        // All served from cache: still just the one decode.
        assert_eq!(counter.load(Ordering::Relaxed), 1, "sizes served from cache");

        // (b) Independent generation: encode each size directly from a fresh full decode,
        // the way the code did before I7b (extract at the size's own max, downscale, encode).
        let full = extract_and_decode(&indep_img, ZOOM.max).unwrap();
        let indep_thumb = encode_size(&indep_img, &full, THUMB).unwrap();
        let indep_preview = encode_size(&indep_img, &full, PREVIEW).unwrap();
        let indep_zoom = encode_size(&indep_img, &full, ZOOM).unwrap();

        // For a plain raster, both routes downscale from the same decode → byte-identical.
        assert_eq!(chain_thumb, indep_thumb, "thumb: chain == independent");
        assert_eq!(chain_preview, indep_preview, "preview: chain == independent");
        assert_eq!(chain_zoom, indep_zoom, "zoom: chain == independent");

        if let Ok(mut list) = analyzers().lock() {
            list.clear();
        }
    }
}
