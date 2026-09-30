// The Darkroom's Lens section says what the switch corrects for this file.
import { describe, expect, it } from "vitest";
import { lensHint } from "../LensRail";

describe("lensHint", () => {
  it("names every correction the file's tables allow", () => {
    expect(lensHint({ source: "Sony built-in", vignetting: true, distortion: true, chromatic: true })).toBe(
      "Sony built-in tables: brightens the corners the lens darkened, straightens lines the lens bent and removes colour fringing at the edges.",
    );
    expect(lensHint({ source: "Sony built-in", vignetting: false, distortion: true, chromatic: false })).toBe(
      "Sony built-in tables: straightens lines the lens bent.",
    );
  });
});
