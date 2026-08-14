/**
 * The library view's data: which photos match the current query, and the grid metadata
 * that hangs off them.
 *
 * This is the front half of issue #10. The shell used to hold four pieces of state
 * (`photos`, `statuses`, `versionCnt`, and the eleven filter arguments) and refresh them
 * by hand, which made two failures easy to write and hard to see:
 *
 * 1. **Badges for the whole result.** A refresh fetched storage status and version counts
 *    for *every* returned id — one 100k-id request per filter change, for a grid showing
 *    thirty tiles. Version counts now ride the photo row (`photo.versionCount`), and
 *    storage status — which cannot ride the row, because deriving it needs volume
 *    reachability that must be stat-ed off the catalog lock — is fetched for the rows the
 *    grid says are on screen.
 * 2. **Late responses winning.** Nothing tied a response to the refresh that asked for it,
 *    so a slow badge fetch from the *previous* filter could land after the new one and
 *    repaint the grid with the old filter's metadata. Every refresh here takes a
 *    generation token, and a response whose token is stale is dropped.
 *
 * Why not {@link ../modules/ownedEvents.forJob}: that filters *pushed* events, which carry
 * the job id they belong to, against the run the panel adopted. A badge response is a
 * *pulled* promise that carries nothing — the generation is known at call time and checked
 * after the await. Same idea, different shape; there is nothing in the event utility this
 * could reuse without inventing an id to stamp onto a response we already own.
 *
 * The hook owns state only. It renders nothing and knows nothing about panels, so the
 * shell can keep shrinking around it (issue #15).
 */
import { useCallback, useMemo, useRef, useState } from "react";
import { listPhotos, photoStatuses } from "./api";
import type { PhotoQuery, StorageStatus } from "./api";
import type { Photo } from "./registry";

/** The library view's state, plus the two ways to drive it. */
export interface LibraryQuery {
  /** The rows the current query returned. */
  photos: Photo[];
  /** How many photos match the query (equals `photos.length` until the grid windows). */
  total: number;
  /** Storage status for the rows that have been asked for, by photo id. */
  statuses: Map<number, StorageStatus>;
  /** Re-run the query. Resolves when this refresh has been applied — or discarded. */
  refresh: () => Promise<void>;
  /**
   * Report which rows are on screen (a half-open `[start, end)` range over `photos`).
   * Stable across renders, so it is safe in an effect's dependency list.
   */
  setVisibleRange: (start: number, end: number) => void;
  /**
   * Ask for specific photos' storage status — for rows the grid never showed, such as the
   * photo a deep link opened straight into the loupe. Ids already fetched are skipped.
   */
  requestStatuses: (ids: number[]) => void;
  /**
   * Drop everything, and disown whatever is in flight.
   *
   * For a catalog switch: the rows on screen belong to a catalog that is no longer open,
   * and a refresh still running against the old one must not be allowed to land.
   */
  clear: () => void;
}

/**
 * Hold the result of `query`, and re-run it on demand.
 *
 * *When* to run is the caller's policy, not this hook's: the shell refreshes once the
 * catalog is open, again whenever `refresh`'s identity changes (which is whenever `query`
 * does), and after any command that alters the library. Pass a **memoized** query — it is
 * `refresh`'s dependency, so a fresh object every render would refetch every render.
 */
export function useLibraryQuery(query: PhotoQuery): LibraryQuery {
  const [photos, setPhotos] = useState<Photo[]>([]);
  const [total, setTotal] = useState(0);
  const [statuses, setStatuses] = useState<Map<number, StorageStatus>>(new Map());

  // Refs, not state: a response resolving after a newer refresh started must be excluded
  // *synchronously*, and state has not updated by then.
  //
  // `generation` is bumped by every refresh and captured by everything it starts. A
  // response whose captured token is not the current one belongs to a refresh nobody is
  // looking at any more, and is dropped rather than written.
  const generation = useRef(0);
  // Ids already requested under the current generation, so scrolling back over rows does
  // not re-ask for them. Cleared with each refresh: a new query means new answers.
  const requested = useRef(new Set<number>());
  // The latest rows and visible range, readable from callbacks without re-creating them.
  const photosRef = useRef<Photo[]>([]);
  const range = useRef<[number, number]>([0, 0]);
  // The ids most recently asked for by id rather than by window — in practice the active
  // photo. A refresh clears the badges, so these are re-fetched alongside the window;
  // otherwise the inspector's storage line would go blank after a rating change whenever
  // the active photo sits outside the rows the grid is showing. Replaced, not
  // accumulated: re-fetching every id ever pinned would be the whole-result fetch again.
  const pinned = useRef<number[]>([]);

  const fetchStatuses = useCallback((gen: number, ids: number[]) => {
    const missing = ids.filter((id) => !requested.current.has(id));
    if (missing.length === 0) return;
    for (const id of missing) requested.current.add(id);
    photoStatuses(missing)
      .then((pairs) => {
        // The guard: a newer refresh has already written the grid's metadata, and this
        // answer describes photos it may no longer be showing.
        if (gen !== generation.current) return;
        setStatuses((prev) => {
          const next = new Map(prev);
          for (const [id, status] of pairs) next.set(id, status);
          return next;
        });
      })
      .catch(() => {
        // Not answered — let a later window (or a retry) ask again.
        if (gen !== generation.current) return;
        for (const id of missing) requested.current.delete(id);
      });
  }, []);

  const requestStatuses = useCallback(
    (ids: number[]) => {
      pinned.current = ids;
      fetchStatuses(generation.current, ids);
    },
    [fetchStatuses],
  );

  const setVisibleRange = useCallback(
    (start: number, end: number) => {
      range.current = [start, end];
      const ids = photosRef.current.slice(start, end).map((p) => p.id);
      fetchStatuses(generation.current, ids);
    },
    [fetchStatuses],
  );

  const refresh = useCallback(async () => {
    const gen = ++generation.current;
    // A failed query is the caller's to report (the shell surfaces it in the status bar),
    // so this deliberately does not swallow it — and it leaves the rows alone rather than
    // blanking them, because an empty grid reads as "no photos match", not "the query
    // failed".
    const page = await listPhotos(query);
    // The same guard as the badges get: two refreshes in flight (a filter change during a
    // live scan, say) must not resolve into the older one's result set.
    if (gen !== generation.current) return;
    photosRef.current = page.photos;
    requested.current = new Set();
    setPhotos(page.photos);
    setTotal(page.total);
    setStatuses(new Map());
    // Re-fetch badges for the window the grid last reported. Both halves are needed: the
    // grid reports when its window *moves*, and a refresh that leaves the scroll position
    // alone moves nothing — but it did just clear the badges, so somebody has to ask
    // again. Before the grid has ever reported (the first refresh of a session) the range
    // is empty and this fetches nothing; the grid's mount effect asks a moment later.
    const [start, end] = range.current;
    fetchStatuses(gen, [
      ...page.photos.slice(start, end).map((p) => p.id),
      ...pinned.current,
    ]);
  }, [query, fetchStatuses]);

  const clear = useCallback(() => {
    // Bumping the generation is the disowning: anything in flight now carries a stale
    // token and will be dropped when it resolves.
    generation.current += 1;
    photosRef.current = [];
    requested.current = new Set();
    pinned.current = [];
    setPhotos([]);
    setTotal(0);
    setStatuses(new Map());
  }, []);

  return useMemo(
    () => ({ photos, total, statuses, refresh, setVisibleRange, requestStatuses, clear }),
    [photos, total, statuses, refresh, setVisibleRange, requestStatuses, clear],
  );
}
