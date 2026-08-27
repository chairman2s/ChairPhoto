// mapPalette (../omarchy.ts): the Omarchy → ThemeTokens role mapping and its contrast guard.
//
// The assertions below deliberately avoid hand-computed hex/float literals for anything that
// passes through the contrast guard (txt/dim/mute/accent) — that guard's exact output depends
// on how many 5% steps it took, which is an implementation detail, not the contract. Instead
// those roles are checked against the *contract* (docs/appearance.md's stated thresholds) via
// a small independent reference implementation of the WCAG math below, so the tests verify
// "the guarantee holds" rather than "the guard's arithmetic matches what I predicted by hand".
// Roles the guard never touches (canvas/elev/well/border/line/sel/panel) are asserted exactly,
// since their formulas are pure, one-shot blends with no clamping to make non-deterministic.
//
// No jsdom needed — mapPalette is pure (see the module's own header), so this runs in the
// suite's default "node" environment.

import { describe, expect, it } from "vitest";
import { mapPalette } from "../omarchy";
import { STANDARD } from "../standard";
import type { OmarchyPalette } from "../tokens";

// ---- independent reference color math (re-derived from docs/appearance.md + the task's
// role-mapping spec, not copied from omarchy.ts) ---------------------------------------------

type RGB = [number, number, number];

function parseHex(hex: string): RGB {
  const s = hex.replace(/^#/, "");
  const full = s.length === 3
    ? s.split("").map((c) => c + c).join("")
    : s.slice(0, 6);
  return [
    parseInt(full.slice(0, 2), 16),
    parseInt(full.slice(2, 4), 16),
    parseInt(full.slice(4, 6), 16),
  ];
}

function toHex([r, g, b]: RGB): string {
  const h = (n: number) => Math.round(Math.min(255, Math.max(0, n))).toString(16).padStart(2, "0");
  return `#${h(r)}${h(g)}${h(b)}`;
}

/** Normalize any parseable hex to lowercase #rrggbb — mirrors what every mapPalette output
 *  token must look like, independent of how omarchy.ts implements it. */
function normalizeHex(hex: string): string {
  return toHex(parseHex(hex));
}

/** `ratio` is the weight of `b`, matching the spec's "mix(a X%, b Y%)" convention. */
function refMix(a: string, b: string, ratio: number): string {
  const [ar, ag, ab] = parseHex(a);
  const [br, bg, bb] = parseHex(b);
  return toHex([ar + (br - ar) * ratio, ag + (bg - ag) * ratio, ab + (bb - ab) * ratio]);
}

function srgbToLinear(channel: number): number {
  const v = channel / 255;
  return v <= 0.03928 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4);
}

function relLuminance(hex: string): number {
  const [r, g, b] = parseHex(hex);
  return 0.2126 * srgbToLinear(r) + 0.7152 * srgbToLinear(g) + 0.0722 * srgbToLinear(b);
}

function wcagContrast(a: string, b: string): number {
  const la = relLuminance(a);
  const lb = relLuminance(b);
  const hi = Math.max(la, lb);
  const lo = Math.min(la, lb);
  return (hi + 0.05) / (lo + 0.05);
}

/** The #000000/#ffffff pole *not* already close to `fgHex` — mirrors omarchy.ts's
 *  poleAwayFrom, independently re-derived from its stated behavior. */
function poleAwayFrom(fgHex: string): string {
  return relLuminance(fgHex) > 0.5 ? "#000000" : "#ffffff";
}

const HEX6 = /^#[0-9a-f]{6}$/;

// ---- fixtures --------------------------------------------------------------------------

// Every optional field populated with a distinct value, and each guarded role's naive value
// chosen to already clear its threshold — the "well-behaved palette" case, where the guard is
// a no-op and the mapping table's identity/override formulas can be checked directly.
// darkForeground is pinned to pure white so its "prefers the override" check is
// contrast-guard-proof by construction (white against a dark panel always passes 4.5, no
// arithmetic needed to know that).
const DARK: OmarchyPalette = {
  mode: "dark",
  background: "#141414",
  foreground: "#e8e8e8",
  accent: "#4d8dff",
  selection: "#ff6600",
  muted: "#8a8a8a",
  darkerBackground: "#0a0a0a",
  lighterBackground: "#1e1e1e",
  darkForeground: "#ffffff",
  green: "#3ddc84",
  red: "#ff5555",
  yellow: "#ffcc33",
};

// Light mode, every optional field omitted — exercises every `?? formula` branch and the
// STANDARD fallback for ok/danger/rating.
const LIGHT: OmarchyPalette = {
  mode: "light",
  background: "#f5f5f0",
  foreground: "#1a1a1a",
  accent: "#3366cc",
  selection: "#ffd54f",
  muted: "#767676",
};

describe("mapPalette — full dark palette", () => {
  const { tokens, mode } = mapPalette(DARK);

  it("returns the palette's own mode", () => {
    expect(mode).toBe("dark");
  });

  it("maps identity roles verbatim, normalized", () => {
    expect(tokens.panel).toBe(normalizeHex(DARK.background));
    expect(tokens.mute).toBe(normalizeHex(DARK.muted));
    expect(tokens.ok).toBe(normalizeHex(DARK.green as string));
    expect(tokens.danger).toBe(normalizeHex(DARK.red as string));
    expect(tokens.rating).toBe(normalizeHex(DARK.yellow as string));
  });

  it("prefers the optional override over the derived formula (unguarded roles)", () => {
    expect(tokens.canvas).toBe(normalizeHex(DARK.darkerBackground as string));
    expect(tokens.elev).toBe(normalizeHex(DARK.lighterBackground as string));
  });

  it("prefers the optional override for dim, a guarded role (white always clears 4.5)", () => {
    expect(tokens.dim).toBe(normalizeHex(DARK.darkForeground as string));
  });

  it("derives well/border/line/sel by their stated blend", () => {
    expect(tokens.well).toBe(refMix(tokens.canvas, "#000000", 0.2)); // dark-mode ratio
    expect(tokens.border).toBe(
      refMix(normalizeHex(DARK.background), normalizeHex(DARK.foreground), 0.14),
    );
    expect(tokens.line).toBe(
      refMix(normalizeHex(DARK.background), normalizeHex(DARK.foreground), 0.07),
    );
    expect(tokens.sel).toBe(
      refMix(normalizeHex(DARK.selection), normalizeHex(DARK.background), 0.7),
    );
  });

  it("scrim and font tokens are ChairPhoto Standard's, untouched", () => {
    expect(tokens.scrim).toBe(STANDARD.scrim);
    expect(tokens["font-sans"]).toBe(STANDARD["font-sans"]);
    expect(tokens["font-display"]).toBe(STANDARD["font-display"]);
  });

  it("meets the contrast guard's thresholds", () => {
    expect(wcagContrast(tokens.txt, tokens.canvas)).toBeGreaterThanOrEqual(4.5);
    expect(wcagContrast(tokens.txt, tokens.panel)).toBeGreaterThanOrEqual(4.5);
    expect(wcagContrast(tokens.dim, tokens.panel)).toBeGreaterThanOrEqual(4.5);
    expect(wcagContrast(tokens.mute, tokens.panel)).toBeGreaterThanOrEqual(3.0);
    expect(wcagContrast(tokens.accent, tokens.canvas)).toBeGreaterThanOrEqual(3.0);
  });

  it("every color token is normalized lowercase #rrggbb", () => {
    for (const [key, value] of Object.entries(tokens)) {
      if (key === "scrim" || key === "font-sans" || key === "font-display") continue;
      expect(value).toMatch(HEX6);
    }
  });
});

describe("mapPalette — light palette, every optional omitted", () => {
  const { tokens, mode } = mapPalette(LIGHT);

  it("returns the palette's own mode", () => {
    expect(mode).toBe("light");
  });

  it("falls back to ChairPhoto Standard for ok/danger/rating", () => {
    expect(tokens.ok).toBe(STANDARD.ok);
    expect(tokens.danger).toBe(STANDARD.danger);
    expect(tokens.rating).toBe(STANDARD.rating);
  });

  it("derives canvas/elev/well/border/line by the stated formula (unguarded roles)", () => {
    expect(tokens.canvas).toBe(
      refMix(normalizeHex(LIGHT.background), poleAwayFrom(LIGHT.foreground), 0.15),
    );
    expect(tokens.elev).toBe(
      refMix(normalizeHex(LIGHT.background), normalizeHex(LIGHT.foreground), 0.08),
    );
    expect(tokens.well).toBe(refMix(tokens.canvas, "#000000", 0.08)); // light-mode ratio
    expect(tokens.border).toBe(
      refMix(normalizeHex(LIGHT.background), normalizeHex(LIGHT.foreground), 0.14),
    );
    expect(tokens.line).toBe(
      refMix(normalizeHex(LIGHT.background), normalizeHex(LIGHT.foreground), 0.07),
    );
  });

  it("derives dim from the formula, distinct from plain foreground (guarded role)", () => {
    expect(tokens.dim).not.toBe(normalizeHex(LIGHT.foreground));
  });

  it("meets the contrast guard's thresholds", () => {
    expect(wcagContrast(tokens.txt, tokens.canvas)).toBeGreaterThanOrEqual(4.5);
    expect(wcagContrast(tokens.txt, tokens.panel)).toBeGreaterThanOrEqual(4.5);
    expect(wcagContrast(tokens.dim, tokens.panel)).toBeGreaterThanOrEqual(4.5);
    expect(wcagContrast(tokens.mute, tokens.panel)).toBeGreaterThanOrEqual(3.0);
    expect(wcagContrast(tokens.accent, tokens.canvas)).toBeGreaterThanOrEqual(3.0);
  });

  it("every color token is normalized lowercase #rrggbb (ok/danger/rating excepted: LIGHT " +
    "omits green/red/yellow, so those fall back to STANDARD's own values verbatim, " +
    "unnormalized by design — see omarchy.ts's role-mapping comment)", () => {
    for (const [key, value] of Object.entries(tokens)) {
      if (["scrim", "font-sans", "font-display", "ok", "danger", "rating"].includes(key)) continue;
      expect(value).toMatch(HEX6);
    }
  });
});

describe("contrast clamp", () => {
  it("pushes low-contrast txt/dim/mute/accent until each pair clears its threshold", () => {
    // background/foreground/accent/muted are all close, dark grays — comfortably below
    // every threshold before the guard runs.
    const lowContrast: OmarchyPalette = {
      mode: "dark",
      background: "#202020",
      foreground: "#282828",
      accent: "#252525",
      selection: "#ff0000",
      muted: "#242424",
    };
    const { tokens } = mapPalette(lowContrast);

    // Prove the guard had work to do: the *naive* (pre-guard) values fail, checked against
    // the actual computed canvas/panel rather than a hand-derived one.
    expect(wcagContrast(normalizeHex(lowContrast.foreground), tokens.canvas)).toBeLessThan(4.5);
    expect(wcagContrast(normalizeHex(lowContrast.muted), tokens.panel)).toBeLessThan(3.0);
    expect(wcagContrast(normalizeHex(lowContrast.accent), tokens.canvas)).toBeLessThan(3.0);

    // And that the guarded output passes.
    expect(wcagContrast(tokens.txt, tokens.canvas)).toBeGreaterThanOrEqual(4.5);
    expect(wcagContrast(tokens.txt, tokens.panel)).toBeGreaterThanOrEqual(4.5);
    expect(wcagContrast(tokens.dim, tokens.panel)).toBeGreaterThanOrEqual(4.5);
    expect(wcagContrast(tokens.mute, tokens.panel)).toBeGreaterThanOrEqual(3.0);
    expect(wcagContrast(tokens.accent, tokens.canvas)).toBeGreaterThanOrEqual(3.0);
  });

  it("is deterministic and total — never throws even when 20 steps can't fully separate", () => {
    const extreme: OmarchyPalette = {
      mode: "light",
      background: "#808080",
      foreground: "#7f7f7f", // one unit off background — as close to a wash as this contract allows
      accent: "#818181",
      selection: "#808080",
      muted: "#7e7e7e",
    };
    expect(() => mapPalette(extreme)).not.toThrow();
    const { tokens } = mapPalette(extreme);
    // ok/danger/rating excepted for the same reason as the light-palette test above:
    // `extreme` omits green/red/yellow too, so those fall back to STANDARD's own casing.
    for (const [key, value] of Object.entries(tokens)) {
      if (["scrim", "font-sans", "font-display", "ok", "danger", "rating"].includes(key)) continue;
      expect(value).toMatch(HEX6);
    }
  });
});

describe("onaccent / onok contrast pick", () => {
  // A mid-gray canvas comfortably contrasts against both a near-black and a near-white
  // accent (>3.0 either way), so the accent/canvas guard never fires here — onaccent's
  // input is guaranteed to be the raw accent given, and the pick direction is unambiguous.
  const withAccent = (accent: string): OmarchyPalette => ({
    mode: "dark",
    background: "#202020",
    foreground: "#e0e0e0",
    accent,
    selection: "#ff0000",
    muted: "#888888",
    darkerBackground: "#808080",
  });

  it("picks a near-white tint against a near-black accent", () => {
    const { tokens } = mapPalette(withAccent("#050505"));
    expect(tokens.accent).toBe("#050505"); // confirms the guard left it untouched
    expect(relLuminance(tokens.onaccent)).toBeGreaterThan(relLuminance(tokens.accent));
    expect(wcagContrast(tokens.onaccent, tokens.accent)).toBeGreaterThan(3);
  });

  it("picks a near-black tint against a near-white accent", () => {
    const { tokens } = mapPalette(withAccent("#fafafa"));
    expect(tokens.accent).toBe("#fafafa");
    expect(relLuminance(tokens.onaccent)).toBeLessThan(relLuminance(tokens.accent));
    expect(wcagContrast(tokens.onaccent, tokens.accent)).toBeGreaterThan(3);
  });

  it("onok is picked the same way, against ok — unguarded, so no shielding fixture needed", () => {
    const { tokens: nearBlackOk } = mapPalette({ ...DARK, green: "#050505" });
    expect(relLuminance(nearBlackOk.onok)).toBeGreaterThan(relLuminance(nearBlackOk.ok));

    const { tokens: nearWhiteOk } = mapPalette({ ...DARK, green: "#fafafa" });
    expect(relLuminance(nearWhiteOk.onok)).toBeLessThan(relLuminance(nearWhiteOk.ok));
  });
});

describe("sel pre-blend", () => {
  it("is a solid, normalized hex color between selection and background — not rgba", () => {
    const { tokens } = mapPalette(DARK);
    expect(tokens.sel).toMatch(HEX6);
    expect(tokens.sel).toBe(refMix(normalizeHex(DARK.selection), normalizeHex(DARK.background), 0.7));
    expect(tokens.sel).not.toBe(normalizeHex(DARK.selection));
    expect(tokens.sel).not.toBe(normalizeHex(DARK.background));
  });
});
