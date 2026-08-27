// The one place appearance *policy* lives: which palette source is active, how a detection
// result becomes painted tokens, and the live-update wiring both windows share. tokens.ts /
// standard.ts / apply.ts / omarchy.ts are the mechanism; this module decides when to use
// which. Two consumers: Preferences (reads/writes the mode) and useAppearance() (applies it,
// in both the main window and the pop-out loupe window — see App.tsx / LoupeWindow.tsx).
//
// Each window is its own JS runtime (a separate WebView, not a shared module instance), so
// the mode-change notification below only reaches subscribers *in the same window* — it is
// not a cross-window bridge. Preferences lives in the main window, so a mode change there
// applies immediately in that window; a pop-out loupe window picks up the new mode from
// localStorage the next time it mounts, and in the meantime keeps following Omarchy live
// (the actual theme-switch broadcast) exactly as before, unaffected by this gap.

import { useEffect, useState } from "react";
import { getSystemTheme, onThemeChanged } from "../modules/api";
import { useOwnedSubscription } from "../modules/ownedEvents";
import { applyStandard, applyTokens } from "./apply";
import { mapPalette } from "./omarchy";
import { loadAppearanceMode, storeAppearanceMode } from "./prefs";
import type { AppearanceMode, SystemThemeResult } from "./tokens";

// ---- mode state: a plain listener set for React state sync within this window, distinct
// from the Tauri `appearance:theme_changed` event handled below. ----------------------------

let currentMode: AppearanceMode = loadAppearanceMode();
const modeListeners = new Set<(mode: AppearanceMode) => void>();

/** The appearance-mode preference, as last set or loaded in *this* window. */
export function getAppearanceMode(): AppearanceMode {
  return currentMode;
}

/** Notified whenever `setAppearanceMode` is called in this window. Returns an unsubscribe. */
export function subscribeAppearanceMode(cb: (mode: AppearanceMode) => void): () => void {
  modeListeners.add(cb);
  return () => {
    modeListeners.delete(cb);
  };
}

/** Persist `mode`, apply it, and notify this window's subscribers (e.g. useAppearance()). */
export function setAppearanceMode(mode: AppearanceMode): void {
  storeAppearanceMode(mode);
  currentMode = mode;
  void refreshAppearance();
  for (const cb of modeListeners) cb(mode);
}

// ---- applying a detection result ------------------------------------------------------

/**
 * Paint a `SystemThemeResult`: a usable Omarchy palette maps onto tokens and is applied
 * tagged "omarchy"; anything else (unavailable, or available with no palette — the two
 * should never diverge per docs/appearance.md, but this does not assume that) falls back to
 * ChairPhoto Standard. Never throws — `mapPalette` is total over a structurally valid
 * palette, and the backend already guarantees that shape.
 */
export function applyResult(r: SystemThemeResult): void {
  if (r.available && r.palette) {
    const { tokens, mode } = mapPalette(r.palette);
    applyTokens(tokens, mode, "omarchy");
  } else {
    applyStandard();
  }
}

/**
 * Re-derive and paint the current appearance from scratch: read the persisted mode, and for
 * "follow-omarchy" pull the *current* system theme rather than waiting for the next
 * `appearance:theme_changed` broadcast (there may not be one soon — the theme may already
 * have been correct before this window/subscription existed). Falls back to Standard on any
 * rejection (`getSystemTheme` itself should not reject per docs/appearance.md, but this does
 * not depend on that holding).
 */
export function refreshAppearance(): Promise<void> {
  const mode = loadAppearanceMode();
  if (mode === "standard") {
    applyStandard();
    return Promise.resolve();
  }
  return getSystemTheme().then(applyResult).catch(() => applyStandard());
}

// ---- the hook both windows call --------------------------------------------------------

/**
 * Owns this window's live appearance: applies the current mode once on mount, and while the
 * mode is "follow-omarchy" keeps a live `appearance:theme_changed` subscription so a desktop
 * theme switch repaints immediately. Call once from each root component (App.tsx,
 * LoupeWindow.tsx) — the loupe window has no module host, so this must not depend on one.
 */
export function useAppearance(): void {
  const [mode, setMode] = useState<AppearanceMode>(() => getAppearanceMode());

  // Apply once on mount. refreshAppearance() itself branches on the persisted mode, so this
  // is correct whichever mode this window starts in.
  useEffect(() => {
    void refreshAppearance();
  }, []);

  // Track this window's mode preference, and — the redesign plan's requirement — repaint
  // immediately the instant it flips back to Follow, rather than waiting on a broadcast.
  useEffect(
    () =>
      subscribeAppearanceMode((next) => {
        setMode(next);
        if (next === "follow-omarchy") {
          void refreshAppearance();
        }
      }),
    [],
  );

  // Live theme-switch subscription, only while following. Re-registers whenever `mode`
  // changes: flipping to Standard tears it down, flipping to Follow installs it.
  useOwnedSubscription(
    async () => {
      if (mode !== "follow-omarchy") return null;
      return onThemeChanged(applyResult);
    },
    [mode],
  );
}
