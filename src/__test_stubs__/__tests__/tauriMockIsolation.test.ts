/**
 * Regression proof for issue #64.
 *
 * `vitest.config.ts` used to alias every `@tauri-apps/*` specifier to one file,
 * `src/__test_stubs__/tauri.ts`. `vi.mock()` keys its factory registry on the *resolved*
 * module id, not the specifier string a test file wrote — so a test mocking two different
 * `@tauri-apps/*` specifiers that resolved to that one file was really registering two
 * factories against one path. Only the last registration took effect; the earlier one
 * silently reverted to whatever the (real or previously mocked) module already was, with
 * no error pointing at the cause. That cost a debugging cycle during #34 (see
 * `src/components/__tests__/IdentityDebtPanel.repair.test.tsx`'s header comment, which
 * routes both `invoke` and `listen` through one `vi.mock("@tauri-apps/api/core", ...)` call
 * specifically to dodge this).
 *
 * This test mocks two *different* `@tauri-apps` specifiers in one file and asserts both
 * factories take effect independently. On the pre-fix single-shared-stub config this fails:
 * the second `vi.mock` below (`@tauri-apps/api/event`) silently wins over the first
 * (`@tauri-apps/api/core`), so `core.invoke()` resolves to the stub's real no-op (`null`)
 * instead of this file's `"core-mock"` sentinel. After the fix (distinct resolved files per
 * specifier), each `vi.mock` targets its own path and both sentinels come through.
 */
import { describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: () => Promise.resolve("core-mock"),
  convertFileSrc: (p: string) => `core-mock:${p}`,
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: () => Promise.resolve("event-mock"),
}));

describe("issue #64 — distinct @tauri-apps specifiers get distinct vi.mock targets", () => {
  it("mocking @tauri-apps/api/core does not get clobbered by mocking @tauri-apps/api/event", async () => {
    const core = await import("@tauri-apps/api/core");
    const event = await import("@tauri-apps/api/event");

    await expect(core.invoke("anything")).resolves.toBe("core-mock");
    await expect(event.listen("anything", () => {})).resolves.toBe("event-mock");
  });
});
