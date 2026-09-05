// Pure drag math of the Darkroom's tone strip (docs/plans/darkroom, Gate 3 test plan).
import { describe, expect, it } from "vitest";
import { applyZoneDrag, MAX_ZONE_EV, ZONE_COUNT } from "../ToneStrip";

describe("applyZoneDrag", () => {
  it("initializes a zeroed strip when no zones exist yet", () => {
    const z = applyZoneDrag(undefined, 2, 0.5);
    expect(z).toHaveLength(ZONE_COUNT);
    expect(z[2]).toBeCloseTo(0.5);
    expect(z.filter((_, i) => i !== 2).every((v) => v === 0)).toBe(true);
  });

  it("clamps to ±MAX_ZONE_EV", () => {
    expect(applyZoneDrag(undefined, 0, 99)[0]).toBe(MAX_ZONE_EV);
    expect(applyZoneDrag(undefined, 0, -99)[0]).toBe(-MAX_ZONE_EV);
  });

  it("never mutates its input", () => {
    const input = Array(ZONE_COUNT).fill(0.25) as number[];
    const out = applyZoneDrag(input, 3, 1);
    expect(input.every((v) => v === 0.25)).toBe(true);
    expect(out[3]).toBeCloseTo(1.25);
  });

  it("repairs a wrong-length array to a fresh strip", () => {
    const out = applyZoneDrag([1, 2], 1, 0.5);
    expect(out).toHaveLength(ZONE_COUNT);
    expect(out[1]).toBeCloseTo(0.5);
    expect(out[0]).toBe(0);
  });
});
