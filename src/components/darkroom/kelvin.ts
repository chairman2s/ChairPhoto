// Kelvin white balance in the Darkroom (docs/plans/raw-foundation, slice 9): pure helpers
// for the slider, the proof sheet and the duel. A Kelvin record states the scene's light —
// `tone.wb = { mode: "kelvin", kelvin, tint }` — and renders on engine 2 around the
// photo's as-shot light, which the backend reads from the camera (its own white-balance
// table where the file has one). Rendering the as-shot Kelvin and tint is the identity, so
// "as shot" and a blank white balance are the same picture.
import type { Tone, VersionEdit } from "../../modules/editing";

/** Settings key: which white-balance slider a fresh engine-2 edit shows — `"kelvin"`
 *  (default) or `"relative"`. */
export const WB_SLIDER_KEY = "develop.wbSlider";

/** How far the proof sheet's warm/cool cells move the light, in mireds. */
export const PROOF_WARMTH_MIREDS = 30;
/** The duel's first warmth step, in mireds (halved on every revisit, like the others). */
export const DUEL_WARMTH_MIREDS = 25;

export const KELVIN_MIN = 2000;
export const KELVIN_MAX = 12000;
/** Tint in Kelvin mode: +100 is one stop less green (magenta). The slider spans ±50. */
export const KELVIN_TINT_RANGE = 50;
/** The slider's resolution: positions 0..SLIDER_STEPS map KELVIN_MIN..KELVIN_MAX. */
export const SLIDER_STEPS = 1000;

/** What Kelvin needs of the photo, and which slider a fresh edit shows. */
export interface KelvinContext {
  asShot: { kelvin: number; tint: number };
  prefer: "kelvin" | "relative";
}

/** Slider position for a Kelvin value — logarithmic, so equal travel is an equal ratio of
 *  temperature (close to equal steps in mireds, the way light shifts look). */
export function kelvinToSlider(kelvin: number): number {
  const k = Math.min(KELVIN_MAX, Math.max(KELVIN_MIN, kelvin));
  return Math.round((SLIDER_STEPS * Math.log(k / KELVIN_MIN)) / Math.log(KELVIN_MAX / KELVIN_MIN));
}

/** Kelvin for a slider position, rounded to 50 K. */
export function sliderToKelvin(pos: number): number {
  const p = Math.min(SLIDER_STEPS, Math.max(0, pos));
  const k = KELVIN_MIN * Math.pow(KELVIN_MAX / KELVIN_MIN, p / SLIDER_STEPS);
  return Math.round(k / 50) * 50;
}

/** `kelvin` moved by `mireds` (negative = a higher Kelvin = a warmer picture), in range. */
export function miredShift(kelvin: number, mireds: number): number {
  // Clamp in mireds, before inverting: a shift past 0 mireds is "hotter than any light",
  // which must land on the warm end, not wrap to the cold one.
  const m = Math.min(1e6 / KELVIN_MIN, Math.max(1e6 / KELVIN_MAX, 1e6 / kelvin + mireds));
  return Math.round(1e6 / m);
}

/** A Kelvin white balance for the record. */
export function kelvinWb(kelvin: number, tint: number): Tone["wb"] {
  return { temp: 0, tint, mode: "kelvin", kelvin };
}

/** What the white-balance rail shows for `wb`: the Kelvin pair (the record's, or as-shot
 *  when the record leaves white balance alone and Kelvin is preferred), or the relative
 *  sliders. Kelvin needs a context — engine 2 with an as-shot light. */
export function wbShown(
  wb: Tone["wb"] | undefined,
  ctx: KelvinContext | null | undefined,
): { mode: "kelvin"; kelvin: number; tint: number } | { mode: "relative" } {
  if (wb?.mode === "kelvin" && wb.kelvin != null) return { mode: "kelvin", kelvin: wb.kelvin, tint: wb.tint ?? 0 };
  if (!ctx) return { mode: "relative" };
  const untouched = !wb || (wb.mode !== "relative" && (wb.temp ?? 0) === 0 && (wb.tint ?? 0) === 0);
  if (ctx.prefer === "kelvin" && untouched) return { mode: "kelvin", kelvin: ctx.asShot.kelvin, tint: ctx.asShot.tint };
  return { mode: "relative" };
}

/** `record` with its white balance moved `mireds` warmer (negative) or cooler, in Kelvin
 *  around what it shows now — the proof sheet's warm/cool cells and the duel's warmth. */
export function withKelvinShift(record: VersionEdit, ctx: KelvinContext, mireds: number): VersionEdit {
  const shown = wbShown(record.tone?.wb, { ...ctx, prefer: "kelvin" });
  const base = shown.mode === "kelvin" ? shown : { kelvin: ctx.asShot.kelvin, tint: ctx.asShot.tint };
  const tone = { ...(record.tone ?? {}), wb: kelvinWb(miredShift(base.kelvin, mireds), base.tint) } as Tone;
  return { ...record, tone };
}
