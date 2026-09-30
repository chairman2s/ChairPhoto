//! The duel (champion/challenger) compare engine. Port of `src/modules/compareDuel.ts`.
//!
//! The pool is the frozen id list Compare opened with; the champion holds the left pane,
//! challengers arrive on the right in pool order. Every verdict names a loser (rejected by
//! the caller); the final verdict also names the overall winner (picked by the caller).
//! N frames = N-1 verdicts.
//!
//! Semantic choices against the TypeScript:
//! - Indices are `usize` and ids `i64` (the catalog's photo id type). JavaScript's
//!   `pool[i] === undefined` out-of-range check is `slice::get` returning `None`.
//! - `winnerId: number | null` is `Option<i64>`.

/// Where a duel stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DuelState {
    /// Index of the reigning champion within the pool.
    pub champion_idx: usize,
    /// Index of the current challenger, always > every index already judged.
    pub challenger_idx: usize,
    /// Set once the last challenger has been judged; the champion is the winner.
    pub done: bool,
}

/// Which pane won a verdict: `Left` keeps the champion, `Right` crowns the challenger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuelSide {
    Left,
    Right,
}

/// What one verdict did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DuelVerdictResult {
    pub next: DuelState,
    /// The frame the verdict eliminated — the caller rejects it.
    pub loser_id: i64,
    /// Set only on the final verdict: the tournament's winner — the caller picks it.
    pub winner_id: Option<i64>,
}

/// The first round: frame 0 against frame 1.
pub fn initial_duel() -> DuelState {
    DuelState { champion_idx: 0, challenger_idx: 1, done: false }
}

/// Round counter for the bar: "Duel <round> of <total_rounds(pool)>".
pub fn duel_round(state: &DuelState) -> usize {
    state.challenger_idx
}

/// How many verdicts a pool of `pool_length` frames takes (never negative).
pub fn duel_total_rounds(pool_length: usize) -> usize {
    pool_length.saturating_sub(1)
}

/// Apply one verdict. `Left` keeps the champion; `Right` crowns the challenger (it moves to
/// the left slot). Returns the new state plus which id lost (reject it) and — on the last
/// round — which id won the whole pool (pick it). `None` when the duel is already done or
/// the state points outside the pool: such verdicts are inert.
pub fn advance_duel(pool: &[i64], state: &DuelState, winner: DuelSide) -> Option<DuelVerdictResult> {
    if state.done {
        return None;
    }
    let champion_id = *pool.get(state.champion_idx)?;
    let challenger_id = *pool.get(state.challenger_idx)?;

    let (winner_idx, loser_id) = match winner {
        DuelSide::Left => (state.champion_idx, challenger_id),
        DuelSide::Right => (state.challenger_idx, champion_id),
    };
    let next_challenger = state.challenger_idx + 1;

    if next_challenger >= pool.len() {
        return Some(DuelVerdictResult {
            next: DuelState {
                champion_idx: winner_idx,
                challenger_idx: state.challenger_idx,
                done: true,
            },
            loser_id,
            winner_id: pool.get(winner_idx).copied(),
        });
    }
    Some(DuelVerdictResult {
        next: DuelState { champion_idx: winner_idx, challenger_idx: next_challenger, done: false },
        loser_id,
        winner_id: None,
    })
}

// Port of `src/modules/__tests__/compareDuel.test.ts` (7 cases, same names). The properties
// worth pinning are the tournament invariants — every verdict eliminates exactly one frame,
// a "right" verdict crowns the challenger, and the final verdict names the winner — because
// an off-by-one here silently rejects the keeper or skips a frame unjudged.
#[cfg(test)]
mod tests {
    use super::*;

    const POOL: [i64; 4] = [10, 20, 30, 40];

    #[test]
    fn starts_with_the_first_two_frames_and_round_1() {
        let s = initial_duel();
        assert_eq!(s, DuelState { champion_idx: 0, challenger_idx: 1, done: false });
        assert_eq!(duel_round(&s), 1);
        assert_eq!(duel_total_rounds(POOL.len()), 3);
    }

    #[test]
    fn left_verdict_keeps_the_champion_and_eliminates_the_challenger() {
        let r = advance_duel(&POOL, &initial_duel(), DuelSide::Left);
        assert!(r.is_some());
        let r = r.unwrap();
        assert_eq!(r.loser_id, 20);
        assert_eq!(r.winner_id, None);
        assert_eq!(r.next, DuelState { champion_idx: 0, challenger_idx: 2, done: false });
    }

    #[test]
    fn right_verdict_crowns_the_challenger_it_takes_the_left_slot() {
        let r = advance_duel(&POOL, &initial_duel(), DuelSide::Right).unwrap();
        assert_eq!(r.loser_id, 10);
        assert_eq!(r.next, DuelState { champion_idx: 1, challenger_idx: 2, done: false });
    }

    #[test]
    fn the_final_verdict_names_the_winner_and_marks_the_duel_done() {
        let last = DuelState { champion_idx: 1, challenger_idx: 3, done: false };
        let left = advance_duel(&POOL, &last, DuelSide::Left).unwrap();
        assert_eq!(left.winner_id, Some(20));
        assert_eq!(left.loser_id, 40);
        assert!(left.next.done);

        let right = advance_duel(&POOL, &last, DuelSide::Right).unwrap();
        assert_eq!(right.winner_id, Some(40));
        assert_eq!(right.loser_id, 20);
        assert!(right.next.done);
    }

    #[test]
    fn a_full_tournament_judges_every_frame_exactly_once() {
        // 20 beats 10, 20 beats 30, 40 beats 20 — losers in verdict order, 40 the winner.
        let mut s = initial_duel();
        let mut losers: Vec<i64> = Vec::new();
        let mut winner: Option<i64> = None;
        for verdict in [DuelSide::Right, DuelSide::Left, DuelSide::Right] {
            let r = advance_duel(&POOL, &s, verdict).unwrap();
            losers.push(r.loser_id);
            winner = r.winner_id;
            s = r.next;
        }
        assert_eq!(losers, vec![10, 30, 20]);
        assert_eq!(winner, Some(40));
        assert!(s.done);
        // Everyone was judged: losers + winner partition the pool.
        let mut all: Vec<i64> = losers.clone();
        all.push(winner.unwrap());
        all.sort();
        let mut pool = POOL.to_vec();
        pool.sort();
        assert_eq!(all, pool);
    }

    #[test]
    fn a_two_frame_pool_is_a_single_verdict() {
        let r = advance_duel(&[1, 2], &initial_duel(), DuelSide::Left).unwrap();
        assert_eq!(r.winner_id, Some(1));
        assert_eq!(r.loser_id, 2);
        assert!(r.next.done);
    }

    #[test]
    fn verdicts_after_done_and_out_of_range_states_are_inert() {
        let done = DuelState { champion_idx: 0, challenger_idx: 1, done: true };
        assert_eq!(advance_duel(&POOL, &done, DuelSide::Left), None);
        let out_of_range = DuelState { champion_idx: 0, challenger_idx: 9, done: false };
        assert_eq!(advance_duel(&POOL, &out_of_range, DuelSide::Left), None);
    }
}
