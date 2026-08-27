// @vitest-environment jsdom
//
// The theme token contract (tokens.ts), the ChairPhoto Standard palette (standard.ts), the
// pre-mount apply step (apply.ts), and the persisted appearance-mode preference (prefs.ts).
//
// Needs jsdom (document, localStorage) — see vitest.config.ts's comment on opting a file
// into jsdom via the leading `@vitest-environment` directive; the suite default is "node".

import { beforeEach, describe, expect, it } from "vitest";
import { STANDARD, STANDARD_MODE } from "../standard";
import { TOKEN_NAMES } from "../tokens";
import { applyStandard, applyTokens } from "../apply";
import { loadAppearanceMode, storeAppearanceMode } from "../prefs";

// Node ≥22 ships an experimental global `localStorage` (`node --help` → `--webstorage`),
// backed by a `--localstorage-file` this project's scripts never set. Under vitest's jsdom
// environment that global wins the race against jsdom's own Storage — same accessor property
// on globalThis, `configurable: true` — but every method throws "is not a function" without a
// valid backing file. prefs.ts's try/catch swallows that silently (by design: it must not
// throw in a browser with storage disabled), which would make every test below pass
// vacuously — "round-trips" would "pass" while every read and write actually failed. Detect
// the broken accessor once and substitute a real in-memory Storage so the tests below
// exercise actual persistence.
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

// Read the real App.css source, the same way src/__tests__/bodyColumns.test.ts does — a
// `?raw` import.meta.glob, un-stubbed for App.css specifically by vitest.config.ts's
// `css.include`. This is what keeps test (3) below an actual cross-check of the shipped
// stylesheet rather than a copy that can drift from it.
const APP_CSS = (
  import.meta.glob("../../App.css", { query: "?raw", eager: true, import: "default" }) as Record<
    string,
    string
  >
)["../../App.css"];

/** The declaration list inside the file's (single) `:root { … }` block. */
function rootBlock(): string {
  const match = APP_CSS.match(/:root\s*\{([^}]*)\}/);
  if (!match) throw new Error("App.css has no :root block");
  return match[1];
}

/** The value App.css's `:root` assigns to `--<name>`, or null if the property isn't declared. */
function cssValue(name: string): string | null {
  // Matches up to the terminating `;` — every token value here is a single declaration with
  // no semicolons inside it (rgba(...) and font stacks included), so this is unambiguous.
  const re = new RegExp(`--${name}\\s*:\\s*([^;]+);`);
  const match = rootBlock().match(re);
  return match ? match[1].trim() : null;
}

describe("theme tokens", () => {
  it("STANDARD defines exactly the tokens in TOKEN_NAMES — no missing, no extra", () => {
    const standardKeys = Object.keys(STANDARD).sort();
    const tokenNames = [...TOKEN_NAMES].sort();
    expect(standardKeys).toEqual(tokenNames);
  });

  it("STANDARD_MODE is dark", () => {
    expect(STANDARD_MODE).toBe("dark");
  });
});

describe("applyTokens", () => {
  beforeEach(() => {
    // Clear whatever a previous test in this file stamped, so each test observes only its
    // own call.
    document.documentElement.removeAttribute("style");
    delete document.documentElement.dataset.appearance;
  });

  it("stamps every token as a custom property on documentElement.style", () => {
    applyTokens(STANDARD, STANDARD_MODE);
    for (const name of TOKEN_NAMES) {
      expect(document.documentElement.style.getPropertyValue(`--${name}`)).toBe(
        STANDARD[name],
      );
    }
  });

  it("sets style.colorScheme to the given mode", () => {
    applyTokens(STANDARD, "light");
    expect(document.documentElement.style.colorScheme).toBe("light");
    applyTokens(STANDARD, "dark");
    expect(document.documentElement.style.colorScheme).toBe("dark");
  });

  it("stamps data-appearance, defaulting to standard", () => {
    applyTokens(STANDARD, STANDARD_MODE);
    expect(document.documentElement.dataset.appearance).toBe("standard");

    applyTokens(STANDARD, STANDARD_MODE, "omarchy");
    expect(document.documentElement.dataset.appearance).toBe("omarchy");
  });

  it("applyStandard() applies STANDARD at STANDARD_MODE, tagged standard", () => {
    applyStandard();
    expect(document.documentElement.style.getPropertyValue("--accent")).toBe(STANDARD.accent);
    expect(document.documentElement.style.colorScheme).toBe(STANDARD_MODE);
    expect(document.documentElement.dataset.appearance).toBe("standard");
  });

  it("is safe to call before any component has mounted (no DOM beyond documentElement)", () => {
    // documentElement always exists in jsdom without a mount; this is the pre-mount shape
    // main.tsx actually calls into.
    expect(() => applyStandard()).not.toThrow();
  });
});

describe("appearance mode preference", () => {
  beforeEach(() => {
    localStorage.clear();
  });

  it("defaults to follow-omarchy when nothing is stored", () => {
    expect(loadAppearanceMode()).toBe("follow-omarchy");
  });

  it("round-trips a stored value", () => {
    storeAppearanceMode("standard");
    expect(loadAppearanceMode()).toBe("standard");

    storeAppearanceMode("follow-omarchy");
    expect(loadAppearanceMode()).toBe("follow-omarchy");
  });

  it("falls back to follow-omarchy for an invalid stored value", () => {
    localStorage.setItem("appearance.mode", "some-garbage-value");
    expect(loadAppearanceMode()).toBe("follow-omarchy");
  });
});

describe("App.css :root stays in lockstep with STANDARD", () => {
  for (const name of TOKEN_NAMES) {
    it(`--${name} matches STANDARD["${name}"]`, () => {
      expect(cssValue(name)).toBe(STANDARD[name]);
    });
  }

  it("declares color-scheme: dark", () => {
    expect(rootBlock()).toMatch(/color-scheme\s*:\s*dark\s*;/);
  });
});
