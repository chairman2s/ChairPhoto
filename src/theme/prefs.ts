import type { AppearanceMode } from "./tokens";

// Which palette source Preferences → Appearance is set to. A per-machine preference — it is
// not part of the catalog and does not travel with it between computers.

const STORAGE_KEY = "appearance.mode";

/**
 * "follow-omarchy" is the product default. Any absent or unrecognized stored value falls
 * back to it — including the pre-redesign case where nothing has been written yet.
 */
export function loadAppearanceMode(): AppearanceMode {
  try {
    const stored = localStorage.getItem(STORAGE_KEY);
    if (stored === "follow-omarchy" || stored === "standard") {
      return stored;
    }
  } catch {
    // localStorage unavailable (disabled, private browsing, non-browser test runner) —
    // fall through to the default below.
  }
  return "follow-omarchy";
}

export function storeAppearanceMode(mode: AppearanceMode): void {
  try {
    localStorage.setItem(STORAGE_KEY, mode);
  } catch {
    // Best-effort persistence; the in-memory choice for this session still applies.
  }
}
