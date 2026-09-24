// "Save as preset" from the Darkroom stores the look, never the framing or the engine.
import { describe, expect, it } from "vitest";
import { lookOnly } from "../presets";
import { parseEdit } from "../editing";

describe("lookOnly", () => {
  it("keeps tone, zones and looks, drops framing, the engine stamp and its display transform", () => {
    const record = parseEdit(
      JSON.stringify({
        engine: 2,
        display: "camera",
        cameraEv: -1.6,
        crop: { x: 0.1, y: 0.1, w: 0.8, h: 0.8, aspect: "4:5" },
        straighten: 1.5,
        perspective: { tl: [0, 0], tr: [1, 0], br: [1, 1], bl: [0, 1] },
        tone: { ev: 0.5, contrast: 0.2 },
        zones: [0, 0.1, 0, 0, 0, 0, 0, 0],
        fade: 0.2,
        lut: { file: "portra.cube", amount: 0.8 },
      }),
    );
    const look = lookOnly(record) as Record<string, unknown>;
    for (const k of ["crop", "straighten", "perspective", "engine", "display", "cameraEv"]) expect(look[k]).toBeUndefined();
    expect((look.tone as { ev: number }).ev).toBe(0.5);
    expect(look.zones).toEqual([0, 0.1, 0, 0, 0, 0, 0, 0]);
    expect(look.fade).toBe(0.2);
    expect(look.lut).toEqual({ file: "portra.cube", amount: 0.8 });
    expect((record as Record<string, unknown>).crop, "the input is not mutated").toBeDefined();
  });
});
