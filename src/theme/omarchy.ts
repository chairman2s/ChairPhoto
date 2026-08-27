// Follow Omarchy: maps a backend-reported OmarchyPalette onto ChairPhoto's semantic
// ThemeTokens contract (tokens.ts). Pure module — no Tauri imports, no DOM — so it runs in
// plain Node/jsdom tests and can be unit-tested independently of the controller that decides
// *when* to call it (./controller.ts).
//
// Mixing convention used throughout the role-mapping table below: `mix(a, b, ratio)` blends
// sRGB channels with `ratio` as the weight of `b` — `mix(x, y, 0.15)` is "85% x, 15% y",
// matching how each formula's comment states its percentages (first color keeps the first
// percentage, second color the second).

import { STANDARD } from "./standard";
import type { OmarchyPalette, ThemeMode, ThemeTokens } from "./tokens";

// ---- color utilities (in-module, deliberately not exported — see docs/appearance.md and
// the module header above for why this stays pure and self-contained) -----------------------

interface RGB {
  r: number;
  g: number;
  b: number;
}

const BLACK: RGB = { r: 0, g: 0, b: 0 };
const WHITE: RGB = { r: 255, g: 255, b: 255 };

function clamp(n: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, n));
}

/**
 * Parse `#rgb`, `#rrggbb`, or `#rrggbbaa` (case-insensitive; alpha is discarded — every
 * token this module produces is a solid color). Never throws: the backend already validates
 * every color in a `SystemThemeResult` before this module ever sees it (docs/appearance.md
 * — "one invalid value invalidates the whole palette"), so a structurally-unparseable input
 * here would mean that contract broke upstream. Falling back to black keeps this module
 * total anyway rather than propagating a crash into paint.
 */
function parseHex(input: string): RGB {
  const s = input.trim().replace(/^#/, "");
  if (s.length === 3) {
    return {
      r: parseInt(s[0] + s[0], 16),
      g: parseInt(s[1] + s[1], 16),
      b: parseInt(s[2] + s[2], 16),
    };
  }
  if (s.length === 6 || s.length === 8) {
    return {
      r: parseInt(s.slice(0, 2), 16),
      g: parseInt(s.slice(2, 4), 16),
      b: parseInt(s.slice(4, 6), 16),
    };
  }
  return { r: 0, g: 0, b: 0 };
}

function toHex(c: RGB): string {
  const h = (n: number) => Math.round(clamp(n, 0, 255)).toString(16).padStart(2, "0");
  return `#${h(c.r)}${h(c.g)}${h(c.b)}`;
}

/** Normalize any parseable hex form to lowercase `#rrggbb`. */
function normalize(hex: string): string {
  return toHex(parseHex(hex));
}

function mixRgb(a: RGB, b: RGB, ratio: number): RGB {
  const t = clamp(ratio, 0, 1);
  return {
    r: a.r + (b.r - a.r) * t,
    g: a.g + (b.g - a.g) * t,
    b: a.b + (b.b - a.b) * t,
  };
}

/** Hex-in, hex-out blend — the shape every formula in the role-mapping table is written against. */
function mix(a: string, b: string, ratio: number): string {
  return toHex(mixRgb(parseHex(a), parseHex(b), ratio));
}

function srgbToLinear(channel: number): number {
  const v = channel / 255;
  return v <= 0.03928 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4);
}

/** WCAG relative luminance, 0 (black) – 1 (white). */
function relativeLuminance(c: RGB): number {
  return 0.2126 * srgbToLinear(c.r) + 0.7152 * srgbToLinear(c.g) + 0.0722 * srgbToLinear(c.b);
}

/** WCAG contrast ratio between two colors: 1 (no contrast) – 21 (max). */
function contrastRatio(hexA: string, hexB: string): number {
  const la = relativeLuminance(parseHex(hexA));
  const lb = relativeLuminance(parseHex(hexB));
  const lighter = Math.max(la, lb);
  const darker = Math.min(la, lb);
  return (lighter + 0.05) / (darker + 0.05);
}

/** The #000000/#ffffff pole *not* already close to `fgHex` — i.e. the one mixing toward
 *  moves a background color away from the foreground's own brightness. */
function poleAwayFrom(fgHex: string): RGB {
  return relativeLuminance(parseHex(fgHex)) > 0.5 ? BLACK : WHITE;
}

/** The pole a mode pushes low-contrast foreground roles toward: white in dark mode
 *  (lightening text/accents against a dark surface), black in light mode. */
function contrastPoleForMode(mode: ThemeMode): RGB {
  return mode === "dark" ? WHITE : BLACK;
}

const MAX_CONTRAST_STEPS = 20;
const CONTRAST_STEP = 0.05;

/**
 * Contrast guard: push `hex` toward `pole` in 5% steps (max 20 — a full walk to the pole)
 * until its contrast against `against` reaches `minRatio`, or the steps run out.
 *
 * Deliberate deviation from the product plan's original "substitute the Standard token"
 * fallback: that would inject a *dark-theme* value into a light palette (or vice versa) the
 * moment one role failed its check, which is a worse mismatch than a slightly-adjusted
 * theme color. Clamping stepwise instead keeps the result recognizably the theme's own
 * color family. Deterministic and total: for a structurally valid palette this always
 * returns (worst case, the pole itself after 20 steps), never throws, and never loops
 * unboundedly.
 */
function ensureContrast(hex: string, against: string, minRatio: number, pole: RGB): string {
  let current = hex;
  for (let i = 0; i < MAX_CONTRAST_STEPS; i++) {
    if (contrastRatio(current, against) >= minRatio) return current;
    current = toHex(mixRgb(parseHex(current), pole, CONTRAST_STEP));
  }
  return current;
}

/**
 * onaccent/onok: pick whichever of a near-black or near-white tint of `fillHex` contrasts
 * better against that fill, so text/icons drawn on an accent or ok chip stay legible
 * regardless of how light or dark the theme's accent/green happens to be. The near-white
 * formula mirrors the near-black one the spec states explicitly (15% fill mixed into the
 * pole) — the spec names only the near-black ratio, so this symmetry is a mapping decision
 * rather than a stated requirement.
 */
function contrastOn(fillHex: string): string {
  const nearBlack = mix(fillHex, "#000000", 0.85);
  const nearWhite = mix(fillHex, "#ffffff", 0.85);
  return contrastRatio(nearBlack, fillHex) >= contrastRatio(nearWhite, fillHex)
    ? nearBlack
    : nearWhite;
}

// ---- role mapping -------------------------------------------------------------------------

/**
 * Map an Omarchy theme's palette onto ChairPhoto's ThemeTokens contract. Pure and total: any
 * structurally valid `OmarchyPalette` (the shape `get_system_theme` / `appearance:theme_changed`
 * already guarantee — see docs/appearance.md) produces a full, contrast-guarded token set.
 */
export function mapPalette(p: OmarchyPalette): { tokens: ThemeTokens; mode: ThemeMode } {
  const mode = p.mode;
  const modePole = contrastPoleForMode(mode);
  const awayPole = poleAwayFrom(p.foreground);

  const panel = normalize(p.background);
  const canvas =
    p.darkerBackground != null
      ? normalize(p.darkerBackground)
      : mix(p.background, toHex(awayPole), 0.15);
  const elev =
    p.lighterBackground != null
      ? normalize(p.lighterBackground)
      : mix(p.background, p.foreground, 0.08);
  const well = mode === "dark" ? mix(canvas, "#000000", 0.2) : mix(canvas, "#000000", 0.08);
  const border = mix(p.background, p.foreground, 0.14);
  const line = mix(p.background, p.foreground, 0.07);
  const dim =
    p.darkForeground != null
      ? normalize(p.darkForeground)
      : mix(p.foreground, p.background, 0.28);
  const mute = normalize(p.muted);
  const sel = mix(p.selection, p.background, 0.7);
  const ok = p.green != null ? normalize(p.green) : STANDARD.ok;
  const danger = p.red != null ? normalize(p.red) : STANDARD.danger;
  const rating = p.yellow != null ? normalize(p.yellow) : STANDARD.rating;

  // Contrast guard (docs above): only the foreground-ish roles that sit on top of a
  // background need it. `canvas`/`panel`/`elev`/`well`/`border`/`line` are never the
  // subject of a pair, only the "against" side, so they pass through unguarded.
  let txt = normalize(p.foreground);
  txt = ensureContrast(txt, canvas, 4.5, modePole);
  txt = ensureContrast(txt, panel, 4.5, modePole);
  const dimGuarded = ensureContrast(dim, panel, 4.5, modePole);
  const muteGuarded = ensureContrast(mute, panel, 3.0, modePole);
  let accent = normalize(p.accent);
  accent = ensureContrast(accent, canvas, 3.0, modePole);

  // onaccent/onok are derived from the *guarded* accent/ok so the "on" color stays legible
  // against the fill ChairPhoto actually paints, not a pre-clamp value it discarded.
  const onaccent = contrastOn(accent);
  const onok = contrastOn(ok);

  const tokens: ThemeTokens = {
    canvas,
    panel,
    elev,
    well,
    border,
    line,
    txt,
    dim: dimGuarded,
    mute: muteGuarded,
    accent,
    onaccent,
    sel,
    ok,
    onok,
    danger,
    rating,
    // Over-photo scrims and font stacks never theme — always ChairPhoto Standard's values,
    // copied verbatim (not renormalized: STANDARD.scrim is rgba(), not hex).
    scrim: STANDARD.scrim,
    "font-sans": STANDARD["font-sans"],
    "font-display": STANDARD["font-display"],
  };

  return { tokens, mode };
}
