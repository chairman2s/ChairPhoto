import { STANDARD, STANDARD_MODE } from "./standard";
import { TOKEN_NAMES, type ThemeMode, type ThemeTokens } from "./tokens";

/**
 * Stamp a palette onto `:root`: every custom property in `tokens`, `color-scheme` (so native
 * controls — scrollbars, dropdowns, checkboxes — render to match), and
 * `data-appearance` (for CSS/debugging that needs to know which palette *source* is active,
 * distinct from `mode`'s light/dark).
 *
 * `appearance` defaults to "standard" since ChairPhoto Standard is the only palette
 * implemented so far; the Follow-Omarchy task passes "omarchy" once it derives live tokens.
 *
 * Must be safe to call before React mounts (main.tsx does, so both the main window and the
 * pop-out loupe window get the palette before first paint) and inside jsdom (no Tauri import
 * here, in ./tokens, or in ./standard).
 */
export function applyTokens(
  tokens: ThemeTokens,
  mode: ThemeMode,
  appearance: "standard" | "omarchy" = "standard",
): void {
  const root = document.documentElement;
  for (const name of TOKEN_NAMES) {
    root.style.setProperty(`--${name}`, tokens[name]);
  }
  root.style.colorScheme = mode;
  root.dataset.appearance = appearance;
}

/** Convenience: apply ChairPhoto Standard. */
export function applyStandard(): void {
  applyTokens(STANDARD, STANDARD_MODE, "standard");
}
