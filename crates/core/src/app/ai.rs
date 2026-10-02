//! AI tagging — the bodies of the Tauri `ai_*` commands the GPUI AI tagging module runs
//! (#126): suggest tags for one photo (optionally a boxed region, optionally a follow-up
//! question), the grouped burst run and its pre-dispatch estimate, and accept/reject.
//!
//! The DTOs are compiled in every build (the Tauri shell's commands answer "not included in
//! this build" without the feature); everything that runs a provider is behind `ai`.
//!
//! **Privacy.** Which provider runs is the catalog's `ai.provider` setting — `ollama` (local,
//! the default) or a cloud provider the user chose, whose own API key must be saved. These
//! bodies do what the settings say; the per-run consent a front end owes the user (the bulk
//! cloud cost confirm) is the front end's, before it calls [`suggest_tags_grouped`]. Private
//! tags are withheld from cloud providers (`plugins::ai::taxonomy_text`). Nothing here logs
//! the configuration (it holds the API keys).
//!
//! **Catalog identity.** Every body takes an optional [`CatalogIdentity`]: with one, each of
//! its catalog phases runs under `with_catalog_as`, so a run started against one catalog
//! fails closed ([`super::CATALOG_CHANGED`]) instead of reading or writing the next one's rows
//! after a switch (map #92). The Tauri shell passes `None`.
//!
//! **Blocking.** The catalog phases take the lock and the preview decode is disk/CPU work;
//! the async bodies move both to blocking workers. Call the sync ones from a worker.
//!
//! See docs/ai-tagging.md.

#[cfg(feature = "ai")]
use super::{now_secs, spawn_blocking, with_catalog, with_catalog_as, AppState, CatalogIdentity};
#[cfg(feature = "ai")]
use crate::catalog::{Catalog, Result as CatalogResult};
#[cfg(feature = "ai")]
use crate::plugins::ai;

/// A tag suggestion from the AI plugin, resolved against the current taxonomy.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
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
    /// Provenance (H15c): when this suggestion was propagated from a burst representative,
    /// its photo id; `None` for a direct per-photo suggestion.
    pub source_photo_id: Option<i64>,
    /// The representative's file name (H15d), pre-resolved for "from DSC01234.ARW". `None`
    /// for direct suggestions.
    pub source_photo_filename: Option<String>,
}

/// A normalized rectangle (0..1, origin top-left) selecting part of a photo. When given, only
/// this region of the preview is sent to the model, so suggestions focus on one detail.
/// Resolution-independent: drawn over a scaled preview, it still maps to the full image.
#[derive(Debug, Clone, Copy, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct Region {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// How a selection collapses to representatives (`ai_grouped_estimate`): the confirm shows
/// "N photos → M representatives → ~$X".
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupedEstimate {
    /// Photos in the selection that exist in the catalog (the clustered set).
    pub total: usize,
    /// Clusters formed — one representative is dispatched per cluster.
    pub representatives: usize,
}

/// What a grouped run did (`ai_suggest_tags_grouped`).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupedDispatchResult {
    /// Photos considered (the clustered set).
    pub total: usize,
    /// Clusters formed = provider calls asked for (one representative per cluster).
    pub representatives: usize,
    /// Provider calls that succeeded (a failed representative leaves its cluster untouched).
    pub dispatched: usize,
    /// Pending suggestions stored across all cluster members (direct + propagated).
    pub propagated: usize,
}

/// `f` under the catalog lock — bound to `expected` when given (`with_catalog_as`).
#[cfg(feature = "ai")]
fn bound<T>(
    state: &AppState,
    expected: Option<CatalogIdentity>,
    f: impl FnOnce(&Catalog) -> CatalogResult<T>,
) -> Result<T, String> {
    match expected {
        Some(e) => with_catalog_as(state, e, f),
        None => with_catalog(state, f),
    }
}

/// [`bound`] on a blocking worker.
#[cfg(feature = "ai")]
async fn bound_blocking<T: Send + 'static>(
    state: &AppState,
    expected: Option<CatalogIdentity>,
    f: impl FnOnce(&Catalog) -> CatalogResult<T> + Send + 'static,
) -> Result<T, String> {
    let state = state.clone();
    spawn_blocking(move || bound(&state, expected, f)).await.map_err(|e| e.to_string())?
}

/// Crop `jpeg` to a normalized `region`, re-encoding as JPEG. Mirrors the editing module's
/// normalized-crop math (`plugins/edit`), clamped to the image bounds.
#[cfg(feature = "ai")]
pub fn crop_region_jpeg(jpeg: &[u8], r: &Region) -> Result<Vec<u8>, String> {
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

/// A photo's persisted pending suggestions (no model call), resolved against the live
/// taxonomy (`ai_get_suggestions`). A propagated suggestion carries its representative's
/// file name, looked up once per distinct representative.
#[cfg(feature = "ai")]
pub fn load_suggestions(c: &Catalog, photo_id: i64) -> CatalogResult<Vec<AiSuggestion>> {
    use rusqlite::OptionalExtension;

    ai::ensure_schema(c.conn())?;
    let mut rep_filenames: std::collections::HashMap<i64, Option<String>> = std::collections::HashMap::new();
    let mut out = Vec::new();
    for s in ai::load_pending(c.conn(), photo_id)? {
        let existing = c.find_tag_id_by_path(&s.path)?;
        let source_photo_filename = match s.source_photo_id {
            Some(rep_id) => rep_filenames
                .entry(rep_id)
                .or_insert_with(|| {
                    c.conn()
                        .query_row("SELECT path FROM photos WHERE id = ?1", rusqlite::params![rep_id], |r| r.get::<_, String>(0))
                        .optional()
                        .ok()
                        .flatten()
                        .and_then(|path| std::path::Path::new(&path).file_name().map(|n| n.to_string_lossy().into_owned()))
                })
                .clone(),
            None => None,
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

/// Accept: assign the tag — creating the path first when it is new, with the model's
/// proposed description and synonyms — and mark the suggestion accepted
/// (`ai_accept_suggestion`); the tag's id.
#[cfg(feature = "ai")]
pub fn accept_suggestion(c: &Catalog, photo_id: i64, path: &str) -> CatalogResult<i64> {
    ai::ensure_schema(c.conn())?;
    let tag_id = match c.find_tag_id_by_path(path)? {
        Some(id) => id,
        None => {
            let id = c.create_tag(path)?;
            let (description, synonyms) = ai::get_proposed(c.conn(), photo_id, path)?;
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
    ai::set_state(c.conn(), photo_id, path, "accepted", now_secs())?;
    Ok(tag_id)
}

/// Reject: never shown or re-suggested for this photo, and fed back into its prompt
/// (`ai_reject_suggestion`).
#[cfg(feature = "ai")]
pub fn reject_suggestion(c: &Catalog, photo_id: i64, path: &str) -> CatalogResult<()> {
    ai::ensure_schema(c.conn())?;
    ai::set_state(c.conn(), photo_id, path, "rejected", now_secs())?;
    Ok(())
}

/// The photo's preview as base64 JPEG, cropped to `region` when given — what a provider is
/// sent. Blocking (disk + decode).
#[cfg(feature = "ai")]
pub fn image_b64(path: &std::path::Path, region: Option<Region>) -> Result<String, String> {
    use base64::Engine;
    let bytes = crate::thumbnails::preview_bytes(path)?;
    let bytes = match region {
        Some(r) => crop_region_jpeg(&bytes, &r)?,
        None => bytes,
    };
    Ok(base64::engine::general_purpose::STANDARD.encode(&bytes))
}

/// Store one direct run's results for `photo_id` as pending — superseding any
/// burst-propagated rows (the model looked at THIS frame) and dropping what is below the
/// confidence floor, rejected here, or new under "existing tags only" — then return the
/// photo's whole pending set.
#[cfg(feature = "ai")]
pub fn store_direct(
    c: &Catalog,
    photo_id: i64,
    config: &ai::Config,
    rejected: &[String],
    raw: &[ai::Raw],
) -> CatalogResult<Vec<AiSuggestion>> {
    let now = now_secs();
    ai::clear_propagated_pending(c.conn(), photo_id)?;
    for r in raw {
        if r.confidence < config.min_confidence || rejected.contains(&r.path) {
            continue;
        }
        let exists = c.find_tag_id_by_path(&r.path)?.is_some();
        if config.existing_only && !exists {
            continue;
        }
        ai::upsert_pending(c.conn(), photo_id, r, now)?;
    }
    load_suggestions(c, photo_id)
}

/// Suggest tags for one photo with the configured provider and persist them
/// (`ai_suggest_tags`): honours per-photo rejections, "existing tags only" and the confidence
/// floor. With `region`, only that box of the photo is sent; with `question`, the model
/// refines toward the user's follow-up. Returns the photo's pending set.
#[cfg(feature = "ai")]
pub async fn suggest_tags(
    state: &AppState,
    expected: Option<CatalogIdentity>,
    photo_id: i64,
    question: Option<String>,
    region: Option<Region>,
) -> Result<Vec<AiSuggestion>, String> {
    // Inputs under the lock; released for the decode and the network call.
    let (config, image_path, taxonomy, rejected) = bound_blocking(state, expected, move |c| {
        ai::ensure_schema(c.conn())?;
        let config = ai::read_config(c)?;
        // Private tags (people's names etc.) are withheld from cloud providers.
        let taxonomy = ai::taxonomy_text(c, config.is_local())?;
        Ok((config, c.require_photo_path(photo_id)?, taxonomy, ai::rejected_paths(c.conn(), photo_id)?))
    })
    .await?;

    let image = spawn_blocking(move || image_b64(&image_path, region)).await.map_err(|e| e.to_string())??;
    let raw = ai::suggest(&config, &image, &taxonomy, &rejected, question.as_deref()).await?;

    bound_blocking(state, expected, move |c| store_direct(c, photo_id, &config, &rejected, &raw)).await
}

/// Cluster `photo_ids` into bursts as the grouped run will: the `ai.burst_time_gap_secs` /
/// `ai.burst_hamming_threshold` settings, then the H15b engine over each photo's capture
/// time, perceptual hash, rating and sharpness (no on-demand scoring). Blocking.
#[cfg(feature = "ai")]
pub fn cluster_photo_ids(
    state: &AppState,
    expected: Option<CatalogIdentity>,
    photo_ids: Vec<i64>,
) -> Result<Vec<crate::burst::Cluster>, String> {
    use crate::burst::{group_into_clusters, BurstConfig, BurstPhoto};
    use crate::burst_analysis::parse_capture_time_secs;

    if photo_ids.is_empty() {
        return Ok(Vec::new());
    }
    let (cfg, inputs) = bound(state, expected, |c| {
        let setting = |key: &str| c.get_setting(key).ok().flatten();
        let cfg = BurstConfig {
            time_gap_secs: setting("ai.burst_time_gap_secs").and_then(|s| s.parse().ok()).unwrap_or(15),
            hamming_threshold: setting("ai.burst_hamming_threshold").and_then(|s| s.parse().ok()).unwrap_or(10),
        };
        Ok((cfg, c.burst_inputs(&photo_ids)?))
    })?;
    let photos: Vec<BurstPhoto> = inputs
        .iter()
        .map(|bi| BurstPhoto {
            id: bi.id,
            capture_ts: bi.capture_time.as_deref().and_then(parse_capture_time_secs),
            phash: bi.phash,
            rating: bi.rating,
            sharpness: bi.sharpness,
        })
        .collect();
    Ok(group_into_clusters::<fn(i64) -> Option<Vec<u8>>>(photos, &cfg, None))
}

/// The pre-dispatch estimate (`ai_grouped_estimate`): the selection grouped exactly as
/// [`suggest_tags_grouped`] will, so a cost is priced per representative. No provider call.
/// Blocking.
#[cfg(feature = "ai")]
pub fn grouped_estimate(
    state: &AppState,
    expected: Option<CatalogIdentity>,
    photo_ids: Vec<i64>,
) -> Result<GroupedEstimate, String> {
    let clusters = cluster_photo_ids(state, expected, photo_ids)?;
    Ok(GroupedEstimate { total: clusters.iter().map(|c| c.photo_ids.len()).sum(), representatives: clusters.len() })
}

/// The grouped burst run (H15c, `ai_suggest_tags_grouped`): cluster the selection, send only
/// each cluster's **representative** to the provider (sequentially — local Ollama is one
/// GPU), and store its suggestions for every member as `pending`: directly on the
/// representative, **propagated** (with its `source_photo_id`, at slightly reduced
/// confidence) on the rest. A later direct run on a member supersedes its propagated rows.
///
/// A representative whose photo or preview is gone, or whose provider call fails, leaves its
/// cluster untouched. No progress is reported: the caller awaits it behind a busy flag
/// (restoring progress means the full job treatment, not a bare emit — #12).
#[cfg(feature = "ai")]
pub async fn suggest_tags_grouped(
    state: &AppState,
    expected: Option<CatalogIdentity>,
    photo_ids: Vec<i64>,
) -> Result<GroupedDispatchResult, String> {
    let st = state.clone();
    let clusters = spawn_blocking(move || cluster_photo_ids(&st, expected, photo_ids)).await.map_err(|e| e.to_string())??;
    let total: usize = clusters.iter().map(|c| c.photo_ids.len()).sum();
    let representatives = clusters.len();

    // Config + taxonomy are stable across the run; read once.
    let (config, taxonomy) = bound_blocking(state, expected, |c| {
        ai::ensure_schema(c.conn())?;
        let config = ai::read_config(c)?;
        let taxonomy = ai::taxonomy_text(c, config.is_local())?;
        Ok((config, taxonomy))
    })
    .await?;
    let config = std::sync::Arc::new(config);

    let (mut dispatched, mut propagated) = (0usize, 0usize);
    for cluster in clusters {
        let rep_id = cluster.representative_id();
        let prep = bound_blocking(state, expected, move |c| Ok((c.require_photo_path(rep_id)?, ai::rejected_paths(c.conn(), rep_id)?))).await;
        let (image_path, rejected) = match prep {
            Ok(v) => v,
            // A catalog switch is not "this photo vanished": stop, writing nothing more.
            Err(e) if e == super::CATALOG_CHANGED => return Err(e),
            Err(_) => continue,
        };
        let image = match spawn_blocking(move || image_b64(&image_path, None)).await.map_err(|e| e.to_string())? {
            Ok(b) => b,
            Err(_) => continue,
        };
        let raw = match ai::suggest(&config, &image, &taxonomy, &rejected, None).await {
            Ok(r) => r,
            Err(_) => continue, // a later re-run can retry this cluster
        };
        dispatched += 1;

        let config = config.clone();
        propagated += bound_blocking(state, expected, move |c| {
            ai::propagate_cluster::<crate::catalog::CatalogError, _>(
                c.conn(),
                &cluster.photo_ids,
                rep_id,
                &raw,
                config.min_confidence,
                config.existing_only,
                now_secs(),
                |path| Ok(c.find_tag_id_by_path(path)?.is_some()),
            )
        })
        .await?;
    }

    Ok(GroupedDispatchResult { total, representatives, dispatched, propagated })
}

/// The models installed on the Ollama server at `url` (`ai_ollama_models`), for the model
/// picker. Local only; an unreachable server is an error the picker shows as "none".
#[cfg(feature = "ai")]
pub async fn ollama_models(url: &str) -> Result<Vec<String>, String> {
    ai::list_ollama_models(url).await
}

/// The built-in prompt template (`ai_default_prompt`): the Advanced editor's default.
#[cfg(feature = "ai")]
pub fn default_prompt() -> &'static str {
    ai::DEFAULT_PROMPT
}

#[cfg(all(test, feature = "ai"))]
mod tests {
    use super::*;
    use crate::test_support::TestTmpDir;

    fn catalog(tag: &str, photos: usize) -> (Catalog, TestTmpDir) {
        let dir = TestTmpDir::new(&format!("app-ai-{tag}"));
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let c = Catalog::open(&dir.join("catalog.chairphoto"), &root).unwrap();
        for i in 0..photos {
            let p = root.join(format!("p{i}.jpg"));
            std::fs::write(&p, b"jpeg").unwrap();
            c.upsert_photo(&p, None, 1, 4).unwrap();
        }
        (c, dir)
    }

    fn raw(path: &str, confidence: f32) -> ai::Raw {
        ai::Raw {
            path: path.into(),
            confidence,
            reason: "seen".into(),
            description: "a proposed tag".into(),
            synonyms: vec!["alias".into()],
        }
    }

    /// A direct run's results are filtered and stored; accepting a new path creates the tag
    /// with the model's description and synonym; a rejection hides it and is remembered.
    #[test]
    fn store_accept_and_reject_round_trip() {
        let (c, _dir) = catalog("roundtrip", 1);
        ai::ensure_schema(c.conn()).unwrap();
        c.create_tag("Animals/Birds").unwrap();
        let config = ai::read_config(&c).unwrap();
        let low = ai::Raw { confidence: 0.01, ..raw("Nature/Fog", 0.01) };
        let mut config_floor = config;
        config_floor.min_confidence = 0.2;
        let stored = store_direct(&c, 1, &config_floor, &[], &[raw("Animals/Birds", 0.9), raw("Animals/Birds/Gull", 0.8), low]).unwrap();
        let paths: Vec<_> = stored.iter().map(|s| (s.path.as_str(), s.is_new)).collect();
        assert_eq!(paths, vec![("Animals/Birds", false), ("Animals/Birds/Gull", true)], "the floor drops Nature/Fog");

        let id = accept_suggestion(&c, 1, "Animals/Birds/Gull").unwrap();
        assert_eq!(c.get_tag(id).unwrap().description, "a proposed tag");
        reject_suggestion(&c, 1, "Animals/Birds").unwrap();
        assert!(load_suggestions(&c, 1).unwrap().is_empty());
        assert_eq!(ai::rejected_paths(c.conn(), 1).unwrap(), vec!["Animals/Birds".to_string()]);
    }

    /// The estimate and every phase of a bound run read only the catalog they were bound to.
    #[test]
    fn a_bound_estimate_refuses_another_open_catalog() {
        let (a, _da) = catalog("bound-a", 3);
        let (b, _db) = catalog("bound-b", 3);
        let state = AppState::default();
        *state.catalog.lock().unwrap() = Some(a);
        let shown = super::super::catalog_identity(&state).unwrap();
        assert_eq!(grouped_estimate(&state, Some(shown), vec![1, 2, 3]).unwrap().total, 3);
        *state.catalog.lock().unwrap() = Some(b);
        assert_eq!(grouped_estimate(&state, Some(shown), vec![1, 2, 3]).unwrap_err(), super::super::CATALOG_CHANGED);
    }
}
