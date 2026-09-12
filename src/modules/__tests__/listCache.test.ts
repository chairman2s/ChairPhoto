// The core invoke's cache for catalog-wide lists (src/modules/api.ts): shared in-flight
// calls, generation bumps on mutations and on explicit invalidation, errors not cached.
import { beforeEach, describe, expect, it, vi } from "vitest";

const calls: string[] = [];
let failNext = false;
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string) => {
    calls.push(cmd);
    if (failNext) {
      failNext = false;
      return Promise.reject(new Error("boom"));
    }
    return Promise.resolve([]);
  },
  convertFileSrc: (p: string) => `asset:///${p}`,
}));

import { assignTag, distinctPhotoValues, getSetting, invalidateListCache, listCacheGeneration, listTags } from "../api";

beforeEach(() => {
  calls.length = 0;
  invalidateListCache();
});

describe("list cache", () => {
  it("serves repeated and concurrent reads of a list from one call", async () => {
    await Promise.all([listTags(), listTags()]);
    await listTags();
    expect(calls.filter((c) => c === "list_tags")).toHaveLength(1);
  });

  it("keys on the arguments", async () => {
    await distinctPhotoValues("camera");
    await distinctPhotoValues("lens");
    await distinctPhotoValues("camera");
    expect(calls.filter((c) => c === "distinct_photo_values")).toHaveLength(2);
  });

  it("refetches after a mutation, and a plain read does not invalidate", async () => {
    await listTags();
    await getSetting("x");
    await listTags();
    expect(calls.filter((c) => c === "list_tags")).toHaveLength(1);
    const before = listCacheGeneration();
    await assignTag(1, 2);
    expect(listCacheGeneration()).toBeGreaterThan(before);
    await listTags();
    expect(calls.filter((c) => c === "list_tags")).toHaveLength(2);
  });

  it("refetches after explicit invalidation", async () => {
    await listTags();
    invalidateListCache();
    await listTags();
    expect(calls.filter((c) => c === "list_tags")).toHaveLength(2);
  });

  it("does not remember a failure", async () => {
    failNext = true;
    await expect(listTags()).rejects.toThrow("boom");
    await listTags();
    expect(calls.filter((c) => c === "list_tags")).toHaveLength(2);
  });
});
