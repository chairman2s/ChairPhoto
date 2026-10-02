//! Burst-relative sharpness flagging (H16e).
//!
//! Clusters a set of photos with the H15b burst engine (`crate::burst`), then flags each
//! cluster's weak frames `soft-in-burst` and its best frame `sharpest-of-burst`. Flagging
//! is advisory only — nothing is ever auto-rejected (see `docs/sharpness-culling.md`).
//! The analysis itself is the core's `burst_analysis`, shared with the GPUI app.

use super::AppState;
use tauri::State;

// ── H16e — Burst-relative sharpness flagging ─────────────────────────────────

pub use crate::burst_analysis::BurstAnalysisResult;

/// Analyse burst-relative sharpness for a set of photos (H16e); see
/// `burst_analysis::analyze_burst_sharpness`, which the GPUI app shares.
#[tauri::command]
pub async fn analyze_burst_sharpness(
    state: State<'_, AppState>,
    photo_ids: Vec<i64>,
) -> Result<BurstAnalysisResult, String> {
    let state = state.inner().clone();
    crate::app::spawn_blocking(move || crate::burst_analysis::analyze_burst_sharpness(&state, &photo_ids))
        .await
        .map_err(|e| e.to_string())?
}
