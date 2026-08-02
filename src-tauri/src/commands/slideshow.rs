//! Slideshow → movie commands: render a selection to an .mp4 via ffmpeg.
//!
//! Gated on the `slideshow` Cargo feature; see `docs/slideshow.md` and `slideshow/`.

use super::*;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Emitter, Manager};

/// Slideshow render options as sent by the frontend (serde camelCase). Mirrors the engine's
/// [`crate::slideshow::SlideshowOptions`] and the dialog (docs/slideshow.md → Options):
/// per-photo duration, optional crossfade + its length, Ken Burns toggle, frame rate, and the
/// chosen aspect/resolution preset as explicit `width`×`height`. Parsed into the engine
/// options by [`make_slideshow`].
#[cfg(feature = "slideshow")]
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlideshowOptionsDto {
    /// Seconds each photo is shown (full clip length, crossfade overlap included).
    pub duration_per_photo: f64,
    /// Crossfade between clips (`true`) or hard cuts (`false`).
    pub transition: bool,
    /// Crossfade length in seconds (ignored when `transition` is false).
    pub transition_duration: f64,
    /// Apply a slow Ken Burns pan/zoom per photo.
    pub ken_burns: bool,
    /// Output frame rate (default 30 in the dialog).
    pub fps: u32,
    /// Output width in pixels (from the aspect/resolution preset).
    pub width: u32,
    /// Output height in pixels (from the aspect/resolution preset).
    pub height: u32,
}

/// Progress event payload for slideshow encoding, emitted as `slideshow:progress`. `done`/
/// `total` are raw ffmpeg output-frame counts (mirrors `import:progress`'s shape).
#[cfg(feature = "slideshow")]
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SlideshowProgress {
    done: u32,
    total: u32,
}

/// A destination path that doesn't already exist (`slideshow.mp4`, then `slideshow (2).mp4`,
/// …) so a repeat render never clobbers an earlier movie. Mirrors [`unique_collage_path`].
#[cfg(feature = "slideshow")]
fn unique_slideshow_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("slideshow");
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

/// Render the selected photos (in the supplied order — the frontend hands them in the user's
/// manual-reorder order) into a single `.mp4` slideshow via ffmpeg, writing it to `dest_dir`.
/// Returns the absolute output path.
///
/// Pipeline (docs/slideshow.md): resolve each photo's Original through the same resolver as
/// export, render a still **frame JPEG** per photo into a temp dir at a resolution comfortably
/// above the output (so Ken Burns zoom has pixels to sample), then hand the ordered frames to
/// [`crate::slideshow::render`] under `spawn_blocking` so the UI thread never blocks. ffmpeg's
/// `-progress` is forwarded as `slideshow:progress` events. Errors clearly when ffmpeg is not
/// installed (the runtime gate is `crate::slideshow::ffmpeg_path()`).
/// Rotate a rendered frame's pixels to its EXIF display orientation and drop the tag, so the
/// frame is physically upright. Needed because ffmpeg does not auto-rotate still inputs from
/// EXIF — a portrait photo (Orientation 6/8) would otherwise appear sideways in the video.
#[cfg(feature = "slideshow")]
fn bake_orientation(path: &Path) -> Result<(), String> {
    use image::ImageDecoder;
    let reader = image::ImageReader::open(path)
        .map_err(|e| e.to_string())?
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let orientation = decoder.orientation().map_err(|e| e.to_string())?;
    if orientation == image::metadata::Orientation::NoTransforms {
        return Ok(()); // already upright — nothing to bake
    }
    let mut img = image::DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    img.apply_orientation(orientation);
    img.save(path).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(feature = "slideshow")]
#[tauri::command]
pub async fn make_slideshow(
    app: AppHandle,
    photo_ids: Vec<i64>,
    opts: SlideshowOptionsDto,
    dest_dir: String,
) -> Result<String, String> {
    if photo_ids.is_empty() {
        return Err("No photos selected for the slideshow".into());
    }

    // Pre-flight: a clear, actionable error before we render any frames if ffmpeg is missing.
    if crate::slideshow::ffmpeg_path().is_none() {
        return Err(
            "ffmpeg not found on PATH — install ffmpeg to render slideshows".to_string(),
        );
    }

    let engine_opts = crate::slideshow::SlideshowOptions {
        duration_per_photo: opts.duration_per_photo,
        transition: opts.transition,
        transition_duration: opts.transition_duration,
        ken_burns: opts.ken_burns,
        fps: opts.fps,
        width: opts.width,
        height: opts.height,
    };

    // Resolve every photo's Original up front (under the lock), then release it so the slow
    // frame renders + encode run off the UI thread. Same resolver the export path uses; an
    // unmounted NAS / offline original is reported rather than silently dropped.
    let resolved = {
        let state = app.state::<AppState>();
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_ref().ok_or("No catalog is open")?;
        crate::export::resolve_originals(catalog, &photo_ids, &[], None)
    };
    if resolved.items.is_empty() {
        return Err(
            "None of the selected photos are reachable (their volumes may be offline)".into(),
        );
    }

    let dir = expand_home(&dest_dir);
    let dest = unique_slideshow_path(&dir.join("slideshow.mp4"));

    // Render frames at a resolution comfortably above the output so the Ken Burns zoom (the
    // engine oversamples the still to 2× the target) has real pixels to crop into. Cap the
    // long edge so a huge RAW doesn't produce gigantic intermediate JPEGs.
    let frame_max_width = (engine_opts.width.max(engine_opts.height) * 2).min(4096);

    let dest_for_write = dest.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        // Temp working dir for the per-photo frame JPEGs (cleaned up after the encode).
        let work = std::env::temp_dir().join(format!(
            "chairphoto_slideshow_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;

        let mut frame_paths: Vec<PathBuf> = Vec::with_capacity(resolved.items.len());
        for (i, item) in resolved.items.iter().enumerate() {
            let frame = work.join(format!("frame_{i:04}.jpg"));
            crate::export::write_item_jpeg(item, Some(frame_max_width), &frame)?;
            // ffmpeg ignores EXIF orientation on still inputs, so a portrait shot would come
            // out sideways. Bake the orientation into the pixels (and drop the tag) here.
            bake_orientation(&frame)?;
            frame_paths.push(frame);
        }

        if let Some(parent) = dest_for_write.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }

        let render_result = crate::slideshow::render(
            &frame_paths,
            &engine_opts,
            &dest_for_write,
            |done, total| {
                let _ = app.emit("slideshow:progress", SlideshowProgress { done, total });
            },
        );

        // Best-effort cleanup of the temp frames regardless of render outcome.
        let _ = std::fs::remove_dir_all(&work);

        render_result
    })
    .await
    .map_err(|e| e.to_string())??;

    Ok(dest.to_string_lossy().into_owned())
}

// ── Map & Geofence commands (feature = "map") ─────────────────────────────────
//
// All commands in this section are compiled only when the `map` feature is on.
// The frontend checks `plugin_features()` and keeps the Map module inert when it's not.

