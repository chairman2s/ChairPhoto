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
/// preview selection, …), so stale cached images are regenerated, not reused.
///
/// v5: single-decode downscale chain — a preview/zoom decode now opportunistically
/// derives (and caches) the smaller sizes from the one in-hand decode. Thumbnails
/// derived from a larger decode differ pixel-for-pixel from an independently
/// extracted small embedded preview, so old caches must not be reused.
const CACHE_VERSION: u32 = 5;

/// One cache size: its longest-edge cap, on-disk tag, and JPEG quality.
#[derive(Clone, Copy)]
struct Size {
    max: u32,
    tag: &'static str,
    quality: u8,
}

const THUMB: Size = Size { max: THUMB_MAX, tag: "t", quality: 80 };
const PREVIEW: Size = Size { max: PREVIEW_MAX, tag: "p", quality: 85 };
const ZOOM: Size = Size { max: ZOOM_MAX, tag: "z", quality: 92 };

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

/// Rotate already-encoded JPEG bytes clockwise by `degrees` (0/90/180/270) and
/// re-encode. Used to apply a photo's non-destructive user-rotation override on top of
/// the EXIF-oriented preview. Rotation by a multiple of 90° resamples nothing (pure
/// pixel permutation); the re-encode keeps display quality. A no-op for 0.
pub fn rotate_jpeg(bytes: Vec<u8>, degrees: i64) -> Result<Vec<u8>, String> {
    let d = ((degrees % 360) + 360) % 360;
    if d == 0 {
        return Ok(bytes);
    }
    let img = image::load_from_memory(&bytes).map_err(|e| e.to_string())?;
    let rotated = match d {
        90 => img.rotate90(),
        180 => img.rotate180(),
        270 => img.rotate270(),
        _ => img,
    };
    let mut out = Cursor::new(Vec::new());
    rotated
        .write_with_encoder(JpegEncoder::new_with_quality(&mut out, 90))
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
    let cache_path = cache_path_for(path, size.max, size.tag)?;
    if let Ok(bytes) = std::fs::read(&cache_path) {
        return Ok(bytes);
    }
    // Decode once for this size. The decode we just paid for is a strict superset of
    // every smaller tier, so derive and cache those too — a lone thumb request stays a
    // thumb decode (no over-decode), but a preview/zoom decode also fills the smaller
    // tiers so the next grid request is a free cache hit.
    let img = extract_and_decode(path, size.max)?;
    run_analyzers(&img, path, size.max);
    let bytes = generate_from_decode(path, &img, size)?;
    for smaller in smaller_sizes(size.max) {
        let cp = cache_path_for(path, smaller.max, smaller.tag)?;
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
        let cp = cache_path_for(path, s.max, s.tag)?;
        if !cp.exists() {
            missing.push(s);
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    // Decode once at the largest needed size; the smaller tiers downscale from it.
    let largest = missing.iter().map(|s| s.max).max().unwrap();
    let img = extract_and_decode(path, largest)?;
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
    let cache_path = cache_path_for(path, size.max, size.tag)?;
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&cache_path, &bytes).ok();
    Ok(bytes)
}

/// Downscale a decoded image to `size` and JPEG-encode it (with Adobe-RGB→sRGB when the
/// source file is Adobe RGB). Pure — no disk writes.
fn encode_size(path: &Path, img: &DynamicImage, size: Size) -> Result<Vec<u8>, String> {
    // thumbnail() only downscales, so a max larger than the image leaves it native.
    let resized = img.thumbnail(size.max, size.max);
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
/// them through ImageMagick's libheif delegate instead (see `decode_heic`).
fn is_heic(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("heic" | "heif")
    )
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
/// the hash covers path + mtime + size so edits invalidate the cache.
fn cache_path_for(path: &Path, max: u32, tag: &str) -> Result<PathBuf, String> {
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
        .join(format!("{tag}{max}v{CACHE_VERSION}"));
    Ok(base.join(format!("{hash:016x}.jpg")))
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
fn cache_dir() -> PathBuf {
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

#[cfg(test)]
mod tests {
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
    struct TestTmpDir(PathBuf);

    impl TestTmpDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("cp-thumb-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestTmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
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
    fn write_test_jpeg(dir: &Path, name: &str, w: u32, h: u32) -> PathBuf {
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
