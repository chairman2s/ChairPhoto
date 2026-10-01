//! Burst-relative sharpness flagging (H16e).
//!
//! Clusters a set of photos with the H15b burst engine (`crate::burst`), then flags each
//! cluster's weak frames `soft-in-burst` and its best frame `sharpest-of-burst`. Flagging
//! is advisory only — nothing is ever auto-rejected (see `docs/sharpness-culling.md`).
//! The analysis itself is the core's `burst_analysis`, shared with the GPUI app.

use super::AppState;
#[cfg(feature = "ai")]
use super::with_catalog_blocking;
#[cfg(feature = "ai")]
use crate::burst_analysis::parse_capture_time_secs;
use tauri::State;

// ── H16e — Burst-relative sharpness flagging ─────────────────────────────────

pub use crate::burst_analysis::{BurstAnalysisResult, BURST_SOFT_THRESHOLD_DEFAULT, BURST_SOFT_THRESHOLD_KEY};

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

// ── H15c — Grouped batch AI dispatch + suggestion propagation ────────────────

/// Read the burst-grouping thresholds from the `ai` plugin's settings, fetch the burst
/// inputs for `photo_ids`, and run the H15b cluster engine. Shared by the grouped
/// AI-dispatch command and its pre-dispatch cost estimate so both agree on the split.
///
/// No on-demand thumbnail sharpness scoring is wired here (representative selection uses
/// stored rating + `photos.sharpness`); the closure slot is `None` like the H16e caller.
#[cfg(feature = "ai")]
pub(super) async fn cluster_photo_ids(
    state: &State<'_, AppState>,
    photo_ids: Vec<i64>,
) -> Result<Vec<crate::burst::Cluster>, String> {
    use crate::burst::{group_into_clusters, BurstConfig, BurstPhoto};

    if photo_ids.is_empty() {
        return Ok(Vec::new());
    }

    let (time_gap_secs, hamming_threshold) = with_catalog_blocking(state, |c| {
        let gap: i64 = c
            .get_setting("ai.burst_time_gap_secs")
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
            .unwrap_or(15);
        let hamming: u32 = c
            .get_setting("ai.burst_hamming_threshold")
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10);
        Ok((gap, hamming))
    })
    .await?;

    let cfg = BurstConfig { time_gap_secs, hamming_threshold };

    let inputs = with_catalog_blocking(state, move |c| c.burst_inputs(&photo_ids)).await?;

    let burst_photos: Vec<BurstPhoto> = inputs
        .iter()
        .map(|bi| BurstPhoto {
            id: bi.id,
            capture_ts: bi.capture_time.as_deref().and_then(parse_capture_time_secs),
            phash: bi.phash,
            rating: bi.rating,
            sharpness: bi.sharpness,
        })
        .collect();

    Ok(group_into_clusters::<fn(i64) -> Option<Vec<u8>>>(
        burst_photos,
        &cfg,
        None,
    ))
}
