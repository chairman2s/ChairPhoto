import { describe, expect, it } from "vitest";
import { badgeFor, formatMegapixels } from "../developSource";

describe("badgeFor", () => {
  it("names a supported RAW by depth and size", () => {
    const b = badgeFor({ source: "raw", camera: "Sony ILCE-7RM6", megapixels: 66.83, bits: 16, decoder: "0.22.0-Devel" });
    expect(b.label).toBe("RAW · 16-bit · 67 MP");
    expect(b.title).toContain("Sony ILCE-7RM6");
    expect(b.tone).toBe("raw");
  });

  it("is honest about an unsupported camera, naming it when known", () => {
    const b = badgeFor({ source: "unsupported", camera: "ILCE-7RM6", reason: "Unsupported file format" });
    expect(b.label).toBe("camera preview · RAW not supported yet · ILCE-7RM6");
    expect(b.title).toContain("Unsupported file format");
    expect(b.tone).toBe("warn");
    expect(badgeFor({ source: "unsupported", camera: null, reason: "x" }).label).toBe(
      "camera preview · RAW not supported yet",
    );
  });

  it("treats a JPEG as its own full quality and a decoder-less build as a warning", () => {
    expect(badgeFor({ source: "jpeg" })).toMatchObject({ label: "JPEG · 8-bit", tone: "plain" });
    expect(badgeFor({ source: "nodecoder" }).tone).toBe("warn");
  });
});

describe("formatMegapixels", () => {
  it("rounds large counts and keeps one decimal for small ones", () => {
    expect(formatMegapixels(66.83)).toBe("67");
    expect(formatMegapixels(33.0)).toBe("33");
    expect(formatMegapixels(9.62)).toBe("9.6");
  });
});
