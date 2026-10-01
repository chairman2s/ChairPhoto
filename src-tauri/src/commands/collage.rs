//! Collage compositor commands — justified-mosaic and freeform layout, preview
//! rendering, and saving a finished collage back into the catalog.
//!
//! Gated on the `collage` Cargo feature; see `docs/collage.md`. The freeform bodies live in
//! core (`app::collage`), shared with the GPUI Collage module; these are thin wrappers.

use super::*;
use crate::app::collage::{cached_preview, decode_upright, parse_aspect, parse_color, write_canvas};
pub use crate::app::collage::{CollageOptionsDto, FreeformOptionsDto, PlacementDto};
use crate::app::unique_path;
use tauri::State;

/// Resolve every id to a reachable original (fails fast on an offline volume).
#[cfg(feature = "collage")]
fn resolve_ids(state: &State<'_, AppState>, photo_ids: &[i64]) -> Result<Vec<PathBuf>, String> {
    with_catalog(state, |c| {
        let mut out = Vec::with_capacity(photo_ids.len());
        for &id in photo_ids {
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
    })
}

/// Composite the given photos (in the supplied order) into a single justified-mosaic image
/// and write it to `dest_dir`. Returns the absolute output path. Superseded by the freeform
/// canvas: no frontend calls it any more (docs/collage.md → Implementation).
#[cfg(feature = "collage")]
#[tauri::command]
pub async fn make_collage(
    state: State<'_, AppState>,
    photo_ids: Vec<i64>,
    opts: CollageOptionsDto,
    dest_dir: String,
    format: String,
) -> Result<String, String> {
    if photo_ids.is_empty() {
        return Err("No photos selected for the collage".into());
    }
    let paths = resolve_ids(&state, &photo_ids)?;
    let engine_opts = opts.to_engine();
    let is_png = format.eq_ignore_ascii_case("png");
    let ext = if is_png { "png" } else { "jpg" };
    let dest = unique_path(&expand_home(&dest_dir).join(format!("collage.{ext}")));

    let dest_for_write = dest.clone();
    crate::app::spawn_blocking(move || -> Result<(), String> {
        let mut images = Vec::with_capacity(paths.len());
        for path in &paths {
            images.push(decode_upright(&crate::thumbnails::zoom_bytes(path)?)?);
        }
        let canvas = crate::collage::compose(images, &engine_opts);
        write_canvas(canvas, &dest_for_write, is_png, engine_opts.background)
    })
    .await
    .map_err(|e| e.to_string())??;

    Ok(dest.to_string_lossy().into_owned())
}

/// Render a scaled-down PNG **preview** of the collage as a `data:image/png;base64,…` URL.
/// Superseded by the freeform canvas: no frontend calls it any more.
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
    let paths = resolve_ids(&state, &photo_ids)?;

    // Scale geometry down (proportionally) so the preview is fast but matches the final layout.
    let scale = if opts.width > PREVIEW_MAX_W { PREVIEW_MAX_W as f64 / opts.width.max(1) as f64 } else { 1.0 };
    let sc = |v: u32| (v as f64 * scale).round() as u32;
    let engine_opts = CollageOptions {
        width: sc(opts.width).max(1),
        aspect: parse_aspect(opts.aspect.as_deref()),
        row_height: sc(opts.row_height).max(1),
        gap: sc(opts.gap),
        background: parse_color(&opts.background),
        fit: if opts.fit.eq_ignore_ascii_case("cover") { Fit::Cover } else { Fit::Contain },
        border_width: sc(opts.border_width),
        corner_radius: sc(opts.corner_radius),
    };

    crate::app::spawn_blocking(move || -> Result<String, String> {
        let mut images = Vec::with_capacity(paths.len());
        for path in &paths {
            images.push(decode_upright(&crate::thumbnails::zoom_bytes(path)?)?);
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

/// Auto-arrange: lay the given photos out as the justified mosaic and return normalized
/// freeform placements (z = order), seeding the freeform canvas.
#[cfg(feature = "collage")]
#[tauri::command]
pub async fn collage_auto_arrange(
    state: State<'_, AppState>,
    photo_ids: Vec<i64>,
    opts: CollageOptionsDto,
) -> Result<Vec<PlacementDto>, String> {
    let state = state.inner().clone();
    crate::app::spawn_blocking(move || {
        crate::app::collage::auto_arrange(&state, None, &photo_ids, &opts, &cached_preview())
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
    let state = state.inner().clone();
    crate::app::spawn_blocking(move || {
        crate::app::collage::make_freeform(&state, None, &placements, &opts, &format, &dest_dir, &cached_preview())
    })
    .await
    .map_err(|e| e.to_string())?
    .map(|p| p.to_string_lossy().into_owned())
}

/// Composite a freeform collage, save it **into the library** (`<root>/Collages/`), index it
/// into the catalog (UUID + sidecar + metadata), tag it `Collage/<kind>`, and return the new
/// photo id. The index is bound to the catalog the ids were resolved in.
#[cfg(feature = "collage")]
#[tauri::command]
pub async fn save_collage_to_catalog(
    state: State<'_, AppState>,
    placements: Vec<PlacementDto>,
    opts: FreeformOptionsDto,
    format: String,
    kind: String,
) -> Result<i64, String> {
    let state = state.inner().clone();
    crate::app::spawn_blocking(move || {
        crate::app::collage::save_to_catalog(&state, None, &placements, &opts, &format, &kind, &cached_preview())
    })
    .await
    .map_err(|e| e.to_string())?
}
