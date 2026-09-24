//! The basic editor's **render engine** (behind the `edit` Cargo feature): applies a
//! version's non-destructive edit record — normalized geometry (perspective, straighten,
//! crop) plus tone and look adjustments — to a
//! source JPEG and returns a new JPEG. It never touches the original file; the caller
//! feeds it the cached preview proxy (live editing/loupe) or, later, a full RAW decode
//! (export). See docs/editing.md. The edit record shape is owned here but stays opaque
//! to the catalog core.

mod auto;
pub mod cover;
#[cfg(test)]
mod bench;
pub mod cube;
pub mod linear;
mod look;
pub mod source;
pub mod timing;
mod zones;

pub use auto::auto_tone_for;
pub use source::{RenderSource, SourceToken, WorkingImage};
pub use zones::zone_masses;

use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, GenericImageView, ImageBuffer, Pixel, Rgb, Rgb32FImage, RgbImage};
use std::sync::Mutex;
use look::{Bw, Grain, Split};
use serde::Deserialize;

/// A version's edit record. All fields optional/defaulted so a partial/empty record
/// renders as a no-op (an unedited copy).
#[derive(Deserialize, Default)]
struct EditRecord {
    #[serde(default)]
    crop: Option<Crop>,
    #[serde(default)]
    tone: Tone,
    /// Four-corner perspective (keystone) correction. `None` = leave the geometry alone.
    #[serde(default)]
    perspective: Option<Perspective>,
    /// Straighten angle in degrees (rotate the image about its centre to level it). The
    /// UI pairs this with an inscribed crop so the rotation's empty corners stay out of
    /// frame. Positive matches a line's screen-space tilt (y-down), so `-tilt` levels it.
    #[serde(default)]
    straighten: f32,
    /// Black & white conversion (channel mixer). `None` = colour.
    #[serde(default)]
    bw: Option<Bw>,
    /// Split toning (sepia, selenium, teal/orange…). `None` = no toning.
    #[serde(default)]
    split: Option<Split>,
    /// Film grain (deterministic; see [`look::Grain`]). `None` = no grain.
    #[serde(default)]
    grain: Option<Grain>,
    /// Lifted matte blacks, 0..1.
    #[serde(default)]
    fade: f32,
    /// Corner shading, -1..1 (negative darkens).
    #[serde(default)]
    vignette: f32,
    /// Optional user-supplied .cube LUT applied to the developed image.
    #[serde(default)]
    lut: Option<LutRef>,
    /// Tone-strip zone offsets in EV, blacks→whites (docs/plans/darkroom). `None` = no
    /// zone curve; a zeroed strip renders identically to none (locked by a test).
    #[serde(default)]
    zones: Option<[f32; 8]>,
    /// Which engine this record was made for (docs/plans/raw-foundation). Absent = 1: the
    /// gamma-domain pipeline on the camera preview, rendered exactly as it always was.
    /// 2: the scene-linear pipeline on the RAW working image. Never reinterpreted.
    #[serde(default = "engine_v1")]
    engine: u32,
    /// Engine 2's rendering transform slot (`linear::DisplayTransform::from_record`).
    #[serde(default)]
    display: Option<String>,
    /// Engine 2: the exposure offset, in EV, that matched this photo's camera JPEG when
    /// the record was made (`linear::camera_match_ev`) — added to the baseline lift, not
    /// shown on the Exposure slider. Absent = 0, so older records render as they did.
    #[serde(default, rename = "cameraEv")]
    camera_ev: f32,
}

fn engine_v1() -> u32 {
    1
}

/// The engine a record asks for, without rendering anything (export dispatch).
pub fn record_engine(edit_json: &str) -> u32 {
    parse_record(edit_json).map(|e| e.engine).unwrap_or(1)
}

/// Reference to a `.cube` LUT by bare filename, resolved against the app-data `luts/`
/// folder — never a path, so edit records stay portable across machines. A missing or
/// corrupt file renders as if no LUT were set (non-fatal).
#[derive(Deserialize)]
struct LutRef {
    file: String,
    /// Blend between the un-LUT-ed and LUT-ed image, 0..1.
    #[serde(default = "one")]
    amount: f32,
}

/// Crop rectangle as fractions (0–1) of the source, so one record works on any size.
#[derive(Deserialize)]
struct Crop {
    #[serde(default)]
    x: f32,
    #[serde(default)]
    y: f32,
    #[serde(default = "one")]
    w: f32,
    #[serde(default = "one")]
    h: f32,
    /// Optional locked aspect ratio (e.g. "1:1", "4:5"). When set, the output is forced
    /// to *exactly* this ratio — independent rounding of the w/h fractions across source
    /// sizes would otherwise leave a "1:1" crop off-square by a pixel.
    #[serde(default)]
    aspect: Option<String>,
}

/// Four-corner perspective (keystone) correction: the source quadrilateral that should
/// become the output rectangle, as fractions of the source in the same convention as
/// [`Crop`] — so one record renders identically on the preview proxy and the full-size
/// original. Photographing a framed picture or a document off-axis turns its rectangle
/// into a trapezoid; mapping that quad back onto a rectangle undoes it.
///
/// Corners are named by their position *on the subject*, so a tilted subject is expressed
/// by which corner is which — one quad carries rotation and keystone together, and no
/// separate angle is needed alongside it.
#[derive(Deserialize)]
struct Perspective {
    tl: [f32; 2],
    tr: [f32; 2],
    br: [f32; 2],
    bl: [f32; 2],
    /// Output aspect (width / height). Absent = derive it from the quad's mean edge
    /// lengths, which is right for a roughly square-on shot and degrades gracefully
    /// otherwise. Recovering the subject's true ratio needs the camera's focal length,
    /// which this engine never sees — it is handed a JPEG, not EXIF — so the UI computes
    /// that and writes an explicit value here.
    #[serde(default)]
    aspect: Option<f32>,
}

/// Largest output edge a [`Perspective`] record may ask for, as a multiple of the source's
/// longest edge. The quad is user-supplied data: without a cap, a mis-dragged handle or a
/// corrupt record could ask for a multi-gigapixel canvas and take the app down.
const MAX_PERSPECTIVE_SCALE: f32 = 4.0;

/// Parse an "A:B" aspect string into the ratio A/B (width over height). Returns `None`
/// for "Free"/"Original"/unparseable values.
fn parse_aspect(aspect: &Option<String>) -> Option<f32> {
    let s = aspect.as_deref()?.trim();
    let (a, b) = s.split_once(':')?;
    let a: f32 = a.trim().parse().ok()?;
    let b: f32 = b.trim().parse().ok()?;
    if a > 0.0 && b > 0.0 {
        Some(a / b)
    } else {
        None
    }
}

fn one() -> f32 {
    1.0
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Tone {
    ev: f32,         // exposure in stops
    contrast: f32,   // -1..1
    highlights: f32, // -1..1 (negative recovers, positive boosts)
    shadows: f32,    // -1..1 (positive lifts)
    whites: f32,     // -1..1 (positive clips brights, negative pulls top end down)
    blacks: f32,     // -1..1 (negative crushes darks, positive lifts black point)
    vibrance: f32,   // -1..1 (saturation boost weighted toward low-sat pixels)
    saturation: f32, // -1..1 (-1 = greyscale)
    wb: Wb,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Wb {
    temp: f32, // -1..1 relative (warm/cool)
    tint: f32, // -1..1 relative (green/magenta)
    /// Engine 2 only: `"relative"` (default) or `"kelvin"` — see `linear::WbSpec`.
    mode: Option<String>,
    /// Engine 2, `mode: "kelvin"`: the scene light in Kelvin.
    kelvin: Option<f32>,
}

/// Whether an edit record renders black & white — an enabled B&W mixer or full
/// desaturation. Drives the monochrome auto-tag (H6); the renderer doesn't use it.
/// Unparseable/empty records are simply "not B&W".
pub fn is_bw(edit_json: &str) -> bool {
    let trimmed = edit_json.trim();
    if trimmed.is_empty() {
        return false;
    }
    match serde_json::from_str::<EditRecord>(trimmed) {
        Ok(edit) => {
            edit.bw.as_ref().is_some_and(|b| b.enabled) || edit.tone.saturation <= -0.999
        }
        Err(_) => false,
    }
}

/// Render `jpeg` with the edits in `edit_json`. `max_edge` (when > 0) downscales the
/// result's longest edge — used to keep live preview fast; pass 0 for full size.
pub fn render_jpeg(jpeg: &[u8], edit_json: &str, max_edge: u32) -> Result<Vec<u8>, String> {
    let img = image::load_from_memory(jpeg).map_err(|e| e.to_string())?;
    let out = render_image(img, edit_json, max_edge)?;
    encode_jpeg(&out, 90)
}

/// One-slot cache of the last decoded proxy: live slider drags render the same proxy
/// many times a second, and the JPEG decode is a large share of each render's cost.
/// Keyed by a fingerprint of the bytes (length + head + tail), so a regenerated proxy
/// (rotation, recovery) can never serve stale pixels. The clone hands the caller its
/// own buffer — a memcpy, ~30× cheaper than a decode.
static DECODE_CACHE: Mutex<Option<(u64, DynamicImage)>> = Mutex::new(None);

fn jpeg_fingerprint(jpeg: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    jpeg.len().hash(&mut h);
    jpeg[..jpeg.len().min(4096)].hash(&mut h);
    jpeg[jpeg.len().saturating_sub(1024)..].hash(&mut h);
    h.finish()
}

/// Decode a proxy JPEG through the one-slot cache. Use for interactive proxy renders
/// only — the hi-res zoom tier is too large to keep resident.
pub fn decode_proxy_cached(jpeg: &[u8]) -> Result<DynamicImage, String> {
    decode_proxy_cached_fp(jpeg_fingerprint(jpeg), jpeg)
}

fn decode_proxy_cached_fp(fp: u64, jpeg: &[u8]) -> Result<DynamicImage, String> {
    if let Some((cached_fp, img)) = &*DECODE_CACHE.lock().unwrap() {
        if *cached_fp == fp {
            return Ok(img.clone());
        }
    }
    let img = image::load_from_memory(jpeg).map_err(|e| e.to_string())?;
    *DECODE_CACHE.lock().unwrap() = Some((fp, img.clone()));
    Ok(img)
}

/// Encode an image to JPEG bytes at `quality` (1–100).
pub fn encode_jpeg(img: &DynamicImage, quality: u8) -> Result<Vec<u8>, String> {
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_with_encoder(JpegEncoder::new_with_quality(&mut out, quality))
        .map_err(|e| e.to_string())?;
    Ok(out.into_inner())
}

/// Encode an image as PNG, fastest compression, no filtering — lossless where that is
/// the point (the GL drag tier's base texture) and cheap enough for a per-geometry render.
pub fn encode_png_fast(img: &DynamicImage) -> Result<Vec<u8>, String> {
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_with_encoder(PngEncoder::new_with_quality(
        &mut out,
        CompressionType::Fast,
        FilterType::NoFilter,
    ))
    .map_err(|e| e.to_string())?;
    Ok(out.into_inner())
}

/// Per-render switches that are not part of the record.
#[derive(Clone, Copy, Default, Debug)]
pub struct RenderOpts {
    /// Geometry as the record says (perspective, straighten, crop, downscale) and no look:
    /// the base the GL drag tier shades in the webview (docs/plans/darkroom/00-status.md).
    pub skip_look: bool,
}

/// Apply the edits in `edit_json` (normalized geometry + tone) to an already-decoded
/// 8-bit image — the export path's full-res source and the tests. Engine 1 only: an
/// engine-2 record needs the RAW working image ([`render_image_opts`] with
/// [`RenderSource::Working`]) and is refused here rather than rendered from the wrong pixels.
pub fn render_image(
    img: DynamicImage,
    edit_json: &str,
    max_edge: u32,
) -> Result<DynamicImage, String> {
    render_image_opts(RenderSource::Decoded(img), edit_json, max_edge, RenderOpts::default())
}

/// Render `src` with the record, dispatching on the record's engine. Engine 1 takes a
/// preview JPEG or a decoded 8-bit image; engine 2 takes the working image. A mismatch is
/// an error, never a silent substitution (docs/plans/raw-foundation).
pub fn render_image_opts(
    src: RenderSource<'_>,
    edit_json: &str,
    max_edge: u32,
    opts: RenderOpts,
) -> Result<DynamicImage, String> {
    let mut t = timing::Stages::start(format!(
        "render_image max_edge={max_edge} skip_look={}",
        opts.skip_look
    ));
    let edit = parse_record(edit_json)?;
    t.mark("parse");
    let rgb = match (edit.engine, src) {
        (1, RenderSource::PreviewJpeg(jpeg)) => {
            let img = image::load_from_memory(jpeg).map_err(|e| e.to_string())?;
            t.mark("decode");
            let framed = frame_image(img, &edit, max_edge, &mut t);
            finish_look(framed.to_rgb8(), &edit, opts, &mut t)
        }
        (1, RenderSource::Decoded(img)) => {
            let framed = frame_image(img, &edit, max_edge, &mut t);
            finish_look(framed.to_rgb8(), &edit, opts, &mut t)
        }
        (2, RenderSource::Working { image, .. }) => {
            let framed = frame_image(DynamicImage::ImageRgb32F(image.linear.clone()), &edit, max_edge, &mut t);
            finish_linear(framed.into_rgb32f(), &edit, &image, opts, &mut t)?
        }
        (1, RenderSource::Working { .. }) => {
            return Err("an engine-1 record renders from the camera preview, not the working image".into())
        }
        (2, _) => return Err("an engine-2 record needs the RAW working image; none is resident".into()),
        (n, _) => return Err(format!("unknown edit engine {n}")),
    };
    t.report(&format!("out={}x{}", rgb.width(), rgb.height()));
    Ok(DynamicImage::ImageRgb8(rgb))
}

/// Render the interactive tier through both caches: the decoded proxy (one slot, engine 1)
/// and the **framed base** — the source after geometry and downscale, before the look.
/// A slider drag changes only the look, so every frame after the first skips the
/// perspective/straighten/crop/`thumbnail()` stages entirely and pays the look plus the
/// encode. Keyed by the source (proxy bytes' fingerprint, or the working image's token),
/// the record's geometry, and the edge; output is byte-identical to [`render_image_opts`]
/// on the same source (locked by `framed_base_cache_renders_identically_on_miss_and_hit`).
pub fn render_proxy(
    src: RenderSource<'_>,
    edit_json: &str,
    max_edge: u32,
    opts: RenderOpts,
) -> Result<DynamicImage, String> {
    let mut t = timing::Stages::start(format!(
        "render_proxy max_edge={max_edge} skip_look={}",
        opts.skip_look
    ));
    let edit = parse_record(edit_json)?;
    t.mark("parse");
    let geometry = geometry_fingerprint(&edit);
    let rgb = match (edit.engine, src) {
        (1, RenderSource::PreviewJpeg(jpeg)) => {
            let fp = jpeg_fingerprint(jpeg);
            let key = FramedKey { source: fp, geometry, max_edge };
            let base = match framed_cache_get(&key) {
                Some(FramedBase::Rgb8(base)) => {
                    t.mark("framed_cache_hit");
                    base
                }
                _ => {
                    let img = decode_proxy_cached_fp(fp, jpeg)?;
                    t.mark("decode_cache");
                    let base = frame_image(img, &edit, max_edge, &mut t).to_rgb8();
                    framed_cache_put(key, FramedBase::Rgb8(base.clone()));
                    t.mark("framed_cache_put");
                    base
                }
            };
            finish_look(base, &edit, opts, &mut t)
        }
        (2, RenderSource::Working { token, image }) => {
            let base = framed_linear(&token, &image, &edit, geometry, max_edge, &mut t);
            finish_linear(base, &edit, &image, opts, &mut t)?
        }
        (1, _) => return Err("engine 1 renders from the camera preview".into()),
        (2, _) => return Err("an engine-2 record needs the RAW working image; none is resident".into()),
        (n, _) => return Err(format!("unknown edit engine {n}")),
    };
    t.report(&format!("out={}x{}", rgb.width(), rgb.height()));
    Ok(DynamicImage::ImageRgb8(rgb))
}

/// The working image after the record's geometry and the downscale, through the
/// framed-base cache.
fn framed_linear(
    token: &SourceToken,
    image: &WorkingImage,
    edit: &EditRecord,
    geometry: u64,
    max_edge: u32,
    t: &mut timing::Stages,
) -> Rgb32FImage {
    let key = FramedKey { source: token_fingerprint(token), geometry, max_edge };
    if let Some(FramedBase::Linear(base)) = framed_cache_get(&key) {
        t.mark("framed_cache_hit");
        return base;
    }
    let framed = frame_image(DynamicImage::ImageRgb32F(image.linear.clone()), edit, max_edge, t);
    let base = framed.into_rgb32f();
    // A full-size linear base is ~800 MB for a 67 MP frame and the cache copies on every
    // hit: only screen-sized bases are kept.
    if base.width().max(base.height()) <= FRAMED_CACHE_MAX_LINEAR_EDGE {
        framed_cache_put(key, FramedBase::Linear(base.clone()));
        t.mark("framed_cache_put");
    }
    base
}

/// A pixel of the framed working image counts as sensor-clipped when any channel is at
/// sensor white. Just under 1.0, because the area-average downscale of a fully clipped
/// block can land a rounding step below it; a block only partly clipped averages lower
/// and is not marked.
pub const CLIP_AT: f32 = 0.999;

/// The stage's sensor-clipping overlay (docs/plans/raw-foundation, mockup 02): for the
/// same record geometry and `max_edge` as the stage render, a transparent PNG marked
/// magenta wherever the RAW itself is clipped — the only white that is gone for good, and
/// unmoved by any slider. Engine 2 only.
pub fn clip_overlay_png(
    token: SourceToken,
    image: std::sync::Arc<WorkingImage>,
    edit_json: &str,
    max_edge: u32,
) -> Result<Vec<u8>, String> {
    let edit = parse_record(edit_json)?;
    if edit.engine != 2 {
        return Err("the sensor-clipping overlay is for engine-2 records".into());
    }
    let mut t = timing::Stages::start(format!("clip_overlay max_edge={max_edge}"));
    let base = framed_linear(&token, &image, &edit, geometry_fingerprint(&edit), max_edge, &mut t);
    let mask = image::RgbaImage::from_fn(base.width(), base.height(), |x, y| {
        if base.get_pixel(x, y).0.iter().any(|&c| c >= CLIP_AT) {
            image::Rgba([255, 0, 255, 190])
        } else {
            image::Rgba([0, 0, 0, 0])
        }
    });
    t.mark("mask");
    let mut out = std::io::Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(mask)
        .write_with_encoder(image::codecs::png::PngEncoder::new_with_quality(
            &mut out,
            image::codecs::png::CompressionType::Fast,
            image::codecs::png::FilterType::NoFilter,
        ))
        .map_err(|e| e.to_string())?;
    let out = out.into_inner();
    t.report(&format!("bytes={}", out.len()));
    Ok(out)
}

/// Engine 2's stage 4: exposure and white balance in linear light, the display transform,
/// then the shared display-domain look (zones, regions, contrast, saturation, and the
/// finish) with exposure already done.
fn finish_linear(
    mut lin: Rgb32FImage,
    edit: &EditRecord,
    image: &WorkingImage,
    opts: RenderOpts,
    t: &mut timing::Stages,
) -> Result<RgbImage, String> {
    if opts.skip_look {
        t.mark("look_skipped");
        return Ok(linear::to_display(&lin, linear::DisplayTransform::Srgb, linear::BASELINE_EV + edit.camera_ev));
    }
    let wb = linear::WbSpec::from_record(
        edit.tone.wb.mode.as_deref(),
        edit.tone.wb.temp,
        edit.tone.wb.tint,
        edit.tone.wb.kelvin,
    );
    let gains = linear::wb_multipliers(&wb, &image.cam_mul, &image.rgb_cam)?;
    linear::apply_exposure_linear(&mut lin, edit.tone.ev, gains);
    t.mark("linear_exposure");
    let transform = linear::DisplayTransform::from_record(edit.display.as_deref());
    let mut rgb = linear::to_display(&lin, transform, linear::BASELINE_EV + edit.camera_ev);
    t.mark("display");
    let lut = edit.lut.as_ref().and_then(|l| {
        crate::commands::luts_dir()
            .ok()
            .and_then(|dir| cube::load(&dir, &l.file))
    });
    t.mark("lut_load");
    look::apply_look_with(&mut rgb, edit, lut.as_deref(), false);
    t.mark("look");
    Ok(rgb)
}

fn parse_record(edit_json: &str) -> Result<EditRecord, String> {
    let trimmed = edit_json.trim();
    serde_json::from_str(if trimmed.is_empty() { "{}" } else { trimmed })
        .map_err(|e| format!("invalid edit record: {e}"))
}

/// Stages 0–3: geometry and the optional downscale. Everything before the look.
fn frame_image(
    mut img: DynamicImage,
    edit: &EditRecord,
    max_edge: u32,
    t: &mut timing::Stages,
) -> DynamicImage {
    // 0) Perspective: map the named quad back onto a rectangle. First, because it
    //    redefines the frame the later stages work within — straighten's centre and
    //    crop's fractions both refer to the rectified image, not the original.
    //    A degenerate quad is non-fatal and simply leaves the geometry alone.
    if let Some(p) = &edit.perspective {
        if let Some(warped) = perspective_warp(&img, p) {
            img = warped;
        }
    }
    t.mark("perspective");

    // 1) Straighten: rotate about the centre. Corners exposed by the rotation are left
    //    black; the UI's inscribed crop keeps them out of the final frame.
    if edit.straighten.abs() > 0.01 {
        img = rotate_about_center(&img, edit.straighten);
    }
    t.mark("straighten");

    // 2) Crop (normalized → pixels), clamped to the image bounds.
    if let Some(c) = &edit.crop {
        let (w, h) = img.dimensions();
        let x = (c.x.clamp(0.0, 1.0) * w as f32).round() as u32;
        let y = (c.y.clamp(0.0, 1.0) * h as f32).round() as u32;
        let mut cw = ((c.w.clamp(0.0, 1.0) * w as f32).round() as u32)
            .clamp(1, w.saturating_sub(x).max(1));
        let mut ch = ((c.h.clamp(0.0, 1.0) * h as f32).round() as u32)
            .clamp(1, h.saturating_sub(y).max(1));
        // Enforce the locked aspect exactly (only ever shrinking, so we stay in bounds),
        // so a "1:1" crop is pixel-perfect square regardless of the source resolution.
        if let Some(ratio) = parse_aspect(&c.aspect) {
            if (cw as f32 / ch as f32) > ratio {
                cw = ((ch as f32 * ratio).round() as u32).clamp(1, cw);
            } else {
                ch = ((cw as f32 / ratio).round() as u32).clamp(1, ch);
            }
        }
        img = img.crop_imm(x, y, cw, ch);
    }
    t.mark("crop");

    // 3) Optional downscale (preview speed). The linear image never goes through the
    //    `image` crate's resampler — it is wrong on f32 buffers (see `linear::downscale_linear`).
    if max_edge > 0 {
        let (w, h) = img.dimensions();
        if w.max(h) > max_edge {
            img = match img {
                DynamicImage::ImageRgb32F(lin) => DynamicImage::ImageRgb32F(linear::downscale_linear(&lin, max_edge)),
                other => other.thumbnail(max_edge, max_edge),
            };
        }
    }
    t.mark("downscale");
    img
}

/// Stage 4: the look — tone, B&W mix, LUT, toning, fade, vignette — in the RGB domain.
/// The LUT (if referenced) is resolved once per render through cube's mtime cache; a
/// missing/corrupt file is non-fatal and the render proceeds without it.
fn finish_look(
    mut rgb: RgbImage,
    edit: &EditRecord,
    opts: RenderOpts,
    t: &mut timing::Stages,
) -> RgbImage {
    if opts.skip_look {
        t.mark("look_skipped");
        return rgb;
    }
    let lut = edit.lut.as_ref().and_then(|l| {
        crate::commands::luts_dir()
            .ok()
            .and_then(|dir| cube::load(&dir, &l.file))
    });
    t.mark("lut_load");
    look::apply_look(&mut rgb, edit, lut.as_deref());
    t.mark("look");
    rgb
}

// ── The framed-base cache ─────────────────────────────────────────────────────────

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct FramedKey {
    /// [`jpeg_fingerprint`] of the proxy bytes, or [`token_fingerprint`] of a working image.
    source: u64,
    /// [`geometry_fingerprint`] of the record.
    geometry: u64,
    max_edge: u32,
}

/// A framed base of either engine.
#[derive(Clone)]
enum FramedBase {
    Rgb8(RgbImage),
    Linear(Rgb32FImage),
}

fn token_fingerprint(token: &SourceToken) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    "working".hash(&mut h);
    token.hash(&mut h);
    h.finish()
}

/// Entries kept: the current photo's drag (720) and settled (1400) tiers, the masses
/// pass (1024), and the loupe's full-size render (0) — one photo's working set. A 2048 px
/// RGB base is ~12 MB, so the cap is memory, not hit rate. Evicts least recently used.
const FRAMED_CACHE_CAP: usize = 4;
/// The largest linear framed base the cache keeps (long edge, px): the loupe's fit render.
pub const FRAMED_CACHE_MAX_LINEAR_EDGE: u32 = 2560;
static FRAMED_CACHE: Mutex<Vec<(FramedKey, FramedBase)>> = Mutex::new(Vec::new());
/// Hits since process start — for the tests and the bench, never for behaviour.
static FRAMED_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn framed_cache_get(key: &FramedKey) -> Option<FramedBase> {
    let mut cache = FRAMED_CACHE.lock().unwrap();
    let pos = cache.iter().position(|(k, _)| k == key)?;
    // Most recently used at the back.
    let entry = cache.remove(pos);
    let base = entry.1.clone();
    cache.push(entry);
    FRAMED_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Some(base)
}

fn framed_cache_put(key: FramedKey, base: FramedBase) {
    let mut cache = FRAMED_CACHE.lock().unwrap();
    cache.retain(|(k, _)| *k != key);
    if cache.len() >= FRAMED_CACHE_CAP {
        cache.remove(0);
    }
    cache.push((key, base));
}

/// Hits so far (tests and the bench).
pub fn framed_cache_hits() -> u64 {
    FRAMED_HITS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Everything [`frame_image`] reads from the record, hashed bit-exactly: two records
/// with the same geometry share a framed base whatever their look says.
fn geometry_fingerprint(edit: &EditRecord) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    edit.straighten.to_bits().hash(&mut h);
    match &edit.crop {
        Some(c) => {
            1u8.hash(&mut h);
            c.x.to_bits().hash(&mut h);
            c.y.to_bits().hash(&mut h);
            c.w.to_bits().hash(&mut h);
            c.h.to_bits().hash(&mut h);
            c.aspect.hash(&mut h);
        }
        None => 0u8.hash(&mut h),
    }
    match &edit.perspective {
        Some(p) => {
            1u8.hash(&mut h);
            for corner in [p.tl, p.tr, p.br, p.bl] {
                corner[0].to_bits().hash(&mut h);
                corner[1].to_bits().hash(&mut h);
            }
            p.aspect.map(f32::to_bits).hash(&mut h);
        }
        None => 0u8.hash(&mut h),
    }
    h.finish()
}

/// Warp the quadrilateral named by `p` onto a rectangle, undoing the keystone of an
/// off-axis shot. The output is sized from the quad's own edge lengths (or its explicit
/// `aspect`), so a picture shot from below comes back at roughly the pixel count it was
/// captured at rather than being stretched to the source's frame.
///
/// `None` for a degenerate quad — collinear or coincident corners have no well-defined
/// rectification — which the caller treats as "leave the geometry alone", the same
/// non-fatal degradation a missing LUT gets.
fn perspective_warp(img: &DynamicImage, p: &Perspective) -> Option<DynamicImage> {
    match img {
        DynamicImage::ImageRgb32F(src) => warp_buffer(src, p).map(DynamicImage::ImageRgb32F),
        other => warp_buffer(&other.to_rgb8(), p).map(DynamicImage::ImageRgb8),
    }
}

fn warp_buffer<T: Sample>(src: &ImageBuffer<Rgb<T>, Vec<T>>, p: &Perspective) -> Option<ImageBuffer<Rgb<T>, Vec<T>>>
where
    Rgb<T>: Pixel<Subpixel = T>,
{
    let (w, h) = src.dimensions();
    let (fw, fh) = (w as f32, h as f32);
    let corner = |c: [f32; 2]| (c[0] * fw, c[1] * fh);
    let (tl, tr, br, bl) = (corner(p.tl), corner(p.tr), corner(p.br), corner(p.bl));

    let dist = |a: (f32, f32), b: (f32, f32)| (a.0 - b.0).hypot(a.1 - b.1);
    let mean_w = (dist(tl, tr) + dist(bl, br)) / 2.0;
    let mean_h = (dist(tl, bl) + dist(tr, br)) / 2.0;
    if !mean_w.is_finite() || !mean_h.is_finite() || mean_w < 1.0 || mean_h < 1.0 {
        return None;
    }

    // Height carries the size estimate and an explicit aspect then sets the width, so a
    // locked ratio comes out exact instead of being the quotient of two independently
    // rounded estimates — the same reasoning as `Crop`'s aspect lock.
    let cap = (fw.max(fh) * MAX_PERSPECTIVE_SCALE).max(1.0);
    let out_h = mean_h;
    let out_w = match p.aspect {
        Some(a) if a.is_finite() && a > 0.0 => out_h * a,
        _ => mean_w,
    };
    let out_w = (out_w.round().clamp(1.0, cap)) as u32;
    let out_h = (out_h.round().clamp(1.0, cap)) as u32;

    // Solve the map from the OUTPUT rectangle onto the source quad. Sampling runs per
    // output pixel, so dest → src is already the direction the loop needs; no inversion.
    let (ow, oh) = (out_w as f32, out_h as f32);
    let m = solve_homography(
        [(0.0, 0.0), (ow, 0.0), (ow, oh), (0.0, oh)],
        [tl, tr, br, bl],
    )?;

    let mut out = ImageBuffer::new(out_w, out_h);
    for y in 0..out_h {
        for x in 0..out_w {
            let (u, v) = (x as f32 + 0.5, y as f32 + 0.5);
            let denom = m[6] * u + m[7] * v + 1.0;
            // The quad's horizon line maps to denom == 0; a pixel there has no finite
            // source point, so it is background rather than a wild sample.
            if denom.abs() < 1e-6 {
                continue;
            }
            let sx = (m[0] * u + m[1] * v + m[2]) / denom;
            let sy = (m[3] * u + m[4] * v + m[5]) / denom;
            if !sx.is_finite() || !sy.is_finite() {
                continue;
            }
            out.put_pixel(x, y, sample_bilinear(src, sx - 0.5, sy - 0.5));
        }
    }
    Some(out)
}

/// Solve the eight coefficients of the projective map taking each `from[i]` to `to[i]`:
///
/// ```text
/// x = (m0·u + m1·v + m2) / (m6·u + m7·v + 1)
/// y = (m3·u + m4·v + m5) / (m6·u + m7·v + 1)
/// ```
///
/// Each correspondence contributes two linear equations in the eight unknowns, giving an
/// 8×8 system solved by Gauss-Jordan with partial pivoting. Accumulates in `f64`: the
/// products of pixel coordinates in the last two columns are large enough that `f32`
/// pivoting visibly bends long edges. `None` when the system is singular — a degenerate
/// quad.
fn solve_homography(from: [(f32, f32); 4], to: [(f32, f32); 4]) -> Option<[f32; 8]> {
    let mut a = [[0f64; 9]; 8];
    for i in 0..4 {
        let (u, v) = (from[i].0 as f64, from[i].1 as f64);
        let (x, y) = (to[i].0 as f64, to[i].1 as f64);
        a[i * 2] = [u, v, 1.0, 0.0, 0.0, 0.0, -u * x, -v * x, x];
        a[i * 2 + 1] = [0.0, 0.0, 0.0, u, v, 1.0, -u * y, -v * y, y];
    }
    for col in 0..8 {
        let pivot = (col..8).max_by(|&r1, &r2| a[r1][col].abs().total_cmp(&a[r2][col].abs()))?;
        if a[pivot][col].abs() < 1e-9 {
            return None;
        }
        a.swap(col, pivot);
        let d = a[col][col];
        for k in col..9 {
            a[col][k] /= d;
        }
        for r in 0..8 {
            if r == col || a[r][col] == 0.0 {
                continue;
            }
            let f = a[r][col];
            for k in col..9 {
                a[r][k] -= f * a[col][k];
            }
        }
    }
    let mut m = [0f32; 8];
    for (i, row) in a.iter().enumerate() {
        if !row[8].is_finite() {
            return None;
        }
        m[i] = row[8] as f32;
    }
    Some(m)
}

/// Rotate an image about its centre by `degrees`, keeping the same canvas size. Pixels
/// pulled from outside the source (the exposed corners) are black; bilinear sampling
/// keeps edges smooth. `degrees` follows screen space (y-down), matching the UI's tilt.
fn rotate_about_center(img: &DynamicImage, degrees: f32) -> DynamicImage {
    match img {
        DynamicImage::ImageRgb32F(src) => DynamicImage::ImageRgb32F(rotate_buffer(src, degrees)),
        other => DynamicImage::ImageRgb8(rotate_buffer(&other.to_rgb8(), degrees)),
    }
}

fn rotate_buffer<T: Sample>(src: &ImageBuffer<Rgb<T>, Vec<T>>, degrees: f32) -> ImageBuffer<Rgb<T>, Vec<T>>
where
    Rgb<T>: Pixel<Subpixel = T>,
{
    let (w, h) = src.dimensions();
    let (sin, cos) = degrees.to_radians().sin_cos();
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    let mut out = ImageBuffer::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            // Inverse rotation (output → source): rotate the offset by -degrees.
            let sx = cos * dx + sin * dy + cx - 0.5;
            let sy = -sin * dx + cos * dy + cy - 0.5;
            out.put_pixel(x, y, sample_bilinear(src, sx, sy));
        }
    }
    out
}

/// A channel type the geometry stages can resample: 8-bit (engine 1, the exact rounding
/// the byte-identity tests lock) or f32 (engine 2's linear working image, no rounding).
trait Sample: image::Primitive + 'static {
    fn to_f32(self) -> f32;
    fn from_f32(v: f32) -> Self;
}
impl Sample for u8 {
    fn to_f32(self) -> f32 {
        self as f32
    }
    fn from_f32(v: f32) -> Self {
        v.round().clamp(0.0, 255.0) as u8
    }
}
impl Sample for f32 {
    fn to_f32(self) -> f32 {
        self
    }
    fn from_f32(v: f32) -> Self {
        v
    }
}

/// Bilinearly sample `img` at fractional `(x, y)`; black for the rotation's exposed
/// corners. Coordinates within half a pixel of the image are clamped to the edge (rather
/// than blackened), so a straightened image has no 1px black sliver at the binding edge.
fn sample_bilinear<T: Sample>(img: &ImageBuffer<Rgb<T>, Vec<T>>, x: f32, y: f32) -> Rgb<T>
where
    Rgb<T>: Pixel<Subpixel = T>,
{
    let (w, h) = img.dimensions();
    if x < -0.5 || y < -0.5 || x > w as f32 - 0.5 || y > h as f32 - 0.5 {
        return Rgb([T::from_f32(0.0), T::from_f32(0.0), T::from_f32(0.0)]);
    }
    let x = x.clamp(0.0, (w - 1) as f32);
    let y = y.clamp(0.0, (h - 1) as f32);
    let (x0, y0) = (x.floor() as u32, y.floor() as u32);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (fx, fy) = (x - x0 as f32, y - y0 as f32);
    let p = |xx, yy| img.get_pixel(xx, yy).0;
    let (p00, p10, p01, p11) = (p(x0, y0), p(x1, y0), p(x0, y1), p(x1, y1));
    let mut out = [T::from_f32(0.0); 3];
    for c in 0..3 {
        let top = p00[c].to_f32() * (1.0 - fx) + p10[c].to_f32() * fx;
        let bot = p01[c].to_f32() * (1.0 - fx) + p11[c].to_f32() * fx;
        out[c] = T::from_f32(top * (1.0 - fy) + bot * fy);
    }
    Rgb(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxy_jpeg(seed: u32) -> Vec<u8> {
        let img = RgbImage::from_fn(96, 64, |x, y| {
            Rgb([
                ((x * 3 + seed) % 256) as u8,
                ((y * 5 + seed * 7) % 256) as u8,
                (((x + y) * 2 + seed * 13) % 256) as u8,
            ])
        });
        encode_jpeg(&DynamicImage::ImageRgb8(img), 95).unwrap()
    }

    #[test]
    fn framed_base_cache_renders_identically_on_miss_and_hit() {
        let jpeg = proxy_jpeg(1);
        let record = r#"{"straighten": 6, "crop": {"x": 0.1, "y": 0.1, "w": 0.8, "h": 0.8},
                         "tone": {"ev": 0.7, "contrast": 0.2}, "vignette": -0.4}"#;
        let plain = render_image(image::load_from_memory(&jpeg).unwrap(), record, 48).unwrap();
        let hits0 = framed_cache_hits();
        let miss = render_proxy(RenderSource::PreviewJpeg(&jpeg), record, 48, RenderOpts::default()).unwrap();
        let hit = render_proxy(RenderSource::PreviewJpeg(&jpeg), record, 48, RenderOpts::default()).unwrap();
        assert!(framed_cache_hits() > hits0, "the second render must hit the framed cache");
        assert_eq!(miss.to_rgb8().as_raw(), plain.to_rgb8().as_raw(), "miss ≠ uncached path");
        assert_eq!(hit.to_rgb8().as_raw(), plain.to_rgb8().as_raw(), "hit ≠ uncached path");

        // A look-only change reuses the base; the output still follows the record.
        let hits1 = framed_cache_hits();
        let brighter = r#"{"straighten": 6, "crop": {"x": 0.1, "y": 0.1, "w": 0.8, "h": 0.8},
                           "tone": {"ev": 1.5}}"#;
        let out = render_proxy(RenderSource::PreviewJpeg(&jpeg), brighter, 48, RenderOpts::default()).unwrap();
        assert!(framed_cache_hits() > hits1, "a look-only change must reuse the framed base");
        let expect = render_image(image::load_from_memory(&jpeg).unwrap(), brighter, 48).unwrap();
        assert_eq!(out.to_rgb8().as_raw(), expect.to_rgb8().as_raw());
    }

    #[test]
    fn framed_base_cache_keys_on_geometry_edge_and_photo() {
        let a = proxy_jpeg(2);
        let b = proxy_jpeg(3);
        let rec = r#"{"tone": {"ev": 0.3}}"#;
        let tilted = r#"{"tone": {"ev": 0.3}, "straighten": 4}"#;
        let base = render_proxy(RenderSource::PreviewJpeg(&a), rec, 48, RenderOpts::default()).unwrap();
        // Same photo, same look, different geometry → different pixels, never a stale base.
        let geo = render_proxy(RenderSource::PreviewJpeg(&a), tilted, 48, RenderOpts::default()).unwrap();
        assert_ne!(base.to_rgb8().as_raw(), geo.to_rgb8().as_raw());
        assert_eq!(
            geo.to_rgb8().as_raw(),
            render_image(image::load_from_memory(&a).unwrap(), tilted, 48).unwrap().to_rgb8().as_raw()
        );
        // Different edge → the right size, not the cached one.
        let big = render_proxy(RenderSource::PreviewJpeg(&a), rec, 64, RenderOpts::default()).unwrap();
        assert_ne!(big.dimensions(), base.dimensions());
        // Another photo with the same dimensions and record → its own pixels.
        let other = render_proxy(RenderSource::PreviewJpeg(&b), rec, 48, RenderOpts::default()).unwrap();
        assert_ne!(other.to_rgb8().as_raw(), base.to_rgb8().as_raw());
        assert_eq!(
            other.to_rgb8().as_raw(),
            render_image(image::load_from_memory(&b).unwrap(), rec, 48).unwrap().to_rgb8().as_raw()
        );
    }

    #[test]
    fn framed_base_cache_is_bounded() {
        let jpeg = proxy_jpeg(4);
        for edge in [8u32, 9, 10, 11, 12, 13, 14] {
            render_proxy(RenderSource::PreviewJpeg(&jpeg), "{}", edge, RenderOpts::default()).unwrap();
        }
        assert!(FRAMED_CACHE.lock().unwrap().len() <= FRAMED_CACHE_CAP);
    }

    #[test]
    fn base_only_skips_look_but_keeps_geometry() {
        let img = DynamicImage::ImageRgb8(RgbImage::from_fn(64, 48, |x, y| {
            Rgb([(x * 4) as u8, (y * 5) as u8, ((x + y) * 2) as u8])
        }));
        let geometry = r#"{"straighten": 15}"#;
        let with_look = r#"{"straighten": 15, "tone": {"ev": 1.0}, "vignette": -0.5}"#;
        let base = render_image_opts(RenderSource::Decoded(img.clone()), with_look, 0, RenderOpts { skip_look: true }).unwrap();
        let plain = render_image(img.clone(), geometry, 0).unwrap();
        assert_eq!(
            base.to_rgb8().as_raw(),
            plain.to_rgb8().as_raw(),
            "a base-only render must equal the geometry-only render, byte for byte"
        );
        let looked = render_image(img, with_look, 0).unwrap();
        assert_ne!(
            base.to_rgb8().as_raw(),
            looked.to_rgb8().as_raw(),
            "the look is what was skipped"
        );
    }

    fn synthetic_working(bright: f32) -> std::sync::Arc<WorkingImage> {
        // A tinted ramp with a neutral patch above display white on the left.
        let linear = Rgb32FImage::from_fn(64, 48, |x, y| {
            if x < 16 {
                return image::Rgb([bright; 3]);
            }
            let v = (x as f32 / 63.0) * 0.6;
            image::Rgb([v, v * (0.5 + y as f32 / 96.0), v * 0.4])
        });
        std::sync::Arc::new(WorkingImage {
            width: 64,
            height: 48,
            linear,
            cam_mul: [1.0; 4],
            rgb_cam: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            decoder: "test",
            camera_ev: None,
        })
    }

    #[test]
    fn engine2_refuses_a_preview_source_and_engine1_refuses_the_working_image() {
        let jpeg = proxy_jpeg(9);
        let e2 = r#"{"engine": 2, "tone": {"ev": 0.5}}"#;
        assert!(render_proxy(RenderSource::PreviewJpeg(&jpeg), e2, 48, RenderOpts::default()).is_err());
        assert!(render_image(image::load_from_memory(&jpeg).unwrap(), e2, 48).is_err());
        let token = SourceToken::Working { photo_id: 1, generation: 1 };
        let e1 = r#"{"tone": {"ev": 0.5}}"#;
        assert!(render_proxy(RenderSource::Working { token, image: synthetic_working(0.5) }, e1, 48, RenderOpts::default()).is_err());
    }

    #[test]
    fn engine2_display_field_selects_the_transform_and_absent_stays_srgb() {
        let img = synthetic_working(1.4);
        let render = |json: &str, gen: u64| {
            let token = SourceToken::Working { photo_id: 3, generation: gen };
            render_proxy(RenderSource::Working { token, image: img.clone() }, json, 0, RenderOpts::default()).unwrap().to_rgb8()
        };
        let plain = render(r#"{"engine": 2}"#, 1);
        let srgb = render(r#"{"engine": 2, "display": "srgb"}"#, 2);
        let camera = render(r#"{"engine": 2, "display": "camera"}"#, 3);
        assert_eq!(plain.as_raw(), srgb.as_raw(), "a record saved without the field renders as it always did");
        assert_ne!(plain.as_raw(), camera.as_raw());
        // The camera curve keeps the 1.4× patch (1.4·2^1.4 ≈ 3.7 lifted) at white either way,
        // and changes the ramp beside it.
        assert_eq!(camera.get_pixel(2, 10).0, [255, 255, 255]);
        assert_ne!(camera.get_pixel(40, 10).0, plain.get_pixel(40, 10).0);
    }

    #[test]
    fn engine2_camera_ev_on_the_record_is_an_exposure_below_the_slider() {
        let img = synthetic_working(0.3);
        let render = |json: &str, gen: u64| {
            let token = SourceToken::Working { photo_id: 4, generation: gen };
            render_proxy(RenderSource::Working { token, image: img.clone() }, json, 0, RenderOpts::default()).unwrap().to_rgb8()
        };
        let matched = render(r#"{"engine": 2, "display": "camera", "cameraEv": -1.0}"#, 1);
        let by_slider = render(r#"{"engine": 2, "display": "camera", "tone": {"ev": -1.0}}"#, 2);
        let none = render(r#"{"engine": 2, "display": "camera"}"#, 3);
        let zero = render(r#"{"engine": 2, "display": "camera", "cameraEv": 0}"#, 4);
        assert_eq!(none.as_raw(), zero.as_raw(), "absent means no offset");
        assert_ne!(matched.as_raw(), none.as_raw());
        let worst = matched.as_raw().iter().zip(by_slider.as_raw()).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
        assert!(worst <= 1, "the same light either way (max |Δ| {worst})");
    }

    #[test]
    fn the_clip_overlay_marks_sensor_white_only_and_matches_the_stage_size() {
        let img = synthetic_working(1.0); // the left patch sits exactly at sensor white
        let token = SourceToken::Working { photo_id: 5, generation: 1 };
        let json = r#"{"engine": 2, "display": "camera.2", "tone": {"ev": -2.0}}"#;
        let png = clip_overlay_png(token.clone(), img.clone(), json, 32).unwrap();
        let mask = image::load_from_memory(&png).unwrap().to_rgba8();
        let stage = render_proxy(RenderSource::Working { token: token.clone(), image: img.clone() }, json, 32, RenderOpts::default()).unwrap();
        assert_eq!(mask.dimensions(), (stage.width(), stage.height()), "the overlay lies exactly on the stage render");
        assert_eq!(mask.get_pixel(2, 10).0, [255, 0, 255, 190], "clipped at the sensor: marked, whatever the sliders say");
        assert_eq!(mask.get_pixel(28, 10).0[3], 0, "below sensor white: transparent");
        assert!(clip_overlay_png(token, img, r#"{"tone": {"ev": 0}}"#, 32).is_err(), "engine 1 has no sensor data");
    }

    #[test]
    fn engine2_renders_the_working_image_and_recovers_headroom() {
        let token = SourceToken::Working { photo_id: 2, generation: 7 };
        let img = synthetic_working(1.4);
        let at0 = render_proxy(RenderSource::Working { token: token.clone(), image: img.clone() }, r#"{"engine": 2}"#, 0, RenderOpts::default()).unwrap().to_rgb8();
        // The baseline lift (BASELINE_EV) sits on top of the record's EV, so the pull has
        // to exceed it: at −3 EV a patch at 1.4× sensor white lands at 1.4·2^(1.4−3) ≈ 0.46.
        let down = render_proxy(RenderSource::Working { token: token.clone(), image: img.clone() }, r#"{"engine": 2, "tone": {"ev": -3.0}}"#, 0, RenderOpts::default()).unwrap().to_rgb8();
        // At 0 EV the bright patch is display white; at −3 EV it is not, and the ramp
        // beside it is darker still — the patch kept its light.
        assert_eq!(at0.get_pixel(2, 10).0, [255, 255, 255]);
        assert!(down.get_pixel(2, 10).0[0] < 255);
        assert!(down.get_pixel(2, 10).0[0] > down.get_pixel(40, 10).0[0]);
        // Engine 1 on an 8-bit rendering of the same scene cannot: white stays white.
        let eight = DynamicImage::ImageRgb8(at0.clone());
        let e1down = render_image(eight, r#"{"tone": {"ev": -1.5}}"#, 0).unwrap().to_rgb8();
        assert_eq!(e1down.get_pixel(2, 10).0, e1down.get_pixel(0, 10).0, "no detail to recover");
        // Geometry runs on the linear image: a straighten + crop record renders the same
        // size as the framed base would, and the cache serves the second render.
        let geo = r#"{"engine": 2, "straighten": 3, "crop": {"x": 0.1, "y": 0.1, "w": 0.8, "h": 0.8}}"#;
        let hits0 = framed_cache_hits();
        let a = render_proxy(RenderSource::Working { token: token.clone(), image: img.clone() }, geo, 32, RenderOpts::default()).unwrap();
        let b = render_proxy(RenderSource::Working { token, image: img }, geo, 32, RenderOpts::default()).unwrap();
        assert!(framed_cache_hits() > hits0);
        assert_eq!(a.to_rgb8().as_raw(), b.to_rgb8().as_raw());
    }

    #[test]
    fn engine_field_defaults_to_1_and_engine1_renders_byte_identically() {
        let jpeg = proxy_jpeg(5);
        let img = image::load_from_memory(&jpeg).unwrap();
        let plain = r#"{"tone": {"ev": 0.4, "contrast": 0.2}, "vignette": -0.3, "zones": [0.1, 0, 0, 0, 0, 0, 0, 0]}"#;
        let tagged = r#"{"engine": 1, "tone": {"ev": 0.4, "contrast": 0.2}, "vignette": -0.3, "zones": [0.1, 0, 0, 0, 0, 0, 0, 0]}"#;
        assert_eq!(record_engine(plain), 1);
        assert_eq!(record_engine(tagged), 1);
        let a = render_image(img.clone(), plain, 48).unwrap();
        let b = render_image(img, tagged, 48).unwrap();
        assert_eq!(a.to_rgb8().as_raw(), b.to_rgb8().as_raw());
    }

    #[test]
    fn png_base_roundtrips_losslessly() {
        let img = DynamicImage::ImageRgb8(RgbImage::from_fn(40, 30, |x, y| {
            Rgb([(x * 6) as u8, (y * 8) as u8, ((x ^ y) * 3) as u8])
        }));
        let png = encode_png_fast(&img).unwrap();
        let back = image::load_from_memory(&png).unwrap().to_rgb8();
        assert_eq!(back.as_raw(), img.to_rgb8().as_raw());
    }

    fn solid_jpeg(r: u8, g: u8, b: u8) -> Vec<u8> {
        let img = RgbImage::from_pixel(16, 16, image::Rgb([r, g, b]));
        let mut out = std::io::Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(img)
            .write_with_encoder(JpegEncoder::new_with_quality(&mut out, 95))
            .unwrap();
        out.into_inner()
    }

    fn mean_rgb(jpeg: &[u8]) -> (f32, f32, f32) {
        let img = image::load_from_memory(jpeg).unwrap().to_rgb8();
        let (mut r, mut g, mut b) = (0f64, 0f64, 0f64);
        for p in img.pixels() {
            r += p[0] as f64;
            g += p[1] as f64;
            b += p[2] as f64;
        }
        let n = img.pixels().count() as f64;
        ((r / n) as f32, (g / n) as f32, (b / n) as f32)
    }

    #[test]
    fn empty_edit_is_a_noop() {
        let src = solid_jpeg(120, 120, 120);
        let out = render_jpeg(&src, "{}", 0).unwrap();
        let (r, _, _) = mean_rgb(&out);
        assert!((r - 120.0).abs() < 4.0, "near-unchanged, got {r}");
    }

    #[test]
    fn positive_ev_brightens() {
        let src = solid_jpeg(100, 100, 100);
        let out = render_jpeg(&src, r#"{"tone":{"ev":1.0}}"#, 0).unwrap();
        let (r, _, _) = mean_rgb(&out);
        assert!(r > 150.0, "1 stop should brighten 100 well past 150, got {r}");
    }

    #[test]
    fn warm_wb_shifts_red_above_blue() {
        let src = solid_jpeg(120, 120, 120);
        let out = render_jpeg(&src, r#"{"tone":{"wb":{"temp":1.0}}}"#, 0).unwrap();
        let (r, _, b) = mean_rgb(&out);
        assert!(r > b + 20.0, "warm should push red above blue, got r={r} b={b}");
    }

    #[test]
    fn crop_reduces_dimensions() {
        let src = solid_jpeg(120, 120, 120);
        let out = render_jpeg(&src, r#"{"crop":{"x":0.0,"y":0.0,"w":0.5,"h":0.5}}"#, 0).unwrap();
        let img = image::load_from_memory(&out).unwrap();
        assert_eq!(img.dimensions(), (8, 8), "half-crop of 16px = 8px");
    }

    #[test]
    fn straighten_keeps_size_and_blackens_corners() {
        // A solid-white image rotated keeps its dimensions; the corners (pulled from
        // outside the source) go black, while the centre stays white.
        let src = solid_jpeg(255, 255, 255);
        let out = render_jpeg(&src, r#"{"straighten":15}"#, 0).unwrap();
        let img = image::load_from_memory(&out).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (16, 16), "rotation preserves canvas size");
        assert!(img.get_pixel(8, 8).0[0] > 240, "centre stays ~white (JPEG-lossy)");
        let corner = img.get_pixel(0, 0).0;
        assert!(corner[0] < 128, "corner is darkened by the rotation, got {corner:?}");
    }

    #[test]
    fn zero_straighten_is_a_noop() {
        let src = solid_jpeg(200, 100, 50);
        let out = render_jpeg(&src, r#"{"straighten":0}"#, 0).unwrap();
        let img = image::load_from_memory(&out).unwrap().to_rgb8();
        assert_eq!(img.get_pixel(0, 0).0, [200, 100, 50], "no rotation, corner intact");
    }

    /// Encode an image the tests built pixel-by-pixel, so a case can start from something
    /// with structure rather than a flat colour.
    fn jpeg_of(img: &RgbImage) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(img.clone())
            .write_with_encoder(JpegEncoder::new_with_quality(&mut out, 95))
            .unwrap();
        out.into_inner()
    }

    /// A black canvas with the convex quad `pts` (in order) painted white — a stand-in for
    /// a framed picture photographed off-axis.
    fn quad_image(w: u32, h: u32, pts: [(f32, f32); 4]) -> RgbImage {
        let side = |a: (f32, f32), b: (f32, f32), px: f32, py: f32| {
            (b.0 - a.0) * (py - a.1) - (b.1 - a.1) * (px - a.0)
        };
        let mut img = RgbImage::from_pixel(w, h, Rgb([0, 0, 0]));
        for y in 0..h {
            for x in 0..w {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let s: [f32; 4] =
                    std::array::from_fn(|i| side(pts[i], pts[(i + 1) % 4], px, py));
                if s.iter().all(|v| *v >= 0.0) || s.iter().all(|v| *v <= 0.0) {
                    img.put_pixel(x, y, Rgb([255, 255, 255]));
                }
            }
        }
        img
    }

    /// Mean brightness of one quadrant of `img`, inset by 12% so the quad's own boundary
    /// (where bilinear sampling picks up the surround) can't sway the reading.
    fn quadrant_mean(img: &RgbImage, right: bool, bottom: bool) -> f32 {
        let (w, h) = img.dimensions();
        let (iw, ih) = (w as f32 * 0.12, h as f32 * 0.12);
        let x0 = if right { w as f32 / 2.0 } else { iw } as u32;
        let x1 = if right { w as f32 - iw } else { w as f32 / 2.0 } as u32;
        let y0 = if bottom { h as f32 / 2.0 } else { ih } as u32;
        let y1 = if bottom { h as f32 - ih } else { h as f32 / 2.0 } as u32;
        let mut sum = 0f64;
        let mut n = 0f64;
        for y in y0..y1 {
            for x in x0..x1 {
                sum += img.get_pixel(x, y).0[0] as f64;
                n += 1.0;
            }
        }
        (sum / n.max(1.0)) as f32
    }

    #[test]
    fn perspective_rectifies_the_named_quad() {
        // A white trapezoid on black — the shape a rectangular picture takes when shot
        // from below and off-axis — with a dark notch inside its top-right corner so the
        // test can tell a correct rectification from one that transposes or rotates the
        // corners. A plain white quad cannot: it is symmetric under those mistakes.
        let quad = [(20.0, 10.0), (78.0, 18.0), (86.0, 84.0), (12.0, 76.0)];
        let mut canvas = quad_image(96, 96, quad);
        let (tl, tr, bl) = (quad[0], quad[1], quad[3]);
        let ex = (tr.0 - tl.0, tr.1 - tl.1);
        let ey = (bl.0 - tl.0, bl.1 - tl.1);
        let notch = (
            tl.0 + 0.78 * ex.0 + 0.20 * ey.0,
            tl.1 + 0.78 * ex.1 + 0.20 * ey.1,
        );
        for dy in -6i32..=6 {
            for dx in -6i32..=6 {
                let (x, y) = (notch.0 as i32 + dx, notch.1 as i32 + dy);
                if x >= 0 && y >= 0 && (x as u32) < 96 && (y as u32) < 96 {
                    canvas.put_pixel(x as u32, y as u32, Rgb([0, 0, 0]));
                }
            }
        }
        let src = jpeg_of(&canvas);
        let out = render_jpeg(
            &src,
            r#"{"perspective":{"tl":[0.2083,0.1042],"tr":[0.8125,0.1875],
                               "br":[0.8958,0.875],"bl":[0.125,0.7917]}}"#,
            0,
        )
        .unwrap();

        let (r, _, _) = mean_rgb(&out);
        let (src_mean, _, _) = mean_rgb(&src);
        // The source is only ~half subject, so a warp that ignored the corners could not
        // fill the frame like this.
        assert!(
            r > src_mean + 70.0,
            "rectified mean {r} must be far above the source's {src_mean}"
        );

        // Orientation: the notch must land in the top-right quadrant and nowhere else.
        let img = image::load_from_memory(&out).unwrap().to_rgb8();
        let top_right = quadrant_mean(&img, true, false);
        let others = [
            quadrant_mean(&img, false, false),
            quadrant_mean(&img, false, true),
            quadrant_mean(&img, true, true),
        ];
        let brightest_other = others.iter().cloned().fold(f32::MIN, f32::max);
        let dimmest_other = others.iter().cloned().fold(f32::MAX, f32::min);
        assert!(
            top_right < dimmest_other - 20.0,
            "the notch must rectify into the top-right quadrant: tr={top_right}, \
             others={others:?} (brightest {brightest_other})"
        );
    }

    #[test]
    fn perspective_full_frame_quad_is_near_identity() {
        // Corners at the frame edges describe "already rectangular" — the warp must give
        // the pixels back unchanged, not resample them into a subtly different image.
        let mut grad = RgbImage::new(96, 96);
        for (x, y, p) in grad.enumerate_pixels_mut() {
            *p = Rgb([(x * 2) as u8, (y * 2) as u8, 128]);
        }
        let src = jpeg_of(&grad);
        let out = render_jpeg(
            &src,
            r#"{"perspective":{"tl":[0,0],"tr":[1,0],"br":[1,1],"bl":[0,1]}}"#,
            0,
        )
        .unwrap();
        let img = image::load_from_memory(&out).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (96, 96), "a full-frame quad keeps the size");
        let got = img.get_pixel(24, 48).0;
        assert!(
            (got[0] as i32 - 48).abs() < 12 && (got[1] as i32 - 96).abs() < 12,
            "pixel should survive the round trip, got {got:?}"
        );
    }

    #[test]
    fn degenerate_perspective_quad_is_nonfatal() {
        // Four coincident corners have no rectification. That must leave the geometry
        // alone rather than fail the render — the same contract a missing LUT gets.
        let src = solid_jpeg(90, 110, 130);
        let out = render_jpeg(
            &src,
            r#"{"perspective":{"tl":[0.5,0.5],"tr":[0.5,0.5],"br":[0.5,0.5],"bl":[0.5,0.5]}}"#,
            0,
        )
        .unwrap();
        let plain = render_jpeg(&src, "{}", 0).unwrap();
        assert_eq!(out, plain, "a degenerate quad must degrade to a no-op");
    }

    #[test]
    fn perspective_aspect_forces_the_output_ratio() {
        // An explicit aspect is what the UI writes once it has recovered the subject's
        // true ratio; the engine must honour it rather than the quad's own proportions.
        let src = jpeg_of(&RgbImage::from_pixel(64, 64, Rgb([200, 200, 200])));
        let out = render_jpeg(
            &src,
            r#"{"perspective":{"tl":[0.1,0.1],"tr":[0.9,0.1],"br":[0.9,0.9],"bl":[0.1,0.9],
                               "aspect":2.0}}"#,
            0,
        )
        .unwrap();
        let (w, h) = image::load_from_memory(&out).unwrap().dimensions();
        let ratio = w as f32 / h as f32;
        assert!((ratio - 2.0).abs() < 0.05, "aspect 2.0 requested, got {ratio} ({w}x{h})");
    }

    #[test]
    fn absurd_perspective_quad_cannot_explode_the_canvas() {
        // A mis-dragged handle or a corrupt record must not turn a 16px thumbnail into a
        // multi-gigapixel allocation.
        let src = solid_jpeg(120, 120, 120);
        let out = render_jpeg(
            &src,
            r#"{"perspective":{"tl":[-500,-500],"tr":[500,-500],
                               "br":[500,500],"bl":[-500,500]}}"#,
            0,
        )
        .unwrap();
        let (w, h) = image::load_from_memory(&out).unwrap().dimensions();
        let cap = (16.0 * MAX_PERSPECTIVE_SCALE) as u32;
        assert!(w <= cap && h <= cap, "output must stay capped, got {w}x{h} (cap {cap})");
    }

    #[test]
    fn locked_aspect_forces_exact_ratio() {
        // w/h fractions that round to different pixel counts (8 vs 9 on a 16px source);
        // the "1:1" lock must pull them back to an exact square.
        let src = solid_jpeg(120, 120, 120);
        let out = render_jpeg(
            &src,
            r#"{"crop":{"x":0.0,"y":0.0,"w":0.5,"h":0.55,"aspect":"1:1"}}"#,
            0,
        )
        .unwrap();
        let (w, h) = image::load_from_memory(&out).unwrap().dimensions();
        assert_eq!(w, h, "1:1 lock must be pixel-perfect square, got {w}x{h}");
    }

    #[test]
    fn saturation_minus_one_gives_greyscale() {
        // saturation=-1 should collapse R,G,B to the same luma value.
        let src = solid_jpeg(200, 100, 50);
        let out = render_jpeg(&src, r#"{"tone":{"saturation":-1.0}}"#, 0).unwrap();
        let (r, g, b) = mean_rgb(&out);
        assert!(
            (r - g).abs() < 3.0 && (r - b).abs() < 3.0,
            "saturation=-1 must be greyscale, got r={r} g={g} b={b}"
        );
    }

    #[test]
    fn bw_red_filter_brightens_reds_and_darkens_blues() {
        // A red-filter B&W conversion must render a red patch brighter than a blue
        // patch, and both outputs must be grey.
        let filter = r#"{"bw":{"enabled":true,"r":0.9,"g":0.15,"b":-0.05}}"#;
        let red = render_jpeg(&solid_jpeg(200, 40, 40), filter, 0).unwrap();
        let blue = render_jpeg(&solid_jpeg(40, 40, 200), filter, 0).unwrap();
        let (rr, rg, rb) = mean_rgb(&red);
        let (br, _, _) = mean_rgb(&blue);
        assert!(
            (rr - rg).abs() < 3.0 && (rr - rb).abs() < 3.0,
            "B&W output must be grey, got r={rr} g={rg} b={rb}"
        );
        assert!(rr > br + 50.0, "red filter: red patch {rr} must beat blue patch {br}");
    }

    #[test]
    fn bw_disabled_keeps_colour() {
        let src = solid_jpeg(200, 100, 50);
        let out = render_jpeg(&src, r#"{"bw":{"enabled":false}}"#, 0).unwrap();
        let (r, _, b) = mean_rgb(&out);
        assert!(r > b + 100.0, "disabled bw must keep colour, got r={r} b={b}");
    }

    #[test]
    fn new_look_params_at_defaults_are_identity() {
        let src = solid_jpeg(128, 128, 128);
        let out = render_jpeg(
            &src,
            r#"{"split":{"shadow_sat":0,"highlight_sat":0},"fade":0,"vignette":0}"#,
            0,
        )
        .unwrap();
        let (r, _, _) = mean_rgb(&out);
        assert!((r - 128.0).abs() < 4.0, "all-default look must be identity, got r={r}");
    }

    #[test]
    fn sepia_split_tints_grey_warm() {
        // Sepia (orange hue on both ends) on a grey image must push red above blue.
        let src = solid_jpeg(128, 128, 128);
        let out = render_jpeg(
            &src,
            r#"{"split":{"shadow_hue":35,"shadow_sat":0.3,"highlight_hue":45,"highlight_sat":0.2}}"#,
            0,
        )
        .unwrap();
        let (r, _, b) = mean_rgb(&out);
        assert!(r > b + 8.0, "sepia must tint warm, got r={r} b={b}");
    }

    #[test]
    fn fade_lifts_blacks() {
        let src = solid_jpeg(0, 0, 0);
        let out = render_jpeg(&src, r#"{"fade":1.0}"#, 0).unwrap();
        let (r, _, _) = mean_rgb(&out);
        assert!(r > 20.0, "full fade must lift pure black well off 0, got {r}");
    }

    #[test]
    fn negative_vignette_darkens_corners_not_centre() {
        let src = solid_jpeg(200, 200, 200);
        let out = render_jpeg(&src, r#"{"vignette":-1.0}"#, 0).unwrap();
        let img = image::load_from_memory(&out).unwrap().to_rgb8();
        let centre = img.get_pixel(8, 8).0[0];
        let corner = img.get_pixel(0, 0).0[0];
        assert!(centre > 185, "centre must stay bright, got {centre}");
        assert!(corner < centre - 30, "corner must darken, got corner={corner} centre={centre}");
    }

    #[test]
    fn grain_is_deterministic_and_seed_dependent() {
        // Same record → identical bytes (export reproducibility); a different seed
        // must change the pattern; amount 0 must be a no-op.
        let src = solid_jpeg(128, 128, 128);
        let a1 = render_jpeg(&src, r#"{"grain":{"amount":0.6,"size":1.0,"seed":7}}"#, 0).unwrap();
        let a2 = render_jpeg(&src, r#"{"grain":{"amount":0.6,"size":1.0,"seed":7}}"#, 0).unwrap();
        let b = render_jpeg(&src, r#"{"grain":{"amount":0.6,"size":1.0,"seed":8}}"#, 0).unwrap();
        let zero = render_jpeg(&src, r#"{"grain":{"amount":0.0}}"#, 0).unwrap();
        assert_eq!(a1, a2, "same edit record must render identical bytes");
        assert_ne!(a1, b, "a different seed must change the grain pattern");
        assert_eq!(zero, render_jpeg(&src, "{}", 0).unwrap(), "amount 0 is a no-op");
    }

    #[test]
    fn grain_roughly_preserves_mean_brightness() {
        let src = solid_jpeg(128, 128, 128);
        let out = render_jpeg(&src, r#"{"grain":{"amount":1.0,"size":1.0,"seed":1}}"#, 0).unwrap();
        let (r, _, _) = mean_rgb(&out);
        assert!((r - 128.0).abs() < 8.0, "grain must be zero-mean-ish, got {r}");
    }

    #[test]
    fn full_ts_shaped_record_parses() {
        // Field-name cross-check with the frontend (src/modules/editing.ts): a record
        // using every field exactly as the TS side serializes it must parse and render.
        let json = r#"{
            "crop": {"x": 0.1, "y": 0.1, "w": 0.8, "h": 0.8, "aspect": "1:1"},
            "tone": {"ev": 0.2, "contrast": 0.1, "highlights": -0.2, "shadows": 0.1,
                     "whites": 0.05, "blacks": -0.05, "vibrance": 0.1, "saturation": -0.2,
                     "wb": {"temp": 0.1, "tint": -0.05}},
            "straighten": 1.5,
            "bw": {"enabled": true, "r": 0.9, "g": 0.15, "b": -0.05},
            "split": {"shadow_hue": 35, "shadow_sat": 0.25, "highlight_hue": 45,
                      "highlight_sat": 0.12, "balance": 0.1},
            "grain": {"amount": 0.5, "size": 1.2, "seed": 42},
            "fade": 0.2,
            "vignette": -0.3,
            "lut": {"file": "some-film.cube", "amount": 0.8}
        }"#;
        let src = solid_jpeg(150, 120, 90);
        render_jpeg(&src, json, 0).expect("full TS-shaped record must parse and render");
    }

    #[test]
    fn is_bw_detects_mixer_and_full_desaturation() {
        assert!(is_bw(r#"{"bw":{"enabled":true}}"#), "enabled mixer is B&W");
        assert!(is_bw(r#"{"tone":{"saturation":-1}}"#), "saturation -1 is B&W");
        assert!(!is_bw(r#"{"bw":{"enabled":false}}"#), "disabled mixer is not");
        assert!(!is_bw(r#"{"tone":{"saturation":-0.5}}"#), "partial desat is not");
        assert!(!is_bw(""), "empty record is not");
        assert!(!is_bw("not json"), "garbage is not");
    }

    #[test]
    fn missing_lut_is_nonfatal() {
        // A record referencing a LUT file that doesn't exist must still render, and
        // identically to the same record without the LUT.
        let src = solid_jpeg(128, 96, 64);
        let with = render_jpeg(&src, r#"{"lut":{"file":"nope-missing.cube"}}"#, 0).unwrap();
        let without = render_jpeg(&src, "{}", 0).unwrap();
        assert_eq!(with, without, "missing LUT must degrade to a no-op");
    }

    #[test]
    fn whites_blacks_vibrance_at_zero_are_identity() {
        // All three new parameters at 0 must not change a neutral image.
        let src = solid_jpeg(128, 128, 128);
        let out = render_jpeg(
            &src,
            r#"{"tone":{"whites":0.0,"blacks":0.0,"vibrance":0.0}}"#,
            0,
        )
        .unwrap();
        let (r, _, _) = mean_rgb(&out);
        assert!(
            (r - 128.0).abs() < 4.0,
            "zero whites/blacks/vibrance should be near-identity, got r={r}"
        );
    }
}
