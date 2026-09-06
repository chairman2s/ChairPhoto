import { describe, expect, it } from "vitest";
import { parseEdit } from "../../../modules/editing";
import { stageJsonFor } from "../stageJson";

describe("stageJsonFor", () => {
  const working = parseEdit(
    JSON.stringify({
      crop: { x: 0.1, y: 0.1, w: 0.8, h: 0.8, aspect: "1:1" },
      straighten: 2,
      perspective: { tl: [0, 0], tr: [1, 0], br: [1, 1], bl: [0, 1] },
      tone: { ev: 0.5 },
    }),
  );

  it("drops the crop and keeps everything else", () => {
    const r = JSON.parse(stageJsonFor(working, false));
    expect(r.crop).toBeUndefined();
    expect(r.straighten).toBe(2);
    expect(r.perspective).toEqual(working.perspective);
    expect(r.tone.ev).toBe(0.5);
  });

  it("drops the perspective only while the handles are up", () => {
    expect(JSON.parse(stageJsonFor(working, true)).perspective).toBeUndefined();
    expect(JSON.parse(stageJsonFor(working, false)).perspective).toBeDefined();
  });

  it("is a pure function of its inputs", () => {
    expect(stageJsonFor(working, false)).toBe(stageJsonFor({ ...working }, false));
    expect(working.crop).toBeDefined();
  });
});
