import type { ThemeMode, ThemeTokens } from "./tokens";

// ChairPhoto Standard: the app-owned warm dark palette, and the new default look. This
// module must not import anything Tauri — it (and apply.ts, which consumes it) needs to run
// before React mounts and inside plain jsdom tests, neither of which has a Tauri runtime.

export const STANDARD: ThemeTokens = {
  canvas: "#14120F",
  panel: "#1B1815",
  elev: "#23201C",
  well: "#0D0C0A",
  border: "#2E2A25",
  line: "#241F1B",
  txt: "#EFE9E0",
  dim: "#A8A093",
  mute: "#6F675C",
  accent: "#E0A458",
  onaccent: "#241C10",
  sel: "rgba(224, 164, 88, 0.14)",
  ok: "#10B981",
  onok: "#04150F",
  danger: "#F87171",
  rating: "#FFD700",
  scrim: "rgba(0, 0, 0, 0.66)",
  "font-sans": '"Instrument Sans", system-ui, -apple-system, "Segoe UI", sans-serif',
  "font-display": '"Instrument Serif", Georgia, serif',
};

export const STANDARD_MODE: ThemeMode = "dark";
