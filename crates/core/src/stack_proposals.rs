//! Auto-stack proposals (C3): burst clustering, phash similarity and timestamp proximity
//! said as one sentence — *these frames are one moment, and this is the keeper* — and the
//! acceptance of one such group.
//!
//! The three readings already existed separately; a proposal is the one sentence they add
//! up to. Accepting it collapses the group to a single tile through the stacking that
//! already ships, so nothing is deleted and the inspector's existing Unstack undoes it one
//! frame at a time.
//!
//! Moved here from the Tauri shell's `commands/culling.rs` (gpui #106), so the GPUI app's
//! "Stack bursts" dialog and the Tauri commands run the same code.

use crate::burst::{group_into_clusters, BurstConfig, BurstPhoto};
use crate::catalog::Catalog;
use serde::Serialize;

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

/// Propose stacks over `photo_ids` — typically the selection, or the whole view.
///
/// Read-only. Nothing is stacked until [`apply_stack_proposal`] is called for a group, one
/// group at a time, because collapsing frames out of the grid is exactly the kind of bulk
/// edit that should not happen as a side effect of asking a question.
pub fn propose_stacks(c: &Catalog, photo_ids: &[i64]) -> crate::catalog::Result<StackProposals> {
    let cfg = BurstConfig {
        time_gap_secs: setting_i64(c, "ai.burst_time_gap_secs", 15),
        hamming_threshold: setting_i64(c, "ai.burst_hamming_threshold", 10).max(0) as u32,
    };
    let (candidates, skipped_stacked) = crate::catalog::culling::stack_candidates(c.conn(), photo_ids)?;
    Ok(build_proposals(&candidates, skipped_stacked, &cfg))
}

fn setting_i64(c: &Catalog, key: &str, default: i64) -> i64 {
    c.get_setting(key).ok().flatten().and_then(|s| s.parse().ok()).unwrap_or(default)
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

/// Stack `member_ids` under `keeper_id`, accepting one proposal, in one transaction.
///
/// Rejects a keeper that is itself stacked under something: `set_stack_parent` would
/// happily create a two-deep stack that the inspector's one-level Stack section cannot
/// show, hiding the frames instead of grouping them.
pub fn apply_stack_proposal(
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

        let r = apply_stack_proposal(&c, 1, &[1, 2, 3, 4]).unwrap();

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

        let r = apply_stack_proposal(&c, 1, &[2]).unwrap();

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

        let err = apply_stack_proposal(&c, 2, &[3]).unwrap_err();

        assert!(format!("{err}").contains("unstack it first"), "{err}");
        assert_eq!(parent_of(&c, 3), None, "and nothing moved");
    }

    #[test]
    fn accepting_the_same_proposal_twice_changes_nothing_further() {
        let (c, _root) = catalog();
        for id in 1..=3 {
            insert(&c, id);
        }

        apply_stack_proposal(&c, 1, &[2, 3]).unwrap();
        let again = apply_stack_proposal(&c, 1, &[2, 3]).unwrap();

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

        let r = apply_stack_proposal(&c, 1, &[2, 2, 2]).unwrap();

        assert_eq!(r.stacked, 1);
    }
}
