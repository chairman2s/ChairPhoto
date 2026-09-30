// The Darkroom's history labels and coalescing (pure).
import { describe, expect, it } from "vitest";
import { AMEND_WINDOW_MS, describeChange, shouldAmend } from "../history";
import { parseEdit } from "../../../modules/editing";

const rec = (o: object) => parseEdit(JSON.stringify(o));

describe("describeChange", () => {
  it("names a single slider with its new value", () => {
    expect(describeChange(rec({}), rec({ tone: { ev: 0.5 } }))).toEqual({ label: "Exposure +0.50", key: "tone.ev" });
    expect(describeChange(rec({ tone: { ev: 0.5 } }), rec({ tone: { ev: -0.25 } })).label).toBe("Exposure −0.25");
    expect(describeChange(rec({}), rec({ tone: { wb: { temp: 0.1, tint: 0 } } })).label).toBe("Temperature +0.10");
  });

  it("names geometry and looks", () => {
    expect(describeChange(rec({}), rec({ crop: { x: 0, y: 0, w: 1, h: 1, aspect: "4:5" } })).label).toBe("Crop 4:5");
    expect(describeChange(rec({ crop: { x: 0, y: 0, w: 1, h: 1 } }), rec({})).label).toBe("Crop removed");
    expect(describeChange(rec({}), rec({ straighten: 1.46 })).label).toBe("Straighten 1.5°");
    expect(describeChange(rec({}), rec({ bw: { enabled: true, r: 1, g: 0, b: 0 } })).label).toBe("Black & white");
    expect(describeChange(rec({}), rec({ lut: { file: "portra.cube", amount: 1 } })).label).toBe("LUT portra");
    expect(describeChange(rec({}), rec({ zones: [0, 0.2, 0, 0, 0, 0, 0, 0] })).label).toBe("Tone strip");
    expect(describeChange(rec({}), rec({ lens: { builtin: true } }))).toEqual({ label: "Lens correction on", key: "lens" });
    expect(describeChange(rec({ lens: { builtin: true } }), rec({})).label).toBe("Lens correction off");
  });

  it("names several changes at once by the caller's label, or generically", () => {
    const from = rec({});
    const to = rec({ tone: { ev: 0.3, contrast: 0.2 }, fade: 0.1 });
    expect(describeChange(from, to, "Proof: Portra").label).toBe("Proof: Portra");
    const generic = describeChange(from, to);
    expect(generic.label).toBe("Adjustments");
    expect(generic.key.startsWith("multi:")).toBe(true);
    // A caller label wins even for a single control (a reset of one slider, say).
    expect(describeChange(rec({ fade: 0.2 }), rec({}), "Reset").label).toBe("Reset");
  });

  it("says Edit when nothing changed", () => {
    expect(describeChange(rec({ fade: 0.2 }), rec({ fade: 0.2 })).key).toBe("none");
  });
});

describe("shouldAmend", () => {
  const exposure = { label: "Exposure +0.50", key: "tone.ev" };
  it("amends the same control still moving at the tip", () => {
    expect(shouldAmend({ key: "tone.ev", at: 1000 }, exposure, 1000 + AMEND_WINDOW_MS - 1, true)).toBe(true);
  });
  it("starts a new step otherwise", () => {
    expect(shouldAmend(null, exposure, 0, true)).toBe(false);
    expect(shouldAmend({ key: "tone.ev", at: 0 }, exposure, AMEND_WINDOW_MS, true)).toBe(false);
    expect(shouldAmend({ key: "tone.contrast", at: 0 }, exposure, 10, true)).toBe(false);
    expect(shouldAmend({ key: "tone.ev", at: 0 }, exposure, 10, false)).toBe(false);
    expect(shouldAmend({ key: "multi:a,b", at: 0 }, { label: "x", key: "multi:a,b" }, 10, true)).toBe(false);
  });
});

import { whenLabel } from "../HistoryPanel";

describe("whenLabel", () => {
  const now = 1_800_000_000_000; // ms
  const at = (secsAgo: number) => now / 1000 - secsAgo;
  it("reads naturally for recent steps", () => {
    expect(whenLabel(at(5), now)).toBe("just now");
    expect(whenLabel(at(5 * 60), now)).toBe("5 min ago");
    expect(whenLabel(at(3 * 3600), now)).toBe("3 h ago");
  });
  it("falls back to the date after a day, and never goes negative", () => {
    expect(whenLabel(at(3 * 86400), now)).toBe(new Date((now / 1000 - 3 * 86400) * 1000).toLocaleDateString());
    expect(whenLabel(at(-60), now)).toBe("just now");
  });
});
