//! Blocking render bodies behind the native media protocols: the bytes an `edit://`
//! request serves, rendered from a photo's resolved preview/zoom tier or a resident RAW
//! working image. Core code — the Tauri protocol handler (`protocol.rs`) and the edit
//! commands call in here; nothing here depends on the command layer. Compiled only with
//! the `edit` feature.

use crate::app::AppState;

/// The blocking body of an edit render — the `edit://` protocol
/// (`protocol::handle_edit_request`, the Darkroom stage) and the `render_edit` command
/// share it. Resolves the photo's path, decodes the source tier, renders, and encodes:
/// JPEG q90, or lossless PNG for a base-only frame (the GL drag tier's texture — a JPEG
/// base would spend the preview↔export parity budget before the shader ran). Operates on
/// the embedded preview or zoom tier, never the original file.
pub fn render_edit_bytes(state: &AppState, job: &crate::image_pool::EditJob) -> Result<Vec<u8>, String> {
    use crate::plugins::edit::{self, timing::Stages, RenderOpts, RenderSource, SourceToken};
    let mut t = Stages::start(format!(
        "render_edit photo={} max_edge={} hi_res={} base_only={} source={}",
        job.photo_id, job.max_edge, job.hi_res, job.base_only, job.source.to_query()
    ));
    // A working-image token renders from the resident RAW decode, or nothing: a stale
    // token (photo switched, session closed) is a 404, never a fallback to other pixels.
    if let SourceToken::Working { .. } = &job.source {
        let opts = RenderOpts { skip_look: job.base_only };
        let image = working_image(&job.source)?;
        if job.clip {
            let bytes = edit::clip_overlay_png(job.source.clone(), image, &job.edit_json, job.max_edge)?;
            t.report(&format!("clip bytes={}", bytes.len()));
            return Ok(bytes);
        }
        let out = edit::render_proxy(
            RenderSource::Working { token: job.source.clone(), image },
            &job.edit_json,
            job.max_edge,
            opts,
        )?;
        t.mark("render");
        let bytes = if job.base_only { edit::encode_png_fast(&out)? } else { edit::encode_jpeg(&out, 90)? };
        t.mark(if job.base_only { "encode_png" } else { "encode_jpeg" });
        t.report(&format!("bytes={}", bytes.len()));
        return Ok(bytes);
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
        let budget = crate::develop::session::cache_budget_bytes(&state);
        let (token, image) = crate::develop::offline::working_image_for(job.photo_id, &path, budget)?;
        t.mark("working_image");
        let out = edit::render_proxy(RenderSource::Working { token, image }, &job.edit_json, job.max_edge, opts)?;
        t.mark("render");
        let bytes = if job.base_only { edit::encode_png_fast(&out)? } else { edit::encode_jpeg(&out, 90)? };
        t.report(&format!("bytes={}", bytes.len()));
        return Ok(bytes);
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
    let bytes = if job.base_only {
        edit::encode_png_fast(&out)?
    } else {
        edit::encode_jpeg(&out, 90)?
    };
    t.mark(if job.base_only { "encode_png" } else { "encode_jpeg" });
    t.report(&format!("bytes={}", bytes.len()));
    Ok(bytes)
}

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

#[cfg(all(test, feature = "raw"))]
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
