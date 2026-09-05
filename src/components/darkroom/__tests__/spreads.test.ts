// Proof-spread record maths (docs/plans/darkroom, Gate 3 test plan).
import { describe, expect, it } from "vitest";
import type { DevelopPreset } from "../../../modules/presets";
import type { VersionEdit } from "../../../modules/editing";
import { DUEL_DIMS, duelPair, PROOF_CELLS, proofSpread } from "../spreads";

const preset = (id: string, category: DevelopPreset["category"], edit: DevelopPreset["edit"]): DevelopPreset => ({
  id,
  name: id,
  category,
  edit,
  builtin: true,
});

const MANY_PRESETS: DevelopPreset[] = [
  preset("portra", "Film", { fade: 0.1, tone: { contrast: 0.05 } as VersionEdit["tone"] }),
  preset("velvia", "Film", { tone: { saturation: 0.4 } as VersionEdit["tone"] }),
  preset("gold", "Film", {}),
  preset("teal", "Color", { split: { shadow_hue: 180, shadow_sat: 0.2, highlight_hue: 40, highlight_sat: 0.1, balance: 0 } }),
  preset("sunset", "Color", {}),
  preset("bw-red", "Monochrome", { bw: { enabled: true, r: 0.9, g: 0.15, b: -0.05 }, tone: { contrast: 0.2 } as VersionEdit["tone"] }),
  preset("bw-soft", "Monochrome", {}),
  preset("sepia", "Monochrome", {}),
  preset("mine", "User", { vignette: -0.2 }),
  preset("extra1", "Film", {}),
  preset("extra2", "Color", {}),
  preset("extra3", "Monochrome", {}),
];

const AUTO: VersionEdit = { tone: { ev: 0.5, contrast: 0.1 } as VersionEdit["tone"] };

describe("proofSpread", () => {
  it("always deals the current state first, exactly once", () => {
    const spread = proofSpread({}, AUTO, MANY_PRESETS);
    expect(spread[0].group).toBe("asShot");
    expect(spread[0].label).toBe("As shot");
    expect(spread.filter((c) => c.group === "asShot")).toHaveLength(1);
    const edited = proofSpread({ tone: { ev: 1 } as VersionEdit["tone"] }, AUTO, MANY_PRESETS);
    expect(edited[0].label).toBe("Current");
  });

  it("caps the sheet at PROOF_CELLS", () => {
    expect(proofSpread({}, AUTO, MANY_PRESETS)).toHaveLength(PROOF_CELLS);
  });

  it("copies the base's framing into every candidate untouched", () => {
    const base: VersionEdit = {
      crop: { x: 0.1, y: 0.2, w: 0.5, h: 0.5, aspect: "1:1" },
      straighten: 1.5,
      perspective: { tl: [0, 0], tr: [1, 0], br: [1, 1], bl: [0, 1] },
    };
    for (const c of proofSpread(base, AUTO, MANY_PRESETS)) {
      expect(c.record.crop).toEqual(base.crop);
      expect(c.record.straighten).toBe(1.5);
      expect(c.record.perspective).toEqual(base.perspective);
    }
  });

  it("keeps the base look and zones on Auto cells, drops them on look cells", () => {
    const base: VersionEdit = { zones: [0, 0.5, 0, 0, 0, 0, 0, 0], fade: 0.3 };
    const spread = proofSpread(base, AUTO, MANY_PRESETS);
    const auto = spread.find((c) => c.label === "Auto")!;
    expect(auto.record.zones).toEqual(base.zones);
    expect(auto.record.fade).toBe(0.3);
    const look = spread.find((c) => c.label === "portra")!;
    expect(look.record.zones).toBeUndefined();
    expect(look.record.fade).toBe(0.1); // the preset's, not the base's
  });

  it("applies the auto fragment, with a preset's own tone keys winning", () => {
    const spread = proofSpread({}, AUTO, MANY_PRESETS);
    const auto = spread.find((c) => c.label === "Auto")!;
    expect(auto.record.tone?.ev).toBe(0.5);
    const bwRed = spread.find((c) => c.label === "bw-red")!;
    expect(bwRed.record.tone?.ev).toBe(0.5); // auto's exposure carries in
    expect(bwRed.record.tone?.contrast).toBe(0.2); // the recipe's contrast wins
  });

  it("warm and cool cells differ only in white balance", () => {
    const spread = proofSpread({}, AUTO, MANY_PRESETS);
    const warm = spread.find((c) => c.label === "Auto · Warm")!;
    const cool = spread.find((c) => c.label === "Auto · Cool")!;
    expect(warm.record.tone?.wb?.temp).toBeCloseTo(0.35);
    expect(cool.record.tone?.wb?.temp).toBeCloseTo(-0.35);
    expect(warm.record.tone?.ev).toBe(cool.record.tone?.ev);
  });

  it("duelPair differs only in its dimension, symmetrically", () => {
    const working: VersionEdit = {
      crop: { x: 0, y: 0, w: 1, h: 1 },
      zones: [0, 0.3, 0, 0, 0, 0, 0, 0],
      tone: { ev: 0.5, contrast: 0.1, wb: { temp: 0.1, tint: -0.05 } } as VersionEdit["tone"],
    };
    for (const dim of DUEL_DIMS) {
      const [a, b] = duelPair(working, dim, 0);
      // Framing, zones, and look travel through untouched on both sides.
      expect(a.crop).toEqual(working.crop);
      expect(b.zones).toEqual(working.zones);
      // Exactly the duelled field differs between A and B.
      const diffKeys = (["ev", "contrast", "shadows"] as const).filter(
        (k) => a.tone?.[k] !== b.tone?.[k],
      );
      const tempDiffers = a.tone?.wb?.temp !== b.tone?.wb?.temp;
      if (dim === "warmth") {
        expect(diffKeys).toHaveLength(0);
        expect(tempDiffers).toBe(true);
        expect(a.tone?.wb?.tint).toBe(-0.05); // tint rides along unchanged
      } else {
        expect(diffKeys).toEqual([dim]);
        expect(tempDiffers).toBe(false);
      }
    }
    // Symmetry around the working value.
    const [a, b] = duelPair(working, "ev", 0);
    expect((a.tone!.ev + b.tone!.ev) / 2).toBeCloseTo(0.5);
  });

  it("duelPair's step decays with each revisit", () => {
    const working: VersionEdit = {};
    const spreadAt = (visit: number) => {
      const [a, b] = duelPair(working, "ev", visit);
      return Math.abs(b.tone!.ev - a.tone!.ev);
    };
    expect(spreadAt(1)).toBeCloseTo(spreadAt(0) / 2);
    expect(spreadAt(3)).toBeCloseTo(spreadAt(0) / 8);
  });

  it("spreads looks across categories round-robin", () => {
    const spread = proofSpread({}, AUTO, MANY_PRESETS);
    const looks = spread.slice(4);
    // First round: one per category in order Film, Color, Monochrome, User.
    expect(looks[0].label).toBe("portra");
    expect(looks[1].label).toBe("teal");
    expect(looks[2].label).toBe("bw-red");
    expect(looks[3].label).toBe("mine");
  });
});
