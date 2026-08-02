// Develop presets: one-click looks for the basic editor. A preset is a *look-only*
// partial edit record (tone + film-look params, never crop/straighten) applied over
// zero defaults, so clicking a preset replaces the look but keeps the framing.
//
// Built-ins are parameter recipes (tunable, no bundled assets); user presets are
// saved per-catalog under the settings key `basic-editor.presets`.

import { getSetting, setSetting } from "./api";
import type { Tone, VersionEdit } from "./editing";

export type PresetCategory = "Monochrome" | "Film" | "Color" | "User";

export interface DevelopPreset {
  /** Builtin: stable slug ("bw-red"); user: crypto.randomUUID(). */
  id: string;
  name: string;
  category: PresetCategory;
  /** Look-only fields — the framing (crop/straighten) is never part of a preset. */
  edit: Omit<VersionEdit, "crop" | "straighten">;
  builtin?: boolean;
}

/** Sparse tone helper: presets only list the keys they touch. */
type PartialTone = Partial<Omit<Tone, "wb">> & { wb?: { temp: number; tint: number } };
const tone = (t: PartialTone) => t as Tone;

// Neutral Rec.601 B&W conversion — the base for every monochrome preset.
const BW_NEUTRAL = { enabled: true, r: 0.299, g: 0.587, b: 0.114 };
const BW_RED = { enabled: true, r: 0.9, g: 0.15, b: -0.05 };
const BW_YELLOW = { enabled: true, r: 0.55, g: 0.4, b: 0.05 };
const BW_GREEN = { enabled: true, r: 0.2, g: 0.7, b: 0.1 };

/**
 * The built-in library. Recipes are starting points tuned by eye — a preset is meant
 * to be clicked, then (optionally) refined with the regular sliders.
 */
export const BUILTIN_PRESETS: DevelopPreset[] = [
  // --- Monochrome styles -------------------------------------------------
  {
    id: "bw-neutral",
    name: "B&W Neutral",
    category: "Monochrome",
    builtin: true,
    edit: { bw: BW_NEUTRAL, tone: tone({ contrast: 0.1 }) },
  },
  {
    id: "bw-red",
    name: "B&W Red Filter",
    category: "Monochrome",
    builtin: true,
    // Dramatic skies: reds/skin bright, blues near-black.
    edit: { bw: BW_RED, tone: tone({ contrast: 0.2 }) },
  },
  {
    id: "bw-yellow",
    name: "B&W Yellow Filter",
    category: "Monochrome",
    builtin: true,
    edit: { bw: BW_YELLOW, tone: tone({ contrast: 0.12 }) },
  },
  {
    id: "bw-green",
    name: "B&W Green Filter",
    category: "Monochrome",
    builtin: true,
    // Classic for foliage and natural skin rendering.
    edit: { bw: BW_GREEN, tone: tone({ contrast: 0.1 }) },
  },
  {
    id: "bw-high-contrast",
    name: "B&W High Contrast",
    category: "Monochrome",
    builtin: true,
    edit: { bw: BW_NEUTRAL, tone: tone({ contrast: 0.45, blacks: -0.15, whites: 0.15 }) },
  },
  {
    id: "sepia",
    name: "Sepia",
    category: "Monochrome",
    builtin: true,
    edit: {
      bw: BW_NEUTRAL,
      split: { shadow_hue: 35, shadow_sat: 0.25, highlight_hue: 45, highlight_sat: 0.12, balance: 0 },
      tone: tone({ contrast: 0.05 }),
    },
  },
  {
    id: "selenium",
    name: "Selenium",
    category: "Monochrome",
    builtin: true,
    // Cool purple-blue toning in the shadows, like a selenium-toned print.
    edit: {
      bw: BW_NEUTRAL,
      split: { shadow_hue: 275, shadow_sat: 0.15, highlight_hue: 250, highlight_sat: 0.06, balance: 0 },
      tone: tone({ contrast: 0.15 }),
    },
  },
  // --- Film stocks --------------------------------------------------------
  {
    id: "tri-x",
    name: "Tri-X 400",
    category: "Film",
    builtin: true,
    // Gritty photojournalism B&W: contrasty, crushed blacks, visible grain.
    edit: {
      bw: { enabled: true, r: 0.35, g: 0.45, b: 0.2 },
      tone: tone({ contrast: 0.3, blacks: -0.1 }),
      grain: { amount: 0.5, size: 1.2, seed: 0 },
    },
  },
  {
    id: "kodak-gold",
    name: "Kodak Gold 200",
    category: "Film",
    builtin: true,
    // Warm consumer negative film: golden highlights, gentle fade, light grain.
    edit: {
      tone: tone({ vibrance: 0.15, wb: { temp: 0.15, tint: 0 } }),
      split: { shadow_hue: 40, shadow_sat: 0, highlight_hue: 45, highlight_sat: 0.08, balance: 0 },
      fade: 0.1,
      grain: { amount: 0.2, size: 1, seed: 0 },
    },
  },
  {
    id: "portra",
    name: "Portra 400",
    category: "Film",
    builtin: true,
    // Soft, low-contrast portrait film with warm shadows and muted saturation.
    edit: {
      tone: tone({ contrast: -0.05, saturation: -0.1, shadows: 0.1, wb: { temp: 0.08, tint: 0 } }),
      split: { shadow_hue: 20, shadow_sat: 0.06, highlight_hue: 40, highlight_sat: 0, balance: 0 },
      grain: { amount: 0.15, size: 1, seed: 0 },
    },
  },
  {
    id: "ektachrome",
    name: "Ektachrome E100",
    category: "Film",
    builtin: true,
    // Clean slide film with a slightly cool cast and blue-leaning shadows.
    edit: {
      tone: tone({ saturation: 0.15, contrast: 0.15, wb: { temp: -0.05, tint: 0 } }),
      split: { shadow_hue: 220, shadow_sat: 0.05, highlight_hue: 200, highlight_sat: 0, balance: 0 },
    },
  },
  {
    id: "kodachrome",
    name: "Kodachrome 64",
    category: "Film",
    builtin: true,
    // Punchy, warm, deep-shadowed slide film with golden highlights.
    edit: {
      tone: tone({ contrast: 0.25, saturation: 0.1, blacks: -0.1, wb: { temp: 0.05, tint: 0 } }),
      split: { shadow_hue: 40, shadow_sat: 0, highlight_hue: 50, highlight_sat: 0.05, balance: 0 },
    },
  },
  {
    id: "velvia",
    name: "Velvia 50",
    category: "Film",
    builtin: true,
    // Landscape slide film: maximum colour punch.
    edit: { tone: tone({ saturation: 0.35, vibrance: 0.2, contrast: 0.2 }) },
  },
  // --- Color looks ---------------------------------------------------------
  {
    id: "auto",
    name: "Auto",
    category: "Color",
    builtin: true,
    edit: { tone: tone({ ev: 0.15, contrast: 0.1, highlights: -0.2, shadows: 0.15 }) },
  },
  {
    id: "landscape",
    name: "Landscape",
    category: "Color",
    builtin: true,
    edit: { tone: tone({ vibrance: 0.35, contrast: 0.1, highlights: -0.25, shadows: 0.1 }) },
  },
  {
    id: "punch",
    name: "Punch",
    category: "Color",
    builtin: true,
    edit: { tone: tone({ contrast: 0.3, vibrance: 0.25, blacks: -0.15 }) },
  },
  {
    id: "faded-matte",
    name: "Faded Matte",
    category: "Color",
    builtin: true,
    edit: {
      tone: tone({ contrast: -0.1, saturation: -0.15 }),
      fade: 0.5,
      grain: { amount: 0.2, size: 1, seed: 0 },
    },
  },
];

// --- user presets (per-catalog, stored as one settings JSON blob) -----------

const USER_PRESETS_KEY = "basic-editor.presets";

export async function loadUserPresets(): Promise<DevelopPreset[]> {
  try {
    const raw = await getSetting(USER_PRESETS_KEY);
    if (!raw) return [];
    const list = JSON.parse(raw);
    if (!Array.isArray(list)) return [];
    return list
      .filter((p): p is DevelopPreset => p && typeof p.id === "string" && typeof p.name === "string")
      .map((p) => ({ ...p, category: "User" as const, builtin: false }));
  } catch {
    return [];
  }
}

/** Whole-array replace — the list is tiny (a few KB at most). */
export async function saveUserPresets(list: DevelopPreset[]): Promise<void> {
  await setSetting(USER_PRESETS_KEY, JSON.stringify(list));
}

/** All presets in display order: built-in groups first, then the user's. */
export async function allPresets(): Promise<DevelopPreset[]> {
  return [...BUILTIN_PRESETS, ...(await loadUserPresets())];
}

/** The categories in display order. */
export const PRESET_CATEGORIES: PresetCategory[] = ["Monochrome", "Film", "Color", "User"];
