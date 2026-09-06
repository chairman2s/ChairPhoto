// Frame-timing helpers for the Darkroom's render loop and the WebGL probe (Gate 0 and
// increment 1 of the GPU-smoothness work, docs/plans/darkroom/00-status.md). Pure
// functions only — the loop stamps samples, these summarize them.

/** Settings key: "1" turns the frame log (and the WebGL probe button) on. Read by
 *  DarkroomView once per mount and toggled in Preferences → Darkroom. */
export const RENDER_TIMING_KEY = "editor.renderTiming";
/** Settings key the darkroom writes its latest timing summary under, so a run can be
 *  read back without the web inspector (the probe does the same for its report). */
export const RENDER_TIMING_SUMMARY_KEY = "editor.renderTiming.lastSummary";

/** Nearest-rank percentile of `xs` (p in 0..100). NaN for an empty list; never mutates. */
export function percentile(xs: number[], p: number): number {
  if (xs.length === 0) return NaN;
  const sorted = [...xs].sort((a, b) => a - b);
  const rank = Math.ceil((Math.min(100, Math.max(0, p)) / 100) * sorted.length);
  return sorted[Math.min(sorted.length - 1, Math.max(0, rank - 1))];
}

/** p50 and p95 of a list, rounded to 0.1 ms for logging. */
export function p50p95(xs: number[]): { p50: number; p95: number } {
  const r = (v: number) => Math.round(v * 10) / 10;
  return { p50: r(percentile(xs, 50)), p95: r(percentile(xs, 95)) };
}

/** Which render produced a frame: the 720 px drag tier, the 1400 px settled tier, a
 *  geometry base for the GL tier, or a GL draw. */
export type Tier = "fast" | "settled" | "base" | "gl";

/** One render request's life: asked for, answered by the backend, on screen. */
export interface FrameSample {
  seq: number;
  tier: Tier;
  /** performance.now() at the invoke. */
  requested: number;
  /** …when the promise resolved (IPC round trip complete). */
  resolved?: number;
  /** …when the pixels were on screen (the <img> loaded, next animation frame). */
  painted?: number;
  /** The result arrived but a newer state had already superseded it. */
  superseded?: boolean;
}

export interface TimingSummary {
  count: number;
  superseded: number;
  /** request → on screen, every painted frame (the number a drag feels). */
  latencyMs: { p50: number; p95: number };
  /** invoke → resolve, for frames that came back over IPC (the data-URL path). */
  ipcMs: { p50: number; p95: number };
  /** resolve → on screen for those same frames (data-URL parse, JPEG decode, layout). */
  paintMs: { p50: number; p95: number };
  /** interval between consecutive painted frames of any tier. */
  cadenceMs: { p50: number; p95: number };
  byTier: Partial<Record<Tier, number>>;
}

export function summarize(samples: FrameSample[]): TimingSummary {
  const latency: number[] = [];
  const ipc: number[] = [];
  const paint: number[] = [];
  const painted: number[] = [];
  const byTier: Partial<Record<Tier, number>> = {};
  let superseded = 0;
  for (const s of samples) {
    byTier[s.tier] = (byTier[s.tier] ?? 0) + 1;
    if (s.superseded) superseded++;
    if (s.resolved !== undefined) ipc.push(s.resolved - s.requested);
    if (s.painted !== undefined) {
      latency.push(s.painted - s.requested);
      painted.push(s.painted);
      if (s.resolved !== undefined) paint.push(s.painted - s.resolved);
    }
  }
  painted.sort((a, b) => a - b);
  const cadence = painted.slice(1).map((t, i) => t - painted[i]);
  return {
    count: samples.length,
    superseded,
    latencyMs: p50p95(latency),
    ipcMs: p50p95(ipc),
    paintMs: p50p95(paint),
    cadenceMs: p50p95(cadence),
    byTier,
  };
}

/** One console line per painted (or superseded) frame. */
export function formatSample(s: FrameSample): string {
  const ms = (v: number | undefined) => (v === undefined ? "—" : `${v.toFixed(1)}ms`);
  const total = s.painted === undefined ? undefined : s.painted - s.requested;
  const ipc = s.resolved === undefined ? undefined : s.resolved - s.requested;
  const paint =
    s.resolved === undefined || s.painted === undefined ? undefined : s.painted - s.resolved;
  return `[edit-timing] seq=${s.seq} tier=${s.tier} total=${ms(total)} ipc=${ms(ipc)} paint=${ms(
    paint,
  )}${s.superseded ? " superseded" : ""}`;
}
