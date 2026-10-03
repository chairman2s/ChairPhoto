// The batch cache warm-up as the shell follows it (App.tsx `onScan` → `cacheImages`).
//
// The core runs the warm-up as an owned job (`app::cache`): a newer warm-up or a catalog switch
// stops the running one, whose `cache_images` call then rejects with `CACHE_CANCELLED`. That is
// a quiet supersede, not a failure. Its `cache:progress` events carry the job id, which grows
// with every start, so progress from a job older than the newest one seen is a superseded
// pass's straggler and moves nothing.

import type { CacheProgress } from "./api";

/** What a stopped warm-up answers (core `app::cache::CACHE_CANCELLED`). */
export const CACHE_CANCELLED = "Cache warm-up cancelled";

/** Whether a `cache_images` rejection is a supersede (a newer warm-up, a catalog switch). */
export function isCacheCancelled(error: unknown): boolean {
  return String(error).startsWith(CACHE_CANCELLED);
}

/** The status line for a `cache_images` result: `null` for a supersede (say nothing). */
export function cacheResultLine(error: unknown | null): string | null {
  if (error == null) return "Cache ready";
  return isCacheCancelled(error) ? null : `Cache failed: ${error}`;
}

/** Follows the newest warm-up's progress. `onProgress` answers the status line, or `null` for a
 *  straggler of an older job. */
export function createCacheFollower() {
  let newest = 0;
  return {
    onProgress(p: CacheProgress): string | null {
      if (p.job < newest) return null;
      newest = p.job;
      return p.done < p.total ? `Caching ${p.done}/${p.total}…` : `Cache ready (${p.total})`;
    },
  };
}
