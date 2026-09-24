// The Darkroom's autosave history (docs/editing.md § History): pure helpers that name a
// change and decide whether it continues the previous step. The steps themselves live in
// the catalog (`photo_version_history`); this file only decides labels and coalescing.
import type { VersionEdit } from "../../modules/editing";

/** How long the same control may keep moving and still amend the step it started. */
export const AMEND_WINDOW_MS = 4000;

/** A change between two records: a label for the History panel, and a key naming the
 *  control(s) that changed — equal keys can coalesce into one step. */
export interface Change {
  label: string;
  key: string;
}

const TONE_NAMES: Record<string, string> = {
  ev: "Exposure",
  contrast: "Contrast",
  highlights: "Highlights",
  shadows: "Shadows",
  whites: "Whites",
  blacks: "Blacks",
  vibrance: "Vibrance",
  saturation: "Saturation",
};

const signed = (v: number) => `${v >= 0 ? "+" : "−"}${Math.abs(v).toFixed(2)}`;
const same = (a: unknown, b: unknown) => JSON.stringify(a ?? null) === JSON.stringify(b ?? null);

/** Every control that differs between `prev` and `next`, as [key, label] pairs. */
function changedControls(prev: VersionEdit, next: VersionEdit): [string, string][] {
  const out: [string, string][] = [];
  const pt = (prev.tone ?? {}) as Partial<Record<string, unknown>>;
  const nt = (next.tone ?? {}) as Partial<Record<string, unknown>>;
  for (const [k, name] of Object.entries(TONE_NAMES)) {
    const a = (pt[k] as number | undefined) ?? 0;
    const b = (nt[k] as number | undefined) ?? 0;
    if (a !== b) out.push([`tone.${k}`, `${name} ${signed(b)}`]);
  }
  const pwb = prev.tone?.wb ?? { temp: 0, tint: 0 };
  const nwb = next.tone?.wb ?? { temp: 0, tint: 0 };
  if ((pwb.temp ?? 0) !== (nwb.temp ?? 0)) out.push(["tone.wb.temp", `Temperature ${signed(nwb.temp ?? 0)}`]);
  if ((pwb.tint ?? 0) !== (nwb.tint ?? 0)) out.push(["tone.wb.tint", `Tint ${signed(nwb.tint ?? 0)}`]);
  if (!same(prev.zones, next.zones)) out.push(["zones", "Tone strip"]);
  if (!same(prev.crop, next.crop)) {
    out.push(["crop", next.crop ? `Crop${next.crop.aspect && next.crop.aspect !== "Free" ? ` ${next.crop.aspect}` : ""}` : "Crop removed"]);
  }
  if ((prev.straighten ?? 0) !== (next.straighten ?? 0)) {
    out.push(["straighten", `Straighten ${(next.straighten ?? 0).toFixed(1)}°`]);
  }
  if (!same(prev.perspective, next.perspective)) {
    out.push(["perspective", next.perspective ? "Perspective" : "Perspective removed"]);
  }
  if (!same(prev.bw, next.bw)) out.push(["bw", next.bw?.enabled ? "Black & white" : "Colour"]);
  if (!same(prev.split, next.split)) out.push(["split", "Split toning"]);
  if (!same(prev.grain, next.grain)) out.push(["grain", "Grain"]);
  if ((prev.fade ?? 0) !== (next.fade ?? 0)) out.push(["fade", `Fade ${(next.fade ?? 0).toFixed(2)}`]);
  if ((prev.vignette ?? 0) !== (next.vignette ?? 0)) out.push(["vignette", `Vignette ${signed(next.vignette ?? 0)}`]);
  if (!same(prev.lut, next.lut)) out.push(["lut", next.lut ? `LUT ${next.lut.file.replace(/\.cube$/i, "")}` : "LUT removed"]);
  return out;
}

/** Name the change from `prev` to `next`. One control: its own label ("Exposure +0.50").
 *  Several at once (a preset, a proof, a reset): `several` if given, else "Adjustments". */
export function describeChange(prev: VersionEdit, next: VersionEdit, several?: string): Change {
  const changed = changedControls(prev, next);
  if (changed.length === 0) return { label: several ?? "Edit", key: "none" };
  if (changed.length === 1 && !several) return { label: changed[0][1], key: changed[0][0] };
  return { label: several ?? "Adjustments", key: `multi:${changed.map(([k]) => k).join(",")}` };
}

/** Whether a new step should amend the previous one: the same single control, still
 *  moving (within the window), and the previous step is the newest one. A labelled
 *  multi-control change (a proof, a reset) always stands as its own step. */
export function shouldAmend(
  last: { key: string; at: number } | null,
  change: Change,
  now: number,
  atTip: boolean,
): boolean {
  return (
    last != null &&
    atTip &&
    change.key === last.key &&
    !change.key.startsWith("multi:") &&
    change.key !== "none" &&
    now - last.at < AMEND_WINDOW_MS
  );
}
