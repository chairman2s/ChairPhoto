// Shell transition timing (dev evidence, behind the Darkroom's render-timing toggle):
// how long Develop → Library takes and where it goes. One transition is a record from the
// Back click to the grid's React commit, its first painted thumbnail and the last of the
// tiles it mounted; the summary is logged as `[shell-timing]` and persisted under a
// setting so it can be read without the inspector. Nothing here runs when the toggle is off
// beyond a boolean check.
import { setSetting } from "./api";

export const SHELL_TIMING_KEY = "editor.renderTiming.lastShell";

export interface Transition {
  from: string;
  start: number;
  /** React commit of the grid mount (Profiler actualDuration, ms) and when it landed. */
  commitAt?: number;
  commitMs?: number;
  firstTileAt?: number;
  tilesMounted: number;
  tilesLoaded: number;
  /** When the last tile so far loaded — lazy images below the fold may never load. */
  lastTileAt?: number;
  /** Tile loads per 250 ms bucket since Back. */
  loadBuckets: number[];
  /** When the grid scrolled to the selection (its mount effect). */
  scrollAt?: number;
  /** Longest gap between two animation frames after Back, and when it ended. */
  maxFrameGapMs: number;
  maxFrameGapAt?: number;
  /** Named moments (first occurrence after Back), ms since Back. */
  marks: Record<string, number>;
  /** Backend commands that took ≥ 50 ms during the transition: name, round trip, size hint. */
  slowInvokes: { cmd: string; startMs: number; ms: number; rows?: number }[];
  finished: boolean;
  quiet?: ReturnType<typeof setTimeout>;
  raf?: number;
}

/** No tile has loaded for this long → the transition is over (lazy tiles below the fold
 *  never fire, so "all tiles loaded" is not a usable end condition). */
const QUIET_MS = 3000;
const BUCKET_MS = 250;

let enabled = false;
let current: Transition | null = null;

export function setShellTimingEnabled(on: boolean): void {
  enabled = on;
}

/** Call at the moment the user leaves a surface (e.g. Back from Develop). */
export function markShellLeave(from: string): void {
  if (!enabled) return;
  if (current?.quiet) clearTimeout(current.quiet);
  if (current?.raf) cancelAnimationFrame(current.raf);
  current = {
    from,
    start: performance.now(),
    tilesMounted: 0,
    tilesLoaded: 0,
    loadBuckets: [],
    maxFrameGapMs: 0,
    marks: {},
    slowInvokes: [],
    finished: false,
  };
  // When does the event loop first breathe after the click?
  queueMicrotask(() => noteMark("microtask"));
  setTimeout(() => noteMark("timeout0"), 0);
  // A started marker, so a transition that never produces a tile is still visible.
  setSetting(SHELL_TIMING_KEY, JSON.stringify({ from, started: true })).catch(() => {});
  const t = current;
  t.quiet = setTimeout(() => finish(t), 10000);
  // Stall detector: a frame gap far above 16 ms is the main thread blocked.
  let last = t.start;
  const tick = (now: number) => {
    if (t.finished) return;
    const gap = now - last;
    if (gap > t.maxFrameGapMs) {
      t.maxFrameGapMs = gap;
      t.maxFrameGapAt = now;
    }
    last = now;
    t.raf = requestAnimationFrame(tick);
  };
  t.raf = requestAnimationFrame(tick);
}

/** Wrap a backend call so slow ones during a transition are attributed by name. `rows`
 *  is a cheap size hint: the length of the first array found one level down. */
export async function timedInvoke<T>(cmd: string, run: () => Promise<T>): Promise<T> {
  const t = current;
  if (!t || t.finished) return run();
  const start = performance.now();
  try {
    return await run();
  } finally {
    const ms = performance.now() - start;
    if (ms >= 50) {
      t.slowInvokes.push({ cmd, startMs: Math.round((start - t.start) * 10) / 10, ms: Math.round(ms * 10) / 10 });
    }
  }
}

/** Attach a row count to the most recent slow invoke of `cmd` (called by the wrapper's caller). */
export function noteInvokeRows(cmd: string, rows: number): void {
  const t = current;
  if (!t) return;
  for (let i = t.slowInvokes.length - 1; i >= 0; i--) {
    if (t.slowInvokes[i].cmd === cmd) {
      t.slowInvokes[i].rows = rows;
      return;
    }
  }
}

/** Record a named moment once per transition (the first time it is hit). */
export function noteMark(label: string): void {
  const t = current;
  if (!t || t.finished || label in t.marks) return;
  t.marks[label] = Math.round((performance.now() - t.start) * 10) / 10;
}

/** The grid scrolled to the selected photo on mount. */
export function noteGridScroll(): void {
  if (!current || current.finished || current.scrollAt !== undefined) return;
  current.scrollAt = performance.now();
}

/** The grid's Profiler onRender for its mount commit. */
export function noteGridCommit(phase: string, actualDuration: number): void {
  if (!current || current.finished || phase !== "mount") return;
  current.commitAt = performance.now();
  current.commitMs = actualDuration;
}

/** A thumbnail tile mounted (whether or not its image has loaded yet). */
export function noteTileMounted(): void {
  if (!current || current.finished) return;
  current.tilesMounted++;
}

/** A thumbnail's image finished loading — the tile is painted on the next frame. */
export function noteTileLoaded(): void {
  const t = current;
  if (!t || t.finished) return;
  t.tilesLoaded++;
  const now = performance.now();
  if (t.firstTileAt === undefined) t.firstTileAt = now;
  t.lastTileAt = now;
  const b = Math.floor((now - t.start) / BUCKET_MS);
  while (t.loadBuckets.length <= b) t.loadBuckets.push(0);
  t.loadBuckets[b]++;
  if (t.quiet) clearTimeout(t.quiet);
  t.quiet = setTimeout(() => finish(t), QUIET_MS);
}

function finish(t: Transition): void {
  if (t.finished) return;
  t.finished = true;
  if (t.raf) cancelAnimationFrame(t.raf);
  const summary = summarizeTransition(t);
  console.debug(`[shell-timing] ${JSON.stringify(summary)}`);
  setSetting(SHELL_TIMING_KEY, JSON.stringify(summary)).catch(() => {});
}

export interface ShellSummary {
  from: string;
  /** Back → grid React commit landed. */
  toCommitMs: number | null;
  /** The commit's own render cost. */
  commitMs: number | null;
  /** Back → first thumbnail painted. */
  toFirstTileMs: number | null;
  /** Back → the last thumbnail that loaded (then nothing for QUIET_MS). */
  toLastTileMs: number | null;
  tilesMounted: number;
  tilesLoaded: number;
  /** Tile loads per 250 ms since Back. */
  loadBuckets: number[];
  toScrollMs: number | null;
  maxFrameGapMs: number;
  maxFrameGapEndMs: number | null;
  marks: Record<string, number>;
  slowInvokes: { cmd: string; startMs: number; ms: number; rows?: number }[];
}

export function summarizeTransition(t: Transition): ShellSummary {
  const r = (v: number | undefined) => (v === undefined ? null : Math.round((v - t.start) * 10) / 10);
  return {
    from: t.from,
    toCommitMs: r(t.commitAt),
    commitMs: t.commitMs === undefined ? null : Math.round(t.commitMs * 10) / 10,
    toFirstTileMs: r(t.firstTileAt),
    toLastTileMs: r(t.lastTileAt),
    tilesMounted: t.tilesMounted,
    tilesLoaded: t.tilesLoaded,
    loadBuckets: t.loadBuckets,
    toScrollMs: r(t.scrollAt),
    maxFrameGapMs: Math.round(t.maxFrameGapMs),
    maxFrameGapEndMs: r(t.maxFrameGapAt),
    marks: t.marks,
    slowInvokes: t.slowInvokes,
  };
}
