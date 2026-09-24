// The grid's thumb:// URL carries the recovery bust and the cover token.
import { describe, expect, it } from "vitest";
import { thumbUrl } from "../Thumbnail";

describe("thumbUrl", () => {
  it("is the bare URL without a bust or a cover", () => {
    expect(thumbUrl(7)).toBe("asset:///7");
    expect(thumbUrl(7, 0, null)).toBe("asset:///7");
  });
  it("adds the bust and the cover token, so either change is a new URL", () => {
    expect(thumbUrl(7, 2)).toBe("asset:///7?v=2");
    expect(thumbUrl(7, undefined, "12:3")).toBe("asset:///7?c=12%3A3");
    expect(thumbUrl(7, 2, "12:3")).toBe("asset:///7?v=2&c=12%3A3");
    expect(thumbUrl(7, undefined, "12:4")).not.toBe(thumbUrl(7, undefined, "12:3"));
  });
});
