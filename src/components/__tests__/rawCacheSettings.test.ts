// Preferences → Darkroom: the RAW decode cache's size field and usage line.
import { describe, expect, it } from "vitest";
import { formatCacheBytes, formatExportParity, parseCacheGb } from "../Preferences";

describe("parseCacheGb", () => {
  it("falls back to the 20 GB default for anything that is not a non-negative number", () => {
    expect(parseCacheGb(null)).toBe(20);
    expect(parseCacheGb(undefined)).toBe(20);
    expect(parseCacheGb("")).toBe(20);
    expect(parseCacheGb("  ")).toBe(20);
    expect(parseCacheGb("lots")).toBe(20);
    expect(parseCacheGb("-3")).toBe(20);
  });

  it("keeps a valid size, including 0 (keep nothing) and fractions", () => {
    expect(parseCacheGb("0")).toBe(0);
    expect(parseCacheGb("50")).toBe(50);
    expect(parseCacheGb("2.5")).toBe(2.5);
  });
});

describe("formatCacheBytes", () => {
  it("shows GB from one GB up and MB below", () => {
    expect(formatCacheBytes(0)).toBe("0 MB");
    expect(formatCacheBytes(400 * 1024 ** 2)).toBe("400 MB");
    expect(formatCacheBytes(3.44 * 1024 ** 3)).toBe("3.4 GB");
  });
});

describe("formatExportParity", () => {
  it("says nothing before a RAW export was checked", () => {
    expect(formatExportParity(null)).toBeNull();
    expect(formatExportParity("not json")).toBeNull();
    expect(formatExportParity('{"checked":0,"differing":0}')).toBeNull();
  });
  it("counts checked and differing exports", () => {
    expect(formatExportParity('{"checked":1,"differing":0}')).toContain("1 RAW export checked, none differed");
    expect(formatExportParity('{"checked":12,"differing":2}')).toContain("12 RAW exports checked, 2 differed");
  });
});
