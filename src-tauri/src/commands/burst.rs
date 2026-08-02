//! Burst-relative sharpness flagging (H16e).
//!
//! Clusters a set of photos with the H15b burst engine (`crate::burst`), then flags each
//! cluster's weak frames `soft-in-burst` and its best frame `sharpest-of-burst`. Flagging
//! is advisory only — nothing is ever auto-rejected (see `docs/sharpness-culling.md`).

use super::{with_catalog_blocking, AppState};
use serde::Serialize;
use tauri::State;

// ── H16e — Burst-relative sharpness flagging ─────────────────────────────────

/// Summary of one `analyze_burst_sharpness` run, returned to the frontend.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BurstAnalysisResult {
    /// Total number of photos considered (the input set, after de-duplication).
    pub total: usize,
    /// Number of clusters formed (≥1 member each).
    pub clusters: usize,
    /// Photos flagged as `soft-in-burst` (below the threshold).
    pub flagged_soft: usize,
    /// Photos crowned as `sharpest-of-burst` (exactly one per cluster with >1 member).
    pub flagged_best: usize,
    /// Photos whose burst flags were cleared (single-photo clusters that lost their
    /// previous flag when re-analysed).
    pub cleared: usize,
}

/// Settings key for the burst-relative soft-in-burst threshold (fraction 0–1).
/// A photo is flagged `soft-in-burst` when its sharpness is below
/// `cluster_median × threshold`. Default: 0.60 (60%).
pub const BURST_SOFT_THRESHOLD_KEY: &str = "sharpness.burst_soft_threshold";
pub const BURST_SOFT_THRESHOLD_DEFAULT: f64 = 0.60;

/// Analyse burst-relative sharpness for a set of photos (H16e).
///
/// 1. Fetch `(capture_time, phash, rating, sharpness)` for each ID.
/// 2. Run the H15b cluster engine (`group_into_clusters`) to split into bursts using
///    the catalog's `ai.burst_time_gap_secs` / `ai.burst_hamming_threshold` settings.
/// 3. For clusters with >1 sharpness-scored member: compute the cluster median and flag
///    members below `threshold × median` as `"soft-in-burst"`. Crown the cluster's
///    sharpest as `"sharpest-of-burst"`. Single-photo clusters: clear any stale flag.
/// 4. Write all flags in a single transaction via `Catalog::set_burst_flags`.
///
/// `photo_ids` is typically the current selection or folder. Unscored photos (sharpness
/// IS NULL) are included in clustering but are not flagged either way (they have no score
/// to compare). Re-running is idempotent: flags are overwritten on every call.
#[tauri::command]
pub async fn analyze_burst_sharpness(
    state: State<'_, AppState>,
    photo_ids: Vec<i64>,
) -> Result<BurstAnalysisResult, String> {
    use crate::burst::{group_into_clusters, BurstConfig, BurstPhoto};

    if photo_ids.is_empty() {
        return Ok(BurstAnalysisResult {
            total: 0,
            clusters: 0,
            flagged_soft: 0,
            flagged_best: 0,
            cleared: 0,
        });
    }

    // ── Step 1: read threshold + clustering settings ──────────────────────────
    let (burst_soft_threshold, time_gap_secs, hamming_threshold) =
        with_catalog_blocking(&state, |c| {
            let soft_t: f64 = c
                .get_setting(BURST_SOFT_THRESHOLD_KEY)
                .ok()
                .flatten()
                .and_then(|s| s.parse().ok())
                .filter(|&v: &f64| v.is_finite() && v > 0.0 && v < 1.0)
                .unwrap_or(BURST_SOFT_THRESHOLD_DEFAULT);
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
            Ok((soft_t, gap, hamming))
        })
        .await?;

    let cfg = BurstConfig { time_gap_secs, hamming_threshold };

    // ── Step 2: fetch burst inputs from catalog ───────────────────────────────
    let inputs = with_catalog_blocking(&state, move |c| {
        c.burst_inputs(&photo_ids)
    })
    .await?;

    let total = inputs.len();

    // Convert BurstInput → BurstPhoto (parse capture_time ISO → unix seconds).
    let burst_photos: Vec<BurstPhoto> = inputs
        .iter()
        .map(|bi| {
            let capture_ts = bi.capture_time.as_deref().and_then(parse_capture_time_secs);
            BurstPhoto {
                id: bi.id,
                capture_ts,
                phash: bi.phash,
                rating: bi.rating,
                sharpness: bi.sharpness,
            }
        })
        .collect();

    // ── Step 3: cluster ───────────────────────────────────────────────────────
    let clusters = group_into_clusters::<fn(i64) -> Option<Vec<u8>>>(
        burst_photos,
        &cfg,
        None, // no on-demand thumb scoring — burst flags require pre-indexed sharpness
    );

    let num_clusters = clusters.len();
    let mut flags: Vec<(i64, String)> = Vec::new();
    let mut flagged_soft = 0usize;
    let mut flagged_best = 0usize;
    let mut cleared = 0usize;

    for cluster in &clusters {
        if cluster.photo_ids.len() == 1 {
            // Single-photo cluster: clear any stale burst flag from a previous run.
            flags.push((cluster.photo_ids[0], String::new()));
            cleared += 1;
            continue;
        }

        // Gather sharpness scores for scored members. Unscored photos (None) are
        // included in the cluster but receive no flag.
        //
        // Iterate `inputs` (already catalog-fetched) rather than zipping
        // cluster.photo_ids with inputs_for_cluster: if any cluster ID was
        // deleted from the catalog between `burst_inputs()` and here,
        // `inputs_for_cluster` silently skips it, causing a zip to pair the
        // remaining IDs with the wrong BurstInputs and potentially crowning the
        // wrong photo as sharpest-of-burst. Filtering inputs by cluster
        // membership avoids that misalignment entirely.
        let scored: Vec<(i64, f64)> = inputs
            .iter()
            .filter(|bi| cluster.photo_ids.contains(&bi.id))
            .filter_map(|bi| bi.sharpness.map(|s| (bi.id, s)))
            .collect();

        if scored.is_empty() {
            // No sharpness scores available — clear stale flags.
            for &id in &cluster.photo_ids {
                flags.push((id, String::new()));
                cleared += 1;
            }
            continue;
        }

        // Compute the cluster median (over scored members only).
        let median = compute_median(&scored.iter().map(|&(_, s)| s).collect::<Vec<_>>());

        // Find the sharpest scored member.
        let best_id = scored
            .iter()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .map(|&(id, _)| id);

        // Assign flags per-member.
        for &id in &cluster.photo_ids {
            if let Some(inp) = inputs.iter().find(|bi| bi.id == id) {
                match inp.sharpness {
                    Some(s) => {
                        if Some(id) == best_id {
                            flags.push((id, "sharpest-of-burst".to_string()));
                            flagged_best += 1;
                        } else if s < median * burst_soft_threshold {
                            flags.push((id, "soft-in-burst".to_string()));
                            flagged_soft += 1;
                        } else {
                            // In-range (not the best, not soft): clear stale flag.
                            flags.push((id, String::new()));
                        }
                    }
                    None => {
                        // Unscored: clear stale flag.
                        flags.push((id, String::new()));
                        cleared += 1;
                    }
                }
            }
        }
    }

    // ── Step 4: persist ───────────────────────────────────────────────────────
    with_catalog_blocking(&state, move |c| c.set_burst_flags(flags)).await?;

    Ok(BurstAnalysisResult {
        total,
        clusters: num_clusters,
        flagged_soft,
        flagged_best,
        cleared,
    })
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

/// Parse the catalog's local-ISO capture_time string ("YYYY-MM-DDTHH:MM:SS") to unix
/// seconds. Returns `None` on any parse failure (missing field, bad format, etc.).
/// The catalog stores dates in local time without timezone; for burst grouping this
/// is fine — all photos in a session share the same camera clock.
fn parse_capture_time_secs(s: &str) -> Option<i64> {
    // Accept both "T" and space separators (some cameras omit the T).
    let s = s.replace(' ', "T");
    // NaiveDateTime::parse_from_str returns a local-clock datetime.
    chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%dT%H:%M:%S")
        .ok()
        .map(|dt| dt.and_utc().timestamp())
}

/// Compute the median of a non-empty slice of f64 values.
/// Uses a sorted copy. For odd-length slices this is the exact median. For
/// even-length slices it returns the **upper-middle** element (`sorted[len/2]`)
/// rather than the average of the two middle elements — i.e. it is biased
/// toward the higher value. For a 2-photo burst [a, b] (a ≤ b) this equals b,
/// making the soft-in-burst threshold 60% of the maximum rather than 60% of
/// the true median. The spec allows "~60%" so this is acceptable, but callers
/// should be aware that the threshold is slightly stricter for even-length
/// clusters than the mathematical median would imply.
fn compute_median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    sorted[sorted.len() / 2]
}

// ── Tests for H16e helpers ────────────────────────────────────────────────────

#[cfg(test)]
mod burst_flag_tests {
    use super::{compute_median, parse_capture_time_secs};
    use crate::catalog::BurstInput;

    // ── compute_median ───────────────────────────────────────────────────────

    #[test]
    fn median_odd_count() {
        // [1.0, 2.0, 3.0] → median is 2.0 (middle element after sort).
        let m = compute_median(&[3.0, 1.0, 2.0]);
        assert!((m - 2.0).abs() < 1e-9);
    }

    #[test]
    fn median_even_count_picks_upper_middle() {
        // [1.0, 2.0, 3.0, 4.0] → sorted[4/2] = sorted[2] = 3.0 (upper-middle element).
        let m = compute_median(&[4.0, 1.0, 3.0, 2.0]);
        assert!((m - 3.0).abs() < 1e-9);
    }

    #[test]
    fn median_single_element() {
        assert!((compute_median(&[7.5]) - 7.5).abs() < 1e-9);
    }

    #[test]
    fn median_empty_returns_zero() {
        assert_eq!(compute_median(&[]), 0.0);
    }

    // ── parse_capture_time_secs ──────────────────────────────────────────────

    #[test]
    fn parse_capture_time_t_separator() {
        let ts = parse_capture_time_secs("2024-03-15T14:30:00");
        assert!(ts.is_some(), "T-separator ISO datetime must parse");
    }

    #[test]
    fn parse_capture_time_space_separator() {
        let ts = parse_capture_time_secs("2024-03-15 14:30:00");
        assert!(ts.is_some(), "space-separator datetime must parse");
    }

    #[test]
    fn parse_capture_time_t_space_same_result() {
        let t1 = parse_capture_time_secs("2024-03-15T14:30:00").unwrap();
        let t2 = parse_capture_time_secs("2024-03-15 14:30:00").unwrap();
        assert_eq!(t1, t2, "T and space separators must parse to the same timestamp");
    }

    #[test]
    fn parse_capture_time_invalid_is_none() {
        assert!(parse_capture_time_secs("not-a-date").is_none());
        assert!(parse_capture_time_secs("").is_none());
    }

    // ── Burst flagging integration: cluster analysis logic ───────────────────
    //
    // These tests exercise the flag-assignment loop directly by simulating the
    // data flow through the burst analysis command's logic.

    /// Build a BurstInput with the given sharpness.
    fn bi(id: i64, sharpness: Option<f64>) -> BurstInput {
        BurstInput {
            id,
            capture_time: Some(format!("2024-01-01T12:00:{:02}", id)),
            phash: None,
            rating: 0,
            sharpness,
        }
    }

    /// Simulate the per-cluster flagging step from `analyze_burst_sharpness`.
    /// Returns a Vec of (id, flag) where flag is "" for cleared/neutral entries.
    fn flag_cluster(cluster_ids: &[i64], inputs: &[BurstInput], threshold: f64) -> Vec<(i64, String)> {
        let mut flags = Vec::new();

        // Get scored members.
        let scored: Vec<(i64, f64)> = cluster_ids
            .iter()
            .filter_map(|&id| {
                inputs.iter().find(|bi| bi.id == id)?.sharpness.map(|s| (id, s))
            })
            .collect();

        if scored.is_empty() {
            for &id in cluster_ids {
                flags.push((id, String::new()));
            }
            return flags;
        }

        let median = compute_median(&scored.iter().map(|&(_, s)| s).collect::<Vec<_>>());
        let best_id = scored
            .iter()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .map(|&(id, _)| id);

        for &id in cluster_ids {
            if let Some(inp) = inputs.iter().find(|bi| bi.id == id) {
                match inp.sharpness {
                    Some(s) => {
                        if Some(id) == best_id {
                            flags.push((id, "sharpest-of-burst".to_string()));
                        } else if s < median * threshold {
                            flags.push((id, "soft-in-burst".to_string()));
                        } else {
                            flags.push((id, String::new()));
                        }
                    }
                    None => flags.push((id, String::new())),
                }
            }
        }
        flags
    }

    #[test]
    fn soft_frame_flagged_below_60pct_of_median() {
        // Three scored frames: 100.0, 100.0, 10.0. Median = 100.0. 60% = 60.0.
        // Frame 3 (10.0) < 60.0 → soft-in-burst.
        // Frames 1 and 2 tie for sharpest → deterministic max picks the higher id (2) as best.
        let inputs = vec![bi(1, Some(100.0)), bi(2, Some(100.0)), bi(3, Some(10.0))];
        let flags = flag_cluster(&[1, 2, 3], &inputs, 0.60);
        let get_flag = |id: i64| flags.iter().find(|(i, _)| *i == id).map(|(_, f)| f.as_str()).unwrap_or("");
        assert_eq!(get_flag(3), "soft-in-burst");
        // One of {1, 2} is the sharpest (both 100.0 — tie goes to the highest id by the
        // max_by tiebreaker in the real code, but here we just check that exactly one is crowned).
        let best_count = flags.iter().filter(|(_, f)| f == "sharpest-of-burst").count();
        assert_eq!(best_count, 1, "exactly one frame should be crowned sharpest-of-burst");
        let soft_count = flags.iter().filter(|(_, f)| f == "soft-in-burst").count();
        assert_eq!(soft_count, 1);
    }

    #[test]
    fn above_threshold_not_flagged_soft() {
        // Two frames: 100.0 and 70.0. Median = 100.0. 60% = 60.0.
        // 70.0 > 60.0 → not soft.
        let inputs = vec![bi(1, Some(100.0)), bi(2, Some(70.0))];
        let flags = flag_cluster(&[1, 2], &inputs, 0.60);
        let soft_count = flags.iter().filter(|(_, f)| f == "soft-in-burst").count();
        assert_eq!(soft_count, 0, "70 is above 60% of median 100 — must not be soft");
    }

    #[test]
    fn unscored_frames_get_neutral_flag() {
        // One scored frame, one unscored. Unscored must not receive a burst flag.
        let inputs = vec![bi(1, Some(200.0)), bi(2, None)];
        let flags = flag_cluster(&[1, 2], &inputs, 0.60);
        let flag2 = flags.iter().find(|(i, _)| *i == 2).map(|(_, f)| f.as_str()).unwrap_or("missing");
        assert_eq!(flag2, "", "unscored photo must get an empty (clear) flag");
    }

    #[test]
    fn all_unscored_cluster_cleared() {
        let inputs = vec![bi(1, None), bi(2, None)];
        let flags = flag_cluster(&[1, 2], &inputs, 0.60);
        assert!(flags.iter().all(|(_, f)| f.is_empty()), "all-unscored cluster gets all-cleared flags");
    }
}
