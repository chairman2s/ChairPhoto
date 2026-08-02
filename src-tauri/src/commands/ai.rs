//! AI tagging commands — provider-agnostic tag suggestions (local Ollama or a cloud
//! model), the accept/reject surface, and grouped dispatch that sends one
//! representative per burst cluster instead of every frame.
//!
//! Gated on the `ai` Cargo feature; see `docs/ai-tagging.md` and `plugins/ai/`.
//! Suggestions are non-destructive: nothing is written to the catalog until accepted.

use super::*;
use serde::Serialize;
#[cfg(feature = "ai")]
use tauri::Emitter;
use tauri::{AppHandle, State};

/// A tag suggestion from the AI plugin, resolved against the current taxonomy.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiSuggestion {
    pub path: String,
    pub confidence: f32,
    pub reason: String,
    pub existing_tag_id: Option<i64>,
    pub is_new: bool,
    /// AI-proposed description/synonyms (meaningful for new tags; stored on accept).
    pub description: String,
    pub synonyms: Vec<String>,
    /// Provenance (H15c): when this suggestion was propagated from a burst
    /// representative, its photo id; `None` for a direct per-photo suggestion. Lets the
    /// review UI show "from DSC01234" and group propagated suggestions.
    pub source_photo_id: Option<i64>,
    /// Filename of the representative photo (H15d): the basename of `source_photo_id`'s
    /// path, pre-resolved so the review UI can show "from DSC01234.ARW" without a
    /// separate round-trip per suggestion. `None` for direct suggestions.
    pub source_photo_filename: Option<String>,
}

/// A normalized rectangle (0..1, origin top-left) selecting part of a photo. When
/// present on `ai_suggest_tags`, only this region of the preview is sent to the model,
/// so suggestions focus on one detail the user boxed. Resolution-independent, so the UI
/// can draw it over a scaled preview and it still maps to the full image.
#[derive(Clone, Copy, serde::Deserialize)]
#[cfg_attr(not(feature = "ai"), allow(dead_code))]
pub struct Region {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Crop `jpeg` to a normalized `region`, re-encoding as JPEG. Mirrors the editing
/// module's normalized-crop math (`plugins/edit`), clamped to the image bounds.
#[cfg(feature = "ai")]
fn crop_region_jpeg(jpeg: &[u8], r: &Region) -> Result<Vec<u8>, String> {
    use image::codecs::jpeg::JpegEncoder;
    use image::GenericImageView;
    let img = image::load_from_memory(jpeg).map_err(|e| e.to_string())?;
    let (w, h) = img.dimensions();
    let x = (r.x.clamp(0.0, 1.0) * w as f32).round() as u32;
    let y = (r.y.clamp(0.0, 1.0) * h as f32).round() as u32;
    let cw = ((r.w.clamp(0.0, 1.0) * w as f32).round() as u32).clamp(1, w.saturating_sub(x).max(1));
    let ch = ((r.h.clamp(0.0, 1.0) * h as f32).round() as u32).clamp(1, h.saturating_sub(y).max(1));
    let mut out = Vec::new();
    img.crop_imm(x, y, cw, ch)
        .write_with_encoder(JpegEncoder::new_with_quality(&mut out, 90))
        .map_err(|e| e.to_string())?;
    Ok(out)
}

/// Suggest tags for a photo via the configured AI provider, persisting results to
/// the plugin's `ai__suggestions` table. Honours per-photo rejections (filtered out
/// and fed into the prompt), the "existing tags only" option, and the confidence
/// threshold. When `region` is set, only that boxed part of the photo is sent. Returns
/// an error if the `ai` backend feature was compiled out.
#[tauri::command]
pub async fn ai_suggest_tags(
    state: State<'_, AppState>,
    photo_id: i64,
    question: Option<String>,
    region: Option<Region>,
) -> Result<Vec<AiSuggestion>, String> {
    #[cfg(not(feature = "ai"))]
    {
        let _ = (&state, photo_id, question, region);
        Err("AI backend not included in this build".into())
    }
    #[cfg(feature = "ai")]
    {
        use crate::plugins::ai;
        // Gather inputs under the lock, then release it for the network call.
        let (config, image_path, taxonomy, rejected) = with_catalog(&state, |c| {
            ai::ensure_schema(c.conn())?;
            let config = ai::read_config(c)?;
            // Private tags (people's names etc.) are withheld from cloud providers; the
            // local model still gets the full taxonomy.
            let taxonomy = ai::taxonomy_text(c, config.is_local())?;
            Ok((
                config,
                c.require_photo_path(photo_id)?,
                taxonomy,
                ai::rejected_paths(c.conn(), photo_id)?,
            ))
        })?;

        let image = tauri::async_runtime::spawn_blocking(move || -> Result<Vec<u8>, String> {
            let bytes = crate::thumbnails::preview_bytes(&image_path)?;
            match region {
                Some(r) => crop_region_jpeg(&bytes, &r),
                None => Ok(bytes),
            }
        })
        .await
        .map_err(|e| e.to_string())??;
        let image_b64 = base64::engine::general_purpose::STANDARD.encode(&image);

        let raw = ai::suggest(&config, &image_b64, &taxonomy, &rejected, question.as_deref()).await?;

        // Persist accepted-shaped results as pending, then return the photo's full
        // pending set (resolved against the live taxonomy).
        with_catalog(&state, |c| {
            let now = now_secs();
            // A direct per-photo run supersedes any burst-propagated pending rows for this
            // photo (H15c): the model looked at THIS frame, so its verdict wins over guesses
            // propagated from a cluster representative.
            ai::clear_propagated_pending(c.conn(), photo_id)?;
            for r in &raw {
                if r.confidence < config.min_confidence || rejected.contains(&r.path) {
                    continue;
                }
                let exists = c.find_tag_id_by_path(&r.path)?.is_some();
                if config.existing_only && !exists {
                    continue;
                }
                ai::upsert_pending(c.conn(), photo_id, r, now)?;
            }
            load_ai_suggestions(c, photo_id)
        })
    }
}

/// List the models installed on the Ollama server (for the model picker). Empty if
/// AI is compiled out or the server is unreachable.
#[tauri::command]
pub async fn ai_ollama_models(ollama_url: String) -> Result<Vec<String>, String> {
    #[cfg(not(feature = "ai"))]
    {
        let _ = ollama_url;
        Ok(Vec::new())
    }
    #[cfg(feature = "ai")]
    {
        crate::plugins::ai::list_ollama_models(&ollama_url).await
    }
}

/// The built-in AI prompt template (for the Advanced prompt editor's default/reset).
#[tauri::command]
pub fn ai_default_prompt() -> String {
    #[cfg(feature = "ai")]
    {
        crate::plugins::ai::DEFAULT_PROMPT.to_string()
    }
    #[cfg(not(feature = "ai"))]
    {
        String::new()
    }
}

/// Load a photo's persisted pending suggestions (no model call). Empty if AI is off.
#[tauri::command]
pub fn ai_get_suggestions(
    state: State<'_, AppState>,
    photo_id: i64,
) -> Result<Vec<AiSuggestion>, String> {
    #[cfg(not(feature = "ai"))]
    {
        let _ = (&state, photo_id);
        Ok(Vec::new())
    }
    #[cfg(feature = "ai")]
    {
        with_catalog(&state, |c| {
            crate::plugins::ai::ensure_schema(c.conn())?;
            load_ai_suggestions(c, photo_id)
        })
    }
}

/// Accept a suggestion: assign the tag (creating it if new) and mark it accepted.
#[tauri::command]
pub fn ai_accept_suggestion(
    state: State<'_, AppState>,
    photo_id: i64,
    path: String,
) -> Result<i64, String> {
    #[cfg(not(feature = "ai"))]
    {
        let _ = (&state, photo_id, path);
        Err("AI backend not included in this build".into())
    }
    #[cfg(feature = "ai")]
    {
        with_catalog(&state, |c| {
            crate::plugins::ai::ensure_schema(c.conn())?;
            let tag_id = match c.find_tag_id_by_path(&path)? {
                Some(id) => id,
                None => {
                    // New tag: create the path (ancestors too) and store the AI's
                    // proposed description + synonyms on it.
                    let id = c.create_tag(&path)?;
                    let (description, synonyms) =
                        crate::plugins::ai::get_proposed(c.conn(), photo_id, &path)?;
                    if !description.is_empty() {
                        c.set_tag_description(id, &description)?;
                    }
                    for syn in synonyms {
                        c.add_synonym(id, &syn, None, true)?;
                    }
                    id
                }
            };
            c.assign_tag(photo_id, tag_id)?;
            crate::plugins::ai::set_state(c.conn(), photo_id, &path, "accepted", now_secs())?;
            Ok(tag_id)
        })
    }
}

/// Reject a suggestion: it won't be shown or re-suggested for this photo.
#[tauri::command]
pub fn ai_reject_suggestion(
    state: State<'_, AppState>,
    photo_id: i64,
    path: String,
) -> Result<(), String> {
    #[cfg(not(feature = "ai"))]
    {
        let _ = (&state, photo_id, path);
        Err("AI backend not included in this build".into())
    }
    #[cfg(feature = "ai")]
    {
        with_catalog(&state, |c| {
            crate::plugins::ai::ensure_schema(c.conn())?;
            crate::plugins::ai::set_state(c.conn(), photo_id, &path, "rejected", now_secs())?;
            Ok(())
        })
    }
}

/// Resolve a photo's persisted pending suggestions against the live taxonomy.
///
/// For propagated suggestions (H15c), looks up the representative's filename from
/// `photos.path` so the review UI can show "from DSC01234.ARW" without a separate
/// per-suggestion round-trip. The lookup is batched: each unique `source_photo_id`
/// is resolved once and cached for the remainder of the loop.
#[cfg(feature = "ai")]
fn load_ai_suggestions(c: &Catalog, photo_id: i64) -> crate::catalog::Result<Vec<AiSuggestion>> {
    use rusqlite::OptionalExtension;

    // Cache: source_photo_id → Option<filename> so we only query the DB once per
    // distinct representative in this photo's suggestion list.
    let mut rep_filenames: std::collections::HashMap<i64, Option<String>> =
        std::collections::HashMap::new();

    let mut out = Vec::new();
    for s in crate::plugins::ai::load_pending(c.conn(), photo_id)? {
        let existing = c.find_tag_id_by_path(&s.path)?;

        // Resolve the representative filename once per unique source_photo_id.
        let source_photo_filename = if let Some(rep_id) = s.source_photo_id {
            rep_filenames
                .entry(rep_id)
                .or_insert_with(|| {
                    // A single-column query avoids loading the full Photo row.
                    c.conn()
                        .query_row(
                            "SELECT path FROM photos WHERE id = ?1",
                            rusqlite::params![rep_id],
                            |r| r.get::<_, String>(0),
                        )
                        .optional()
                        .ok()
                        .flatten()
                        .and_then(|path| {
                            std::path::Path::new(&path)
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                        })
                })
                .clone()
        } else {
            None
        };

        out.push(AiSuggestion {
            is_new: existing.is_none(),
            existing_tag_id: existing,
            path: s.path,
            confidence: s.confidence,
            reason: s.reason,
            description: s.description,
            synonyms: s.synonyms,
            source_photo_id: s.source_photo_id,
            source_photo_filename,
        });
    }
    Ok(out)
}

/// Summary of one grouped-dispatch estimate: how the selection collapses to
/// representatives. The frontend renders "N photos → M representatives → ~$X".
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupedEstimate {
    /// Photos in the selection that exist in the catalog (the clustered set).
    pub total: usize,
    /// Number of clusters formed — one representative is dispatched per cluster.
    pub representatives: usize,
}

/// Pre-dispatch cost estimate for a grouped batch run (extends I5b). Groups the selection
/// exactly as [`ai_suggest_tags_grouped`] will, so the confirm dialog can show the real
/// photos→representatives reduction (and multiply the per-image cost by `representatives`,
/// not by the raw photo count). No provider call is made.
#[tauri::command]
pub async fn ai_grouped_estimate(
    state: State<'_, AppState>,
    photo_ids: Vec<i64>,
) -> Result<GroupedEstimate, String> {
    #[cfg(not(feature = "ai"))]
    {
        let _ = (&state, photo_ids);
        Err("AI backend not included in this build".into())
    }
    #[cfg(feature = "ai")]
    {
        let clusters = burst::cluster_photo_ids(&state, photo_ids).await?;
        let total: usize = clusters.iter().map(|c| c.photo_ids.len()).sum();
        Ok(GroupedEstimate {
            total,
            representatives: clusters.len(),
        })
    }
}

/// Summary of one grouped batch AI run, returned to the frontend.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupedDispatchResult {
    /// Photos considered (the clustered set — the input IDs that exist in the catalog).
    pub total: usize,
    /// Clusters formed = provider calls made (one representative dispatched per cluster).
    pub representatives: usize,
    /// Provider calls that succeeded (a failed representative leaves its cluster untouched).
    pub dispatched: usize,
    /// Total pending suggestions stored across all cluster members (direct + propagated).
    pub propagated: usize,
}

/// Progress payload emitted as `ai:grouped:progress` while dispatching representatives.
/// Only constructed on the `ai` code path (mirrors the `Region` convention).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(not(feature = "ai"), allow(dead_code))]
pub struct GroupedProgress {
    pub done: usize,
    pub total: usize,
}

/// Grouped batch suggestion run (H15c). Instead of dispatching every selected photo to
/// the provider, cluster the selection (H15b: capture-time gaps → perceptual-hash
/// confirmation), send only each cluster's **representative** frame to the provider, and
/// store the returned suggestions for **every** cluster member as `pending`:
/// - the representative gets its suggestions directly (source_photo_id = NULL);
/// - the other members get the same suggestions **propagated**, stamped with the
///   representative's `source_photo_id` and at a slightly reduced confidence
///   ([`PROPAGATION_CONFIDENCE_FACTOR`]).
///
/// This is the ~90% cost cut on burst-heavy shoots. A later direct per-photo run
/// (`ai_suggest_tags`) supersedes that photo's propagated rows. Local Ollama and cloud
/// providers alike go through this path; representatives are dispatched sequentially
/// (local Ollama is single-GPU) with `ai:grouped:progress` events.
#[tauri::command]
pub async fn ai_suggest_tags_grouped(
    app: AppHandle,
    state: State<'_, AppState>,
    photo_ids: Vec<i64>,
) -> Result<GroupedDispatchResult, String> {
    #[cfg(not(feature = "ai"))]
    {
        let _ = (&app, &state, photo_ids);
        Err("AI backend not included in this build".into())
    }
    #[cfg(feature = "ai")]
    {
        use crate::plugins::ai;

        let clusters = burst::cluster_photo_ids(&state, photo_ids).await?;
        let total: usize = clusters.iter().map(|c| c.photo_ids.len()).sum();
        let representatives = clusters.len();

        // Config + taxonomy are stable across the whole run; read once under the lock.
        let (config, taxonomy) = with_catalog_blocking(&state, |c| {
            ai::ensure_schema(c.conn())?;
            let config = ai::read_config(c)?;
            let taxonomy = ai::taxonomy_text(c, config.is_local())?;
            Ok((config, taxonomy))
        })
        .await?;

        let mut dispatched = 0usize;
        let mut propagated = 0usize;

        for (idx, cluster) in clusters.iter().enumerate() {
            let rep_id = cluster.representative_id();

            // Per-representative inputs: image path + this photo's rejections. If the
            // photo vanished (e.g. deleted mid-run), skip the whole cluster.
            let prep = with_catalog_blocking(&state, move |c| {
                Ok((
                    c.require_photo_path(rep_id)?,
                    ai::rejected_paths(c.conn(), rep_id)?,
                ))
            })
            .await;
            let (image_path, rejected) = match prep {
                Ok(v) => v,
                Err(_) => {
                    let _ = app.emit(
                        "ai:grouped:progress",
                        GroupedProgress { done: idx + 1, total: representatives },
                    );
                    continue;
                }
            };

            // Decode the representative's preview off the async thread.
            let image = tauri::async_runtime::spawn_blocking(move || {
                crate::thumbnails::preview_bytes(&image_path)
            })
            .await
            .map_err(|e| e.to_string())?;
            let image = match image {
                Ok(bytes) => bytes,
                Err(_) => {
                    let _ = app.emit(
                        "ai:grouped:progress",
                        GroupedProgress { done: idx + 1, total: representatives },
                    );
                    continue;
                }
            };
            let image_b64 = base64::engine::general_purpose::STANDARD.encode(&image);

            // Dispatch ONLY the representative to the provider.
            let raw = match ai::suggest(&config, &image_b64, &taxonomy, &rejected, None).await {
                Ok(r) => r,
                Err(_) => {
                    // Provider failure leaves the cluster untouched — a later re-run can
                    // retry it. Report progress and move on.
                    let _ = app.emit(
                        "ai:grouped:progress",
                        GroupedProgress { done: idx + 1, total: representatives },
                    );
                    continue;
                }
            };
            dispatched += 1;

            // Persist: representative directly, other members propagated. Rejections and
            // existing-only/confidence filtering are applied per member. Shares the exact
            // propagation logic (`propagate_cluster`) that the H15c tests drive directly.
            let member_ids = cluster.photo_ids.clone();
            let config_ref = &config;
            propagated += with_catalog(&state, |c| {
                ai::propagate_cluster::<crate::catalog::CatalogError, _>(
                    c.conn(),
                    &member_ids,
                    rep_id,
                    &raw,
                    config_ref.min_confidence,
                    config_ref.existing_only,
                    now_secs(),
                    |path| Ok(c.find_tag_id_by_path(path)?.is_some()),
                )
            })?;

            let _ = app.emit(
                "ai:grouped:progress",
                GroupedProgress { done: idx + 1, total: representatives },
            );
        }

        Ok(GroupedDispatchResult {
            total,
            representatives,
            dispatched,
            propagated,
        })
    }
}
