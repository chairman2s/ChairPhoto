// The shell-transition instrument (dev toggle): pure summary math, and the no-op contract
// when no transition is in flight.
import { describe, expect, it } from "vitest";
import { noteMark, noteTileLoaded, summarizeTransition, timedInvoke } from "../shellTiming";

describe("summarizeTransition", () => {
  it("reports every moment relative to Back, to a tenth of a millisecond", () => {
    const summary = summarizeTransition({
      from: "develop",
      start: 1000,
      commitAt: 1118.44,
      commitMs: 25.06,
      firstTileAt: 1198,
      lastTileAt: 3585,
      tilesMounted: 310,
      tilesLoaded: 66,
      loadBuckets: [56, 0, 0, 10],
      scrollAt: 1123,
      maxFrameGapMs: 2176.4,
      maxFrameGapAt: 3317,
      marks: { "grid-effects": 120 },
      slowInvokes: [{ cmd: "list_tags", startMs: 120, ms: 2240, rows: 1614 }],
      finished: true,
    });
    expect(summary).toEqual({
      from: "develop",
      toCommitMs: 118.4,
      commitMs: 25.1,
      toFirstTileMs: 198,
      toLastTileMs: 2585,
      tilesMounted: 310,
      tilesLoaded: 66,
      loadBuckets: [56, 0, 0, 10],
      toScrollMs: 123,
      maxFrameGapMs: 2176,
      maxFrameGapEndMs: 2317,
      marks: { "grid-effects": 120 },
      slowInvokes: [{ cmd: "list_tags", startMs: 120, ms: 2240, rows: 1614 }],
    });
  });

  it("leaves unknown moments null rather than inventing zeros", () => {
    const s = summarizeTransition({
      from: "develop",
      start: 0,
      tilesMounted: 0,
      tilesLoaded: 0,
      loadBuckets: [],
      maxFrameGapMs: 0,
      marks: {},
      slowInvokes: [],
      finished: true,
    });
    expect(s.toCommitMs).toBeNull();
    expect(s.toFirstTileMs).toBeNull();
    expect(s.maxFrameGapEndMs).toBeNull();
  });
});

describe("outside a transition", () => {
  it("timedInvoke is a passthrough and the note functions are no-ops", async () => {
    expect(await timedInvoke("x", () => Promise.resolve(42))).toBe(42);
    expect(() => {
      noteMark("anything");
      noteTileLoaded();
    }).not.toThrow();
  });
});
