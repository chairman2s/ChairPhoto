// Kelvin white balance in the Darkroom (slice 9): pure helpers, the proof sheet's warm and
// cool cells, the duel's warmth round, and the History's names for Kelvin changes.
import { describe, expect, it } from "vitest";
import {
  KELVIN_MAX,
  KELVIN_MIN,
  kelvinToSlider,
  kelvinWb,
  miredShift,
  SLIDER_STEPS,
  sliderToKelvin,
  wbShown,
  withKelvinShift,
  type KelvinContext,
} from "../kelvin";
import { duelPair, proofSpread } from "../spreads";
import { describeChange } from "../history";
import { parseEdit } from "../../../modules/editing";

const ctx: KelvinContext = { asShot: { kelvin: 5313, tint: 2.4 }, prefer: "kelvin" };

describe("the Kelvin slider", () => {
  it("spans the range logarithmically and round-trips to 50 K", () => {
    expect(kelvinToSlider(KELVIN_MIN)).toBe(0);
    expect(kelvinToSlider(KELVIN_MAX)).toBe(SLIDER_STEPS);
    for (const k of [2500, 3200, 5200, 6500, 9000]) {
      expect(Math.abs(sliderToKelvin(kelvinToSlider(k)) - k)).toBeLessThanOrEqual(50);
    }
    // Log: the midpoint is the geometric mean, not the arithmetic one.
    expect(sliderToKelvin(SLIDER_STEPS / 2)).toBe(Math.round(Math.sqrt(KELVIN_MIN * KELVIN_MAX) / 50) * 50);
  });

  it("a negative mired shift is a higher Kelvin (a warmer picture), clamped to range", () => {
    expect(miredShift(5000, -30)).toBeGreaterThan(5000);
    expect(miredShift(5000, 30)).toBeLessThan(5000);
    expect(miredShift(11800, -100)).toBe(KELVIN_MAX);
  });
});

describe("which white balance the rail shows", () => {
  it("a Kelvin record shows its own light", () => {
    expect(wbShown(kelvinWb(4200, -3), ctx)).toEqual({ mode: "kelvin", kelvin: 4200, tint: -3 });
    expect(wbShown(kelvinWb(4200, -3), null)).toEqual({ mode: "kelvin", kelvin: 4200, tint: -3 });
  });

  it("an untouched white balance shows as-shot Kelvin when preferred, relative otherwise", () => {
    expect(wbShown({ temp: 0, tint: 0 }, ctx)).toEqual({ mode: "kelvin", kelvin: 5313, tint: 2.4 });
    expect(wbShown(undefined, ctx)).toEqual({ mode: "kelvin", kelvin: 5313, tint: 2.4 });
    expect(wbShown({ temp: 0, tint: 0 }, { ...ctx, prefer: "relative" })).toEqual({ mode: "relative" });
    expect(wbShown({ temp: 0, tint: 0 }, null)).toEqual({ mode: "relative" });
  });

  it("a relative edit, or an explicit relative choice, stays relative", () => {
    expect(wbShown({ temp: 0.3, tint: 0 }, ctx)).toEqual({ mode: "relative" });
    expect(wbShown({ temp: 0, tint: 0, mode: "relative" }, ctx)).toEqual({ mode: "relative" });
  });
});

describe("warmth in Kelvin on the RAW", () => {
  it("the proof sheet's warm cell states a higher Kelvin than as-shot, the cool one lower", () => {
    const cells = proofSpread({}, {}, [], ctx);
    const warm = cells.find((c) => c.label === "Auto · Warm")!.record.tone!.wb;
    const cool = cells.find((c) => c.label === "Auto · Cool")!.record.tone!.wb;
    expect(warm.mode).toBe("kelvin");
    expect(warm.kelvin!).toBeGreaterThan(5313);
    expect(cool.kelvin!).toBeLessThan(5313);
    expect(warm.tint).toBeCloseTo(2.4);
    // Without the context (engine 1, or no as-shot light) it stays the relative nudge.
    const rel = proofSpread({}, {}, []).find((c) => c.label === "Auto · Warm")!.record.tone!.wb;
    expect(rel.mode).toBeUndefined();
    expect(rel.temp).toBeGreaterThan(0);
  });

  it("the duel's warmth round steps stated light around what the photo shows", () => {
    const working = { tone: { wb: kelvinWb(6000, 0) } } as never;
    const [cooler, warmer] = duelPair(working, "warmth", 0, ctx);
    expect(cooler.tone!.wb.kelvin!).toBeLessThan(6000);
    expect(warmer.tone!.wb.kelvin!).toBeGreaterThan(6000);
    const [c2] = duelPair(working, "warmth", 1, ctx);
    expect(6000 - c2.tone!.wb.kelvin!).toBeLessThan(6000 - cooler.tone!.wb.kelvin!);
  });

  it("a shift keeps the rest of the record", () => {
    const r = withKelvinShift(parseEdit('{"engine":2,"fade":0.2,"tone":{"ev":0.5}}'), ctx, -30);
    expect(r.engine).toBe(2);
    expect(r.fade).toBe(0.2);
    expect(r.tone!.ev).toBe(0.5);
  });
});

describe("History names Kelvin changes", () => {
  it("the light, the tint, and back to as-shot", () => {
    const at = (wb: object) => parseEdit(JSON.stringify({ tone: { wb } }));
    expect(describeChange(at({ temp: 0, tint: 0 }), at(kelvinWb(4800, 0))).label).toBe("White balance 4800 K");
    expect(describeChange(at(kelvinWb(4800, 0)), at(kelvinWb(4800, 6))).label).toBe("Tint +6");
    expect(describeChange(at(kelvinWb(4800, 6)), at({ temp: 0, tint: 0 })).label).toBe("White balance as shot");
    expect(describeChange(at({ temp: 0, tint: 0 }), at({ temp: 0.3, tint: 0 })).label).toBe("Temperature +0.30");
  });
});
