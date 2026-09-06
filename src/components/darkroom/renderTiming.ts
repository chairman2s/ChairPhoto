// Frame-timing helpers for the Darkroom's render loop and the WebGL probe (Gate 0 and
// increment 1 of the GPU-smoothness work, docs/plans/darkroom/00-status.md). Pure
// functions only — the loop stamps samples, these summarize them.

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
