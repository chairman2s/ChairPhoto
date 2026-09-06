// The Darkroom stage's native render URL (src/modules/api.ts). The Tauri stub maps
// convertFileSrc(p, scheme) to `asset:///p`, so only the query is under test here.
import { describe, expect, it } from "vitest";
import { base64url, editRenderUrl } from "../api";

describe("base64url", () => {
  it("is RFC 4648 §5 without padding", () => {
    expect(base64url("")).toBe("");
    expect(base64url("{}")).toBe("e30");
    // '+' and '/' in standard base64 become '-' and '_'.
    expect(base64url("ûÿ")).toBe("w7vDvw");
    expect(base64url("???")).toBe("Pz8_");
  });

  it("encodes UTF-8, so a LUT filename with non-ASCII survives", () => {
    const json = JSON.stringify({ lut: { file: "Kodak Portra ☺.cube" } });
    expect(base64url(json)).toMatch(/^[A-Za-z0-9_-]+$/);
  });
});

describe("editRenderUrl", () => {
  it("carries the record, edge and flags in the query", () => {
    expect(editRenderUrl(123, "{}", { maxEdge: 720 })).toBe("asset:///123?r=e30&m=720");
    expect(editRenderUrl(123, "{}", { maxEdge: 1400, baseOnly: true })).toBe(
      "asset:///123?r=e30&m=1400&b=1",
    );
    expect(editRenderUrl(7, "{}", { hiRes: true, bust: 42 })).toBe("asset:///7?r=e30&m=0&hi=1&v=42");
  });

  it("is deterministic: same record, same URL", () => {
    const json = JSON.stringify({ tone: { ev: 0.5 }, zones: [0, 0.1, 0, 0, 0, 0, 0, 0] });
    expect(editRenderUrl(1, json, { maxEdge: 720 })).toBe(editRenderUrl(1, json, { maxEdge: 720 }));
    expect(editRenderUrl(1, json, { maxEdge: 720 })).not.toBe(editRenderUrl(1, json, { maxEdge: 1400 }));
  });
});
