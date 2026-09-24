import { describe, expect, it } from "vitest";
import { asLinearRecord, ENGINE_LINEAR, forLinearEngine, isLinear, parseEdit } from "../editing";

describe("engine id on the record", () => {
  it("is absent (engine 1) on every existing record, and survives a round trip", () => {
    expect(isLinear(parseEdit('{"tone":{"ev":1}}'))).toBe(false);
    expect(isLinear(parseEdit('{"engine":2}'))).toBe(true);
    expect(JSON.parse(JSON.stringify(parseEdit('{"engine":2,"tone":{"ev":1}}'))).engine).toBe(ENGINE_LINEAR);
  });

  it("forLinearEngine copies geometry, resets tone and look, and stamps engine 2", () => {
    const out = forLinearEngine({
      crop: { x: 0.1, y: 0.1, w: 0.8, h: 0.8, aspect: "1:1" },
      straighten: 2,
      tone: { ev: 1 } as never,
      bw: { enabled: true, r: 1, g: 0, b: 0 } as never,
      zones: [0, 1, 0, 0, 0, 0, 0, 0],
    });
    expect(out).toEqual({ crop: { x: 0.1, y: 0.1, w: 0.8, h: 0.8, aspect: "1:1" }, perspective: undefined, straighten: 2, engine: 2, display: "camera" });
  });

  it("a record becoming engine 2 gets the camera look; an engine-2 record keeps its own", () => {
    expect(asLinearRecord({ tone: { ev: 0.5 } as never })).toEqual({ tone: { ev: 0.5 }, engine: 2, display: "camera" });
    expect(asLinearRecord({ engine: 1, fade: 0.1 })).toEqual({ engine: 2, fade: 0.1, display: "camera" });
    // Saved before the default changed: no display means plain sRGB, forever.
    const old = parseEdit('{"engine":2,"fade":0.1}');
    expect(asLinearRecord(old)).toBe(old);
    expect(asLinearRecord(parseEdit('{"engine":2,"display":"soft"}')).display).toBe("soft");
  });
});
