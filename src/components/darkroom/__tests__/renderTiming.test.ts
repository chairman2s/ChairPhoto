// Pure timing math shared by the WebGL probe and the darkroom's frame log.
import { describe, expect, it } from "vitest";
import { formatSample, p50p95, percentile, summarize, type FrameSample } from "../renderTiming";

describe("percentile", () => {
  it("is NaN for an empty list", () => {
    expect(percentile([], 50)).toBeNaN();
  });

  it("uses nearest rank on a sorted copy", () => {
    const xs = [10, 1, 5, 3, 8, 2, 9, 4, 7, 6];
    expect(percentile(xs, 50)).toBe(5);
    expect(percentile(xs, 95)).toBe(10);
    expect(percentile(xs, 0)).toBe(1);
    expect(percentile(xs, 100)).toBe(10);
  });

  it("never mutates its input", () => {
    const xs = [3, 1, 2];
    percentile(xs, 50);
    expect(xs).toEqual([3, 1, 2]);
  });

  it("handles a single sample", () => {
    expect(percentile([4.2], 95)).toBe(4.2);
  });
});

describe("p50p95", () => {
  it("rounds to a tenth of a millisecond", () => {
    expect(p50p95([1.04, 1.06, 1.08])).toEqual({ p50: 1.1, p95: 1.1 });
  });
});

describe("summarize", () => {
  const frames: FrameSample[] = [
    { seq: 1, tier: "fast", requested: 0, resolved: 40, painted: 50 },
    { seq: 2, tier: "fast", requested: 90, resolved: 130, superseded: true },
    { seq: 3, tier: "fast", requested: 180, resolved: 220, painted: 230 },
    { seq: 3, tier: "settled", requested: 430, resolved: 530, painted: 550 },
  ];

  it("splits ipc, paint and cadence and counts superseded frames", () => {
    const s = summarize(frames);
    expect(s.count).toBe(4);
    expect(s.superseded).toBe(1);
    expect(s.latencyMs).toEqual({ p50: 50, p95: 120 });
    expect(s.ipcMs).toEqual({ p50: 40, p95: 100 });
    expect(s.paintMs).toEqual({ p50: 10, p95: 20 });
    // painted at 50, 230, 550 → intervals 180 and 320
    expect(s.cadenceMs).toEqual({ p50: 180, p95: 320 });
    expect(s.byTier).toEqual({ fast: 3, settled: 1 });
  });

  it("measures request→paint for URL-served frames that never resolve over IPC", () => {
    const s = summarize([
      { seq: 1, tier: "fast", requested: 100, painted: 160 },
      { seq: 2, tier: "settled", requested: 400, painted: 500 },
    ]);
    expect(s.latencyMs).toEqual({ p50: 60, p95: 100 });
    expect(s.ipcMs.p50).toBeNaN();
    expect(s.paintMs.p50).toBeNaN();
    expect(s.cadenceMs).toEqual({ p50: 340, p95: 340 });
  });

  it("is all-NaN but well-formed for no samples", () => {
    const s = summarize([]);
    expect(s.count).toBe(0);
    expect(s.ipcMs.p50).toBeNaN();
    expect(s.cadenceMs.p95).toBeNaN();
  });
});

describe("formatSample", () => {
  it("prints the round trip and paint, and marks superseded frames", () => {
    expect(formatSample({ seq: 7, tier: "settled", requested: 0, resolved: 12.34, painted: 20 })).toBe(
      "[edit-timing] seq=7 tier=settled total=20.0ms ipc=12.3ms paint=7.7ms",
    );
    expect(formatSample({ seq: 8, tier: "fast", requested: 0, resolved: 5, superseded: true })).toBe(
      "[edit-timing] seq=8 tier=fast total=— ipc=5.0ms paint=— superseded",
    );
    expect(formatSample({ seq: 9, tier: "fast", requested: 10, painted: 52 })).toBe(
      "[edit-timing] seq=9 tier=fast total=42.0ms ipc=— paint=—",
    );
  });
});
