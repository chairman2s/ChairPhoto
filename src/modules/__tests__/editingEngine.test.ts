import { describe, expect, it } from "vitest";
import { asLinearRecord, ENGINE_LINEAR, forLinearEngine, isEngine1Version, isLinear, parseEdit } from "../editing";

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
    expect(out).toEqual({ crop: { x: 0.1, y: 0.1, w: 0.8, h: 0.8, aspect: "1:1" }, perspective: undefined, straighten: 2, engine: 2, display: "camera.2" });
  });

  it("a record becoming engine 2 gets the camera look; an engine-2 record keeps its own", () => {
    expect(asLinearRecord({ tone: { ev: 0.5 } as never })).toEqual({ tone: { ev: 0.5 }, engine: 2, display: "camera.2" });
    expect(asLinearRecord({ engine: 1, fade: 0.1 })).toEqual({ engine: 2, fade: 0.1, display: "camera.2" });
    // Saved before the default changed: no display means plain sRGB, forever.
    const old = parseEdit('{"engine":2,"fade":0.1}');
    expect(asLinearRecord(old)).toBe(old);
    expect(asLinearRecord(parseEdit('{"engine":2,"display":"soft"}')).display).toBe("soft");
  });

  it("stamps this frame's camera match on a new record only, and leaves 0 off", () => {
    expect(asLinearRecord({ fade: 0.1 }, -1.6)).toEqual({ fade: 0.1, engine: 2, display: "camera.2", cameraEv: -1.6 });
    expect(asLinearRecord({ fade: 0.1 }, 0)).not.toHaveProperty("cameraEv");
    // A saved engine-2 record keeps its own match (or none), whatever this open measured.
    const saved = parseEdit('{"engine":2,"display":"camera","cameraEv":-0.5}');
    expect(asLinearRecord(saved, -1.6)).toBe(saved);
    // The match belongs to the camera transform; a record asking for another gets none.
    expect(asLinearRecord({ display: "srgb" }, -1.6)).not.toHaveProperty("cameraEv");
  });
});

describe("old versions, honestly (slice 7)", () => {
  it("a saved record without the engine-2 stamp that holds anything is engine 1", () => {
    expect(isEngine1Version(parseEdit('{"tone":{"ev":0.5}}'))).toBe(true);
    expect(isEngine1Version(parseEdit('{"engine":1,"fade":0.2}'))).toBe(true);
    expect(isEngine1Version(parseEdit('{"engine":2,"display":"camera.2"}'))).toBe(false);
    // A blank version holds nothing whose meaning could change.
    expect(isEngine1Version(parseEdit("{}"))).toBe(false);
  });

  it("the fork keeps the framing, resets tone and look, and takes this frame's match", () => {
    const old = parseEdit(
      '{"crop":{"x":0.1,"y":0,"w":0.8,"h":1,"aspect":"4:5"},"straighten":1.2,"tone":{"ev":0.8},"fade":0.3,"lut":{"file":"a.cube","amount":1}}',
    );
    expect(forLinearEngine(old, -1.6)).toEqual({
      crop: { x: 0.1, y: 0, w: 0.8, h: 1, aspect: "4:5" },
      perspective: undefined,
      straighten: 1.2,
      engine: 2,
      display: "camera.2",
      cameraEv: -1.6,
    });
    expect(forLinearEngine(old)).not.toHaveProperty("cameraEv");
  });
});
