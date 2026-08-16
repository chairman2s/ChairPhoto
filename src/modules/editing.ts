// Shared types + presets for the basic editor (H5b). The edit record is stored as the
// opaque `edit_json` on a photo version and interpreted by the render engine
// (src-tauri/src/plugins/edit). See docs/editing.md.

/** Crop rectangle as fractions (0–1) of the source — resolution-independent. */
export interface Crop {
  x: number;
  y: number;
  w: number;
  h: number;
  /** The chosen aspect preset label (UI bookkeeping; engine ignores it). */
  aspect?: string;
}

/** One corner of a {@link Perspective} quad, as fractions (0–1) of the source. */
export type QuadPoint = [number, number];

/**
 * Four-corner perspective (keystone) correction: the source quadrilateral that should
 * become the output rectangle, as fractions of the source — resolution-independent, so
 * one record renders the same on the preview proxy and the full-size original.
 *
 * Corners are named by their position on the *subject*, not on the frame, so a tilted
 * picture is expressed by which corner is which. One quad therefore carries rotation and
 * keystone together and needs no angle beside it.
 */
export interface Perspective {
  tl: QuadPoint;
  tr: QuadPoint;
  br: QuadPoint;
  bl: QuadPoint;
  /**
   * Output aspect (width / height). Absent = the engine derives it from the quad's mean
   * edge lengths, which is right for a roughly square-on shot. Recovering the subject's
   * true ratio needs the camera's focal length, which the engine never sees.
   */
  aspect?: number;
}

/** The four corners in drawing order, so the overlay polygon and the handles agree. */
export const QUAD_CORNERS = ["tl", "tr", "br", "bl"] as const;
export type QuadCorner = (typeof QUAD_CORNERS)[number];

/**
 * The quad a fresh perspective edit starts from — the whole frame, inset far enough that
 * all four handles are visible and grabbable instead of pinned under the frame's edge.
 */
export const DEFAULT_QUAD: Perspective = {
  tl: [0.06, 0.06],
  tr: [0.94, 0.06],
  br: [0.94, 0.94],
  bl: [0.06, 0.94],
};

export interface Tone {
  ev: number; // exposure, stops
  contrast: number; // -1..1
  highlights: number; // -1..1
  shadows: number; // -1..1
  whites: number; // -1..1
  blacks: number; // -1..1
  vibrance: number; // -1..1
  saturation: number; // -1..1
  wb: { temp: number; tint: number }; // -1..1 each
}

/**
 * Black & white conversion via a channel mixer. Classic contrast filters are weight
 * recipes (red ≈ {0.9, 0.15, -0.05}); the engine normalizes weights by their sum.
 * Field names are snake_case to match the Rust serde names exactly.
 */
export interface Bw {
  enabled: boolean;
  r: number;
  g: number;
  b: number;
}

/** Split toning: hues in degrees (0–360), sats 0–1, balance -1..1 shifts the crossover. */
export interface Split {
  shadow_hue: number;
  shadow_sat: number;
  highlight_hue: number;
  highlight_sat: number;
  balance: number;
}

/** Deterministic film grain — the seed is part of the record so exports reproduce. */
export interface Grain {
  amount: number; // 0..1
  size: number; // 0.5..3, 1 = default
  seed: number;
}

/** A .cube LUT by bare filename in the app's luts folder (portable; missing = no-op). */
export interface LutRef {
  file: string;
  amount: number; // 0..1 blend
}

export interface VersionEdit {
  crop?: Crop;
  tone?: Tone;
  /** Four-corner perspective correction; absent = leave the geometry alone. */
  perspective?: Perspective;
  /** Straighten angle in degrees (rotate the image about its centre to level it). */
  straighten?: number;
  bw?: Bw;
  split?: Split;
  grain?: Grain;
  /** Lifted matte blacks, 0..1. */
  fade?: number;
  /** Corner shading, -1..1 (negative darkens). */
  vignette?: number;
  lut?: LutRef;
}

/** The look-only slice of an edit — everything except framing (crop/straighten). */
export type Look = Pick<VersionEdit, "bw" | "split" | "grain" | "fade" | "vignette" | "lut">;

/** An all-off look (used to reset before applying a preset). */
export const ZERO_LOOK: Look = {
  bw: undefined,
  split: undefined,
  grain: undefined,
  fade: 0,
  vignette: 0,
  lut: undefined,
};

/**
 * The look fields of an edit with all-default values dropped, so the stored edit_json
 * stays minimal (and an untouched record keeps the engine's identity fast path).
 */
export const lookFields = (l: Look): Partial<VersionEdit> => ({
  bw: l.bw,
  split: l.split,
  grain: l.grain && l.grain.amount > 0 ? l.grain : undefined,
  fade: l.fade || undefined,
  vignette: l.vignette || undefined,
  lut: l.lut,
});

/** B&W contrast-filter chips for the Effects section — channel-mixer weight recipes. */
export const BW_FILTERS: { label: string; bw: Bw }[] = [
  { label: "Neutral", bw: { enabled: true, r: 0.299, g: 0.587, b: 0.114 } },
  { label: "Red", bw: { enabled: true, r: 0.9, g: 0.15, b: -0.05 } },
  { label: "Yellow", bw: { enabled: true, r: 0.55, g: 0.4, b: 0.05 } },
  { label: "Green", bw: { enabled: true, r: 0.2, g: 0.7, b: 0.1 } },
];

/** Clamp the straighten angle to the supported range. */
export const STRAIGHTEN_MAX = 45;
export const clampStraighten = (deg: number) =>
  Math.min(Math.max(deg, -STRAIGHTEN_MAX), STRAIGHTEN_MAX);

/**
 * Given a line drawn on the (possibly already-straightened) image, return the *extra*
 * rotation in degrees that levels it — to horizontal if the line is within 45° of
 * horizontal, otherwise to vertical. Coordinates are screen-space (y-down).
 */
export function levelFromLine(x1: number, y1: number, x2: number, y2: number): number {
  const deg = (Math.atan2(y2 - y1, x2 - x1) * 180) / Math.PI;
  let m = deg; // fold to (-90, 90] — a line has no direction
  while (m > 90) m -= 180;
  while (m <= -90) m += 180;
  if (Math.abs(m) <= 45) return -m; // level to horizontal
  return m > 0 ? 90 - m : -90 - m; // level to vertical
}

/**
 * The largest centred crop (same aspect as the W×H image) that fits inside the image
 * after straightening by `degrees`, as normalized fractions — keeps the rotation's black
 * corners out of frame. Returns the full frame at ~0°.
 */
export function inscribedCrop(imgW: number, imgH: number, degrees: number): Crop {
  const a = (Math.abs(degrees) * Math.PI) / 180;
  if (a < 1e-4 || imgW <= 0 || imgH <= 0) {
    return { x: 0, y: 0, w: 1, h: 1, aspect: "Original" };
  }
  const cos = Math.cos(a);
  const sin = Math.sin(a);
  const s = Math.min(
    imgW / (imgW * cos + imgH * sin),
    imgH / (imgW * sin + imgH * cos),
  );
  // The inscribed corners touch the edge exactly; pull in ~0.3% so bilinear sampling at
  // the very edge can't leave a 1px black sliver.
  const sc = Math.min(1, Math.max(0.05, s * 0.997));
  return { x: (1 - sc) / 2, y: (1 - sc) / 2, w: sc, h: sc, aspect: "Original" };
}

export const ZERO_TONE: Tone = {
  ev: 0,
  contrast: 0,
  highlights: 0,
  shadows: 0,
  whites: 0,
  blacks: 0,
  vibrance: 0,
  saturation: 0,
  wb: { temp: 0, tint: 0 },
};

/**
 * Crop presets. `ratio` = width/height. `null` ratio: "Original" = no crop, "Free" = a
 * crop with no locked aspect (distinguished by label). Social sizes are flagged.
 */
export const ASPECTS: { label: string; ratio: number | null; hint?: string }[] = [
  { label: "Original", ratio: null },
  { label: "Free", ratio: null, hint: "Crop with no fixed aspect" },
  { label: "1:1", ratio: 1, hint: "Instagram / Facebook square" },
  { label: "4:5", ratio: 4 / 5, hint: "Instagram portrait (max)" },
  { label: "1.91:1", ratio: 1.91, hint: "Instagram / Facebook link" },
  { label: "9:16", ratio: 9 / 16, hint: "Reels · Stories · TikTok · Snapchat · Shorts" },
  { label: "16:9", ratio: 16 / 9, hint: "Facebook · video" },
  { label: "3:2", ratio: 3 / 2 },
  { label: "2:3", ratio: 2 / 3 },
  { label: "4:3", ratio: 4 / 3 },
  { label: "3:4", ratio: 3 / 4 },
];

/** Composition overlay drawn inside the crop. Persisted as a preference. */
export type CropOverlay = "none" | "thirds" | "phi" | "golden";
export const OVERLAYS: { key: CropOverlay; label: string }[] = [
  { key: "none", label: "None" },
  { key: "thirds", label: "Thirds" },
  { key: "phi", label: "Phi grid" },
  { key: "golden", label: "Golden" },
];

/** Grid line offsets (as fractions) for the line-based overlays. */
export const OVERLAY_LINES: Record<"thirds" | "phi", number[]> = {
  thirds: [1 / 3, 2 / 3],
  phi: [0.382, 0.618],
};

/**
 * A golden (Fibonacci) spiral as an SVG path in a 0–100 viewBox (stretched to the crop
 * via preserveAspectRatio="none"). Chained quarter-arcs with radii in the 1/phi ratio
 * (61.8 → 38.2 → 23.6 → 14.6 → 9), curling inward.
 */
export const GOLDEN_SPIRAL_PATH =
  "M 0,61.8 A 61.8,61.8 0 0 1 61.8,0 A 38.2,38.2 0 0 1 100,38.2 " +
  "A 23.6,23.6 0 0 1 76.4,61.8 A 14.6,14.6 0 0 1 61.8,47.2 A 9,9 0 0 1 70.8,38.2";

/** Parse a version's stored edit_json into a VersionEdit (tolerant of empty/partial). */
export function parseEdit(editJson: string | undefined): VersionEdit {
  if (!editJson) return {};
  try {
    return JSON.parse(editJson) as VersionEdit;
  } catch {
    return {};
  }
}

/**
 * The largest crop rect of a given pixel aspect ratio inside an image, scaled by `size`
 * (0–1) and centred. Returns normalized {x,y,w,h}. `ratio` is width/height in pixels.
 */
export function fitCrop(
  imgW: number,
  imgH: number,
  ratio: number,
  size: number,
): { x: number; y: number; w: number; h: number } {
  // Degenerate inputs (image not yet measured, bad ratio) → full frame, no NaN crop.
  if (imgW <= 0 || imgH <= 0 || ratio <= 0) {
    return { x: 0, y: 0, w: 1, h: 1 };
  }
  let cwPx: number;
  let chPx: number;
  if (imgW / imgH >= ratio) {
    chPx = imgH;
    cwPx = ratio * imgH;
  } else {
    cwPx = imgW;
    chPx = imgW / ratio;
  }
  cwPx *= size;
  chPx *= size;
  const w = Math.min(1, cwPx / imgW);
  const h = Math.min(1, chPx / imgH);
  return { x: (1 - w) / 2, y: (1 - h) / 2, w, h };
}
