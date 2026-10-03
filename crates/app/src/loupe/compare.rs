//! Compare's state (App.tsx's `compareIds`/`compareStart`/`compareMode`/`duelState`/
//! `compareFocusId` and the callbacks around them), as plain data with no GPUI: two to four
//! frames side by side, so the choice between them is made on pixels.
//!
//! - **A frozen pool.** The selection when Compare opened, in selection order. Culling inside
//!   Compare rejects frames, which a live selection or filter would drop out from under the
//!   panes; the pool keeps them. The panes look their rows up in the current result, so a
//!   rating shows on its own pane, and a row that vanished is left out.
//! - **Grid mode** shows [`MAX_PANES`] at a time, paged; **duel mode** is the champion
//!   (left) against the next challenger (right), one verdict per frame (`compare_duel`).
//! - **Culling keys act on the focused pane**, never on the whole selection.
//! - **Writes** are what the caller performs ([`Verdict::writes`]): pick and reject marks
//!   through `ShellState`'s culling queue, bound to the catalog the pool was read from
//!   (`ShellState` keeps that identity next to the session).
//! - **A verdict moves on only once written.** [`CompareSession::verdict`] and
//!   [`CompareSession::keep`] leave the session where it is and hold further verdicts until
//!   the caller [`settle`](CompareSession::settle)s that one: written, the duel advances (or
//!   the grid pages on); failed, it stays on the same pair (React awaited `applyMark` before
//!   it moved the duel on).
//! - **A verdict belongs to its session.** Each open and each mode switch starts a new
//!   epoch, and a verdict carries the one it was decided in. Settling it on a later session
//!   (Compare closed and reopened, or Duel→Grid→Duel, while the write was in flight) changes
//!   nothing — not the round, not that session's own pending verdict.

use chairphoto_core::catalog::PickState;
use chairphoto_model::compare_duel::{advance_duel, duel_round, duel_total_rounds, initial_duel, DuelSide, DuelState};
use std::sync::atomic::{AtomicU64, Ordering};

/// Hands out session epochs, unique for the process: two sessions never share one, whatever
/// opened them.
static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);

fn next_epoch() -> u64 {
    NEXT_EPOCH.fetch_add(1, Ordering::Relaxed)
}

/// Most panes at once: beyond this each frame is too small to judge.
pub const MAX_PANES: usize = 4;

/// The per-machine preference key for the mode (React's `localStorage` `panel.compareMode`).
pub const MODE_PREF: &str = "panel.compareMode";

/// How a pool larger than one screen is worked through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareMode {
    /// Batches of up to [`MAX_PANES`] side by side.
    Grid,
    /// Champion against challenger, two at a time. The default: one ←/→ verdict per frame.
    Duel,
}

impl CompareMode {
    pub fn from_pref(v: Option<&str>) -> Self {
        if v == Some("grid") {
            CompareMode::Grid
        } else {
            CompareMode::Duel
        }
    }

    pub fn pref(self) -> &'static str {
        match self {
            CompareMode::Grid => "grid",
            CompareMode::Duel => "duel",
        }
    }
}

/// The marks one Compare decision writes, in order, and where the session goes once they
/// are written ([`CompareSession::settle`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub writes: Vec<(i64, PickState)>,
    /// The epoch of the session (and mode) it was decided in.
    epoch: u64,
    next: Advance,
}

/// Where a settled verdict takes the session, from the state it was decided in. A verdict
/// from another epoch never gets this far ([`CompareSession::settle`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Advance {
    Duel { from: DuelState, to: DuelState },
    Grid { from_start: usize, keeper: i64 },
}

/// An open Compare. See the module docs.
#[derive(Debug, Clone, PartialEq)]
pub struct CompareSession {
    pool: Vec<i64>,
    /// Index of the first frame of the grid batch.
    start: usize,
    mode: CompareMode,
    duel: DuelState,
    focus: Option<i64>,
    /// A verdict is being written: no other is decided until it settles.
    pending: bool,
    /// This session in this mode; see the module docs.
    epoch: u64,
}

impl CompareSession {
    /// Compare the selection `ids` (two or more), opening on the batch that holds `active`.
    /// A duel starts at the top of the pool with the challenger focused — the frame being
    /// judged; the grid focuses the active photo (else the batch's first).
    pub fn open(ids: &[i64], active: Option<i64>, mode: CompareMode) -> Option<Self> {
        if ids.len() < 2 {
            return None;
        }
        let active_at = active.and_then(|a| ids.iter().position(|&i| i == a));
        let start = active_at.map_or(0, |i| i / MAX_PANES * MAX_PANES);
        let focus = match mode {
            CompareMode::Duel => ids[1],
            CompareMode::Grid => active_at.map_or(ids[start], |i| ids[i]),
        };
        Some(CompareSession {
            pool: ids.to_vec(),
            start,
            mode,
            duel: initial_duel(),
            focus: Some(focus),
            pending: false,
            epoch: next_epoch(),
        })
    }

    pub fn pool(&self) -> &[i64] {
        &self.pool
    }

    pub fn start(&self) -> usize {
        self.start
    }

    pub fn mode(&self) -> CompareMode {
        self.mode
    }

    pub fn duel(&self) -> DuelState {
        self.duel
    }

    /// "Duel r of n".
    pub fn duel_progress(&self) -> (usize, usize) {
        (duel_round(&self.duel), duel_total_rounds(self.pool.len()))
    }

    /// The ids on screen this round: the grid batch, or champion and challenger (the
    /// champion alone once the duel is done).
    pub fn batch(&self) -> Vec<i64> {
        match self.mode {
            CompareMode::Duel if self.duel.done => self.pool.get(self.duel.champion_idx).copied().into_iter().collect(),
            CompareMode::Duel => [self.duel.champion_idx, self.duel.challenger_idx]
                .iter()
                .filter_map(|&i| self.pool.get(i).copied())
                .collect(),
            CompareMode::Grid => self.pool.iter().skip(self.start).take(MAX_PANES).copied().collect(),
        }
    }

    /// The duel's reigning champion (`None` in grid mode).
    pub fn champion(&self) -> Option<i64> {
        (self.mode == CompareMode::Duel).then(|| self.pool.get(self.duel.champion_idx).copied()).flatten()
    }

    /// The pane the culling keys act on, among `on_screen` (the batch's ids whose rows still
    /// exist): the focus when it is there, else the first pane.
    pub fn focused(&self, on_screen: &[i64]) -> Option<i64> {
        match self.focus {
            Some(f) if on_screen.contains(&f) => Some(f),
            _ => on_screen.first().copied(),
        }
    }

    /// The raw focus (a pane pressed, an arrow moved it).
    pub fn focus(&self) -> Option<i64> {
        self.focus
    }

    /// A mouse press in a pane focuses it.
    pub fn set_focus(&mut self, id: i64) {
        if self.batch().contains(&id) {
            self.focus = Some(id);
        }
    }

    /// Cycle the focus through `on_screen` by `delta`, wrapping (←/↑ and →/↓ in the grid).
    pub fn cycle_focus(&mut self, on_screen: &[i64], delta: isize) {
        if on_screen.is_empty() {
            return;
        }
        let n = on_screen.len() as isize;
        let at = self.focus.and_then(|f| on_screen.iter().position(|&i| i == f)).unwrap_or(0) as isize;
        self.focus = Some(on_screen[(at + delta).rem_euclid(n) as usize]);
    }

    /// Swap presentation: the duel restarts at the top of the pool (its bookkeeping means
    /// nothing across modes); the grid keeps its page.
    pub fn switch_mode(&mut self, mode: CompareMode) {
        self.mode = mode;
        // A verdict still being written was decided in the old mode: a new epoch makes
        // settling it a no-op, and this mode may decide its own meanwhile.
        self.pending = false;
        self.epoch = next_epoch();
        self.duel = initial_duel();
        self.focus = match mode {
            CompareMode::Duel => self.pool.get(1).or(self.pool.first()).copied(),
            CompareMode::Grid => self.pool.get(self.start).copied(),
        };
    }

    /// One grid batch on (`dir` = 1) or back (−1), clamped at the pool's ends; the focus
    /// lands on the new batch's first frame. Returns whether it moved. Grid mode only.
    pub fn page(&mut self, dir: isize) -> bool {
        if self.mode != CompareMode::Grid {
            return false;
        }
        let next = self.start as isize + dir * MAX_PANES as isize;
        if next < 0 || next as usize >= self.pool.len() {
            return false;
        }
        self.start = next as usize;
        self.focus = Some(self.pool[self.start]);
        true
    }

    /// Whether a verdict is being written.
    pub fn pending(&self) -> bool {
        self.pending
    }

    /// One duel verdict: the loser is rejected; on the last round the winner is picked. Once
    /// [settled](Self::settle) as written, the focus moves to the next challenger (the winner
    /// once done). `None` when there is nothing to decide (grid mode, a finished duel, a
    /// verdict still being written).
    pub fn verdict(&mut self, side: DuelSide) -> Option<Verdict> {
        if self.mode != CompareMode::Duel || self.pending {
            return None;
        }
        let result = advance_duel(&self.pool, &self.duel, side)?;
        let mut writes = vec![(result.loser_id, PickState::Reject)];
        if let Some(winner) = result.winner_id {
            writes.push((winner, PickState::Pick));
        }
        self.pending = true;
        Some(Verdict { writes, epoch: self.epoch, next: Advance::Duel { from: self.duel, to: result.next } })
    }

    /// The writes of `verdict` landed (`written`) or failed. Written, the session moves on —
    /// unless it has moved since the verdict was decided; failed, it stays where it was.
    /// Either way the next verdict may be decided. A verdict from another session or mode
    /// (another epoch) changes nothing: that session's pending verdict is its own.
    pub fn settle(&mut self, verdict: &Verdict, written: bool) {
        if verdict.epoch != self.epoch {
            return;
        }
        self.pending = false;
        if !written {
            return;
        }
        match verdict.next {
            Advance::Duel { from, to } if self.mode == CompareMode::Duel && self.duel == from => {
                self.duel = to;
                let focus_idx = if to.done { to.champion_idx } else { to.challenger_idx };
                self.focus = self.pool.get(focus_idx).copied();
            }
            Advance::Grid { from_start, keeper } if self.mode == CompareMode::Grid && self.start == from_start => {
                self.focus = Some(keeper);
                self.page(1);
            }
            _ => {}
        }
    }

    /// "Keep this" / K on `keeper`. In a duel it is the verdict for that pane's side. In the
    /// grid the keeper is picked and its on-screen rivals rejected — the batch, never the
    /// whole pool — then, once [settled](Self::settle) as written, the next batch comes up.
    pub fn keep(&mut self, keeper: i64) -> Option<Verdict> {
        if self.mode == CompareMode::Duel {
            let side = if Some(keeper) == self.champion() { DuelSide::Left } else { DuelSide::Right };
            return self.verdict(side);
        }
        let batch = self.batch();
        if self.pending || !batch.contains(&keeper) {
            return None;
        }
        let mut writes = vec![(keeper, PickState::Pick)];
        writes.extend(batch.iter().filter(|&&id| id != keeper).map(|&id| (id, PickState::Reject)));
        self.pending = true;
        Some(Verdict { writes, epoch: self.epoch, next: Advance::Grid { from_start: self.start, keeper } })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(ids: &[i64], active: Option<i64>) -> CompareSession {
        CompareSession::open(ids, active, CompareMode::Grid).unwrap()
    }

    #[test]
    fn compare_needs_two_frames_and_opens_on_the_active_batch() {
        assert!(CompareSession::open(&[1], Some(1), CompareMode::Grid).is_none());
        let ids: Vec<i64> = (1..=10).collect();
        let s = grid(&ids, Some(6));
        assert_eq!((s.start(), s.batch(), s.focus()), (4, vec![5, 6, 7, 8], Some(6)));
        let s = grid(&ids, None);
        assert_eq!((s.start(), s.focus()), (0, Some(1)));
        let d = CompareSession::open(&ids, Some(6), CompareMode::Duel).unwrap();
        assert_eq!((d.batch(), d.focus(), d.champion()), (vec![1, 2], Some(2), Some(1)), "challenger focused");
    }

    #[test]
    fn grid_pages_by_four_and_clamps() {
        let ids: Vec<i64> = (1..=10).collect();
        let mut s = grid(&ids, None);
        assert!(!s.page(-1));
        assert!(s.page(1));
        assert_eq!((s.batch(), s.focus()), (vec![5, 6, 7, 8], Some(5)));
        assert!(s.page(1));
        assert_eq!(s.batch(), vec![9, 10]);
        assert!(!s.page(1), "past the end");
    }

    #[test]
    fn keep_in_the_grid_picks_the_keeper_rejects_only_the_batch_and_pages_on() {
        let ids: Vec<i64> = (1..=6).collect();
        let mut s = grid(&ids, None);
        let v = s.keep(3).unwrap();
        assert_eq!(
            v.writes,
            vec![(3, PickState::Pick), (1, PickState::Reject), (2, PickState::Reject), (4, PickState::Reject)]
        );
        s.settle(&v, true);
        assert_eq!((s.batch(), s.focus()), (vec![5, 6], Some(5)));
        assert!(s.keep(1).is_none(), "not on screen");
        // On the last batch keep stays put.
        let v = s.keep(6).unwrap();
        assert_eq!(v.writes, vec![(6, PickState::Pick), (5, PickState::Reject)]);
        s.settle(&v, true);
        assert_eq!((s.start(), s.focus()), (4, Some(6)));
    }

    #[test]
    fn a_duel_rejects_each_loser_and_picks_the_final_winner() {
        let mut s = CompareSession::open(&[1, 2, 3], None, CompareMode::Duel).unwrap();
        assert_eq!(s.duel_progress(), (1, 2));
        let v = s.verdict(DuelSide::Right).unwrap();
        assert_eq!(v.writes, vec![(1, PickState::Reject)]);
        s.settle(&v, true);
        assert_eq!((s.batch(), s.champion(), s.focus()), (vec![2, 3], Some(2), Some(3)));
        // K on the champion's pane is a left verdict.
        let v = s.keep(2).unwrap();
        assert_eq!(v.writes, vec![(3, PickState::Reject), (2, PickState::Pick)]);
        s.settle(&v, true);
        assert!(s.duel().done);
        assert_eq!((s.batch(), s.focus()), (vec![2], Some(2)));
        assert!(s.verdict(DuelSide::Left).is_none(), "done");
    }

    #[test]
    fn a_verdict_moves_on_only_once_written_and_holds_the_next_until_then() {
        let mut s = CompareSession::open(&[1, 2, 3], None, CompareMode::Duel).unwrap();
        let v = s.verdict(DuelSide::Right).unwrap();
        assert_eq!((s.batch(), s.focus()), (vec![1, 2], Some(2)), "not before the write lands");
        assert!(s.verdict(DuelSide::Left).is_none() && s.keep(1).is_none(), "one verdict at a time");
        s.settle(&v, false);
        assert_eq!((s.batch(), s.duel_progress()), (vec![1, 2], (1, 2)), "a failed write stays on the pair");
        let v = s.verdict(DuelSide::Right).unwrap();
        s.settle(&v, true);
        assert_eq!((s.batch(), s.focus()), (vec![2, 3], Some(3)));
        // A verdict decided before a mode switch changes nothing when it settles.
        let v = s.verdict(DuelSide::Left).unwrap();
        s.switch_mode(CompareMode::Grid);
        s.settle(&v, true);
        assert_eq!((s.mode(), s.start(), s.duel_progress().0), (CompareMode::Grid, 0, 1));
        // The grid pages on only once its Keep is written.
        let mut g = grid(&(1..=6).collect::<Vec<_>>(), None);
        let v = g.keep(2).unwrap();
        g.settle(&v, false);
        assert_eq!((g.start(), g.focus()), (0, Some(1)));
    }

    /// #205: a verdict settles only the session (and mode) it was decided in.
    #[test]
    fn a_verdict_settles_only_its_own_session() {
        // Closed and reopened on another set while the write was in flight.
        let mut old = CompareSession::open(&[1, 2, 3], None, CompareMode::Duel).unwrap();
        let v = old.verdict(DuelSide::Right).unwrap();
        let mut new = CompareSession::open(&[4, 5, 6], None, CompareMode::Duel).unwrap();
        let own = new.verdict(DuelSide::Left).unwrap();
        new.settle(&v, true);
        assert_eq!((new.batch(), new.duel_progress(), new.focus()), (vec![4, 5], (1, 2), Some(5)), "not advanced");
        assert!(new.pending(), "its own verdict is still being written");
        new.settle(&own, true);
        assert_eq!((new.batch(), new.pending()), (vec![4, 6], false));
        // Duel→Grid→Duel inside the write window: the restarted duel is on the same round
        // the verdict was decided in, and still ignores it.
        let mut s = CompareSession::open(&[1, 2, 3], None, CompareMode::Duel).unwrap();
        let v = s.verdict(DuelSide::Right).unwrap();
        s.switch_mode(CompareMode::Grid);
        s.switch_mode(CompareMode::Duel);
        let own = s.verdict(DuelSide::Left).unwrap();
        s.settle(&v, true);
        assert_eq!((s.batch(), s.duel_progress(), s.pending()), (vec![1, 2], (1, 2), true));
        s.settle(&own, true);
        assert_eq!((s.batch(), s.pending()), (vec![1, 3], false));
    }

    #[test]
    fn focus_cycles_and_follows_presses_only_on_screen() {
        let mut s = grid(&[1, 2, 3, 4, 5], None);
        s.cycle_focus(&[1, 2, 3, 4], -1);
        assert_eq!(s.focus(), Some(4));
        s.cycle_focus(&[1, 2, 3, 4], 1);
        assert_eq!(s.focus(), Some(1));
        s.set_focus(5);
        assert_eq!(s.focus(), Some(1), "5 is not on screen");
        // A vanished focused row: the keys act on the first pane.
        s.set_focus(3);
        assert_eq!(s.focused(&[1, 2, 4]), Some(1));
    }

    #[test]
    fn switching_mode_restarts_the_duel_and_keeps_the_grid_page() {
        let ids: Vec<i64> = (1..=6).collect();
        let mut s = grid(&ids, Some(6));
        s.switch_mode(CompareMode::Duel);
        assert_eq!((s.batch(), s.focus()), (vec![1, 2], Some(2)));
        let v = s.verdict(DuelSide::Left).unwrap();
        s.settle(&v, true);
        s.switch_mode(CompareMode::Grid);
        assert_eq!((s.start(), s.batch(), s.focus()), (4, vec![5, 6], Some(5)));
        assert!(!s.duel().done && s.duel_progress().0 == 1);
        assert_eq!(CompareMode::from_pref(Some("grid")), CompareMode::Grid);
        assert_eq!(CompareMode::from_pref(None), CompareMode::Duel);
    }
}
