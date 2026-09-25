import { describe, expect, it } from "vitest";
import { badgeFor, formatMegapixels, INITIAL_SOURCE, isPreparing, reduceSource } from "../developSource";

describe("badgeFor", () => {
  it("names a supported RAW by depth and size", () => {
    const b = badgeFor({ source: "raw", camera: "Sony ILCE-7RM6", megapixels: 66.45, bits: 16, decoder: "0.22.0-Devel" });
    expect(b.label).toBe("RAW · 16-bit · 66.5 MP");
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
  it("keeps one decimal and drops a trailing zero", () => {
    expect(formatMegapixels(66.45)).toBe("66.5");
    expect(formatMegapixels(33.0)).toBe("33");
    expect(formatMegapixels(9.62)).toBe("9.6");
  });
});

describe("reduceSource", () => {
  const raw = { source: "raw" as const, camera: "Sony", megapixels: 66.5, bits: 16, decoder: "x", token: "w:5:3", photoId: 5 };

  it("starts on the preview and moves to the RAW token when it becomes resident", () => {
    const preparing = reduceSource(INITIAL_SOURCE, { source: "preview", preparing: true, photoId: 5 }, 5);
    expect(preparing.token).toBeUndefined();
    expect(preparing.engine).toBe(1);
    const resident = reduceSource(preparing, raw, 5);
    expect(resident.token).toBe("w:5:3");
    expect(resident.engine).toBe(2);
  });

  it("ignores events for another photo", () => {
    const s = reduceSource(INITIAL_SOURCE, { ...raw, photoId: 6 }, 5);
    expect(s).toBe(INITIAL_SOURCE);
  });

  it("drops back to the preview when the source is not a resident RAW", () => {
    const resident = reduceSource(INITIAL_SOURCE, raw, 5);
    const gone = reduceSource(resident, { source: "unsupported", camera: null, reason: "x", photoId: 5 }, 5);
    expect(gone.token).toBeUndefined();
    expect(gone.engine).toBe(1);
    expect(gone.cameraEv).toBe(0);
  });

  it("carries the as-shot light for Kelvin, and none off the RAW", () => {
    expect(reduceSource(INITIAL_SOURCE, { ...raw, asShotWb: [5313, 2.4] }, 5).asShotWb).toEqual({ kelvin: 5313, tint: 2.4 });
    expect(reduceSource(INITIAL_SOURCE, raw, 5).asShotWb).toBeNull();
    expect(reduceSource(INITIAL_SOURCE, { source: "jpeg", photoId: 5 }, 5).asShotWb).toBeNull();
  });

  it("carries the resident RAW's camera match, and 0 when it was not measured", () => {
    expect(reduceSource(INITIAL_SOURCE, { ...raw, cameraEv: -1.6 }, 5).cameraEv).toBe(-1.6);
    expect(reduceSource(INITIAL_SOURCE, raw, 5).cameraEv).toBe(0);
  });

  it("autosave waits only while the RAW is being prepared", () => {
    expect(isPreparing({ source: "preview", preparing: true })).toBe(true);
    expect(isPreparing({ source: "preview", preparing: false })).toBe(false);
    expect(isPreparing(null)).toBe(false);
    expect(isPreparing({ source: "jpeg" })).toBe(false);
    expect(isPreparing({ source: "unsupported", camera: null, reason: "x" })).toBe(false);
  });

  it("badges the preparing state honestly", () => {
    expect(badgeFor({ source: "preview", preparing: true }).label).toContain("preparing");
    expect(badgeFor({ source: "preview", preparing: false }).tone).toBe("plain");
  });
});
