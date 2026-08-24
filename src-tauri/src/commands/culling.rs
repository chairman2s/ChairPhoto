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

// ── C3 — auto-stack proposals ────────────────────────────────────────────────
//
// Burst clustering, phash similarity and timestamp proximity already exist, but only as
// three separate readings. A proposal is the one sentence they add up to: *these frames
// are one moment — here is the keeper*. Accepting it collapses the group to a single tile
// through the stacking that already ships, so nothing is deleted and the inspector's
// existing Unstack undoes it one frame at a time.

/// How many proposals to return in one pass. A whole-library run over a wedding shoot can
/// produce thousands; a list that long is not reviewable, and the point of a proposal is
/// that a person reads it.
const MAX_PROPOSALS: usize = 200;

/// One group of frames the engine believes is a single moment.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StackProposal {
    /// The frame to stack the rest under, chosen by the engine's own representative rule
    /// (highest rating, then sharpest).
    pub keeper_id: i64,
    /// Why that frame won, in the terms the rule actually used.
    pub reason: String,
    /// Seconds from the first frame to the last.
    pub span_secs: i64,
    /// Widest dHash distance from the keeper, or `None` when frames are not all hashed.
    pub max_distance: Option<u32>,
    /// Members with no sharpness score — the rule could not weigh these.
    pub unscored: usize,
    /// Photos currently stacked under a *member*. Accepting re-homes them onto the keeper
    /// (`set_stack_parent` flattens rather than nesting), so the count is disclosed.
    pub absorbed_children: i64,
    /// Every frame, capture order, keeper included.
    pub members: Vec<ProposalFrame>,
}

/// One frame of a proposal, as the review list shows it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposalFrame {
    pub photo_id: i64,
    pub file_name: String,
    pub sharpness: Option<f64>,
    pub rating: i64,
    pub burst_flag: Option<String>,
    /// dHash distance from the proposed keeper; `None` when either is unhashed.
    pub hamming_distance: Option<u32>,
    /// Photos already stacked under this frame, which accepting would re-home.
    pub child_count: i64,
    pub is_keeper: bool,
}

/// The result of one `propose_stacks` pass.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StackProposals {
    pub proposals: Vec<StackProposal>,
    /// Photos examined — the requested ids that exist and are present.
    pub considered: usize,
    /// Requested photos left out because they are already stacked under something.
    pub skipped_stacked: usize,
    /// More groups were found than [`MAX_PROPOSALS`]; run again after accepting these.
    pub truncated: bool,
    pub time_gap_secs: i64,
    pub hamming_threshold: u32,
}

/// What accepting a proposal actually did.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StackApplied {
    /// Frames moved under the keeper.
    pub stacked: usize,
    /// Photos that were stacked under a member and are now under the keeper instead.
    pub absorbed: usize,
}

/// Propose stacks over `photo_ids` (C3) — typically the selection, or the whole view.
///
/// Read-only. Nothing is stacked until `apply_stack_proposal` is called for a group, one
/// group at a time, because collapsing frames out of the grid is exactly the kind of bulk
/// edit that should not happen as a side effect of asking a question.
#[tauri::command]
pub async fn propose_stacks(
    state: State<'_, AppState>,
    photo_ids: Vec<i64>,
) -> Result<StackProposals, String> {
    with_catalog_blocking(&state, move |c| {
        let cfg = BurstConfig {
            time_gap_secs: setting_i64(c, "ai.burst_time_gap_secs", 15),
            hamming_threshold: setting_i64(c, "ai.burst_hamming_threshold", 10).max(0) as u32,
        };
        let (candidates, skipped_stacked) =
            crate::catalog::culling::stack_candidates(c.conn(), &photo_ids)?;
        Ok(build_proposals(&candidates, skipped_stacked, &cfg))
    })
    .await
}

/// Cluster the candidates and turn every multi-frame cluster into a proposal.
///
/// Split from the command so the grouping decisions — which clusters become proposals, how
/// many are returned, and why each keeper won — are testable without a Tauri `State`.
fn build_proposals(
    candidates: &[crate::catalog::culling::StackCandidate],
    skipped_stacked: usize,
    cfg: &BurstConfig,
) -> StackProposals {
    let clusters = group_into_clusters::<fn(i64) -> Option<Vec<u8>>>(
        candidates
            .iter()
            .map(|s| BurstPhoto {
                id: s.id,
                capture_ts: s.capture_ts,
                phash: s.phash,
                rating: s.rating,
                sharpness: s.sharpness,
            })
            .collect(),
        cfg,
        None,
    );

    // Only a cluster of more than one frame is a group worth collapsing.
    let groups: Vec<&crate::burst::Cluster> =
        clusters.iter().filter(|cl| cl.photo_ids.len() > 1).collect();
    let truncated = groups.len() > MAX_PROPOSALS;

    StackProposals {
        proposals: groups
            .into_iter()
            .take(MAX_PROPOSALS)
            .map(|cl| build_proposal(cl, candidates))
            .collect(),
        considered: candidates.len() + skipped_stacked,
        skipped_stacked,
        truncated,
        time_gap_secs: cfg.time_gap_secs,
        hamming_threshold: cfg.hamming_threshold,
    }
}

/// Turn one cluster into a reviewable proposal.
///
/// Split from the command so the reason text and the disclosures are testable without a
/// Tauri `State` — the reason is a claim about why a frame won, and a claim that drifts
/// from the rule is worse than no claim.
fn build_proposal(
    cluster: &crate::burst::Cluster,
    candidates: &[crate::catalog::culling::StackCandidate],
) -> StackProposal {
    let of = |id: i64| candidates.iter().find(|s| s.id == id);
    let keeper_id = cluster.representative_id();
    let keeper = of(keeper_id);
    let keeper_hash = keeper.and_then(|k| k.phash);

    let members: Vec<ProposalFrame> = cluster
        .photo_ids
        .iter()
        .filter_map(|id| of(*id))
        .map(|s| ProposalFrame {
            photo_id: s.id,
            file_name: s.path.rsplit('/').next().unwrap_or(&s.path).to_string(),
            sharpness: s.sharpness,
            rating: s.rating,
            burst_flag: s.burst_flag.clone(),
            hamming_distance: match (keeper_hash, s.phash) {
                (Some(a), Some(b)) => Some(crate::phash::hamming_distance(a, b)),
                _ => None,
            },
            child_count: s.child_count,
            is_keeper: s.id == keeper_id,
        })
        .collect();

    let times: Vec<i64> = cluster.photo_ids.iter().filter_map(|id| of(*id)?.capture_ts).collect();
    let span_secs = match (times.iter().min(), times.iter().max()) {
        (Some(lo), Some(hi)) => hi - lo,
        _ => 0,
    };

    // `None` unless every frame is hashed: a maximum over a subset would understate how
    // far apart the group actually is.
    let max_distance = if members.iter().all(|m| m.hamming_distance.is_some()) {
        members.iter().filter_map(|m| m.hamming_distance).max()
    } else {
        None
    };

    StackProposal {
        keeper_id,
        reason: keeper_reason(&members),
        span_secs,
        max_distance,
        unscored: members.iter().filter(|m| m.sharpness.is_none()).count(),
        absorbed_children: members.iter().filter(|m| !m.is_keeper).map(|m| m.child_count).sum(),
        members,
    }
}

/// Why the keeper won, phrased in the terms `select_representative` actually used:
/// highest rating first, sharpness as the tiebreak, lowest id when neither separates them.
fn keeper_reason(members: &[ProposalFrame]) -> String {
    let Some(keeper) = members.iter().find(|m| m.is_keeper) else {
        return "no keeper".into();
    };
    let others = || members.iter().filter(|m| !m.is_keeper);

    if keeper.rating > 0 && others().all(|m| m.rating < keeper.rating) {
        return format!("highest rating ({}★)", keeper.rating);
    }
    if let Some(score) = keeper.sharpness {
        if others().all(|m| m.sharpness.is_none_or(|s| s < score)) {
            let rated = keeper.rating > 0 && others().any(|m| m.rating == keeper.rating);
            return if rated {
                format!("sharpest of the {}★ frames ({score:.0})", keeper.rating)
            } else {
                format!("sharpest frame ({score:.0})")
            };
        }
    }
    if members.iter().all(|m| m.sharpness.is_none()) {
        return "first frame — none of these are scored yet".into();
    }
    "first frame — nothing separates them".into()
}

/// Stack `member_ids` under `keeper_id` (C3), accepting one proposal.
///
/// Rejects a keeper that is itself stacked under something: `set_stack_parent` would
/// happily create a two-deep stack that the inspector's one-level Stack section cannot
/// show, hiding the frames instead of grouping them.
#[tauri::command]
pub async fn apply_stack_proposal(
    state: State<'_, AppState>,
    keeper_id: i64,
    member_ids: Vec<i64>,
) -> Result<StackApplied, String> {
    with_catalog_blocking(&state, move |c| apply_stack_in_catalog(c, keeper_id, &member_ids)).await
}

/// The whole acceptance, over a real catalog. Split from the command so the guard and the
/// transaction can be tested without a Tauri `State`.
fn apply_stack_in_catalog(
    c: &crate::catalog::Catalog,
    keeper_id: i64,
    member_ids: &[i64],
) -> crate::catalog::Result<StackApplied> {
    let keeper = c.get_photo(keeper_id)?;
    if keeper.stack_parent_id.is_some() {
        return Err(crate::catalog::CatalogError::Validation(
            "the keeper is itself stacked under another photo — unstack it first".into(),
        ));
    }

    let mut to_stack: Vec<i64> = member_ids.iter().copied().filter(|id| *id != keeper_id).collect();
    to_stack.sort_unstable();
    to_stack.dedup();

    // One transaction for the whole group: a half-applied proposal would leave some frames
    // collapsed under a keeper and the rest loose in the grid, which is neither the state
    // the user asked for nor the one they had.
    let tx = c.conn().unchecked_transaction()?;
    let mut absorbed = 0usize;
    for id in &to_stack {
        // Counted before the move: afterwards these rows point at the keeper and are
        // indistinguishable from frames stacked directly.
        absorbed += c.list_stack_children(*id)?.len();
        c.set_stack_parent(*id, keeper_id)?;
    }
    tx.commit()?;

    Ok(StackApplied { stacked: to_stack.len(), absorbed })
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

// ── C3 — proposals ───────────────────────────────────────────────────────────

#[cfg(test)]
mod proposal_tests {
    use super::*;
    use crate::catalog::culling::StackCandidate;
    use crate::catalog::Catalog;

    fn candidate(id: i64, ts: i64, rating: i64, sharpness: Option<f64>) -> StackCandidate {
        StackCandidate {
            id,
            path: format!("2024/DSC_{id:04}.NEF"),
            capture_ts: Some(ts),
            phash: Some(0),
            rating,
            sharpness,
            burst_flag: None,
            child_count: 0,
        }
    }

    fn cfg() -> BurstConfig {
        BurstConfig { time_gap_secs: 15, hamming_threshold: 10 }
    }

    #[test]
    fn a_lone_frame_is_not_a_group_to_collapse() {
        // Two bursts and one photo shot an hour later. Only the bursts are proposals —
        // offering to "stack" a single photo would be an action with no effect.
        let candidates = vec![
            candidate(1, 0, 0, Some(100.0)),
            candidate(2, 2, 0, Some(90.0)),
            candidate(3, 5_000, 0, Some(80.0)),
            candidate(4, 9_000, 0, Some(70.0)),
            candidate(5, 9_002, 0, Some(60.0)),
        ];

        let r = build_proposals(&candidates, 0, &cfg());

        assert_eq!(r.proposals.len(), 2);
        assert_eq!(r.considered, 5);
        assert!(!r.truncated);
        assert!(
            r.proposals.iter().all(|p| p.members.len() == 2),
            "the loner is in no proposal at all"
        );
    }

    #[test]
    fn the_keeper_is_the_engines_representative_and_the_reason_says_why() {
        // Rating decides first, so the 3-star frame keeps even though another is sharper.
        let candidates = vec![
            candidate(1, 0, 0, Some(500.0)),
            candidate(2, 1, 3, Some(100.0)),
            candidate(3, 2, 0, Some(300.0)),
        ];

        let r = build_proposals(&candidates, 0, &cfg());
        let p = &r.proposals[0];

        assert_eq!(p.keeper_id, 2);
        assert_eq!(p.reason, "highest rating (3★)");
        assert!(p.members.iter().find(|m| m.photo_id == 2).unwrap().is_keeper);
        assert_eq!(p.span_secs, 2);
    }

    #[test]
    fn sharpness_decides_when_no_rating_does() {
        let candidates = vec![
            candidate(1, 0, 0, Some(100.0)),
            candidate(2, 1, 0, Some(320.0)),
            candidate(3, 2, 0, Some(90.0)),
        ];

        let r = build_proposals(&candidates, 0, &cfg());

        assert_eq!(r.proposals[0].keeper_id, 2);
        assert_eq!(r.proposals[0].reason, "sharpest frame (320)");
    }

    #[test]
    fn an_unscored_group_says_it_had_nothing_to_choose_by() {
        // The engine still picks a keeper, but claiming it is "the sharpest" would be a
        // fabrication — nothing in this group has been scored.
        let candidates = vec![candidate(1, 0, 0, None), candidate(2, 1, 0, None)];

        let r = build_proposals(&candidates, 0, &cfg());
        let p = &r.proposals[0];

        assert_eq!(p.reason, "first frame — none of these are scored yet");
        assert_eq!(p.unscored, 2, "and the count says how much was unweighed");
    }

    #[test]
    fn absorbed_children_are_counted_before_they_are_moved() {
        // A member with its own stacked JPEG: accepting re-homes that JPEG onto the
        // keeper, which is a consequence the reviewer has to be able to see first.
        let mut candidates = vec![candidate(1, 0, 0, Some(300.0)), candidate(2, 1, 0, Some(90.0))];
        candidates[1].child_count = 2;

        let r = build_proposals(&candidates, 0, &cfg());

        assert_eq!(r.proposals[0].keeper_id, 1);
        assert_eq!(r.proposals[0].absorbed_children, 2);
    }

    #[test]
    fn the_keepers_own_children_are_not_counted_as_absorbed() {
        // They are already under the keeper; nothing moves.
        let mut candidates = vec![candidate(1, 0, 0, Some(300.0)), candidate(2, 1, 0, Some(90.0))];
        candidates[0].child_count = 3;

        let r = build_proposals(&candidates, 0, &cfg());

        assert_eq!(r.proposals[0].keeper_id, 1);
        assert_eq!(r.proposals[0].absorbed_children, 0);
    }

    #[test]
    fn an_unhashed_frame_withholds_the_group_distance() {
        // A maximum over the hashed subset would understate how far apart the group is.
        let mut candidates = vec![candidate(1, 0, 0, Some(300.0)), candidate(2, 1, 0, Some(90.0))];
        candidates[1].phash = None;

        let r = build_proposals(&candidates, 0, &cfg());

        assert_eq!(r.proposals[0].max_distance, None);
        assert_eq!(
            r.proposals[0].members.iter().find(|m| m.photo_id == 2).unwrap().hamming_distance,
            None
        );
    }

    #[test]
    fn skipped_stacked_photos_are_reported_not_silently_dropped() {
        // `stack_candidates` leaves out photos already stacked under something; the count
        // has to survive to the UI or the pass looks like it examined everything.
        let candidates = vec![candidate(1, 0, 0, Some(100.0)), candidate(2, 1, 0, Some(90.0))];

        let r = build_proposals(&candidates, 7, &cfg());

        assert_eq!(r.skipped_stacked, 7);
        assert_eq!(r.considered, 9, "considered counts the skipped ones too");
    }

    #[test]
    fn more_groups_than_the_cap_are_capped_and_flagged() {
        // One two-frame burst per hour, more than the cap allows.
        let mut candidates = Vec::new();
        for g in 0..(MAX_PROPOSALS as i64 + 5) {
            candidates.push(candidate(g * 2 + 1, g * 3600, 0, Some(100.0)));
            candidates.push(candidate(g * 2 + 2, g * 3600 + 1, 0, Some(90.0)));
        }

        let r = build_proposals(&candidates, 0, &cfg());

        assert_eq!(r.proposals.len(), MAX_PROPOSALS);
        assert!(r.truncated, "the reviewer must know more groups are waiting");
    }

    // ── Applying a proposal ──────────────────────────────────────────────────

    fn catalog() -> (Catalog, crate::test_support::TestSubPath) {
        let dir = crate::test_support::TestTmpDir::new("stack-apply");
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let catalog = Catalog::open(&dir.join("test.chairphoto"), &root).unwrap();
        (catalog, dir.into_subpath("photos"))
    }

    fn insert(c: &Catalog, id: i64) {
        c.conn()
            .execute(
                "INSERT INTO photos(id, uuid, path, mtime_ns, size, extension, created_at,
                                    updated_at)
                 VALUES(?1, ?2, ?3, 0, 0, 'nef', 0, 0)",
                rusqlite::params![id, format!("uuid-{id}"), format!("DSC_{id:04}.NEF")],
            )
            .unwrap();
    }

    fn parent_of(c: &Catalog, id: i64) -> Option<i64> {
        c.conn()
            .query_row("SELECT stack_parent_id FROM photos WHERE id = ?1", [id], |r| {
                r.get::<_, Option<i64>>(0)
            })
            .unwrap()
    }

    #[test]
    fn accepting_moves_every_member_under_the_keeper() {
        let (c, _root) = catalog();
        for id in 1..=4 {
            insert(&c, id);
        }

        let r = apply_stack_in_catalog(&c, 1, &[1, 2, 3, 4]).unwrap();

        assert_eq!(r.stacked, 3, "the keeper is not stacked under itself");
        assert_eq!(parent_of(&c, 1), None, "and stays a top-level photo");
        for id in 2..=4 {
            assert_eq!(parent_of(&c, id), Some(1));
        }
    }

    #[test]
    fn a_members_own_stack_is_re_homed_onto_the_keeper_not_hidden_two_deep() {
        // Photo 3 is the camera JPEG under RAW 2. Stacking 2 under keeper 1 must not leave
        // 3 dangling below a stacked photo, where the one-level Stack section cannot show
        // it. `set_stack_parent` flattens; the report says how many moved.
        let (c, _root) = catalog();
        for id in 1..=3 {
            insert(&c, id);
        }
        c.set_stack_parent(3, 2).unwrap();

        let r = apply_stack_in_catalog(&c, 1, &[2]).unwrap();

        assert_eq!(r.stacked, 1);
        assert_eq!(r.absorbed, 1);
        assert_eq!(parent_of(&c, 3), Some(1), "the JPEG follows its RAW to the keeper");
        assert_eq!(c.list_stack_children(1).unwrap().len(), 2);
    }

    #[test]
    fn a_keeper_that_is_itself_stacked_is_refused() {
        // Accepting would build a two-deep stack the inspector cannot render, hiding the
        // frames rather than grouping them.
        let (c, _root) = catalog();
        for id in 1..=3 {
            insert(&c, id);
        }
        c.set_stack_parent(2, 1).unwrap();

        let err = apply_stack_in_catalog(&c, 2, &[3]).unwrap_err();

        assert!(format!("{err}").contains("unstack it first"), "{err}");
        assert_eq!(parent_of(&c, 3), None, "and nothing moved");
    }

    #[test]
    fn accepting_the_same_proposal_twice_changes_nothing_further() {
        let (c, _root) = catalog();
        for id in 1..=3 {
            insert(&c, id);
        }

        apply_stack_in_catalog(&c, 1, &[2, 3]).unwrap();
        let again = apply_stack_in_catalog(&c, 1, &[2, 3]).unwrap();

        assert_eq!(again.stacked, 2, "it reports what it set");
        assert_eq!(again.absorbed, 0, "but nothing was re-homed the second time");
        assert_eq!(c.list_stack_children(1).unwrap().len(), 2, "and the stack is unchanged");
    }

    #[test]
    fn a_duplicated_member_id_is_stacked_once() {
        let (c, _root) = catalog();
        for id in 1..=2 {
            insert(&c, id);
        }

        let r = apply_stack_in_catalog(&c, 1, &[2, 2, 2]).unwrap();

        assert_eq!(r.stacked, 1);
    }
}
