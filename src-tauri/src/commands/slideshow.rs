//! Slideshow → movie commands: render a selection to an .mp4 via ffmpeg.
//!
//! Gated on the `slideshow` Cargo feature; see `docs/slideshow.md`. The body lives in core
//! (`app::slideshow`), shared with the GPUI Slideshow module.

use super::*;
use tauri::{AppHandle, Manager};

/// Slideshow render options as sent by the frontend (serde camelCase). Mirrors the engine's
/// [`crate::slideshow::SlideshowOptions`] and the dialog (docs/slideshow.md → Options):
/// per-photo duration, optional crossfade + its length, Ken Burns toggle, frame rate, and the
/// chosen aspect/resolution preset as explicit `width`×`height`.
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

/// Render the selected photos (in the supplied order) into a single `.mp4` slideshow via
/// ffmpeg, writing it to `dest_dir`. Returns the absolute output path. The claim (catalog
/// lock) and the frame renders + encode run on a blocking worker
/// ([`crate::app::slideshow`]); ffmpeg's `-progress` is forwarded as `slideshow:progress`.
/// A newer render or a catalog switch cancels this one.
#[cfg(feature = "slideshow")]
#[tauri::command]
pub async fn make_slideshow(
    app: AppHandle,
    photo_ids: Vec<i64>,
    opts: SlideshowOptionsDto,
    dest_dir: String,
) -> Result<String, String> {
    let state = app.state::<AppState>().inner().clone();
    let engine_opts = crate::slideshow::SlideshowOptions {
        duration_per_photo: opts.duration_per_photo,
        transition: opts.transition,
        transition_duration: opts.transition_duration,
        ken_burns: opts.ken_burns,
        fps: opts.fps,
        width: opts.width,
        height: opts.height,
    };
    crate::app::spawn_blocking(move || {
        let ffmpeg = crate::slideshow::ffmpeg_path().map(std::path::PathBuf::from);
        crate::app::slideshow::claim_slideshow(&state, None, &photo_ids, engine_opts, &dest_dir, ffmpeg)?.run()
    })
    .await
    .map_err(|e| e.to_string())?
    .map(|p| p.to_string_lossy().into_owned())
}
