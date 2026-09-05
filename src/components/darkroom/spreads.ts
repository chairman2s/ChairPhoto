// Proof-spread candidates for the Darkroom (docs/plans/darkroom): pure record maths,
// no IO — unit-tested. A candidate never touches the base's framing: crop, straighten,
// and perspective travel through untouched. Look cells replace the look wholesale —
// zones included, since a fresh look deserves a flat strip — while Auto cells keep the
// base's look and only fix its tone.
import type { Tone, VersionEdit } from "../../modules/editing";
import type { DevelopPreset } from "../../modules/presets";

export interface ProofCandidate {
  label: string;
  group: "asShot" | "auto" | "film" | "bw" | "look";
  record: VersionEdit;
}

export const PROOF_CELLS = 12;

const geometryOf = (
  base: VersionEdit,
): Pick<VersionEdit, "crop" | "straighten" | "perspective"> => ({
  ...(base.crop ? { crop: base.crop } : {}),
  ...(base.straighten !== undefined ? { straighten: base.straighten } : {}),
  ...(base.perspective ? { perspective: base.perspective } : {}),
});

/** Sparse-over-sparse tone merge (`b`'s keys win); undefined when both are absent. */
const mergeTone = (a: Tone | undefined, b: Tone | undefined): Tone | undefined =>
  a || b ? ({ ...(a ?? {}), ...(b ?? {}) } as Tone) : undefined;

const warmed = (t: Tone | undefined, temp: number): Tone =>
  ({ ...(t ?? {}), wb: { temp, tint: t?.wb?.tint ?? 0 } }) as Tone;

const groupOf = (p: DevelopPreset): ProofCandidate["group"] =>
  p.category === "Film" ? "film" : p.category === "Monochrome" ? "bw" : "look";

/** Round-robin across categories so the spread shows range, not one family. */
const pickLooks = (presets: DevelopPreset[], n: number): DevelopPreset[] => {
  const order = ["Film", "Color", "Monochrome", "User"] as const;
  const queues = order.map((c) => presets.filter((p) => p.category === c));
  const out: DevelopPreset[] = [];
  for (let round = 0; out.length < n; round++) {
    let added = false;
    for (const q of queues) {
      const p = q[round];
      if (p && out.length < n) {
        out.push(p);
        added = true;
      }
    }
    if (!added) break;
  }
  return out;
};

/**
 * The spread: the current state (always first — declining is a click), three Auto
 * cells (fix, warm, cool), and looks over the preset library until the sheet is full.
 * `auto` is the `suggest_auto_tone` fragment, parsed.
 */
export function proofSpread(
  base: VersionEdit,
  auto: VersionEdit,
  presets: DevelopPreset[],
): ProofCandidate[] {
  const geo = geometryOf(base);
  const autoTone = mergeTone(base.tone, auto.tone);
  const out: ProofCandidate[] = [
    {
      label: Object.keys(base).length > 0 ? "Current" : "As shot",
      group: "asShot",
      record: base,
    },
    { label: "Auto", group: "auto", record: { ...base, tone: autoTone } },
    { label: "Auto · Warm", group: "auto", record: { ...base, tone: warmed(autoTone, 0.35) } },
    { label: "Auto · Cool", group: "auto", record: { ...base, tone: warmed(autoTone, -0.35) } },
  ];
  for (const p of pickLooks(presets, PROOF_CELLS - out.length)) {
    out.push({
      label: p.name,
      group: groupOf(p),
      // Framing + Auto's exposure + the preset's whole look; the preset's own tone
      // keys win over Auto's (a B&W recipe's contrast is part of the recipe).
      record: { ...geo, ...p.edit, tone: mergeTone(autoTone, p.edit.tone) },
    });
  }
  return out;
}
