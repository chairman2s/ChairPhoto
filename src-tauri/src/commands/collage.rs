//! Collage compositor commands — justified-mosaic and freeform layout, preview
//! rendering, and saving a finished collage back into the catalog.
//!
//! Gated on the `collage` Cargo feature; see `docs/collage.md` and `collage/`.

use super::*;
use std::path::{Path, PathBuf};
use tauri::State;

/// Collage render options as sent by the frontend (serde camelCase). Parsed into the
/// engine's [`crate::collage::CollageOptions`] by [`make_collage`]: `aspect` is a string
/// (`"free"` or a `"W:H"` ratio) and `background` is a hex/`rgba(...)` color, both
/// human-friendly for the dialog and turned into the engine's typed fields here.
#[cfg(feature = "collage")]
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CollageOptionsDto {
    pub width: u32,
    /// `null`/`"free"` = grow with rows; else a ratio like `"1:1"`, `"4:5"`, `"9:16"`.
    pub aspect: Option<String>,
    pub row_height: u32,
    pub gap: u32,
    /// Mat color as `#rgb`/`#rrggbb`/`#rrggbbaa` or `rgba(r,g,b,a)`.
    pub background: String,
    /// `"contain"` (whole photos) or `"cover"` (uniform, lightly cropped tiles).
    pub fit: String,
    pub border_width: u32,
    pub corner_radius: u32,
}

/// Parse `"W:H"` (or `"free"`/empty) into the engine's optional aspect ratio. Anything
/// unparseable falls back to Free rather than erroring — the layout still produces a
/// valid (row-growing) canvas.
#[cfg(feature = "collage")]
fn parse_aspect(s: Option<&str>) -> Option<(u32, u32)> {
    let s = s.unwrap_or("").trim();
    if s.is_empty() || s.eq_ignore_ascii_case("free") {
        return None;
    }
    let (w, h) = s.split_once(':')?;
    let w: u32 = w.trim().parse().ok()?;
    let h: u32 = h.trim().parse().ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    Some((w, h))
}

/// Parse a `#rgb`/`#rrggbb`/`#rrggbbaa` hex or `rgba(r,g,b,a)` color into RGBA bytes.
/// Defaults to opaque white on anything unrecognized (the natural mat color).
#[cfg(feature = "collage")]
fn parse_color(s: &str) -> [u8; 4] {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        let parse2 = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        match hex.len() {
            // #rgb → expand each nibble (e.g. "f0a" → ff 00 aa).
            3 => {
                let nib = |c: char| c.to_digit(16).map(|v| (v * 17) as u8);
                let mut it = hex.chars();
                if let (Some(r), Some(g), Some(b)) = (
                    it.next().and_then(nib),
                    it.next().and_then(nib),
                    it.next().and_then(nib),
                ) {
                    return [r, g, b, 255];
                }
            }
            6 => {
                if let (Some(r), Some(g), Some(b)) = (parse2(0), parse2(2), parse2(4)) {
                    return [r, g, b, 255];
                }
            }
            8 => {
                if let (Some(r), Some(g), Some(b), Some(a)) =
                    (parse2(0), parse2(2), parse2(4), parse2(6))
                {
                    return [r, g, b, a];
                }
            }
            _ => {}
        }
    } else if let Some(inner) = s
        .strip_prefix("rgba(")
        .or_else(|| s.strip_prefix("rgb("))
        .and_then(|t| t.strip_suffix(')'))
    {
        let mut parts = inner.split(',').map(str::trim);
        let r = parts.next().and_then(|v| v.parse::<u32>().ok());
        let g = parts.next().and_then(|v| v.parse::<u32>().ok());
        let b = parts.next().and_then(|v| v.parse::<u32>().ok());
        // Alpha is 0.0–1.0 in CSS rgba(); default opaque when absent.
        let a = parts
            .next()
            .and_then(|v| v.parse::<f32>().ok())
            .map(|f| (f.clamp(0.0, 1.0) * 255.0).round() as u8)
            .unwrap_or(255);
        if let (Some(r), Some(g), Some(b)) = (r, g, b) {
            return [r.min(255) as u8, g.min(255) as u8, b.min(255) as u8, a];
        }
    }
    [255, 255, 255, 255]
}

/// A destination path that doesn't already exist: returns `path` if free, else
/// `stem (2).ext`, `stem (3).ext`, … Mirrors the export module's collision guard so a
/// repeat render never clobbers an earlier collage.
#[cfg(feature = "collage")]
fn unique_collage_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("collage");
    let ext = path.extension().and_then(|s| s.to_str());
    for n in 2..10_000 {
        let mut name = format!("{stem} ({n})");
        if let Some(ext) = ext {
            name.push('.');
            name.push_str(ext);
        }
        let candidate = dir.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    path.to_path_buf() // pathological fallback (10k collisions)
}

/// Composite the given photos (in the supplied order — the frontend hands them in the
/// user's manual-reorder order) into a single justified-mosaic image and write it to
/// `dest_dir`. Returns the absolute output path.
///
/// Each photo renders from its cached embedded preview ([`crate::thumbnails::zoom_bytes`]),
/// resolved through the same resolver as export/thumbnails — never a full RAW decode, which
/// keeps the render memory-bounded. The CPU-bound layout + compose runs under
/// `spawn_blocking` so the UI thread is never blocked. `format` is `"png"` (alpha kept) or
/// anything else → JPEG (alpha flattened onto the background mat, since JPEG has none).
#[cfg(feature = "collage")]
#[tauri::command]
pub async fn make_collage(
    state: State<'_, AppState>,
    photo_ids: Vec<i64>,
    opts: CollageOptionsDto,
    dest_dir: String,
    format: String,
) -> Result<String, String> {
    use crate::collage::{CollageOptions, Fit};

    if photo_ids.is_empty() {
        return Err("No photos selected for the collage".into());
    }

    // Resolve every photo to a reachable original up front (under the lock), so an
    // unmounted NAS fails fast with a clear message rather than mid-render.
    let paths: Vec<PathBuf> = with_catalog(&state, |c| {
        let mut out = Vec::with_capacity(photo_ids.len());
        for &id in &photo_ids {
            match c.resolve_photo_path(id)? {
                Some(p) => out.push(p),
                None => {
                    return Err(crate::catalog::CatalogError::NotFound(format!(
                        "photo {id} is not currently reachable (its volume may be offline)"
                    )))
                }
            }
        }
        Ok(out)
    })?;

    let engine_opts = CollageOptions {
        width: opts.width,
        aspect: parse_aspect(opts.aspect.as_deref()),
        row_height: opts.row_height,
        gap: opts.gap,
        background: parse_color(&opts.background),
        fit: if opts.fit.eq_ignore_ascii_case("cover") {
            Fit::Cover
        } else {
            Fit::Contain
        },
        border_width: opts.border_width,
        corner_radius: opts.corner_radius,
    };

    let is_png = format.eq_ignore_ascii_case("png");
    let dir = expand_home(&dest_dir);
    let ext = if is_png { "png" } else { "jpg" };
    let dest = unique_collage_path(&dir.join(format!("collage.{ext}")));

    // Decode each preview and run the compositor entirely off the UI thread — both the
    // JPEG decodes and the per-tile Lanczos3 resize are CPU-bound.
    let dest_for_write = dest.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        let mut images = Vec::with_capacity(paths.len());
        for path in &paths {
            let bytes = crate::thumbnails::zoom_bytes(path)?;
            let img = decode_upright(&bytes)?;
            images.push(img);
        }

        let canvas = crate::collage::compose(images, &engine_opts);

        if let Some(parent) = dest_for_write.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }

        if is_png {
            // PNG keeps the RGBA canvas as-is (a transparent mat survives).
            canvas
                .save_with_format(&dest_for_write, image::ImageFormat::Png)
                .map_err(|e| e.to_string())?;
        } else {
            // JPEG has no alpha: flatten the canvas onto the (opaque) background mat.
            let bg = engine_opts.background;
            let mut flat = image::RgbImage::new(canvas.width(), canvas.height());
            for (x, y, px) in canvas.enumerate_pixels() {
                let a = px[3] as f32 / 255.0;
                let blend = |c: usize| {
                    (px[c] as f32 * a + bg[c] as f32 * (1.0 - a)).round() as u8
                };
                flat.put_pixel(x, y, image::Rgb([blend(0), blend(1), blend(2)]));
            }
            use image::codecs::jpeg::JpegEncoder;
            let file = std::fs::File::create(&dest_for_write).map_err(|e| e.to_string())?;
            let mut writer = std::io::BufWriter::new(file);
            image::DynamicImage::ImageRgb8(flat)
                .write_with_encoder(JpegEncoder::new_with_quality(&mut writer, 92))
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())??;

    Ok(dest.to_string_lossy().into_owned())
}

/// Render a scaled-down PNG **preview** of the collage as a `data:image/png;base64,…` URL
/// (no file written), for the dialog's live preview. The geometry (width, row height, gap,
/// border, corner radius) is scaled down proportionally to `PREVIEW_MAX_W` so the preview's
/// layout matches the final render but is cheap to produce. PNG keeps the mat's alpha so a
/// transparent background previews faithfully.
#[cfg(feature = "collage")]
#[tauri::command]
pub async fn collage_preview(
    state: State<'_, AppState>,
    photo_ids: Vec<i64>,
    opts: CollageOptionsDto,
) -> Result<String, String> {
    use crate::collage::{CollageOptions, Fit};
    const PREVIEW_MAX_W: u32 = 900;

    if photo_ids.is_empty() {
        return Err("No photos selected for the collage".into());
    }

    let paths: Vec<PathBuf> = with_catalog(&state, |c| {
        let mut out = Vec::with_capacity(photo_ids.len());
        for &id in &photo_ids {
            match c.resolve_photo_path(id)? {
                Some(p) => out.push(p),
                None => {
                    return Err(crate::catalog::CatalogError::NotFound(format!(
                        "photo {id} is not currently reachable (its volume may be offline)"
                    )))
                }
            }
        }
        Ok(out)
    })?;

    // Scale geometry down (proportionally) so the preview is fast but matches the final layout.
    let scale = if opts.width > PREVIEW_MAX_W {
        PREVIEW_MAX_W as f64 / opts.width.max(1) as f64
    } else {
        1.0
    };
    let sc = |v: u32| (v as f64 * scale).round() as u32;
    let engine_opts = CollageOptions {
        width: sc(opts.width).max(1),
        aspect: parse_aspect(opts.aspect.as_deref()),
        row_height: sc(opts.row_height).max(1),
        gap: sc(opts.gap),
        background: parse_color(&opts.background),
        fit: if opts.fit.eq_ignore_ascii_case("cover") {
            Fit::Cover
        } else {
            Fit::Contain
        },
        border_width: sc(opts.border_width),
        corner_radius: sc(opts.corner_radius),
    };

    tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
        let mut images = Vec::with_capacity(paths.len());
        for path in &paths {
            let bytes = crate::thumbnails::zoom_bytes(path)?;
            let img = decode_upright(&bytes)?;
            images.push(img);
        }
        let canvas = crate::collage::compose(images, &engine_opts);
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(canvas)
            .write_to(&mut buf, image::ImageFormat::Png)
            .map_err(|e| e.to_string())?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(buf.into_inner());
        Ok(format!("data:image/png;base64,{b64}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Write a composed RGBA canvas to `dest`: PNG keeps alpha (a transparent mat survives);
/// any other format → JPEG q92 with alpha flattened onto `background` (JPEG has no alpha).
#[cfg(feature = "collage")]
fn write_collage_canvas(
    canvas: image::RgbaImage,
    dest: &Path,
    is_png: bool,
    background: [u8; 4],
) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    if is_png {
        canvas
            .save_with_format(dest, image::ImageFormat::Png)
            .map_err(|e| e.to_string())?;
    } else {
        let mut flat = image::RgbImage::new(canvas.width(), canvas.height());
        for (x, y, px) in canvas.enumerate_pixels() {
            let a = px[3] as f32 / 255.0;
            let blend = |c: usize| (px[c] as f32 * a + background[c] as f32 * (1.0 - a)).round() as u8;
            flat.put_pixel(x, y, image::Rgb([blend(0), blend(1), blend(2)]));
        }
        use image::codecs::jpeg::JpegEncoder;
        let file = std::fs::File::create(dest).map_err(|e| e.to_string())?;
        let mut writer = std::io::BufWriter::new(file);
        image::DynamicImage::ImageRgb8(flat)
            .write_with_encoder(JpegEncoder::new_with_quality(&mut writer, 92))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// One freeform tile placement from the canvas editor: normalized `x/y/w/h` (0–1 of the
/// canvas) + stacking order `z`. Matches the TS `Placement`.
#[cfg(feature = "collage")]
fn half() -> f32 {
    0.5
}
#[cfg(feature = "collage")]
fn one() -> f32 {
    1.0
}

/// Decode JPEG/preview bytes and bake in EXIF orientation (so a portrait shot isn't sideways
/// in the collage — `image::load_from_memory` does not auto-rotate). A no-op when the bytes
/// carry no orientation.
#[cfg(feature = "collage")]
fn decode_upright(bytes: &[u8]) -> Result<image::DynamicImage, String> {
    use image::ImageDecoder;
    let mut decoder = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .into_decoder()
        .map_err(|e| e.to_string())?;
    let orientation = decoder.orientation().map_err(|e| e.to_string())?;
    let mut img = image::DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    img.apply_orientation(orientation);
    Ok(img)
}

#[cfg(feature = "collage")]
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy)]
#[serde(rename_all = "camelCase")]
pub struct PlacementDto {
    pub photo_id: i64,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub z: i64,
    /// Focal offset (0..1; default 0.5 centered) for the cover-crop — pan within the frame.
    #[serde(default = "half")]
    pub ox: f32,
    #[serde(default = "half")]
    pub oy: f32,
    /// Zoom factor (≥1; default 1) for the cover-crop.
    #[serde(default = "one")]
    pub zoom: f32,
}

/// Freeform render options: the canvas pixel size + mat/border/corner styling.
#[cfg(feature = "collage")]
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FreeformOptionsDto {
    pub width: u32,
    pub height: u32,
    pub background: String,
    pub border_width: u32,
    pub corner_radius: u32,
}

/// Auto-arrange: lay the given photos out as the justified mosaic and return the result as
/// normalized freeform [`PlacementDto`]s (z = order), seeding the freeform canvas. Aspects
/// come from the upright (EXIF-applied) preview so the layout matches the rendered output.
#[cfg(feature = "collage")]
#[tauri::command]
pub async fn collage_auto_arrange(
    state: State<'_, AppState>,
    photo_ids: Vec<i64>,
    opts: CollageOptionsDto,
) -> Result<Vec<PlacementDto>, String> {
    use crate::collage::CollageOptions;
    if photo_ids.is_empty() {
        return Ok(Vec::new());
    }

    let paths: Vec<PathBuf> = with_catalog(&state, |c| {
        let mut out = Vec::with_capacity(photo_ids.len());
        for &id in &photo_ids {
            match c.resolve_photo_path(id)? {
                Some(p) => out.push(p),
                None => {
                    return Err(crate::catalog::CatalogError::NotFound(format!(
                        "photo {id} is not currently reachable (its volume may be offline)"
                    )))
                }
            }
        }
        Ok(out)
    })?;

    let engine_opts = CollageOptions {
        width: opts.width.max(1),
        aspect: parse_aspect(opts.aspect.as_deref()),
        row_height: opts.row_height.max(1),
        gap: opts.gap,
        background: [0, 0, 0, 255],
        fit: crate::collage::Fit::Contain,
        border_width: 0,
        corner_radius: 0,
    };

    tauri::async_runtime::spawn_blocking(move || -> Result<Vec<PlacementDto>, String> {
        let mut aspects = Vec::with_capacity(paths.len());
        for path in &paths {
            let bytes = crate::thumbnails::zoom_bytes(path)?;
            let img = decode_upright(&bytes)?;
            let (w, h) = (img.width(), img.height());
            aspects.push((w as f32 / h.max(1) as f32).max(0.01));
        }
        let (rects, height) = crate::collage::layout(&aspects, &engine_opts);
        let cw = engine_opts.width as f32;
        let ch = height.max(1) as f32;
        Ok(photo_ids
            .iter()
            .zip(rects.iter())
            .enumerate()
            .map(|(i, (&photo_id, r))| PlacementDto {
                photo_id,
                x: r.x as f32 / cw,
                y: r.y as f32 / ch,
                w: r.w as f32 / cw,
                h: r.h as f32 / ch,
                z: i as i64,
                ox: 0.5,
                oy: 0.5,
                zoom: 1.0,
            })
            .collect())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Resolve placements to reachable original paths (fails fast on an offline volume).
#[cfg(feature = "collage")]
fn resolve_placements(
    state: &State<'_, AppState>,
    placements: &[PlacementDto],
) -> Result<Vec<PathBuf>, String> {
    with_catalog(state, |c| {
        let mut out = Vec::with_capacity(placements.len());
        for p in placements {
            match c.resolve_photo_path(p.photo_id)? {
                Some(path) => out.push(path),
                None => {
                    return Err(crate::catalog::CatalogError::NotFound(format!(
                        "photo {} is not currently reachable (its volume may be offline)",
                        p.photo_id
                    )))
                }
            }
        }
        Ok(out)
    })
}

/// Shared freeform render params: tile rects + engine options + canvas size + PNG flag + mat.
#[cfg(feature = "collage")]
fn freeform_params(
    placements: &[PlacementDto],
    opts: &FreeformOptionsDto,
    format: &str,
) -> (
    Vec<crate::collage::Placement>,
    crate::collage::CollageOptions,
    u32,
    u32,
    bool,
    [u8; 4],
) {
    use crate::collage::{CollageOptions, Fit, Placement};
    let bg = parse_color(&opts.background);
    let engine_opts = CollageOptions {
        width: opts.width.max(1),
        aspect: None,
        row_height: 1,
        gap: 0,
        background: bg,
        fit: Fit::Contain,
        border_width: opts.border_width,
        corner_radius: opts.corner_radius,
    };
    let rects = placements
        .iter()
        .map(|p| Placement {
            x: p.x,
            y: p.y,
            w: p.w,
            h: p.h,
            z: p.z,
            ox: p.ox,
            oy: p.oy,
            zoom: p.zoom,
        })
        .collect();
    let is_png = format.eq_ignore_ascii_case("png");
    (rects, engine_opts, opts.width.max(1), opts.height.max(1), is_png, bg)
}

/// Decode each photo's preview (upright) and composite the freeform collage to `dest` off the
/// UI thread.
#[cfg(feature = "collage")]
async fn render_freeform_to_file(
    paths: Vec<PathBuf>,
    rects: Vec<crate::collage::Placement>,
    engine_opts: crate::collage::CollageOptions,
    cw: u32,
    ch: u32,
    is_png: bool,
    bg: [u8; 4],
    dest: PathBuf,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        let mut items = Vec::with_capacity(paths.len());
        for (path, rect) in paths.iter().zip(rects.into_iter()) {
            let bytes = crate::thumbnails::zoom_bytes(path)?;
            let img = decode_upright(&bytes)?;
            items.push((img, rect));
        }
        let canvas = crate::collage::compose_freeform(items, cw, ch, &engine_opts);
        write_collage_canvas(canvas, &dest, is_png, bg)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Composite a freeform collage from explicit tile placements (the canvas editor) and write
/// it to `dest_dir`. Returns the output path.
#[cfg(feature = "collage")]
#[tauri::command]
pub async fn make_collage_freeform(
    state: State<'_, AppState>,
    placements: Vec<PlacementDto>,
    opts: FreeformOptionsDto,
    format: String,
    dest_dir: String,
) -> Result<String, String> {
    if placements.is_empty() {
        return Err("No photos in the collage".into());
    }
    let paths = resolve_placements(&state, &placements)?;
    let (rects, engine_opts, cw, ch, is_png, bg) = freeform_params(&placements, &opts, &format);
    let ext = if is_png { "png" } else { "jpg" };
    let dest = unique_collage_path(&expand_home(&dest_dir).join(format!("collage.{ext}")));
    render_freeform_to_file(paths, rects, engine_opts, cw, ch, is_png, bg, dest.clone()).await?;
    Ok(dest.to_string_lossy().into_owned())
}

/// Composite a freeform collage, save it **into the library** (`<root>/Collages/`), index it
/// into the catalog (UUID + sidecar + metadata), and return the new photo id so the UI can
/// jump to it.
#[cfg(feature = "collage")]
#[tauri::command]
pub async fn save_collage_to_catalog(
    state: State<'_, AppState>,
    placements: Vec<PlacementDto>,
    opts: FreeformOptionsDto,
    format: String,
    kind: String,
) -> Result<i64, String> {
    if placements.is_empty() {
        return Err("No photos in the collage".into());
    }
    let paths = resolve_placements(&state, &placements)?;
    let (rects, engine_opts, cw, ch, is_png, bg) = freeform_params(&placements, &opts, &format);

    // Collages live under the library root so they're catalog-managed (relative paths).
    let root = with_catalog(&state, |c| Ok(c.root().to_path_buf()))?;
    let ext = if is_png { "png" } else { "jpg" };
    let dest = unique_collage_path(&root.join("Collages").join(format!("collage.{ext}")));

    render_freeform_to_file(paths, rects, engine_opts, cw, ch, is_png, bg, dest.clone()).await?;

    // Index the new file (no import batch — a collage isn't a camera ingest). Lock taken
    // after the render await, never across it.
    let guard = state.catalog.lock().map_err(|e| e.to_string())?;
    let catalog = guard.as_ref().ok_or("No catalog is open")?;
    let photo_id = crate::scanner::index_generated_file(catalog, &dest)?;

    // Auto-tag the collage so they're grouped/filterable, e.g. "Collage/Grid". These are
    // organizational tags — mark the "Collage" parent AND the leaf non-exportable so they're
    // never emitted as keywords on export/publish (Instagram/Flickr/SmugMug).
    let kind = kind.trim();
    if !kind.is_empty() {
        if let Ok(parent) = catalog.create_tag("Collage") {
            let _ = catalog.set_tag_exportable(parent, false);
        }
        if let Ok(tag_id) = catalog.create_tag(&format!("Collage/{kind}")) {
            let _ = catalog.set_tag_exportable(tag_id, false);
            let _ = catalog.assign_tag(photo_id, tag_id);
        }
    }
    Ok(photo_id)
}

#[cfg(all(test, feature = "collage"))]
mod collage_tests {
    use super::{parse_aspect, parse_color};

    #[test]
    fn aspect_free_and_empty_are_none() {
        assert_eq!(parse_aspect(None), None);
        assert_eq!(parse_aspect(Some("")), None);
        assert_eq!(parse_aspect(Some("free")), None);
        assert_eq!(parse_aspect(Some("Free")), None);
    }

    #[test]
    fn aspect_ratios_parse() {
        assert_eq!(parse_aspect(Some("1:1")), Some((1, 1)));
        assert_eq!(parse_aspect(Some("4:5")), Some((4, 5)));
        assert_eq!(parse_aspect(Some(" 16 : 9 ")), Some((16, 9)));
    }

    #[test]
    fn aspect_garbage_falls_back_to_free() {
        assert_eq!(parse_aspect(Some("nonsense")), None);
        assert_eq!(parse_aspect(Some("1:0")), None);
        assert_eq!(parse_aspect(Some("0:1")), None);
    }

    #[test]
    fn color_hex_forms() {
        assert_eq!(parse_color("#ffffff"), [255, 255, 255, 255]);
        assert_eq!(parse_color("#000000"), [0, 0, 0, 255]);
        assert_eq!(parse_color("#f0a"), [255, 0, 170, 255]);
        assert_eq!(parse_color("#11223344"), [0x11, 0x22, 0x33, 0x44]);
    }

    #[test]
    fn color_rgba_and_fallback() {
        assert_eq!(parse_color("rgba(10, 20, 30, 0.5)"), [10, 20, 30, 128]);
        assert_eq!(parse_color("rgb(1,2,3)"), [1, 2, 3, 255]);
        // Unrecognized → opaque white (a sensible default mat).
        assert_eq!(parse_color("bogus"), [255, 255, 255, 255]);
    }
}

