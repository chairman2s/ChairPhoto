/**
 * The Library surface as one session: what the grid is asking for, what is selected, and
 * what a catalog switch has to forget.
 *
 * This is the back half of issue #15. {@link ./libraryQuery.useLibraryQuery} already owns
 * the *result* of a query — the rows, the badge map and the generation token that keeps a
 * slow response from an older filter off a newer grid. What it does not own is everything
 * that decides *which* query to run and *which* rows the user is acting on, and that was
 * still spread across the shell: eleven filter states, four mutually-exclusive scopes, an
 * active id, a selection list, a Shift anchor, an off-grid stack-child view, and a
 * catalog-switch handler that had to remember to reset all of it in one place.
 *
 * The shell keeps its render composition. What moves here is the *state transitions*:
 *
 *  - **Scope.** The four sidebar scopes (tag / album / import batch / smart album) are
 *    mutually exclusive, and the culling chips, facets, storage tier, camera, lens,
 *    colour labels and sort AND on top of whichever one is active. One object, one set of
 *    verbs, one derived {@link PhotoQuery}.
 *  - **Selection.** Plain / Ctrl / Shift semantics, the anchor a Shift range spans from,
 *    keyboard stepping, select-all, and the off-grid stack child the loupe can show even
 *    though the grid never listed it. The shell used to index into the row array in five
 *    places to do this.
 *  - **Reset.** A catalog switch invalidates every id in the three groups above at once.
 *    {@link LibrarySession.reset} is that transition, and it is a plain function call —
 *    testable without mounting the app, which is the point of the extraction.
 *
 * What deliberately stays in the shell: the tag tree (the sidebar's, not the grid's), the
 * loupe/develop view flags, progress indicators, dialogs, and every panel's reload key.
 * Those are shell concerns that survive a query change, and folding them in here would
 * just move the pile rather than split it.
 *
 * ### On identity
 *
 * `query` is what {@link useLibraryQuery} refetches on, so a *new object with the same
 * values* is a wasted round trip to SQLite. Every scope verb therefore returns the current
 * scope object unchanged when nothing actually differs (see {@link applyPatch}) — clicking
 * the culling chip you are already on must not refetch the library. The two resets are the
 * exception: they allocate unconditionally, because after a catalog switch the same
 * filters describe a different catalog and the rows must be fetched again.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useLibraryQuery } from "./libraryQuery";
import type {
  CullingFilter,
  ImportBatch,
  PhotoQuery,
  PhotoSort,
  StorageStatus,
  StorageTier,
} from "./api";
import type { Photo } from "./registry";

/**
 * Everything that decides which photos the Library is showing.
 *
 * Field for field the shell's old filter states, and — apart from `batch` — field for
 * field {@link PhotoQuery}, so the derived query is a rename (`filter` → `cullingFilter`)
 * rather than a translation. `batch` rides along because the export panel and the filter
 * bar need the row, not just its id.
 */
export interface LibraryScope {
  /** Restrict to a tag and its descendants. Excludes the other three scopes. */
  tagId: number | null;
  /** Restrict to an album (which also imposes the album's own member order). */
  albumId: number | null;
  /** Restrict to one import batch. */
  batchId: number | null;
  /** The batch row behind `batchId`, for panels that need its label, not just its id. */
  batch: ImportBatch | null;
  /** Evaluate a saved smart-album rule live. */
  smartAlbumId: number | null;
  /** The culling chip: all / unrated / pick / reject / edited. */
  filter: CullingFilter;
  storageTier: StorageTier;
  /** Derived boolean facets, ANDed in. */
  facets: string[];
  camera: string | null;
  lens: string | null;
  /** Colour labels, OR-combined ("" = the No-label dot). Empty = no label filter. */
  labels: string[];
  sort: PhotoSort;
}

/** The whole library, oldest first — what a fresh catalog opens on. */
export function defaultScope(): LibraryScope {
  return {
    tagId: null,
    albumId: null,
    batchId: null,
    batch: null,
    smartAlbumId: null,
    filter: "all",
    storageTier: "all",
    facets: [],
    camera: null,
    lens: null,
    labels: [],
    sort: "date",
  };
}

const SCOPE_KEYS = Object.keys(defaultScope()) as (keyof LibraryScope)[];

/**
 * Apply `patch`, returning the *same* scope object when nothing changed.
 *
 * The identity is load-bearing: it is what `query` — and therefore the refetch — keys on.
 * React's own `useState` bail-out gives scalars this for free; one combined object does
 * not, so it is done here instead.
 */
function applyPatch(scope: LibraryScope, patch: Partial<LibraryScope>): LibraryScope {
  const next = { ...scope, ...patch };
  return SCOPE_KEYS.every((k) => Object.is(next[k], scope[k])) ? scope : next;
}

/** Modifier keys as the grid reports them from a click. */
export interface SelectMods {
  ctrl?: boolean;
  shift?: boolean;
}

/** Who the user is acting on. */
export interface LibrarySelection {
  /** The active/primary photo — the one the inspector, loupe and Develop view show. */
  activeId: number | null;
  /** Every selected id, in selection order. */
  ids: number[];
  /**
   * The active photo's row: normally the selected grid tile, or the off-grid stack child
   * being viewed. `null` when the active id names no row we hold (mid-refresh, say).
   */
  active: Photo | null;
  /** The selected rows that are actually in the current result — for the host bridge. */
  photos: Photo[];
  /**
   * What a bulk action applies to: the whole selection, or the active photo when nothing
   * is selected. Empty when neither exists, so callers can act unconditionally.
   */
  targets: number[];
  /**
   * A photo outside the grid's result — a stacked child, which the grid hides under its
   * master — that is nevertheless being viewed.
   */
  extraPhoto: Photo | null;
  /** The master to return to from a stack-child view ("Back to original"). */
  stackOrigin: number | null;
}

/** The Library surface's state, and the verbs that move it. */
export interface LibrarySession {
  // --- the rows -------------------------------------------------------------
  /** The rows the current query returned. */
  photos: Photo[];
  /** How many photos match the query (see {@link useLibraryQuery}). */
  total: number;
  /** Storage status for the rows that have been asked for, by photo id. */
  statuses: Map<number, StorageStatus>;
  /** The current scope as the one typed query the backend takes. Memoized. */
  query: PhotoQuery;
  /** Re-run the query. Resolves when this refresh has been applied — or discarded. */
  refresh: () => Promise<void>;
  /** Report which rows are on screen, as a half-open `[start, end)` over `photos`. */
  setVisibleRange: (start: number, end: number) => void;

  // --- the scope ------------------------------------------------------------
  scope: LibraryScope;
  /** Pick a tag scope (or clear it). Clears the other three scopes. */
  selectTag: (tagId: number | null) => void;
  /** Pick an album scope (or clear it). Clears the other three scopes. */
  selectAlbum: (albumId: number | null) => void;
  /** Pick an import-batch scope (or clear it). Clears the other three scopes. */
  selectBatch: (batch: ImportBatch | null) => void;
  /** Pick a smart-album scope (or clear it). Clears the other three scopes. */
  selectSmartAlbum: (smartAlbumId: number | null) => void;
  setFilter: (filter: CullingFilter) => void;
  setStorageTier: (tier: StorageTier) => void;
  setSort: (sort: PhotoSort) => void;
  setCamera: (camera: string | null) => void;
  setLens: (lens: string | null) => void;
  /** Add or remove one derived facet. */
  toggleFacet: (key: string) => void;
  /** Add or remove one colour label. */
  toggleLabel: (label: string) => void;
  /**
   * Widen to the whole library, keeping the chosen order.
   *
   * For a deep link, whose target may sit outside every active filter: the grid has to
   * contain the photo before it can be selected. The sort is not a filter — it cannot hide
   * a row — so it survives.
   */
  clearScope: () => void;

  // --- the selection --------------------------------------------------------
  selection: LibrarySelection;
  /**
   * Select with modifier-key semantics: plain = single, Ctrl/Cmd = toggle, Shift = range
   * from the anchor. Supersedes any off-grid stack-child view.
   */
  select: (id: number, mods?: SelectMods) => void;
  /**
   * Make one photo the whole selection and the new Shift anchor, without disturbing an
   * off-grid view. Keyboard navigation's step; a click should use {@link select}.
   */
  selectSingle: (id: number) => void;
  /**
   * Set the selection without touching the anchor — for a module navigating the shell
   * (the Tag Graph, the map filmstrip) rather than the user working the grid.
   */
  selectQuiet: (id: number) => void;
  /** Select every row in the current result, keeping the active photo if there is one. */
  selectAll: () => void;
  /**
   * Move the active photo `delta` rows through the result, stopping at either end.
   * `extend` grows the Shift range instead of replacing the selection.
   */
  stepActive: (delta: number, extend?: boolean) => void;
  /**
   * View any stack member. A row the grid is showing selects normally; a stacked child is
   * off-grid, so it is held aside and its master remembered for "Back to original".
   */
  viewPhoto: (photo: Photo) => void;
  /** Return from a stack-child view to the master it is stacked under. */
  backToOriginal: () => void;

  // --- lifecycle ------------------------------------------------------------
  /**
   * Forget the previous catalog: scope back to the whole library, nothing selected, no
   * rows, and any request still in flight disowned.
   *
   * Every id above — tag, album, batch, smart album, photo — belongs to the catalog that
   * just closed, and would name something else entirely (or nothing) in the one that
   * just opened.
   */
  reset: () => void;
}

/**
 * Hold one Library session.
 *
 * *When* to refresh stays the caller's policy, as it is for {@link useLibraryQuery}: the
 * shell refreshes once the catalog is open, whenever `refresh`'s identity changes (which
 * is whenever `query` does), and after any command that alters the library.
 */
export function useLibrarySession(): LibrarySession {
  const [scope, setScope] = useState<LibraryScope>(defaultScope);
  const patch = useCallback((next: Partial<LibraryScope>) => {
    setScope((cur) => applyPatch(cur, next));
  }, []);

  const query = useMemo<PhotoQuery>(
    () => ({
      tagId: scope.tagId,
      albumId: scope.albumId,
      batchId: scope.batchId,
      smartAlbumId: scope.smartAlbumId,
      facets: scope.facets,
      cullingFilter: scope.filter,
      storageTier: scope.storageTier,
      camera: scope.camera,
      lens: scope.lens,
      labels: scope.labels,
      sort: scope.sort,
    }),
    [scope],
  );
  const library = useLibraryQuery(query);
  const { photos, requestStatuses, clear: clearLibrary } = library;

  const [activeId, setActiveId] = useState<number | null>(null);
  const [ids, setIds] = useState<number[]>([]);
  // Fixed anchor for Shift ranges (Shift-click and Shift+Arrow). Set on any plain/toggle
  // selection; the range spans anchor↔active while Shift extends it. A ref so extending
  // neither moves the anchor nor re-renders.
  const anchor = useRef<number | null>(null);
  const [extraPhoto, setExtraPhoto] = useState<Photo | null>(null);
  const [stackOrigin, setStackOrigin] = useState<number | null>(null);

  // --- scope verbs ----------------------------------------------------------
  // The four scopes are one primary filter each (the culling chips and facets still AND on
  // top — see docs/smart-albums.md), so picking one clears the other three. Clearing one
  // leaves the others alone: there is nothing to be exclusive with.
  const selectTag = useCallback(
    (tagId: number | null) => {
      patch(
        tagId != null
          ? { tagId, albumId: null, batchId: null, batch: null, smartAlbumId: null }
          : { tagId },
      );
    },
    [patch],
  );
  const selectAlbum = useCallback(
    (albumId: number | null) => {
      patch(
        albumId != null
          ? { albumId, tagId: null, batchId: null, batch: null, smartAlbumId: null }
          : { albumId },
      );
    },
    [patch],
  );
  const selectBatch = useCallback(
    (batch: ImportBatch | null) => {
      const batchId = batch?.id ?? null;
      patch(
        batch != null
          ? { batchId, batch, tagId: null, albumId: null, smartAlbumId: null }
          : { batchId, batch },
      );
    },
    [patch],
  );
  const selectSmartAlbum = useCallback(
    (smartAlbumId: number | null) => {
      patch(
        smartAlbumId != null
          ? { smartAlbumId, tagId: null, albumId: null, batchId: null, batch: null }
          : { smartAlbumId },
      );
    },
    [patch],
  );
  const setFilter = useCallback((filter: CullingFilter) => patch({ filter }), [patch]);
  const setStorageTier = useCallback(
    (storageTier: StorageTier) => patch({ storageTier }),
    [patch],
  );
  const setSort = useCallback((sort: PhotoSort) => patch({ sort }), [patch]);
  const setCamera = useCallback((camera: string | null) => patch({ camera }), [patch]);
  const setLens = useCallback((lens: string | null) => patch({ lens }), [patch]);
  const toggleFacet = useCallback((key: string) => {
    setScope((cur) => ({
      ...cur,
      facets: cur.facets.includes(key)
        ? cur.facets.filter((k) => k !== key)
        : [...cur.facets, key],
    }));
  }, []);
  const toggleLabel = useCallback((label: string) => {
    setScope((cur) => ({
      ...cur,
      labels: cur.labels.includes(label)
        ? cur.labels.filter((l) => l !== label)
        : [...cur.labels, label],
    }));
  }, []);
  const clearScope = useCallback(() => {
    setScope((cur) => ({ ...defaultScope(), sort: cur.sort }));
  }, []);

  // --- selection verbs ------------------------------------------------------
  // These read `photos` from state rather than a ref, so a handler created in one render
  // ranges over the rows *that* render was showing. It matters for auto-advance after a
  // cull: the decision is applied, the library refreshes, and only then does the handler
  // step forward — over the list it started from, not the one the refresh produced (from
  // which the photo it just rated may have been filtered out).
  const select = useCallback(
    (id: number, mods?: SelectMods) => {
      const from = anchor.current ?? activeId;
      if (mods?.shift && from != null) {
        anchor.current = from; // pin it, so further Shift steps span the same origin
        const a = photos.findIndex((p) => p.id === from);
        const b = photos.findIndex((p) => p.id === id);
        if (a !== -1 && b !== -1) {
          const [lo, hi] = a < b ? [a, b] : [b, a];
          setIds(photos.slice(lo, hi + 1).map((p) => p.id));
        }
      } else if (mods?.ctrl) {
        setIds((prev) => (prev.includes(id) ? prev.filter((x) => x !== id) : [...prev, id]));
        anchor.current = id; // subsequent Shift ranges from this toggle
      } else {
        setIds([id]);
        anchor.current = id;
      }
      setActiveId(id);
      setExtraPhoto(null); // a grid selection supersedes an off-grid stack-child view
      setStackOrigin(null);
    },
    [photos, activeId],
  );

  const selectSingle = useCallback((id: number) => {
    setActiveId(id);
    setIds([id]);
    anchor.current = id; // plain navigation resets the Shift anchor
  }, []);

  const selectQuiet = useCallback((id: number) => {
    setActiveId(id);
    setIds([id]);
  }, []);

  const selectAll = useCallback(() => {
    if (!photos.length) return;
    setIds(photos.map((p) => p.id));
    const active = activeId ?? photos[0].id;
    setActiveId(active);
    anchor.current = active;
  }, [photos, activeId]);

  const stepActive = useCallback(
    (delta: number, extend = false) => {
      if (activeId == null) return;
      // An off-grid stack child is in no row, so `from` is -1 and a forward step lands on
      // the first row — the same place the shell's inline index arithmetic put it.
      const from = photos.findIndex((p) => p.id === activeId);
      const to = from + delta;
      if (to < 0 || to >= photos.length) return;
      const next = photos[to].id;
      if (extend) select(next, { shift: true });
      else selectSingle(next);
    },
    [photos, activeId, select, selectSingle],
  );

  const viewPhoto = useCallback(
    (photo: Photo) => {
      if (photos.some((p) => p.id === photo.id)) {
        setExtraPhoto(null);
        setStackOrigin(null);
      } else {
        setExtraPhoto(photo);
        setStackOrigin(photo.stackParentId ?? activeId);
      }
      setActiveId(photo.id);
      setIds([photo.id]);
    },
    [photos, activeId],
  );

  const backToOriginal = useCallback(() => {
    const origin = stackOrigin;
    setStackOrigin(null);
    if (origin != null) select(origin);
  }, [stackOrigin, select]);

  // --- derived selection ----------------------------------------------------
  const active = useMemo(
    () =>
      photos.find((p) => p.id === activeId) ??
      (extraPhoto && extraPhoto.id === activeId ? extraPhoto : null),
    [photos, activeId, extraPhoto],
  );
  const selectedPhotos = useMemo(
    () => photos.filter((p) => ids.includes(p.id)),
    [photos, ids],
  );
  const targets = useMemo(
    () => (ids.length ? ids : activeId != null ? [activeId] : []),
    [ids, activeId],
  );
  const selection = useMemo<LibrarySelection>(
    () => ({
      activeId,
      ids,
      active,
      photos: selectedPhotos,
      targets,
      extraPhoto,
      stackOrigin,
    }),
    [activeId, ids, active, selectedPhotos, targets, extraPhoto, stackOrigin],
  );

  // The active photo's storage badge, which the inspector, the loupe and the right-click
  // menu all read. The grid asks for the rows it is showing; a photo reached some other
  // way — a deep link straight into the loupe, a stack child — never was one.
  useEffect(() => {
    if (activeId != null) requestStatuses([activeId]);
  }, [activeId, requestStatuses]);

  // --- lifecycle ------------------------------------------------------------
  const reset = useCallback(() => {
    // A fresh object, unconditionally: the same filters over a different catalog are a
    // different question, so this must invalidate `query` and make the shell refetch even
    // when the previous catalog was being viewed unfiltered.
    setScope(defaultScope());
    setActiveId(null);
    setIds([]);
    anchor.current = null;
    setExtraPhoto(null);
    setStackOrigin(null);
    // Drops the rows immediately (the grid must not show the closed catalog's photos while
    // the new query resolves) and disowns whatever is still in flight against it.
    clearLibrary();
  }, [clearLibrary]);

  return useMemo(
    () => ({
      photos,
      total: library.total,
      statuses: library.statuses,
      query,
      refresh: library.refresh,
      setVisibleRange: library.setVisibleRange,
      scope,
      selectTag,
      selectAlbum,
      selectBatch,
      selectSmartAlbum,
      setFilter,
      setStorageTier,
      setSort,
      setCamera,
      setLens,
      toggleFacet,
      toggleLabel,
      clearScope,
      selection,
      select,
      selectSingle,
      selectQuiet,
      selectAll,
      stepActive,
      viewPhoto,
      backToOriginal,
      reset,
    }),
    [
      photos,
      library.total,
      library.statuses,
      library.refresh,
      library.setVisibleRange,
      query,
      scope,
      selectTag,
      selectAlbum,
      selectBatch,
      selectSmartAlbum,
      setFilter,
      setStorageTier,
      setSort,
      setCamera,
      setLens,
      toggleFacet,
      toggleLabel,
      clearScope,
      selection,
      select,
      selectSingle,
      selectQuiet,
      selectAll,
      stepActive,
      viewPhoto,
      backToOriginal,
      reset,
    ],
  );
}
