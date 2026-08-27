// The semantic design-token contract for ChairPhoto's UI.
//
// Every themeable surface should read one of these custom properties (or, until the CSS
// migration lands, one of the legacy `--bg`/`--text`/… aliases defined alongside them in
// App.css's `:root`) rather than a hardcoded color. "ChairPhoto Standard" (./standard.ts) is
// the app-owned palette implemented so far; a later task adds live "Follow Omarchy" palettes
// derived from the user's system theme, over this same contract.

/** Every token name, in the exact order the app's `:root` block declares them. */
export const TOKEN_NAMES = [
  "canvas",
  "panel",
  "elev",
  "well",
  "border",
  "line",
  "txt",
  "dim",
  "mute",
  "accent",
  "onaccent",
  "sel",
  "ok",
  "onok",
  "danger",
  "rating",
  "scrim",
  "font-sans",
  "font-display",
] as const;

export type TokenName = (typeof TOKEN_NAMES)[number];

/** One CSS value per token. A palette (STANDARD, and later an Omarchy-derived one) is this shape. */
export type ThemeTokens = Record<TokenName, string>;

/** Whether a palette is dark or light — drives `color-scheme` so native controls (scrollbars,
 *  dropdowns, checkboxes) render to match rather than defaulting to the OS theme. */
export type ThemeMode = "light" | "dark";

/**
 * Which palette source the user has chosen in Preferences → Appearance.
 *
 * "follow-omarchy": derive the palette live from the user's Omarchy/system theme. This is the
 * product default, but the detection itself is a follow-up task — until it lands, choosing
 * this mode still renders ChairPhoto Standard (see applyStandard() and Preferences.tsx).
 *
 * "standard": always render ChairPhoto Standard, regardless of the system theme.
 */
export type AppearanceMode = "follow-omarchy" | "standard";

/**
 * The subset of an Omarchy theme's palette ChairPhoto maps onto its tokens. Field names and
 * casing match the backend's system-theme command JSON exactly (camelCase) — this is the
 * frontend's contract with that payload, not a UI convenience shape. Unused until the
 * Follow-Omarchy task wires up detection and a tokens.ts → OmarchyPalette mapping.
 */
export interface OmarchyPalette {
  mode: ThemeMode;
  accent: string;
  selection: string;
  muted: string;
  background: string;
  foreground: string;
  darkBackground?: string | null;
  darkerBackground?: string | null;
  lighterBackground?: string | null;
  darkForeground?: string | null;
  lightForeground?: string | null;
  brightForeground?: string | null;
  red?: string | null;
  yellow?: string | null;
  green?: string | null;
  cyan?: string | null;
  blue?: string | null;
  magenta?: string | null;
  brightRed?: string | null;
  brightYellow?: string | null;
  brightGreen?: string | null;
  brightCyan?: string | null;
  brightBlue?: string | null;
  brightMagenta?: string | null;
}

/** The backend's system-theme detection result: whether an Omarchy theme is available at all. */
export interface SystemThemeResult {
  available: boolean;
  themeName: string | null;
  palette: OmarchyPalette | null;
}
