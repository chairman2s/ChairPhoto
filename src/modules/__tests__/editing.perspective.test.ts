import { describe, expect, it } from "vitest";
import {
  DEFAULT_QUAD,
  parseEdit,
  Perspective,
  QUAD_CORNERS,
  VersionEdit,
} from "../editing";

/**
 * The perspective quad crosses two boundaries: it is serialized into the opaque
 * `edit_json` a version stores, and it is read back by the Rust render engine
 * (`src-tauri/src/plugins/edit/mod.rs`). Both sides agree on *fractions of the source*
 * and on the corner names, so these tests pin the shape rather than any rendering.
 */
describe("perspective edit record", () => {
  it("round-trips through the stored edit_json", () => {
    const perspective: Perspective = {
      tl: [0.098, 0.171],
      tr: [0.853, 0.106],
      br: [0.878, 0.9],
      bl: [0.083, 0.921],
    };
    const saved = JSON.stringify({ perspective, straighten: 0 } satisfies VersionEdit);
    const back = parseEdit(saved);
    expect(back.perspective).toEqual(perspective);
  });

  it("is absent, not defaulted, when a version has no perspective edit", () => {
    // A record without the field must stay without it: writing a full-frame quad instead
    // would push every untouched version through the warp for nothing.
    expect(parseEdit(JSON.stringify({ straighten: 2 })).perspective).toBeUndefined();
    expect(parseEdit("{}").perspective).toBeUndefined();
    expect(parseEdit(undefined).perspective).toBeUndefined();
  });

  it("survives a record written by an older build", () => {
    // Records are canonical and never baked, so a version saved before this feature
    // existed must still parse — it simply has no quad.
    const legacy = '{"crop":{"x":0.1,"y":0,"w":0.8,"h":1},"tone":{"ev":0.5}}';
    const back = parseEdit(legacy);
    expect(back.perspective).toBeUndefined();
    expect(back.crop).toEqual({ x: 0.1, y: 0, w: 0.8, h: 1 });
  });

  it("names its corners in drawing order, so the outline is a simple quad", () => {
    // QUAD_CORNERS drives both the overlay polygon and the handles. Out of order it
    // would draw a bow-tie across the image rather than the subject's outline.
    expect([...QUAD_CORNERS]).toEqual(["tl", "tr", "br", "bl"]);
  });

  it("starts from an inset full frame with every corner grabbable", () => {
    const xs = QUAD_CORNERS.map((c) => DEFAULT_QUAD[c][0]);
    const ys = QUAD_CORNERS.map((c) => DEFAULT_QUAD[c][1]);
    for (const v of [...xs, ...ys]) {
      expect(v).toBeGreaterThan(0);
      expect(v).toBeLessThan(1);
    }
    // Inset, but still recognisably the whole frame rather than a small box.
    expect(Math.max(...xs) - Math.min(...xs)).toBeGreaterThan(0.8);
    expect(Math.max(...ys) - Math.min(...ys)).toBeGreaterThan(0.8);
  });
});
