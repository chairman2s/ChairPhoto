// The duel (champion/challenger) compare engine, pure so App's wiring stays thin and the
// tournament arithmetic is unit-testable. The pool is the frozen id list Compare opened
// with; the champion holds the left pane, challengers arrive on the right in pool order.
// Every verdict names a loser (rejected by the caller); the final verdict also names the
// overall winner (picked by the caller). N frames = N-1 verdicts.

export interface DuelState {
  /** Index of the reigning champion within the pool. */
  championIdx: number;
  /** Index of the current challenger, always > every index already judged. */
  challengerIdx: number;
  /** Set once the last challenger has been judged; the champion is the winner. */
  done: boolean;
}

export interface DuelVerdictResult {
  next: DuelState;
  /** The frame the verdict eliminated — the caller rejects it. */
  loserId: number;
  /** Set only on the final verdict: the tournament's winner — the caller picks it. */
  winnerId: number | null;
}

export function initialDuel(): DuelState {
  return { championIdx: 0, challengerIdx: 1, done: false };
}

/** Round counter for the bar: "Duel <round> of <totalRounds(pool)>". */
export function duelRound(state: DuelState): number {
  return state.challengerIdx;
}

export function duelTotalRounds(poolLength: number): number {
  return Math.max(poolLength - 1, 0);
}

/**
 * Apply one verdict. "left" keeps the champion; "right" crowns the challenger (it moves
 * to the left slot). Returns the new state plus which id lost (reject it) and — on the
 * last round — which id won the whole pool (pick it).
 */
export function advanceDuel(
  pool: number[],
  state: DuelState,
  winner: "left" | "right",
): DuelVerdictResult | null {
  if (state.done) return null;
  const championId = pool[state.championIdx];
  const challengerId = pool[state.challengerIdx];
  if (championId === undefined || challengerId === undefined) return null;

  const winnerIdx = winner === "left" ? state.championIdx : state.challengerIdx;
  const loserId = winner === "left" ? challengerId : championId;
  const nextChallenger = state.challengerIdx + 1;

  if (nextChallenger >= pool.length) {
    return {
      next: { championIdx: winnerIdx, challengerIdx: state.challengerIdx, done: true },
      loserId,
      winnerId: pool[winnerIdx] ?? null,
    };
  }
  return {
    next: { championIdx: winnerIdx, challengerIdx: nextChallenger, done: false },
    loserId,
    winnerId: null,
  };
}
