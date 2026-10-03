// The cache warm-up as the shell follows it: a superseded call is quiet, and a superseded
// pass's progress moves nothing (review claude-159-160, L2).
import { describe, expect, it } from "vitest";
import { CACHE_CANCELLED, cacheResultLine, createCacheFollower, isCacheCancelled } from "../cacheWarmup";

describe("cacheResultLine", () => {
  it("reports success and failure as before", () => {
    expect(cacheResultLine(null)).toBe("Cache ready");
    expect(cacheResultLine("disk full")).toBe("Cache failed: disk full");
  });

  it("says nothing when a newer warm-up or a catalog switch superseded the call", () => {
    expect(isCacheCancelled(CACHE_CANCELLED)).toBe(true);
    expect(isCacheCancelled(new Error("x"))).toBe(false);
    expect(cacheResultLine(CACHE_CANCELLED)).toBeNull();
  });
});

describe("createCacheFollower", () => {
  it("shows the newest job's progress and drops an older job's stragglers", () => {
    const follow = createCacheFollower();
    expect(follow.onProgress({ job: 1, done: 1, total: 4 })).toBe("Caching 1/4…");
    expect(follow.onProgress({ job: 2, done: 1, total: 9 })).toBe("Caching 1/9…");
    expect(follow.onProgress({ job: 1, done: 2, total: 4 })).toBeNull();
    expect(follow.onProgress({ job: 2, done: 9, total: 9 })).toBe("Cache ready (9)");
  });
});
