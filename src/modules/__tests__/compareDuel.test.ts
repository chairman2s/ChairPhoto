/**
 * The duel engine: N frames, N-1 verdicts, exactly one survivor. The properties worth
 * pinning are the tournament invariants — every verdict eliminates exactly one frame,
 * a "right" verdict crowns the challenger, and the final verdict names the winner —
 * because an off-by-one here silently rejects the keeper or skips a frame unjudged.
 */
import { describe, expect, it } from "vitest";

import {
  advanceDuel,
  duelRound,
  duelTotalRounds,
  initialDuel,
  type DuelState,
} from "../compareDuel";

const pool = [10, 20, 30, 40];

describe("compareDuel", () => {
  it("starts with the first two frames and round 1", () => {
    const s = initialDuel();
    expect(s).toEqual({ championIdx: 0, challengerIdx: 1, done: false });
    expect(duelRound(s)).toBe(1);
    expect(duelTotalRounds(pool.length)).toBe(3);
  });

  it("left verdict keeps the champion and eliminates the challenger", () => {
    const r = advanceDuel(pool, initialDuel(), "left");
    expect(r).not.toBeNull();
    expect(r!.loserId).toBe(20);
    expect(r!.winnerId).toBeNull();
    expect(r!.next).toEqual({ championIdx: 0, challengerIdx: 2, done: false });
  });

  it("right verdict crowns the challenger — it takes the left slot", () => {
    const r = advanceDuel(pool, initialDuel(), "right");
    expect(r!.loserId).toBe(10);
    expect(r!.next).toEqual({ championIdx: 1, challengerIdx: 2, done: false });
  });

  it("the final verdict names the winner and marks the duel done", () => {
    const last: DuelState = { championIdx: 1, challengerIdx: 3, done: false };
    const left = advanceDuel(pool, last, "left");
    expect(left!.winnerId).toBe(20);
    expect(left!.loserId).toBe(40);
    expect(left!.next.done).toBe(true);

    const right = advanceDuel(pool, last, "right");
    expect(right!.winnerId).toBe(40);
    expect(right!.loserId).toBe(20);
    expect(right!.next.done).toBe(true);
  });

  it("a full tournament judges every frame exactly once", () => {
    // 20 beats 10, 20 beats 30, 40 beats 20 — losers in verdict order, 40 the winner.
    let s = initialDuel();
    const losers: number[] = [];
    let winner: number | null = null;
    for (const verdict of ["right", "left", "right"] as const) {
      const r = advanceDuel(pool, s, verdict)!;
      losers.push(r.loserId);
      winner = r.winnerId;
      s = r.next;
    }
    expect(losers).toEqual([10, 30, 20]);
    expect(winner).toBe(40);
    expect(s.done).toBe(true);
    // Everyone was judged: losers + winner partition the pool.
    expect([...losers, winner].sort()).toEqual([...pool].sort());
  });

  it("a two-frame pool is a single verdict", () => {
    const r = advanceDuel([1, 2], initialDuel(), "left");
    expect(r!.winnerId).toBe(1);
    expect(r!.loserId).toBe(2);
    expect(r!.next.done).toBe(true);
  });

  it("verdicts after done, and out-of-range states, are inert", () => {
    expect(advanceDuel(pool, { championIdx: 0, challengerIdx: 1, done: true }, "left")).toBeNull();
    expect(advanceDuel(pool, { championIdx: 0, challengerIdx: 9, done: false }, "left")).toBeNull();
  });
});
