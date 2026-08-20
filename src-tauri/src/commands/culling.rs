//! "Why is this flagged" — the derivation behind a photo's culling badges (C6).
//!
//! The grid's badges are verdicts with the reasoning thrown away: a `~B` says a frame is
//! soft *in its burst* without showing the burst, the median it lost to, or the frame that
//! won. This command puts the derivation back, by re-running the same engine and the same
//! rule over the same neighbours (`catalog::culling::burst_neighbourhood` →
//! `burst::group_into_clusters` → `burst::flag_cluster`) and returning the numbers rather
//! than a sentence about them.
//!
//! Two honesty obligations shape the payload:
//!
//! - **The recomputed verdict can differ from the stored flag.** `analyze_burst_sharpness`
//!   clustered whatever set it was given, possibly months ago and possibly a folder rather
//!   than the library. When the two disagree the payload reports both and marks the badge
//!   stale, because "your badge is out of date" is the true answer and silently showing
//!   the fresh verdict would hide it.
//! - **A cluster that could not be seen whole is labelled.** `truncated` rides through to
//!   the UI so a rank of "3 of 8" is never printed for a burst that may have had 40 frames.
//!
//! Blink/gaze has no entry here: the face pipeline stores the 5-point ArcFace template
//! (one centre per eye), from which eye-openness cannot be derived at any confidence. That
//! signal needs its own model (backlog C2), and an empty row promising it would be worse
//! than its absence.

use super::{with_catalog_blocking, AppState};
use crate::burst::{flag_cluster, group_into_clusters, BurstConfig, BurstPhoto};
use crate::catalog::culling::CullingNeighbour;
use serde::Serialize;
use tauri::State;

/// How many cluster frames to return. A timelapse can chain thousands of frames into one
/// cluster; the reader needs the subject, the winner and enough context to judge, not the
/// whole run. `cluster_size` still reports the true total, so the UI can say how many were
/// left out.
const MAX_FRAMES: usize = 60;

/// Everything known about why one photo carries the badges it carries.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhotoSignals {
    pub photo_id: i64,
    /// `None` when the background indexer has not scored this photo yet.
    pub sharpness: Option<SharpnessSignal>,
    /// `None` when the photo has no usable capture time, so it belongs to no burst.
    pub burst: Option<BurstSignal>,
    pub stack: StackSignal,
    /// Named edit versions — the `⧉ N` badge.
    pub version_count: i64,
}

/// The absolute sharpness score and the library-wide bar it is judged against.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SharpnessSignal {
    pub score: f64,
    /// How the score was computed: `"tile"`, `"face"` or `"afpoint"` (H16c). Thresholds
    /// are not comparable across methods, which is why the method is shown, not hidden.
    pub method: Option<String>,
    /// `sharpness.soft_threshold` — the bar behind the grid's `~` badge.
    pub soft_threshold: f64,
    pub below_threshold: bool,
}

/// The burst-relative reading: the cluster, the median, and where this frame sits in it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BurstSignal {
    /// Frames in the cluster after both splits (time gap, then visual similarity).
    pub cluster_size: usize,
    /// Frames in the surrounding time run, before the visual split. Larger than
    /// `cluster_size` when the engine decided the run covered more than one scene.
    pub time_group_size: usize,
    /// How many cluster frames carry a sharpness score — the only ones the rule compares.
    pub scored: usize,
    /// This frame's 1-based place among the scored frames, sharpest first.
    pub rank: Option<usize>,
    /// Median sharpness of the scored frames (upper-middle for even counts).
    pub median: Option<f64>,
    /// `median × soft_fraction` — score below this and the frame is soft-in-burst.
    pub cutoff: Option<f64>,
    /// The configured `sharpness.burst_soft_threshold` fraction, not the 0.60 default
    /// that the badge tooltips used to assert regardless of the setting.
    pub soft_fraction: f64,
    /// The verdict this reconstruction reaches now: `"soft-in-burst"`,
    /// `"sharpest-of-burst"`, or `None` for neither.
    pub verdict: Option<String>,
    /// What `photos.burst_flag` currently holds, from the last analysis run.
    pub stored_flag: Option<String>,
    /// The stored flag and the fresh verdict disagree — the badge is out of date, usually
    /// because the last run clustered this photo among a different set of photos.
    pub stale: bool,
    /// The cluster could not be seen whole (row cap or time span), so `cluster_size`,
    /// `rank` and `median` are lower bounds on a possibly larger burst.
    pub truncated: bool,
    /// Clustering settings this reconstruction used.
    pub time_gap_secs: i64,
    pub hamming_threshold: u32,
    /// The sharpest scored frame — the one a `sharpest-of-burst` crown belongs to.
    pub best: Option<ClusterFrame>,
    /// Cluster members, capture order, at most [`MAX_FRAMES`]. Always includes the
    /// subject and, when there is one, `best`.
    pub frames: Vec<ClusterFrame>,
}

/// One frame of the cluster, as the explanation shows it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterFrame {
    pub photo_id: i64,
    /// Filename only. The catalog-root-relative path is not a physical location and is
    /// too long to read in a list.
    pub file_name: String,
    pub sharpness: Option<f64>,
    pub rating: i64,
    /// This frame's verdict under the same recomputation.
    pub verdict: Option<String>,
    /// dHash distance from the subject, 0 = identical hash. `None` when either frame has
    /// not been hashed yet. Below `hamming_threshold` means "the engine considers this the
    /// same scene" — this is the near-duplicate signal, at burst scope.
    pub hamming_distance: Option<u32>,
    pub is_subject: bool,
}

/// RAW+JPEG stacking — the `▤ N` badge.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StackSignal {
    /// Photos stacked under this one.
    pub child_count: i64,
    /// The master this photo is stacked under, when it is itself a derivative.
    pub parent_id: Option<i64>,
}

/// Explain every culling signal on one photo (C6).
///
/// Read-only: it recomputes, and never writes a flag back. Persisting the fresh verdict
/// here would repair the badge as a side effect of looking at it, which hides from the
/// user that their last analysis run is out of date and turns opening an inspector panel
/// into a catalog mutation.
#[tauri::command]
pub async fn explain_photo_signals(
    state: State<'_, AppState>,
    photo_id: i64,
) -> Result<PhotoSignals, String> {
    with_catalog_blocking(&state, move |c| {
        let photo = c.get_photo(photo_id)?;

        let soft_threshold = c.soft_threshold();
        let sharpness = photo.sharpness.map(|score| SharpnessSignal {
            score,
            method: photo.sharpness_method.clone(),
            soft_threshold,
            below_threshold: score < soft_threshold,
        });

        // Validated exactly as `analyze_burst_sharpness` validates it, so an
        // out-of-range setting cannot make the explanation and the badge disagree.
        let soft_fraction = c
            .get_setting(super::burst::BURST_SOFT_THRESHOLD_KEY)
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
            .filter(|&v: &f64| v.is_finite() && v > 0.0 && v < 1.0)
            .unwrap_or(super::burst::BURST_SOFT_THRESHOLD_DEFAULT);
        let time_gap_secs = setting_i64(c, "ai.burst_time_gap_secs", 15);
        let hamming_threshold = setting_i64(c, "ai.burst_hamming_threshold", 10).max(0) as u32;

        let neighbourhood = crate::catalog::culling::burst_neighbourhood(
            c.conn(),
            photo_id,
            time_gap_secs,
        )?;

        let burst = neighbourhood.map(|n| {
            build_burst_signal(
                photo_id,
                &n.members,
                n.truncated,
                &BurstConfig { time_gap_secs, hamming_threshold },
                soft_fraction,
                photo.burst_flag.clone(),
            )
        });

        Ok(PhotoSignals {
            photo_id,
            sharpness,
            burst,
            stack: StackSignal {
                child_count: photo.stack_count,
                parent_id: photo.stack_parent_id,
            },
            version_count: photo.version_count,
        })
    })
    .await
}

/// Cluster the time run, find the subject's cluster, and apply the flagging rule to it.
///
/// Split out of the command so it is exercised without a Tauri `State`: this is where the
/// explanation could silently stop matching the badge, so it is the part worth testing.
fn build_burst_signal(
    photo_id: i64,
    time_group: &[CullingNeighbour],
    truncated: bool,
    cfg: &BurstConfig,
    soft_fraction: f64,
    stored_flag: Option<String>,
) -> BurstSignal {
    let as_burst = |n: &CullingNeighbour| BurstPhoto {
        id: n.id,
        capture_ts: Some(n.capture_ts),
        phash: n.phash,
        rating: n.rating,
        sharpness: n.sharpness,
    };

    // The engine splits the time run again on visual similarity, so the cluster the rule
    // judged is a subset of the run. Running the real `group_into_clusters` rather than
    // re-deriving the split keeps this reading identical to the analysis run's.
    let clusters = group_into_clusters::<fn(i64) -> Option<Vec<u8>>>(
        time_group.iter().map(as_burst).collect(),
        cfg,
        None,
    );
    let member_ids: Vec<i64> = clusters
        .iter()
        .find(|c| c.photo_ids.contains(&photo_id))
        .map(|c| c.photo_ids.clone())
        // The subject came out of its own neighbourhood, so it is always in some cluster;
        // fall back to a lone frame rather than panicking if that ever stops holding.
        .unwrap_or_else(|| vec![photo_id]);

    let members: Vec<BurstPhoto> = member_ids
        .iter()
        .filter_map(|id| time_group.iter().find(|n| n.id == *id))
        .map(as_burst)
        .collect();

    let flagging = flag_cluster(&members, soft_fraction);
    let verdict = flagging
        .verdicts
        .iter()
        .find(|(id, _)| *id == photo_id)
        .and_then(|&(_, v)| v.db_str())
        .map(str::to_string);

    let subject_hash = time_group.iter().find(|n| n.id == photo_id).and_then(|n| n.phash);
    let frame_of = |id: i64| -> Option<ClusterFrame> {
        let n = time_group.iter().find(|n| n.id == id)?;
        Some(ClusterFrame {
            photo_id: n.id,
            file_name: n.path.rsplit('/').next().unwrap_or(&n.path).to_string(),
            sharpness: n.sharpness,
            rating: n.rating,
            verdict: flagging
                .verdicts
                .iter()
                .find(|(vid, _)| *vid == id)
                .and_then(|&(_, v)| v.db_str())
                .map(str::to_string),
            hamming_distance: match (subject_hash, n.phash) {
                (Some(a), Some(b)) => Some(crate::phash::hamming_distance(a, b)),
                _ => None,
            },
            is_subject: n.id == photo_id,
        })
    };

    let best = flagging.best.and_then(|(id, _)| frame_of(id));

    // Keep the frames nearest the subject in capture order, then re-add the subject and
    // the winner if the window cut either out — a list that omits the frame you are
    // looking at explains nothing.
    let mut frames: Vec<ClusterFrame> = if member_ids.len() <= MAX_FRAMES {
        member_ids.iter().filter_map(|id| frame_of(*id)).collect()
    } else {
        let at = member_ids.iter().position(|id| *id == photo_id).unwrap_or(0);
        let start = at.saturating_sub(MAX_FRAMES / 2).min(member_ids.len() - MAX_FRAMES);
        let mut window: Vec<ClusterFrame> = member_ids[start..start + MAX_FRAMES]
            .iter()
            .filter_map(|id| frame_of(*id))
            .collect();
        if let Some(b) = best.clone() {
            if !window.iter().any(|f| f.photo_id == b.photo_id) {
                window.push(b);
            }
        }
        window
    };
    if !frames.iter().any(|f| f.is_subject) {
        if let Some(f) = frame_of(photo_id) {
            frames.push(f);
        }
    }

    let fresh = verdict.clone();
    BurstSignal {
        cluster_size: member_ids.len(),
        time_group_size: time_group.len(),
        scored: flagging.scored,
        rank: flagging.rank_of(photo_id, &members),
        median: flagging.median,
        cutoff: flagging.cutoff,
        soft_fraction,
        stale: stored_flag.as_deref() != fresh.as_deref(),
        verdict,
        stored_flag,
        truncated,
        time_gap_secs: cfg.time_gap_secs,
        hamming_threshold: cfg.hamming_threshold,
        best,
        frames,
    }
}

fn setting_i64(c: &crate::catalog::Catalog, key: &str, default: i64) -> i64 {
    c.get_setting(key)
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn neighbour(id: i64, ts: i64, sharpness: Option<f64>, phash: Option<u64>) -> CullingNeighbour {
        CullingNeighbour {
            id,
            path: format!("2024/DSC_{id:04}.NEF"),
            capture_ts: ts,
            phash,
            rating: 0,
            sharpness,
            sharpness_method: Some("tile".into()),
            burst_flag: None,
        }
    }

    fn cfg() -> BurstConfig {
        BurstConfig { time_gap_secs: 15, hamming_threshold: 10 }
    }

    #[test]
    fn the_explanation_reaches_the_same_verdict_as_the_badge() {
        // Scores 100/100/10: the median is 100, the cutoff 60, and frame 3 is soft. The
        // explanation must name that cutoff, not restate the rule in words.
        let run = vec![
            neighbour(1, 0, Some(100.0), None),
            neighbour(2, 1, Some(100.0), None),
            neighbour(3, 2, Some(10.0), None),
        ];

        let s = build_burst_signal(3, &run, false, &cfg(), 0.60, Some("soft-in-burst".into()));

        assert_eq!(s.verdict.as_deref(), Some("soft-in-burst"));
        assert_eq!(s.median, Some(100.0));
        assert_eq!(s.cutoff, Some(60.0));
        assert_eq!(s.rank, Some(3), "last of three scored frames");
        assert_eq!(s.cluster_size, 3);
        assert_eq!(s.scored, 3);
        assert!(!s.stale, "the stored flag agrees with the recomputation");
        assert_eq!(s.best.as_ref().map(|b| b.photo_id), Some(2), "ties crown the later frame");
    }

    #[test]
    fn a_flag_the_recomputation_does_not_reach_is_reported_stale_not_hidden() {
        // The stored flag came from a run over a different photo set. Showing only the
        // fresh verdict would quietly contradict the badge still on screen.
        let run = vec![
            neighbour(1, 0, Some(100.0), None),
            neighbour(2, 1, Some(90.0), None),
        ];

        let s = build_burst_signal(2, &run, false, &cfg(), 0.60, Some("soft-in-burst".into()));

        assert_eq!(s.stored_flag.as_deref(), Some("soft-in-burst"));
        assert_eq!(s.verdict, None, "90 is well above the 60 cutoff");
        assert!(s.stale);
    }

    #[test]
    fn the_visual_split_narrows_the_cluster_below_the_time_run() {
        // Four frames one second apart, but frames 3 and 4 are a different scene: the
        // engine splits the run, so the burst judged is two frames, not four.
        let run = vec![
            neighbour(1, 0, Some(100.0), Some(0x0000_0000_0000_0000)),
            neighbour(2, 1, Some(90.0), Some(0x0000_0000_0000_0001)),
            neighbour(3, 2, Some(80.0), Some(0xFFFF_FFFF_FFFF_FFFF)),
            neighbour(4, 3, Some(70.0), Some(0xFFFF_FFFF_FFFF_FFFE)),
        ];

        let s = build_burst_signal(1, &run, false, &cfg(), 0.60, None);

        assert_eq!(s.time_group_size, 4, "all four are within the time gap");
        assert_eq!(s.cluster_size, 2, "but only two share the scene");
        assert_eq!(
            s.frames.iter().map(|f| f.photo_id).collect::<Vec<_>>(),
            vec![1, 2],
            "and the frames shown are the ones actually compared"
        );
    }

    #[test]
    fn hamming_distance_is_measured_from_the_subject() {
        let run = vec![
            neighbour(1, 0, Some(100.0), Some(0b0000)),
            neighbour(2, 1, Some(90.0), Some(0b0011)),
            neighbour(3, 2, Some(80.0), None),
        ];

        let s = build_burst_signal(1, &run, false, &cfg(), 0.60, None);
        let d = |id: i64| {
            s.frames.iter().find(|f| f.photo_id == id).and_then(|f| f.hamming_distance)
        };

        assert_eq!(d(1), Some(0), "the subject is identical to itself");
        assert_eq!(d(2), Some(2));
        assert_eq!(d(3), None, "an unhashed frame has no distance, and does not get a 0");
    }

    #[test]
    fn a_capped_cluster_still_shows_the_subject_and_the_winner() {
        // A timelapse chains far more frames than the list can carry. Whatever is dropped,
        // the frame being explained and the frame that beat it must survive.
        let mut run: Vec<CullingNeighbour> =
            (0..MAX_FRAMES as i64 + 40).map(|i| neighbour(i + 1, i, Some(10.0), None)).collect();
        // The sharpest frame sits at the very start; the subject at the very end.
        run[0].sharpness = Some(999.0);
        let subject = run.last().unwrap().id;

        let s = build_burst_signal(subject, &run, false, &cfg(), 0.60, None);

        assert_eq!(s.cluster_size, MAX_FRAMES + 40, "the true size is still reported");
        assert!(s.frames.len() <= MAX_FRAMES + 2, "the list itself stays bounded");
        assert!(s.frames.iter().any(|f| f.is_subject), "the subject is never dropped");
        assert!(
            s.frames.iter().any(|f| f.photo_id == 1),
            "nor is the frame that won the cluster"
        );
    }

    #[test]
    fn an_unscored_subject_gets_no_rank_and_no_verdict() {
        let run = vec![
            neighbour(1, 0, Some(100.0), None),
            neighbour(2, 1, None, None),
        ];

        let s = build_burst_signal(2, &run, false, &cfg(), 0.60, None);

        assert_eq!(s.rank, None, "an unscored frame has no place in the ordering");
        assert_eq!(s.verdict, None);
        assert_eq!(s.scored, 1);
        assert!(!s.stale, "no stored flag and no fresh verdict is agreement, not staleness");
    }

    #[test]
    fn truncation_rides_through_to_the_caller() {
        let run = vec![neighbour(1, 0, Some(100.0), None), neighbour(2, 1, Some(90.0), None)];
        let s = build_burst_signal(1, &run, true, &cfg(), 0.60, None);
        assert!(s.truncated, "the UI must be able to say the burst may be larger");
    }
}
