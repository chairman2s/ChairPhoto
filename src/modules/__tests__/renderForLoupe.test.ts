// Loupe renders (src/modules/api.ts): a RAW-engine record is a native edit:// URL at the
// loupe's fit edge (full size to zoom), carrying the session token when there is one; an
// engine-1 record goes through render_edit as it always has.
import { beforeEach, describe, expect, it, vi } from "vitest";

const invoke = vi.fn((..._args: unknown[]) => Promise.resolve("data:image/jpeg;base64,xx"));
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invoke(...args),
  convertFileSrc: (p: string) => `asset:///${p}`,
}));

const { LOUPE_FIT_EDGE, renderForLoupe } = await import("../api");

describe("renderForLoupe", () => {
  beforeEach(() => invoke.mockClear());

  it("renders an engine-2 record natively, at the fit edge, with the session token", async () => {
    const url = await renderForLoupe(5, '{"engine":2}', { source: "w:5:9" });
    expect(url).toContain(`m=${LOUPE_FIT_EDGE}`);
    expect(url).toContain("s=w%3A5%3A9");
    expect(invoke).not.toHaveBeenCalled();
  });

  it("asks for the full picture to zoom, and needs no token outside Develop", async () => {
    const url = await renderForLoupe(5, '{"engine":2}', { hi: true });
    expect(url).toContain("m=0");
    expect(url).not.toContain("s=");
  });

  it("leaves engine-1 records on render_edit", async () => {
    await renderForLoupe(5, '{"fade":0.1}', { hi: true });
    expect(invoke).toHaveBeenCalledWith("render_edit", { photoId: 5, editJson: '{"fade":0.1}', maxEdge: 0, hiRes: true });
  });
});
