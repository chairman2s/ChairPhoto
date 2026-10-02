//! AI tagging commands — provider-agnostic tag suggestions (local Ollama or a cloud
//! model), the accept/reject surface, and grouped dispatch that sends one
//! representative per burst cluster instead of every frame.
//!
//! Gated on the `ai` Cargo feature; see `docs/ai-tagging.md` and `plugins/ai/`.
//! Suggestions are non-destructive: nothing is written to the catalog until accepted.
//!
//! The bodies live in the core (`app::ai`), which the GPUI app runs too (#126); the commands
//! here are their thin Tauri wrappers.

use super::*;
pub use crate::app::ai::{AiSuggestion, GroupedDispatchResult, GroupedEstimate, Region};
#[cfg(feature = "ai")]
use crate::app::ai as core_ai;
use tauri::State;

/// Suggest tags for a photo via the configured AI provider, persisting results to the
/// plugin's `ai__suggestions` table (`app::ai::suggest_tags`). When `region` is set, only
/// that boxed part of the photo is sent. Errors if the `ai` backend was compiled out.
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
        core_ai::suggest_tags(&state, None, photo_id, question, region).await
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
        core_ai::ollama_models(&ollama_url).await
    }
}

/// The built-in AI prompt template (for the Advanced prompt editor's default/reset).
#[tauri::command]
pub fn ai_default_prompt() -> String {
    #[cfg(feature = "ai")]
    {
        core_ai::default_prompt().to_string()
    }
    #[cfg(not(feature = "ai"))]
    {
        String::new()
    }
}

/// Load a photo's persisted pending suggestions (no model call). Empty if AI is off.
#[tauri::command(async)]
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
        with_catalog(&state, |c| core_ai::load_suggestions(c, photo_id))
    }
}

/// Accept a suggestion: assign the tag (creating it if new) and mark it accepted.
#[tauri::command(async)]
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
        with_catalog(&state, |c| core_ai::accept_suggestion(c, photo_id, &path))
    }
}

/// Reject a suggestion: it won't be shown or re-suggested for this photo.
#[tauri::command(async)]
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
        with_catalog(&state, |c| core_ai::reject_suggestion(c, photo_id, &path))
    }
}

/// Pre-dispatch cost estimate for a grouped batch run: groups the selection exactly as
/// [`ai_suggest_tags_grouped`] will, so the confirm can price per representative. No
/// provider call is made.
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
        let state = state.inner().clone();
        crate::app::spawn_blocking(move || core_ai::grouped_estimate(&state, None, photo_ids))
            .await
            .map_err(|e| e.to_string())?
    }
}

/// Grouped batch suggestion run (H15c): one representative per burst cluster goes to the
/// provider; its suggestions are stored for every member, propagated ones reviewable
/// (`app::ai::suggest_tags_grouped`).
#[tauri::command]
pub async fn ai_suggest_tags_grouped(
    state: State<'_, AppState>,
    photo_ids: Vec<i64>,
) -> Result<GroupedDispatchResult, String> {
    #[cfg(not(feature = "ai"))]
    {
        let _ = (&state, photo_ids);
        Err("AI backend not included in this build".into())
    }
    #[cfg(feature = "ai")]
    {
        core_ai::suggest_tags_grouped(&state, None, photo_ids).await
    }
}
