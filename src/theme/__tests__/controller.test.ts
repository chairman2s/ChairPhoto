// @vitest-environment jsdom
//
// theme/controller.ts: the appearance-policy layer used by both windows (App.tsx,
// LoupeWindow.tsx) — refreshAppearance()/applyResult() (which mode paints what) and
// setAppearanceMode()/getAppearanceMode()/subscribeAppearanceMode() (the mode-change side
// channel useAppearance() reacts to). The hook itself (useAppearance()) needs a React render
// harness and isn't covered here; this file exercises the plain functions it's built from.
//
// Needs jsdom (document, localStorage) — see vitest.config.ts's comment on opting a file
// into jsdom, and src/theme/__tests__/theme.test.ts's header for why the localStorage
// polyfill below is necessary (reused verbatim from that file, per this task's note that
// every file touching prefs.ts needs its own copy — vi.mock factories and module state
// aren't shared across test files).

import { beforeEach, describe, expect, it, vi } from "vitest";
import type { OmarchyPalette, SystemThemeResult } from "../tokens";

// Node ≥22 ships an experimental global `localStorage` that shadows jsdom's real one but
// throws on every call without a `--localstorage-file`. See theme.test.ts's header for the
// full explanation; this substitutes a real in-memory Storage so prefs.ts's reads/writes
// (exercised indirectly through refreshAppearance/setAppearanceMode below) are genuine.
if (typeof (globalThis as { localStorage?: Storage }).localStorage?.clear !== "function") {
  const backing = new Map<string, string>();
  const memoryStorage: Storage = {
    getItem: (key) => (backing.has(key) ? (backing.get(key) as string) : null),
    setItem: (key, value) => void backing.set(key, String(value)),
    removeItem: (key) => void backing.delete(key),
    clear: () => backing.clear(),
    key: (index) => Array.from(backing.keys())[index] ?? null,
    get length() {
      return backing.size;
    },
  };
  Object.defineProperty(globalThis, "localStorage", {
    value: memoryStorage,
    configurable: true,
    writable: true,
  });
}

// What the mocked `getSystemTheme()` resolves to for the currently-running test — set inside
// each `it()` before calling into the controller. `onThemeChanged` is mocked too (never
// actually invoked by the functions under test here; useAppearance() is the only consumer,
// and it isn't rendered in this file) so importing controller.ts never touches the real
// Tauri `listen` binding.
let systemThemeResult: SystemThemeResult = { available: false, themeName: null, palette: null };

vi.mock("../../modules/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../modules/api")>();
  return {
    ...actual,
    getSystemTheme: () => Promise.resolve(systemThemeResult),
    onThemeChanged: () => Promise.resolve(() => {}),
  };
});

import { applyResult, getAppearanceMode, refreshAppearance, setAppearanceMode, subscribeAppearanceMode } from "../controller";
import { STANDARD, STANDARD_MODE } from "../standard";
import { mapPalette } from "../omarchy";
import { loadAppearanceMode, storeAppearanceMode } from "../prefs";
import type { AppearanceMode } from "../tokens";

const PALETTE: OmarchyPalette = {
  mode: "dark",
  background: "#141414",
  foreground: "#e8e8e8",
  accent: "#4d8dff",
  selection: "#ff6600",
  muted: "#8a8a8a",
};

function resetDom(): void {
  document.documentElement.removeAttribute("style");
  delete document.documentElement.dataset.appearance;
}

beforeEach(() => {
  resetDom();
  localStorage.clear();
  systemThemeResult = { available: false, themeName: null, palette: null };
});

describe("applyResult", () => {
  it("available + palette applies mapped tokens tagged omarchy", () => {
    applyResult({ available: true, themeName: "tokyo-night", palette: PALETTE });
    const { tokens, mode } = mapPalette(PALETTE);
    expect(document.documentElement.dataset.appearance).toBe("omarchy");
    expect(document.documentElement.style.colorScheme).toBe(mode);
    expect(document.documentElement.style.getPropertyValue("--accent")).toBe(tokens.accent);
  });

  it("unavailable falls back to ChairPhoto Standard", () => {
    applyResult({ available: false, themeName: null, palette: null });
    expect(document.documentElement.dataset.appearance).toBe("standard");
    expect(document.documentElement.style.getPropertyValue("--accent")).toBe(STANDARD.accent);
    expect(document.documentElement.style.colorScheme).toBe(STANDARD_MODE);
  });

  it("available but no palette (should never happen per docs/appearance.md) still falls back", () => {
    applyResult({ available: true, themeName: "odd", palette: null });
    expect(document.documentElement.dataset.appearance).toBe("standard");
  });
});

describe("refreshAppearance", () => {
  it("standard mode applies ChairPhoto Standard without consulting the backend", async () => {
    storeAppearanceMode("standard");
    // Even though a valid theme is "available", standard mode must not use it.
    systemThemeResult = { available: true, themeName: "tokyo-night", palette: PALETTE };
    await refreshAppearance();
    expect(document.documentElement.dataset.appearance).toBe("standard");
  });

  it("follow-omarchy + unavailable falls back to Standard", async () => {
    storeAppearanceMode("follow-omarchy");
    systemThemeResult = { available: false, themeName: null, palette: null };
    await refreshAppearance();
    expect(document.documentElement.dataset.appearance).toBe("standard");
  });

  it("follow-omarchy + available applies the mapped tokens", async () => {
    storeAppearanceMode("follow-omarchy");
    systemThemeResult = { available: true, themeName: "tokyo-night", palette: PALETTE };
    await refreshAppearance();
    const { tokens } = mapPalette(PALETTE);
    expect(document.documentElement.dataset.appearance).toBe("omarchy");
    expect(document.documentElement.style.getPropertyValue("--txt")).toBe(tokens.txt);
  });
});

describe("setAppearanceMode", () => {
  it("persists the mode and notifies subscribers", () => {
    storeAppearanceMode("follow-omarchy"); // start from a known value
    const seen: AppearanceMode[] = [];
    const unsubscribe = subscribeAppearanceMode((m) => seen.push(m));

    setAppearanceMode("standard");

    expect(seen).toEqual(["standard"]);
    expect(getAppearanceMode()).toBe("standard");
    expect(loadAppearanceMode()).toBe("standard");
    unsubscribe();
  });

  it("a subscriber released via its unsubscribe is not notified by a later call", () => {
    const seen: AppearanceMode[] = [];
    const unsubscribe = subscribeAppearanceMode((m) => seen.push(m));
    unsubscribe();

    setAppearanceMode("follow-omarchy");

    expect(seen).toEqual([]);
  });

  it("applies the new mode immediately (standard case, synchronous)", () => {
    setAppearanceMode("standard");
    expect(document.documentElement.dataset.appearance).toBe("standard");
  });
});
