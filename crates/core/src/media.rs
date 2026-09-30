//! Blocking render bodies behind the native media protocols: the bytes a `thumb://`,
//! `preview://`, `zoom://` or (with the `edit` feature) `edit://` request serves.
//! [`render_bytes`] is the [`ImagePool`](crate::image_pool::ImagePool) runner every front end
//! installs (`app::boot`); [`render_edit_bytes`] renders from a photo's resolved preview/zoom
//! tier or a resident RAW working image. Core code — the Tauri protocol handler
//! (`protocol.rs`), the edit commands and the GPUI app call in here; nothing here depends on
//! a command layer.
//!
//! [`render_image`] is the same render for a front end that draws pixels itself (the GPUI
//! app, #101): the same key, the same resolution and disk caches, but it returns the decoded,
//! rotated image instead of JPEG/PNG bytes, so nothing is re-encoded only to be decoded again.
//! `render_edit_image` (the `edit` feature) is the Darkroom's frame without the encode.

use crate::app::AppState;
use crate::catalog::ResolveMode;
use crate::image_pool::{ImageKind, JobKey};
use crate::thumbnails::{preview_bytes, thumbnail_bytes, zoom_bytes};
use image::DynamicImage;
use std::path::PathBuf;

/// One photo tier, resolved: a reachable copy (if any), the user rotation, the cover
/// version's record (thumbnails only), and whether it is a video.
struct Resolved {
    absolute: Option<PathBuf>,
    rotation: i64,
    cover: Option<String>,
    is_video: bool,
}

/// Gather the path CANDIDATES (pure SQL) and the rotation under a brief lock, then stat them
/// OFF the lock via `pick_existing` so a slow/offline NAS can't serialize the whole app.
/// `pick_existing` still returns the best available copy (local cache > primary > backup);
/// the reachability cache only reorders the stats.
fn resolve(state: &AppState, id: i64, kind: ImageKind) -> Result<Resolved, String> {
    let (candidates, rotation, cover) = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_ref().ok_or("no catalog open")?;
        let candidates = catalog.photo_path_candidates(id).map_err(|e| e.to_string())?;
        let rotation = catalog.photo_rotation(id).unwrap_or(0);
        // The cover version's settings, when the grid should show a version's look.
        let cover = match kind {
            ImageKind::Thumb => catalog.cover_of(id).ok().flatten().map(|(_, _, json)| json),
            _ => None,
        };
        (candidates, rotation, cover)
    };
    let is_video = candidates.iter().any(|c| crate::scanner::is_video(&c.path));
    // A thumbnail has a persistent fallback below, so it resolves in FastDisplay: a
    // cached-unreachable volume is never statted and the grid falls back at once. Preview
    // and zoom have no fallback — they need the original, so they keep strict checking.
    let mode = match kind {
        ImageKind::Thumb => ResolveMode::FastDisplay,
        ImageKind::Preview | ImageKind::Zoom => ResolveMode::OriginalRequired,
    };
    let absolute = crate::volume_health::pick_existing(&candidates, &state.volume_health, mode);
    Ok(Resolved { absolute, rotation, cover, is_video })
}

/// Render one image and return its JPEG bytes, or an error string.
///
/// The runner [`app::boot`](crate::app::boot) injects into the
/// [`ImagePool`](crate::image_pool::ImagePool); the Tauri protocol's no-pool fallback calls it
/// directly.
///
/// It matches on [`JobKey`], whose `Edit` variant exists only with this crate's `edit` feature,
/// so the match must live in this crate under that same gate. In a front end it would be gated
/// by the front end's `edit` instead, and Cargo's feature unification can turn the core's on
/// while the front end's is off (another workspace member asked for it) — a non-exhaustive
/// match that fails to compile.
pub fn render_bytes(state: &AppState, key: JobKey) -> Result<Vec<u8>, String> {
    let (id, kind) = match key {
        JobKey::Photo { id, kind } => (id, kind),
        #[cfg(feature = "edit")]
        JobKey::Edit(job) => return render_edit_bytes(state, &job),
    };
    let Resolved { absolute, rotation, cover, .. } = resolve(state, id, kind)?;
    match absolute {
        Some(absolute) => match kind {
            ImageKind::Thumb => {
                // A cover shows the version's look; if it cannot be rendered (no edit
                // engine in this build, an engine-2 cover without the decoder, a failure)
                // the plain thumbnail below is served instead.
                if let Some(json) = &cover {
                    #[cfg(feature = "edit")]
                    match crate::plugins::edit::cover::cover_thumb(&absolute, id, json) {
                        Ok(bytes) => return crate::thumbnails::rotate_jpeg(bytes, rotation),
                        Err(e) => eprintln!("cover thumbnail for photo {id}: {e}"),
                    }
                    #[cfg(not(feature = "edit"))]
                    let _ = json;
                }
                // Apply the user rotation on top of the file's baked EXIF orientation, then
                // keep the rotated id-keyed copy so the photo stays browsable (correctly
                // oriented) after it's offloaded and the NAS goes offline.
                let bytes = crate::thumbnails::rotate_jpeg(thumbnail_bytes(&absolute)?, rotation)?;
                crate::thumbnails::save_persistent_thumb(id, &bytes);
                Ok(bytes)
            }
            ImageKind::Preview => crate::thumbnails::rotate_jpeg(preview_bytes(&absolute)?, rotation),
            ImageKind::Zoom => crate::thumbnails::rotate_jpeg(zoom_bytes(&absolute)?, rotation),
        },
        // Original unreachable (e.g. offloaded + NAS unmounted): fall back to the kept
        // thumbnail so the grid still shows the photo. Preview/zoom need the original.
        None => {
            let e = format!("no reachable copy of photo {id}");
            match kind {
                ImageKind::Thumb => crate::thumbnails::read_persistent_thumb(id).ok_or(e),
                _ => Err(e),
            }
        }
    }
}

/// A decoded image from [`render_image`]: display-ready (oriented, user rotation applied,
/// sRGB) pixels, usually `Rgb8` straight from a cached JPEG.
#[derive(Clone, Debug)]
pub struct DecodedImage {
    pub image: DynamicImage,
    /// A video whose poster frame could not be made (no `ffmpeg`, an unreadable clip): this
    /// is [`video_tile`], not the clip's pixels. A front end can mark it as a video.
    pub video_tile: bool,
}

impl DecodedImage {
    fn pixels(image: DynamicImage) -> Self {
        Self { image, video_tile: false }
    }
}

/// [`render_bytes`] for a front end that draws decoded pixels (the GPUI app's pool runner):
/// the same key, resolution, disk caches, cover thumbnails, persistent-thumbnail fallback and
/// rotation, returning the image instead of JPEG bytes.
///
/// The thumb/preview/zoom tiers still come from the on-disk JPEG caches — one decode and no
/// re-encode: the user rotation is a pixel permutation here, where `render_bytes` re-encodes.
/// The one encode left is the persistent thumbnail of a *rotated* photo, which must stay the
/// file `render_bytes` would have written. An edit key renders through `render_edit_image`,
/// with no JPEG/PNG. A video with no poster frame is [`video_tile`]
/// ([`DecodedImage::video_tile`]) rather than an error: there is no inline playback (#97).
pub fn render_image(state: &AppState, key: JobKey) -> Result<DecodedImage, String> {
    let (id, kind) = match key {
        JobKey::Photo { id, kind } => (id, kind),
        #[cfg(feature = "edit")]
        JobKey::Edit(job) => return render_edit_image(state, &job).map(DecodedImage::pixels),
    };
    let resolved = resolve(state, id, kind)?;
    let is_video = resolved.is_video;
    match decode_tier(id, kind, resolved) {
        Ok(image) => Ok(DecodedImage::pixels(image)),
        Err(e) if is_video => {
            eprintln!("video poster for photo {id}: {e}; showing the video tile");
            Ok(DecodedImage { image: video_tile(), video_tile: true })
        }
        Err(e) => Err(e),
    }
}

/// [`render_image`]'s photo body: the cached tier's JPEG, decoded once and rotated.
fn decode_tier(id: i64, kind: ImageKind, resolved: Resolved) -> Result<DynamicImage, String> {
    let Resolved { absolute, rotation, cover, .. } = resolved;
    let decode = |bytes: &[u8]| image::load_from_memory(bytes).map_err(|e| e.to_string());
    let rotate = |img| crate::thumbnails::rotate_image(img, rotation);
    let Some(absolute) = absolute else {
        let e = format!("no reachable copy of photo {id}");
        return match kind {
            // The kept thumbnail is already rotated.
            ImageKind::Thumb => decode(&crate::thumbnails::read_persistent_thumb(id).ok_or(e)?),
            _ => Err(e),
        };
    };
    match kind {
        ImageKind::Thumb => {
            if let Some(json) = &cover {
                #[cfg(feature = "edit")]
                match crate::plugins::edit::cover::cover_thumb(&absolute, id, json) {
                    Ok(bytes) => return decode(&bytes).map(rotate),
                    Err(e) => eprintln!("cover thumbnail for photo {id}: {e}"),
                }
                #[cfg(not(feature = "edit"))]
                let _ = json;
            }
            let bytes = thumbnail_bytes(&absolute)?;
            let img = rotate(decode(&bytes)?);
            // The same persistent copy `render_bytes` keeps: the cached JPEG as-is, or, for a
            // rotated photo, `rotate_jpeg`'s re-encode of these same pixels.
            if ((rotation % 360) + 360) % 360 == 0 {
                crate::thumbnails::save_persistent_thumb(id, &bytes);
            } else if let Ok(rotated) = crate::thumbnails::encode_rotated_jpeg(&img) {
                crate::thumbnails::save_persistent_thumb(id, &rotated);
            }
            Ok(img)
        }
        ImageKind::Preview => decode(&preview_bytes(&absolute)?).map(rotate),
        ImageKind::Zoom => decode(&zoom_bytes(&absolute)?).map(rotate),
    }
}

/// The generic tile a video shows when no poster frame can be made: 16:9, dark, with a
/// light "play" triangle in a ring. Drawn, not loaded — it must work with no assets and no
/// `ffmpeg`.
pub fn video_tile() -> DynamicImage {
    const W: u32 = 512;
    const H: u32 = 288;
    let (cx, cy, r) = (W as f32 / 2.0, H as f32 / 2.0, 44.0_f32);
    // The triangle: left edge at `left`, tip at `tip`, half-height shrinking to 0 at the tip.
    let (left, tip) = (-r * 0.45, r * 0.7);
    DynamicImage::ImageRgb8(image::RgbImage::from_fn(W, H, |x, y| {
        let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
        let in_triangle =
            dx >= left && dx <= tip && dy.abs() <= (tip - dx) / (tip - left) * r * 0.6;
        if in_triangle {
            image::Rgb([0xe6, 0xe6, 0xe6])
        } else if dx * dx + dy * dy <= r * r {
            image::Rgb([0x4a, 0x4a, 0x4a])
        } else {
            image::Rgb([0x26, 0x26, 0x26])
        }
    }))
}

/// What an edit render produced before encoding: a frame, or the sensor-clipping overlay,
/// which `clip_overlay_png` hands back already encoded.
#[cfg(feature = "edit")]
enum EditOut {
    Frame(DynamicImage),
    ClipPng(Vec<u8>),
}

#[cfg(feature = "edit")]
/// The blocking body of an edit render — the `edit://` protocol
/// (`protocol::handle_edit_request`, the Darkroom stage) and the `render_edit` command
/// share it. Resolves the photo's path, decodes the source tier, renders, and encodes:
/// JPEG q90, or lossless PNG for a base-only frame (the GL drag tier's texture — a JPEG
/// base would spend the preview↔export parity budget before the shader ran). Operates on
/// the embedded preview or zoom tier, never the original file.
pub fn render_edit_bytes(state: &AppState, job: &crate::image_pool::EditJob) -> Result<Vec<u8>, String> {
    use crate::plugins::edit::{self, timing::Stages};
    let mut t = Stages::start(format!(
        "render_edit photo={} max_edge={} hi_res={} base_only={} source={}",
        job.photo_id, job.max_edge, job.hi_res, job.base_only, job.source.to_query()
    ));
    let out = match render_edit(state, job, &mut t)? {
        EditOut::ClipPng(bytes) => {
            t.report(&format!("clip bytes={}", bytes.len()));
            return Ok(bytes);
        }
        EditOut::Frame(out) => out,
    };
    let bytes = if job.base_only {
        edit::encode_png_fast(&out)?
    } else {
        edit::encode_jpeg(&out, 90)?
    };
    t.mark(if job.base_only { "encode_png" } else { "encode_jpeg" });
    t.report(&format!("bytes={}", bytes.len()));
    Ok(bytes)
}

#[cfg(feature = "edit")]
/// [`render_edit_bytes`] without the encode: the frame `render_proxy` (or the hi-res and
/// engine-2 paths) produced, for a front end that uploads pixels (the GPUI Darkroom, #101).
/// The same source selection, caches and errors; `base_only` still skips the look. The
/// clipping overlay (`clip`) arrives from the renderer as PNG and is decoded here.
pub fn render_edit_image(state: &AppState, job: &crate::image_pool::EditJob) -> Result<DynamicImage, String> {
    use crate::plugins::edit::timing::Stages;
    let mut t = Stages::start(format!(
        "render_edit_image photo={} max_edge={} hi_res={} base_only={} source={}",
        job.photo_id, job.max_edge, job.hi_res, job.base_only, job.source.to_query()
    ));
    let out = match render_edit(state, job, &mut t)? {
        EditOut::Frame(out) => out,
        EditOut::ClipPng(bytes) => image::load_from_memory(&bytes).map_err(|e| e.to_string())?,
    };
    t.report(&format!("{}x{}", out.width(), out.height()));
    Ok(out)
}

#[cfg(feature = "edit")]
/// The shared body of [`render_edit_bytes`] and [`render_edit_image`], up to the encode.
fn render_edit(
    state: &AppState,
    job: &crate::image_pool::EditJob,
    t: &mut crate::plugins::edit::timing::Stages,
) -> Result<EditOut, String> {
    use crate::plugins::edit::{self, RenderOpts, RenderSource, SourceToken};
    // A working-image token renders from the resident RAW decode, or nothing: a stale
    // token (photo switched, session closed) is a 404, never a fallback to other pixels.
    if let SourceToken::Working { .. } = &job.source {
        let opts = RenderOpts { skip_look: job.base_only };
        let image = working_image(&job.source)?;
        if job.clip {
            return Ok(EditOut::ClipPng(edit::clip_overlay_png(
                job.source.clone(),
                image,
                &job.edit_json,
                job.max_edge,
            )?));
        }
        let out = edit::render_proxy(
            RenderSource::Working { token: job.source.clone(), image },
            &job.edit_json,
            job.max_edge,
            opts,
        )?;
        t.mark("render");
        return Ok(EditOut::Frame(out));
    }
    // Gather path candidates under a brief lock (pure SQL), then stat + decode + render
    // off the lock so a slow/offline NAS can't serialize the app.
    let candidates = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_ref().ok_or("No catalog is open")?;
        catalog.photo_path_candidates(job.photo_id).map_err(|e| e.to_string())?
    };
    t.mark("candidates");
    // OriginalRequired: an edit render needs the real original, so a cached-unreachable
    // flag must never stand in for a stat.
    let path = crate::volume_health::pick_existing(
        &candidates,
        &state.volume_health,
        crate::catalog::ResolveMode::OriginalRequired,
    )
    .ok_or_else(|| format!("no reachable copy of photo {}", job.photo_id))?;
    t.mark("pick_path");
    if job.clip {
        return Err("the sensor-clipping overlay needs the RAW working image".into());
    }
    let opts = RenderOpts { skip_look: job.base_only };
    // An engine-2 record with no session token (the Library loupe, a version outside
    // Develop): the RAW through its pipeline from a bounded offline load, never the
    // camera preview. `hi_res` needs nothing more — engine 2 is always the full decode.
    #[cfg(feature = "raw")]
    if edit::record_engine(&job.edit_json) == 2 {
        let budget = crate::develop::session::cache_budget_bytes(state);
        let (token, image) = crate::develop::offline::working_image_for(job.photo_id, &path, budget)?;
        t.mark("working_image");
        let out = edit::render_proxy(RenderSource::Working { token, image }, &job.edit_json, job.max_edge, opts)?;
        t.mark("render");
        return Ok(EditOut::Frame(out));
    }
    let out = if job.hi_res {
        // Zoom tier: too large to keep resident — decode per render.
        let jpeg = crate::thumbnails::zoom_bytes(&path)?;
        t.mark("zoom_bytes");
        let img = image::load_from_memory(&jpeg).map_err(|e| e.to_string())?;
        t.mark("decode");
        edit::render_image_opts(RenderSource::Decoded(img), &job.edit_json, job.max_edge, opts)?
    } else {
        // Proxy tier: live sliders render this many times a second — through the decode
        // cache and the framed-base cache, so a look-only frame pays look + encode.
        let jpeg = crate::thumbnails::preview_bytes(&path)?;
        t.mark("preview_bytes");
        edit::render_proxy(RenderSource::PreviewJpeg(&jpeg), &job.edit_json, job.max_edge, opts)?
    };
    t.mark("render");
    Ok(EditOut::Frame(out))
}

#[cfg(feature = "edit")]
/// The resident working image a token names — or a clear error, never other pixels.
pub fn working_image(token: &crate::plugins::edit::SourceToken) -> Result<std::sync::Arc<crate::plugins::edit::WorkingImage>, String> {
    #[cfg(feature = "raw")]
    {
        return crate::develop::resident(token)
            .ok_or_else(|| format!("working image {} is not resident", token.to_query()));
    }
    #[cfg(not(feature = "raw"))]
    {
        let _ = token;
        Err("this build has no RAW decoder; no working image can be resident".into())
    }
}

#[cfg(all(test, feature = "edit", feature = "raw"))]
mod tests {
    use super::*;

    /// A working-image token nothing resident answers to is an error — the `edit://`
    /// responder turns it into a 404 — never a fall-through to the preview pixels.
    #[test]
    fn a_stale_working_token_is_an_error_not_other_pixels() {
        let token = crate::plugins::edit::SourceToken::Working { photo_id: 999_999, generation: 1 };
        let err = match working_image(&token) {
            Err(e) => e,
            Ok(_) => panic!("a token nothing minted found an image"),
        };
        assert!(err.contains("w:999999:1") && err.contains("not resident"), "{err}");
    }
}
