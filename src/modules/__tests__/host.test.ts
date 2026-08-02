/**
 * H8e — Host-version enforcement + contract tests.
 *
 * Tests for:
 *   - satisfies()        — semver range matching for module `requires`
 *   - hostSatisfies()    — ">=" floor check for `minHostVersion`
 *   - unmetRequirement() — composed constraint checker feeding `blockedReason`
 *   - enableModule()     — satisfied module registers and calls onLoad;
 *                          newer-host refused with a clear blockedReason message;
 *                          a throwing onLoad is skipped without crashing the host.
 *
 * Tauri APIs are stubbed via the aliases in vitest.config.ts — no Tauri runtime
 * required. React's useSyncExternalStore is fine in a Node environment.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";

import {
  satisfies,
  hostSatisfies,
  unmetRequirement,
  register,
  enableModule,
  disableModule,
  listModules,
  backendAvailable,
} from "../host";
import type { ChairPhotoAPI, ChairPhotoModule } from "../registry";
import { __events } from "../../__test_stubs__/tauri";

// ── Helper ────────────────────────────────────────────────────────────────────

/** Build a minimal valid ChairPhotoModule for test registration. */
function makeModule(
  id: string,
  overrides: Partial<ChairPhotoModule> = {},
): ChairPhotoModule {
  return {
    id,
    name: overrides.name ?? id,
    version: overrides.version ?? "1.0.0",
    description: overrides.description,
    backendFeature: overrides.backendFeature,
    requires: overrides.requires,
    publicationMarker: overrides.publicationMarker,
    onLoad: overrides.onLoad ?? vi.fn(),
    onUnload: overrides.onUnload,
    onPhotoSelected: overrides.onPhotoSelected,
  };
}

// Silence console noise emitted by the host for rejected modules.
beforeEach(() => {
  vi.spyOn(console, "warn").mockImplementation(() => {});
  vi.spyOn(console, "error").mockImplementation(() => {});
});

// ─────────────────────────────────────────────────────────────────────────────
// satisfies() — semver range helper for `requires` version constraints
// ─────────────────────────────────────────────────────────────────────────────

describe("satisfies()", () => {
  it("accepts any version when range is empty, *, or x", () => {
    expect(satisfies("1.2.3", "")).toBe(true);
    expect(satisfies("1.2.3", "*")).toBe(true);
    expect(satisfies("1.2.3", "x")).toBe(true);
    expect(satisfies("1.2.3", undefined)).toBe(true);
  });

  it("matches an exact version string", () => {
    expect(satisfies("1.2.3", "1.2.3")).toBe(true);
    expect(satisfies("1.2.4", "1.2.3")).toBe(false);
    expect(satisfies("2.0.0", "1.2.3")).toBe(false);
  });

  it("^X.Y.Z: allows same major and >= X.Y.Z when major > 0", () => {
    expect(satisfies("1.0.0", "^1.0.0")).toBe(true);
    expect(satisfies("1.9.9", "^1.0.0")).toBe(true);
    expect(satisfies("2.0.0", "^1.0.0")).toBe(false); // different major
    expect(satisfies("0.9.9", "^1.0.0")).toBe(false); // older
  });

  it("^0.Y.Z: treats a 0.x minor bump as breaking (npm caret semantics)", () => {
    expect(satisfies("0.1.3", "^0.1.0")).toBe(true);
    expect(satisfies("0.1.9", "^0.1.0")).toBe(true);
    expect(satisfies("0.2.0", "^0.1.0")).toBe(false); // minor bump is breaking on 0.x
    expect(satisfies("0.0.9", "^0.1.0")).toBe(false); // older
  });

  it(">=X.Y.Z: accepts anything at or above the floor", () => {
    expect(satisfies("1.2.3", ">=1.2.3")).toBe(true);
    expect(satisfies("1.2.4", ">=1.2.3")).toBe(true);
    expect(satisfies("2.0.0", ">=1.2.3")).toBe(true);
    expect(satisfies("1.2.2", ">=1.2.3")).toBe(false);
    expect(satisfies("0.9.0", ">=1.0.0")).toBe(false);
  });

  it("treats unparseable inputs conservatively as non-satisfying", () => {
    expect(satisfies("not-semver", "^1.0.0")).toBe(false);
    expect(satisfies("1.2.3", "^not-semver")).toBe(false);
  });
});

// ─────────────────────────────────────────────────────────────────────────────
// hostSatisfies() — minHostVersion floor check
// ─────────────────────────────────────────────────────────────────────────────

describe("hostSatisfies()", () => {
  it("is satisfied when minHostVersion is omitted, empty, *, or x", () => {
    expect(hostSatisfies("0.1.0", "")).toBe(true);
    expect(hostSatisfies("0.1.0", "*")).toBe(true);
    expect(hostSatisfies("0.1.0", "x")).toBe(true);
    expect(hostSatisfies("0.1.0", undefined)).toBe(true);
  });

  it("is satisfied when host version equals the floor exactly", () => {
    expect(hostSatisfies("0.1.0", "0.1.0")).toBe(true);
  });

  it("is satisfied when host version is newer than the floor", () => {
    expect(hostSatisfies("0.2.0", "0.1.0")).toBe(true);
    expect(hostSatisfies("1.0.0", "0.9.9")).toBe(true);
    expect(hostSatisfies("1.5.3", "1.0.0")).toBe(true);
  });

  it("is NOT satisfied when host version is older than the floor", () => {
    expect(hostSatisfies("0.0.9", "0.1.0")).toBe(false);
    expect(hostSatisfies("0.1.0", "0.2.0")).toBe(false);
    expect(hostSatisfies("0.9.9", "1.0.0")).toBe(false);
  });

  it("is NOT satisfied when the host version is unknown (empty string)", () => {
    // hostVersion="" means we couldn't read the app version; conservatively
    // treat any non-empty floor as unmet so a module never loads against an
    // unknown host.
    expect(hostSatisfies("", "0.1.0")).toBe(false);
  });
});

// ─────────────────────────────────────────────────────────────────────────────
// unmetRequirement() — blockedReason source
// ─────────────────────────────────────────────────────────────────────────────

describe("unmetRequirement()", () => {
  it("returns a descriptive message when the module is not registered", () => {
    const reason = unmetRequirement("h8e-never-registered");
    expect(reason).not.toBeNull();
    expect(reason).toMatch(/not found/i);
  });

  it("returns null for a dep-free module with no minHostVersion constraint", () => {
    const mod = makeModule("h8e-unreq-depfree");
    register(mod);
    // No requires, no backendFeature, no minHostVersion → nothing blocks it.
    expect(unmetRequirement("h8e-unreq-depfree")).toBeNull();
  });

  it("returns a host-version message when minHostVersion is newer than the host", () => {
    // hostVersion is "" at this point (initHost has not been called); any
    // non-empty minHostVersion floor is therefore unmet.
    const mod = makeModule("h8e-unreq-future-host");
    register(mod, { minHostVersion: "99.0.0" });
    const reason = unmetRequirement("h8e-unreq-future-host");
    expect(reason).not.toBeNull();
    expect(reason).toMatch(/99\.0\.0/); // required floor in the message
    expect(reason).toMatch(/unknown/i);  // running host is unknown → "unknown"
  });

  it("returns a message naming the missing dependency", () => {
    const mod = makeModule("h8e-unreq-missing-dep", {
      requires: [{ id: "h8e-dep-that-does-not-exist", version: "^1.0.0" }],
    });
    register(mod);
    const reason = unmetRequirement("h8e-unreq-missing-dep");
    expect(reason).not.toBeNull();
    expect(reason).toMatch(/h8e-dep-that-does-not-exist/);
  });
});

// ─────────────────────────────────────────────────────────────────────────────
// enableModule() — lifecycle contract
// ─────────────────────────────────────────────────────────────────────────────

describe("enableModule()", () => {
  it("enables a module that has no deps and calls onLoad once", () => {
    const onLoad = vi.fn();
    register(makeModule("h8e-enable-nodep", { onLoad }));

    enableModule("h8e-enable-nodep", false);

    const info = listModules().find((m) => m.id === "h8e-enable-nodep");
    expect(info?.enabled).toBe(true);
    expect(onLoad).toHaveBeenCalledOnce();

    disableModule("h8e-enable-nodep", false);
  });

  it("passes a complete host API object to onLoad", () => {
    let receivedApi: unknown;
    const onLoad = vi.fn((api: unknown) => {
      receivedApi = api;
    });
    register(makeModule("h8e-enable-api", { onLoad }));

    enableModule("h8e-enable-api", false);

    const api = receivedApi as Record<string, unknown>;
    // Spot-check required ChairPhotoAPI surface.
    expect(typeof api.invoke).toBe("function");
    expect(typeof api.registerPanel).toBe("function");
    expect(typeof api.getSetting).toBe("function");
    expect(typeof api.showToast).toBe("function");

    disableModule("h8e-enable-api", false);
  });

  it("refuses and sets a clear blockedReason when minHostVersion is newer than host", () => {
    // hostVersion = "" (initHost not called) → any non-empty floor is unmet.
    const onLoad = vi.fn();
    register(makeModule("h8e-enable-toonew", { onLoad }), { minHostVersion: "99.0.0" });

    enableModule("h8e-enable-toonew", false);

    const info = listModules().find((m) => m.id === "h8e-enable-toonew");
    // Module must not be enabled.
    expect(info?.enabled).toBe(false);
    // onLoad must NOT have been called for a refused module.
    expect(onLoad).not.toHaveBeenCalled();
    // blockedReason must name the required floor so the Modules panel can show it.
    expect(info?.blockedReason).toMatch(/99\.0\.0/);
  });

  it("does NOT crash when onLoad is a no-op (external stub with missing code)", () => {
    // External stubs registered by registerExternalStub() use `onLoad: () => {}`.
    // This simulates that path: the host must enable the stub cleanly.
    register(makeModule("h8e-enable-noop", { onLoad: () => {} }));
    expect(() => enableModule("h8e-enable-noop", false)).not.toThrow();
    const info = listModules().find((m) => m.id === "h8e-enable-noop");
    expect(info?.enabled).toBe(true);
    disableModule("h8e-enable-noop", false);
  });

  it("skips a throwing onLoad without crashing and leaves the module disabled", () => {
    register(
      makeModule("h8e-enable-throws", {
        onLoad: () => {
          throw new Error("onLoad exploded");
        },
      }),
    );

    // The host must swallow the error and not propagate it.
    expect(() => enableModule("h8e-enable-throws", false)).not.toThrow();

    const info = listModules().find((m) => m.id === "h8e-enable-throws");
    // Module is rolled back to disabled (the host's error-isolation contract).
    expect(info?.enabled).toBe(false);
  });

  it("is idempotent — calling enable twice invokes onLoad only once", () => {
    const onLoad = vi.fn();
    register(makeModule("h8e-enable-idem", { onLoad }));

    enableModule("h8e-enable-idem", false);
    enableModule("h8e-enable-idem", false);

    expect(onLoad).toHaveBeenCalledOnce();

    disableModule("h8e-enable-idem", false);
  });
});

// ─────────────────────────────────────────────────────────────────────────────
// backendAvailable() — feature-gate helper
// ─────────────────────────────────────────────────────────────────────────────

describe("backendAvailable()", () => {
  it("returns true when backendFeature is not declared", () => {
    // features = [] (pluginFeatures mock returns [] since initHost was not run).
    // A pure-frontend module with no backendFeature is always available.
    expect(backendAvailable(makeModule("h8e-ba-nobackend"))).toBe(true);
  });

  it("returns false when backendFeature is not in the compiled feature list", () => {
    expect(
      backendAvailable(
        makeModule("h8e-ba-missingfeature", { backendFeature: "absent-feature" }),
      ),
    ).toBe(false);
  });
});

// ─────────────────────────────────────────────────────────────────────────────
// ChairPhotoAPI.onEvent() — backend event subscription contract
// ─────────────────────────────────────────────────────────────────────────────

describe("ChairPhotoAPI.onEvent()", () => {
  /** Modules enabled by apiFromLoad, disabled again after each test. */
  const enabledHere: string[] = [];

  /**
   * Enable a throwaway module and return the host API it was handed. The module stays
   * enabled for the duration of the test — the contract should be exercised on a live
   * handle, not a stale one — and is disabled in afterEach.
   */
  function apiFromLoad(id: string): ChairPhotoAPI {
    let received: ChairPhotoAPI | undefined;
    register(makeModule(id, { onLoad: (api: ChairPhotoAPI) => void (received = api) }));
    enableModule(id, false);
    enabledHere.push(id);
    if (!received) throw new Error("onLoad did not run");
    return received;
  }

  beforeEach(() => {
    __events.reset();
  });

  afterEach(() => {
    while (enabledHere.length) disableModule(enabledHere.pop()!, false);
  });

  it("is present on the injected API", () => {
    expect(typeof apiFromLoad("evt-present").onEvent).toBe("function");
  });

  it("subscribes to the requested event name", async () => {
    const api = apiFromLoad("evt-name");
    await api.onEvent!("demo:progress", () => {});

    expect(__events.calls).toHaveLength(1);
    expect(__events.calls[0].event).toBe("demo:progress");
  });

  it("delivers the unwrapped payload, not Tauri's event envelope", async () => {
    const api = apiFromLoad("evt-payload");
    const seen: unknown[] = [];
    await api.onEvent!<{ done: number }>("demo:progress", (p) => seen.push(p));

    __events.emit("demo:progress", { done: 3 });

    // The module sees the payload itself — no { payload: … } wrapper leaks through.
    expect(seen).toEqual([{ done: 3 }]);
  });

  it("returns an Unsubscribe that actually stops delivery", async () => {
    const api = apiFromLoad("evt-unsub");
    const seen: unknown[] = [];
    const stop = await api.onEvent!<number>("demo:progress", (p) => seen.push(p));

    __events.emit("demo:progress", 1);
    expect(seen).toEqual([1]);

    expect(__events.unlistenCount).toBe(0);
    expect(stop()).toBeUndefined(); // contract-owned () => void, not Tauri's UnlistenFn
    expect(__events.unlistenCount).toBe(1);

    // The point of the test: after unsubscribing, further events must not arrive.
    __events.emit("demo:progress", 2);
    expect(seen).toEqual([1]);
  });

  it("keeps subscriptions independent per event name", async () => {
    const api = apiFromLoad("evt-multi");
    const a: unknown[] = [];
    const b: unknown[] = [];
    await api.onEvent!("one:progress", (p) => a.push(p));
    await api.onEvent!("two:progress", (p) => b.push(p));

    __events.emit("one:progress", 1);

    expect(a).toEqual([1]);
    expect(b).toEqual([]);
  });
});
