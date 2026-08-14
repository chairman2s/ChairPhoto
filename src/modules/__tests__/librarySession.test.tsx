// @vitest-environment jsdom
/**
 * Tests for the Library session (issue #15).
 *
 * The session is the state the shell used to hold inline — the scope that decides the
 * query, the selection the user acts on, and the reset that a catalog switch performs. The
 * point of extracting it is that all three can be driven here, directly, with **no App
 * mounted**: no catalog, no modules, no panels, no `catalog:switched` round trip. The
 * shell's job is reduced to wiring the event to {@link useLibrarySession.reset}.
 *
 * What each group pins:
 *
 *  - **catalog switch** — the reset forgets every id that belonged to the closed catalog
 *    (scope, selection, Shift anchor, rows), invalidates the query so the shell refetches
 *    even when the previous catalog was being viewed unfiltered, and disowns a request
 *    still running against it.
 *  - **scope** — the four sidebar scopes are mutually exclusive; re-picking what is already
 *    picked does not invalidate the query (which is what refetches the library).
 *  - **selection** — plain / Ctrl / Shift semantics, the anchor a range spans from,
 *    keyboard stepping at the ends of the result, and the off-grid stack child the grid
 *    never listed.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";
import { act, renderHook } from "@testing-library/react";

import type { ImportBatch, PhotoPage } from "../api";
import type { Photo } from "../registry";

vi.mock("../api", () => ({
  listPhotos: vi.fn(),
  photoStatuses: vi.fn(),
}));

// Imported after the mock so these are the mock functions themselves.
import { listPhotos, photoStatuses } from "../api";
import { defaultScope, useLibrarySession } from "../librarySession";

const mockListPhotos = vi.mocked(listPhotos);
const mockPhotoStatuses = vi.mocked(photoStatuses);

/** A promise this test resolves by hand. */
function deferred<T>() {
  let settle!: (value: T) => void;
  const promise = new Promise<T>((resolve) => {
    settle = resolve;
  });
  return { promise, resolve: settle };
}

function photo(id: number, extra: Partial<Photo> = {}): Photo {
  return {
    id,
    uuid: `uuid-${id}`,
    path: `photo${id}.jpg`,
    rating: 0,
    label: "",
    pickState: "none",
    captureTime: null,
    width: null,
    height: null,
    cameraModel: null,
    lens: null,
    aperture: null,
    shutterSpeed: null,
    iso: null,
    metadataReady: 1,
    sharpness: null,
    sharpnessMethod: null,
    burstFlag: null,
    ...extra,
  };
}

function page(ids: number[], total = ids.length): PhotoPage {
  return { photos: ids.map((id) => photo(id)), offset: 0, total };
}

const BATCH: ImportBatch = {
  id: 42,
  uuid: "batch-uuid",
  sourceLabel: "/media/card",
  note: "",
  createdAt: 0,
  photoCount: 3,
};

/** Mount the session with `ids` already listed, as one refresh would leave it. */
async function sessionWith(ids: number[]) {
  mockListPhotos.mockResolvedValue(page(ids));
  const rendered = renderHook(() => useLibrarySession());
  await act(async () => {
    await rendered.result.current.refresh();
  });
  return rendered;
}

beforeEach(() => {
  mockListPhotos.mockReset();
  mockPhotoStatuses.mockReset();
  mockPhotoStatuses.mockResolvedValue([]);
});

describe("useLibrarySession — a catalog switch", () => {
  it("forgets the scope, the selection and the rows", async () => {
    const { result } = await sessionWith([1, 2, 3]);

    act(() => {
      result.current.selectTag(7);
      result.current.setFilter("pick");
      result.current.setStorageTier("nas");
      result.current.setSort("sharpness_asc");
      result.current.setCamera("X-T5");
      result.current.setLens("XF 35mm");
      result.current.toggleFacet("has-gps");
      result.current.toggleLabel("Red");
    });
    act(() => result.current.select(2));
    act(() => result.current.select(3, { ctrl: true }));
    expect(result.current.selection.ids).toEqual([2, 3]);
    expect(result.current.photos).toHaveLength(3);

    act(() => result.current.reset());

    expect(result.current.scope).toEqual(defaultScope());
    expect(result.current.selection.activeId).toBeNull();
    expect(result.current.selection.ids).toEqual([]);
    expect(result.current.selection.active).toBeNull();
    expect(result.current.selection.targets).toEqual([]);
    expect(result.current.selection.extraPhoto).toBeNull();
    expect(result.current.selection.stackOrigin).toBeNull();
    expect(result.current.photos).toEqual([]);
    expect(result.current.total).toBe(0);
    expect(result.current.statuses.size).toBe(0);
  });

  it("invalidates the query even when the closed catalog was unfiltered", async () => {
    // The trap this pins: a shell that refetches when `query` changes would show an empty
    // grid forever after switching between two catalogs the user never filtered.
    const { result } = await sessionWith([1, 2]);
    const before = result.current.query;

    act(() => result.current.reset());

    expect(result.current.query).not.toBe(before);
    expect(result.current.query).toEqual(before);
  });

  it("disowns a refresh still running against the catalog that closed", async () => {
    const inFlight = deferred<PhotoPage>();
    mockListPhotos
      .mockResolvedValueOnce(page([1, 2]))
      .mockReturnValueOnce(inFlight.promise);

    const { result } = renderHook(() => useLibrarySession());
    await act(async () => {
      await result.current.refresh();
    });

    let switching!: Promise<void>;
    act(() => {
      switching = result.current.refresh();
    });
    act(() => result.current.reset());

    await act(async () => {
      inFlight.resolve(page([7, 8]));
      await switching;
    });
    expect(result.current.photos).toEqual([]);
  });

  it("forgets the Shift anchor, so the next range starts fresh", async () => {
    const { result } = await sessionWith([1, 2, 3, 4, 5]);
    act(() => result.current.select(2)); // anchor ← 2

    act(() => result.current.reset());
    await act(async () => {
      await result.current.refresh(); // the new catalog happens to list the same ids
    });

    // With the anchor still at 2 this would select 2..4; the switch dropped it, so a Shift
    // click with nothing active is an ordinary single selection.
    act(() => result.current.select(4, { shift: true }));
    expect(result.current.selection.ids).toEqual([4]);
  });
});

describe("useLibrarySession — the scope", () => {
  it("keeps the four sidebar scopes mutually exclusive", () => {
    const { result } = renderHook(() => useLibrarySession());

    act(() => result.current.selectTag(7));
    expect(result.current.scope.tagId).toBe(7);

    act(() => result.current.selectAlbum(3));
    expect(result.current.scope).toMatchObject({
      albumId: 3,
      tagId: null,
      batchId: null,
      batch: null,
      smartAlbumId: null,
    });

    act(() => result.current.selectBatch(BATCH));
    expect(result.current.scope).toMatchObject({
      batchId: BATCH.id,
      batch: BATCH,
      tagId: null,
      albumId: null,
      smartAlbumId: null,
    });

    act(() => result.current.selectSmartAlbum(5));
    expect(result.current.scope).toMatchObject({
      smartAlbumId: 5,
      tagId: null,
      albumId: null,
      batchId: null,
      batch: null,
    });

    act(() => result.current.selectTag(9));
    expect(result.current.scope).toMatchObject({ tagId: 9, smartAlbumId: null });
  });

  it("leaves the other scopes alone when one is cleared", () => {
    const { result } = renderHook(() => useLibrarySession());
    act(() => result.current.selectTag(7));
    act(() => result.current.selectAlbum(null)); // the filter bar's "clear album" chip
    expect(result.current.scope.tagId).toBe(7);
    expect(result.current.scope.albumId).toBeNull();
  });

  it("derives the backend query from the scope", () => {
    const { result } = renderHook(() => useLibrarySession());
    act(() => {
      result.current.selectBatch(BATCH);
      result.current.setFilter("reject");
      result.current.setSort("sharpness_desc");
      result.current.toggleFacet("has-gps");
      result.current.toggleLabel("Blue");
    });
    expect(result.current.query).toEqual({
      tagId: null,
      albumId: null,
      batchId: BATCH.id,
      smartAlbumId: null,
      facets: ["has-gps"],
      cullingFilter: "reject",
      storageTier: "all",
      camera: null,
      lens: null,
      labels: ["Blue"],
      sort: "sharpness_desc",
    });
    // `batch` is the session's, not the query's — the backend filters on the id.
    expect(result.current.query).not.toHaveProperty("batch");
  });

  it("does not invalidate the query when the value is already current", () => {
    const { result } = renderHook(() => useLibrarySession());
    act(() => result.current.setFilter("pick"));
    const picked = result.current.query;

    // Clicking the chip you are already on, or the tag that is already the scope: a new
    // query object here would refetch the whole library for nothing.
    act(() => result.current.setFilter("pick"));
    expect(result.current.query).toBe(picked);
    act(() => result.current.selectTag(null));
    expect(result.current.query).toBe(picked);

    act(() => result.current.setFilter("reject"));
    expect(result.current.query).not.toBe(picked);
  });

  it("toggles facets and labels on and off", () => {
    const { result } = renderHook(() => useLibrarySession());
    act(() => result.current.toggleFacet("has-gps"));
    act(() => result.current.toggleFacet("published:flickr"));
    expect(result.current.scope.facets).toEqual(["has-gps", "published:flickr"]);
    act(() => result.current.toggleFacet("has-gps"));
    expect(result.current.scope.facets).toEqual(["published:flickr"]);

    act(() => result.current.toggleLabel(""));
    expect(result.current.scope.labels).toEqual([""]);
    act(() => result.current.toggleLabel(""));
    expect(result.current.scope.labels).toEqual([]);
  });

  it("widens to the whole library for a deep link, keeping the chosen order", () => {
    const { result } = renderHook(() => useLibrarySession());
    act(() => {
      result.current.selectTag(7);
      result.current.setFilter("pick");
      result.current.setStorageTier("local");
      result.current.setCamera("X-T5");
      result.current.toggleFacet("has-gps");
      result.current.toggleLabel("Red");
      result.current.setSort("sharpness_asc");
    });

    act(() => result.current.clearScope());

    // Nothing can hide the linked photo any more…
    expect(result.current.scope).toEqual({ ...defaultScope(), sort: "sharpness_asc" });
    // …but the order is not a filter, so following a link does not silently re-sort the
    // grid the user was working in. A catalog switch, which does, is `reset`.
    expect(result.current.scope.sort).toBe("sharpness_asc");
  });
});

describe("useLibrarySession — the selection", () => {
  it("replaces on a plain click, toggles on Ctrl, and ranges on Shift", async () => {
    const { result } = await sessionWith([1, 2, 3, 4, 5]);

    act(() => result.current.select(2));
    expect(result.current.selection.ids).toEqual([2]);
    expect(result.current.selection.activeId).toBe(2);

    act(() => result.current.select(4, { ctrl: true }));
    expect(result.current.selection.ids).toEqual([2, 4]);
    act(() => result.current.select(4, { ctrl: true }));
    expect(result.current.selection.ids).toEqual([2]);
    expect(result.current.selection.activeId).toBe(4); // still the one last clicked

    // The Ctrl-click pinned the anchor at 4, so the range spans 4↔1 — backwards, and
    // inclusive of both ends.
    act(() => result.current.select(1, { shift: true }));
    expect(result.current.selection.ids).toEqual([1, 2, 3, 4]);

    // Extending again spans the same origin rather than walking the anchor along.
    act(() => result.current.select(5, { shift: true }));
    expect(result.current.selection.ids).toEqual([4, 5]);
  });

  it("selects every row, keeping the active photo as the anchor", async () => {
    const { result } = await sessionWith([1, 2, 3]);
    act(() => result.current.select(2));
    act(() => result.current.selectAll());
    expect(result.current.selection.ids).toEqual([1, 2, 3]);
    expect(result.current.selection.activeId).toBe(2);

    // The anchor moved with select-all, so a following Shift click ranges from the active
    // photo, not from wherever the last range ended.
    act(() => result.current.select(3, { shift: true }));
    expect(result.current.selection.ids).toEqual([2, 3]);
  });

  it("selects the first row when select-all runs with nothing active", async () => {
    const { result } = await sessionWith([4, 5, 6]);
    act(() => result.current.selectAll());
    expect(result.current.selection.activeId).toBe(4);
    expect(result.current.selection.ids).toEqual([4, 5, 6]);
  });

  it("steps the active photo through the result and stops at both ends", async () => {
    const { result } = await sessionWith([1, 2, 3]);
    act(() => result.current.select(1));

    act(() => result.current.stepActive(1));
    expect(result.current.selection.activeId).toBe(2);
    act(() => result.current.stepActive(1));
    expect(result.current.selection.activeId).toBe(3);
    act(() => result.current.stepActive(1)); // already the last row
    expect(result.current.selection.activeId).toBe(3);

    act(() => result.current.stepActive(-1, true)); // Shift+Arrow extends
    expect(result.current.selection.activeId).toBe(2);
    expect(result.current.selection.ids).toEqual([2, 3]);

    act(() => result.current.selectSingle(1));
    act(() => result.current.stepActive(-1)); // already the first row
    expect(result.current.selection.activeId).toBe(1);
    expect(result.current.selection.ids).toEqual([1]);
  });

  it("views a stacked child the grid never listed, and comes back", async () => {
    const { result } = await sessionWith([1, 2, 3]);
    act(() => result.current.select(2));

    const child = photo(99, { stackParentId: 2 });
    act(() => result.current.viewPhoto(child));
    expect(result.current.selection.activeId).toBe(99);
    expect(result.current.selection.extraPhoto).toBe(child);
    expect(result.current.selection.stackOrigin).toBe(2);
    // The active photo resolves even though no row holds it.
    expect(result.current.selection.active).toBe(child);
    // …and it is what a bulk action would apply to.
    expect(result.current.selection.targets).toEqual([99]);

    act(() => result.current.backToOriginal());
    expect(result.current.selection.activeId).toBe(2);
    expect(result.current.selection.extraPhoto).toBeNull();
    expect(result.current.selection.stackOrigin).toBeNull();
    expect(result.current.selection.active?.id).toBe(2);
  });

  it("clears the off-grid view when the grid is selected again", async () => {
    const { result } = await sessionWith([1, 2, 3]);
    act(() => result.current.viewPhoto(photo(99, { stackParentId: 1 })));
    act(() => result.current.select(3));
    expect(result.current.selection.extraPhoto).toBeNull();
    expect(result.current.selection.stackOrigin).toBeNull();

    // A row the grid *is* showing is an ordinary selection, not an off-grid view.
    act(() => result.current.viewPhoto(photo(1)));
    expect(result.current.selection.extraPhoto).toBeNull();
    expect(result.current.selection.activeId).toBe(1);
  });

  it("navigates without moving the anchor, for a module driving the shell", async () => {
    const { result } = await sessionWith([1, 2, 3, 4]);
    act(() => result.current.select(1)); // anchor ← 1
    act(() => result.current.selectQuiet(3));
    expect(result.current.selection.ids).toEqual([3]);

    // The anchor is still where the user left it, so a Shift click spans 1↔4.
    act(() => result.current.select(4, { shift: true }));
    expect(result.current.selection.ids).toEqual([1, 2, 3, 4]);
  });

  it("falls back to the active photo when nothing is selected", async () => {
    const { result } = await sessionWith([1, 2, 3]);
    expect(result.current.selection.targets).toEqual([]);

    act(() => result.current.select(2));
    act(() => result.current.select(2, { ctrl: true })); // toggled the only one back off
    expect(result.current.selection.ids).toEqual([]);
    expect(result.current.selection.targets).toEqual([2]);
  });

  it("exposes the selected rows for the host bridge", async () => {
    const { result } = await sessionWith([1, 2, 3]);
    act(() => result.current.select(1));
    act(() => result.current.select(3, { ctrl: true }));
    expect(result.current.selection.photos.map((p) => p.id)).toEqual([1, 3]);
  });

  it("asks for the active photo's storage badge, wherever it came from", async () => {
    const { result } = await sessionWith([1, 2, 3]);
    await act(async () => {
      result.current.select(2);
    });
    expect(mockPhotoStatuses).toHaveBeenCalledWith([2]);

    // Including a stack child, which is in no row the grid ever reported.
    await act(async () => {
      result.current.viewPhoto(photo(99, { stackParentId: 2 }));
    });
    expect(mockPhotoStatuses).toHaveBeenCalledWith([99]);
  });
});
