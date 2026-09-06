// Pure timing math shared by the WebGL probe and the darkroom's frame log.
import { describe, expect, it } from "vitest";
import { p50p95, percentile } from "../renderTiming";

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
