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

// ── Duels ───────────────────────────────────────────────────────────────────
// One round explores one dimension: A and B sit symmetrically around the working
// state, the step halves on every revisit (coordinate descent by eye). v1 duels the
// four numeric dims; a "look" round needs preset semantics and waits (00-status.md).

export type DuelDim = "ev" | "warmth" | "contrast" | "shadows";
export const DUEL_DIMS: DuelDim[] = ["ev", "warmth", "contrast", "shadows"];
export const DUEL_LABELS: Record<DuelDim, string> = {
  ev: "Exposure",
  warmth: "Warmth",
  contrast: "Contrast",
  shadows: "Shadows",
};
/** First-visit step per dimension; halves per revisit. Tunable (Gate 3 §least-confident). */
const DUEL_BASE_STEP: Record<DuelDim, number> = {
  ev: 0.4,
  warmth: 0.2,
  contrast: 0.2,
  shadows: 0.25,
};

const clamp1 = (v: number) => Math.min(1, Math.max(-1, v));
const withTone = (r: VersionEdit, patch: Partial<Tone>): VersionEdit => ({
  ...r,
  tone: { ...(r.tone ?? {}), ...patch } as Tone,
});

/** A/B variants around `working` for `dim`; `visit` counts prior rounds on this dim. */
export function duelPair(
  working: VersionEdit,
  dim: DuelDim,
  visit: number,
): [VersionEdit, VersionEdit] {
  const step = DUEL_BASE_STEP[dim] * Math.pow(0.5, Math.max(0, visit));
  const t = working.tone;
  switch (dim) {
    case "ev": {
      const ev = t?.ev ?? 0;
      return [withTone(working, { ev: ev - step }), withTone(working, { ev: ev + step })];
    }
    case "warmth": {
      const wb = t?.wb ?? { temp: 0, tint: 0 };
      return [
        withTone(working, { wb: { temp: clamp1(wb.temp - step), tint: wb.tint } }),
        withTone(working, { wb: { temp: clamp1(wb.temp + step), tint: wb.tint } }),
      ];
    }
    case "contrast": {
      const c = t?.contrast ?? 0;
      return [
        withTone(working, { contrast: clamp1(c - step) }),
        withTone(working, { contrast: clamp1(c + step) }),
      ];
    }
    case "shadows": {
      const s = t?.shadows ?? 0;
      return [
        withTone(working, { shadows: clamp1(s - step) }),
        withTone(working, { shadows: clamp1(s + step) }),
      ];
    }
  }
}

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
