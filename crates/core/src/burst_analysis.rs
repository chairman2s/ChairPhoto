//! Burst-relative sharpness flagging (H16e).
//!
//! Clusters a set of photos with the H15b burst engine ([`crate::burst`]), then flags each
//! cluster's weak frames `soft-in-burst` and its best frame `sharpest-of-burst`. Flagging
//! is advisory only — nothing is ever auto-rejected (see `docs/sharpness-culling.md`).
//!
//! Moved here from the Tauri shell's `commands/burst.rs` (gpui #106), so the GPUI app's
//! "Analyse burst sharpness" and the Tauri command run the same code.

use crate::app::{with_catalog, AppState};
use crate::burst::{group_into_clusters, BurstConfig, BurstPhoto, BurstVerdict};
use serde::Serialize;

/// Summary of one [`analyze_burst_sharpness`] run.
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

/// Analyse burst-relative sharpness for a set of photos. Blocking: call it on a worker.
///
/// 1. Read the threshold and the clustering settings (`ai.burst_time_gap_secs`,
///    `ai.burst_hamming_threshold`), and `(capture_time, phash, rating, sharpness)` for
///    each id — under the catalog lock.
/// 2. Run the H15b cluster engine (`group_into_clusters`) — off the lock.
/// 3. For clusters with >1 sharpness-scored member: flag members below
///    `threshold × median` as `"soft-in-burst"` and crown the sharpest
///    `"sharpest-of-burst"`. Single-photo clusters: clear any stale flag.
/// 4. Write all flags in a single transaction via `Catalog::set_burst_flags`.
///
/// `photo_ids` is typically the current selection or view. Unscored photos (sharpness IS
/// NULL) are clustered but flagged neither way (they have no score to compare).
/// Re-running is idempotent: flags are overwritten on every call.
pub fn analyze_burst_sharpness(state: &AppState, photo_ids: &[i64]) -> Result<BurstAnalysisResult, String> {
    if photo_ids.is_empty() {
        return Ok(BurstAnalysisResult { total: 0, clusters: 0, flagged_soft: 0, flagged_best: 0, cleared: 0 });
    }

    // ── Step 1: settings and inputs, under the lock ───────────────────────────
    let (burst_soft_threshold, cfg, inputs) = with_catalog(state, |c| {
        let setting = |key: &str| c.get_setting(key).ok().flatten();
        let soft_t: f64 = setting(BURST_SOFT_THRESHOLD_KEY)
            .and_then(|s| s.parse().ok())
            .filter(|&v: &f64| v.is_finite() && v > 0.0 && v < 1.0)
            .unwrap_or(BURST_SOFT_THRESHOLD_DEFAULT);
        let time_gap_secs: i64 = setting("ai.burst_time_gap_secs").and_then(|s| s.parse().ok()).unwrap_or(15);
        let hamming_threshold: u32 =
            setting("ai.burst_hamming_threshold").and_then(|s| s.parse().ok()).unwrap_or(10);
        let inputs = c.burst_inputs(photo_ids)?;
        Ok((soft_t, BurstConfig { time_gap_secs, hamming_threshold }, inputs))
    })?;

    let total = inputs.len();
    let burst_photo = |bi: &crate::catalog::BurstInput| BurstPhoto {
        id: bi.id,
        capture_ts: bi.capture_time.as_deref().and_then(parse_capture_time_secs),
        phash: bi.phash,
        rating: bi.rating,
        sharpness: bi.sharpness,
    };

    // ── Step 2: cluster, off the lock ─────────────────────────────────────────
    let clusters = group_into_clusters::<fn(i64) -> Option<Vec<u8>>>(
        inputs.iter().map(burst_photo).collect(),
        &cfg,
        None, // no on-demand thumb scoring — burst flags require pre-indexed sharpness
    );

    let num_clusters = clusters.len();
    let mut flags: Vec<(i64, String)> = Vec::new();
    let mut flagged_soft = 0usize;
    let mut flagged_best = 0usize;
    let mut cleared = 0usize;

    for cluster in &clusters {
        // Look each member up in `inputs` by id rather than zipping cluster.photo_ids
        // against a positionally-filtered slice: if any cluster ID was deleted from the
        // catalog between `burst_inputs()` and here, a zip would pair the remaining IDs
        // with the wrong BurstInputs and could crown the wrong photo sharpest-of-burst.
        // A missing id contributes no member, and so gets no flag.
        let members: Vec<BurstPhoto> = cluster
            .photo_ids
            .iter()
            .filter_map(|id| inputs.iter().find(|bi| bi.id == *id))
            .map(burst_photo)
            .collect();

        // The rule itself lives in `crate::burst` so that `explain_photo_signals` applies
        // the same one when it explains the flag this run persists.
        for &(id, verdict) in &crate::burst::flag_cluster(&members, burst_soft_threshold).verdicts {
            flags.push((id, verdict.db_str().unwrap_or("").to_string()));
            match verdict {
                BurstVerdict::Sharpest => flagged_best += 1,
                BurstVerdict::Soft => flagged_soft += 1,
                // In-range: scored, but neither best nor soft. Clears a stale flag
                // without counting as one of the cleared — those are the unscored.
                BurstVerdict::InRange => {}
                BurstVerdict::Unscored => cleared += 1,
            }
        }
    }

    // ── Step 4: persist ───────────────────────────────────────────────────────
    with_catalog(state, |c| c.set_burst_flags(flags))?;

    Ok(BurstAnalysisResult { total, clusters: num_clusters, flagged_soft, flagged_best, cleared })
}

/// Parse the catalog's local-ISO capture_time string ("YYYY-MM-DDTHH:MM:SS") to unix
/// seconds. Returns `None` on any parse failure (missing field, bad format, etc.).
/// The catalog stores dates in local time without timezone; for burst grouping this
/// is fine — all photos in a session share the same camera clock.
pub fn parse_capture_time_secs(s: &str) -> Option<i64> {
    // Accept both "T" and space separators (some cameras omit the T).
    let s = s.replace(' ', "T");
    // NaiveDateTime::parse_from_str returns a local-clock datetime.
    chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%dT%H:%M:%S")
        .ok()
        .map(|dt| dt.and_utc().timestamp())
}

#[cfg(test)]
mod burst_flag_tests {
    use super::parse_capture_time_secs;
    use crate::burst::{flag_cluster, BurstPhoto, BurstVerdict};

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

    // ── The flags this command persists ──────────────────────────────────────
    //
    // These call `crate::burst::flag_cluster` — the same function the command's
    // per-cluster loop calls. An earlier version of this module re-implemented the
    // rule in a test helper, which could only ever prove that the copy agreed with
    // itself.

    /// A cluster member with the given sharpness. Timestamps are one second apart so
    /// the ids also read as capture order; `flag_cluster` ignores them either way.
    fn m(id: i64, sharpness: Option<f64>) -> BurstPhoto {
        BurstPhoto { id, capture_ts: Some(id), phash: None, rating: 0, sharpness }
    }

    fn verdict_of(f: &crate::burst::ClusterFlagging, id: i64) -> BurstVerdict {
        f.verdicts.iter().find(|(i, _)| *i == id).map(|&(_, v)| v).expect("member is flagged")
    }

    #[test]
    fn soft_frame_flagged_below_60pct_of_median() {
        // Three scored frames: 100.0, 100.0, 10.0. Median = 100.0, cutoff = 60.0.
        // Frame 3 (10.0) is below the cutoff; exactly one of the tied 100.0 frames is
        // crowned, and the other reads as in-range rather than soft.
        let members = [m(1, Some(100.0)), m(2, Some(100.0)), m(3, Some(10.0))];
        let f = flag_cluster(&members, 0.60);

        assert_eq!(f.median, Some(100.0));
        assert_eq!(f.cutoff, Some(60.0));
        assert_eq!(verdict_of(&f, 3), BurstVerdict::Soft);
        assert_eq!(
            f.verdicts.iter().filter(|(_, v)| *v == BurstVerdict::Sharpest).count(),
            1,
            "exactly one frame is crowned sharpest-of-burst"
        );
        assert_eq!(f.verdicts.iter().filter(|(_, v)| *v == BurstVerdict::Soft).count(), 1);
    }

    #[test]
    fn the_crowned_frame_ranks_first() {
        // The explanation and the flag are two readings of one rule: whichever frame is
        // crowned must also be the one `rank_of` puts at rank 1, ties included.
        let members = [m(1, Some(100.0)), m(2, Some(100.0)), m(3, Some(10.0))];
        let f = flag_cluster(&members, 0.60);
        let crowned = f
            .verdicts
            .iter()
            .find(|(_, v)| *v == BurstVerdict::Sharpest)
            .map(|&(id, _)| id)
            .unwrap();

        assert_eq!(f.best.map(|(id, _)| id), Some(crowned));
        assert_eq!(f.rank_of(crowned, &members), Some(1));
        assert_eq!(f.rank_of(3, &members), Some(3), "the soft frame ranks last of three");
        assert_eq!(f.scored, 3);
    }

    #[test]
    fn above_threshold_not_flagged_soft() {
        // Two frames: 100.0 and 70.0. Median (upper-middle) = 100.0, cutoff = 60.0.
        // 70.0 is above the cutoff, so it is in-range, not soft.
        let members = [m(1, Some(100.0)), m(2, Some(70.0))];
        let f = flag_cluster(&members, 0.60);
        assert_eq!(verdict_of(&f, 2), BurstVerdict::InRange);
        assert_eq!(f.verdicts.iter().filter(|(_, v)| *v == BurstVerdict::Soft).count(), 0);
    }

    #[test]
    fn unscored_frames_get_neutral_flag() {
        // One scored frame, one unscored. The unscored one is not called soft — a
        // missing score is not evidence of softness.
        let members = [m(1, Some(200.0)), m(2, None)];
        let f = flag_cluster(&members, 0.60);
        assert_eq!(verdict_of(&f, 2), BurstVerdict::Unscored);
        assert_eq!(verdict_of(&f, 2).db_str(), None, "and it clears the column");
        assert_eq!(f.scored, 1);
    }

    #[test]
    fn all_unscored_cluster_cleared() {
        let members = [m(1, None), m(2, None)];
        let f = flag_cluster(&members, 0.60);
        assert!(f.verdicts.iter().all(|(_, v)| *v == BurstVerdict::Unscored));
        assert_eq!(f.median, None, "nothing to take a median of");
    }

    #[test]
    fn a_lone_frame_is_not_a_burst() {
        // A single-photo cluster clears rather than crowning itself sharpest-of-burst:
        // the command counts this as `cleared`, resetting a flag left by an earlier run
        // over a different photo set.
        let members = [m(1, Some(100.0))];
        let f = flag_cluster(&members, 0.60);
        assert_eq!(verdict_of(&f, 1), BurstVerdict::Unscored);
        assert_eq!(f.best, None);
    }
}
