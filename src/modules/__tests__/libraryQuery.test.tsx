// @vitest-environment jsdom
/**
 * Tests for the library view's state (issue #10).
 *
 * The race these exist for: a refresh fires two requests (the photo rows, then the badges
 * for the visible window), and nothing stops an *older* refresh's response from arriving
 * after a newer one has already written. In the app that shows up as a filter change
 * whose grid briefly wears the previous filter's storage icons — or, worse, keeps them,
 * since nothing arrives afterwards to correct it.
 *
 * The interleaving is **forced**, not sampled: every request here is a promise this file
 * resolves by hand, so "the old refresh answers last" happens on demand rather than when
 * the scheduler happens to allow it.
 *
 * What each group pins:
 *
 *  - stale badge response → dropped; the newer refresh's badges survive.
 *  - stale photo page → dropped; the newer refresh's rows survive.
 *  - windowed badges → status is fetched for the rows the grid reports, not for every
 *    matching photo (which is what made a 100k-photo refresh a 100k-id request).
 *  - refresh → badges are re-fetched for the last reported window, because a refresh
 *    clears them and a grid whose window did not move never reports again.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";
import { act, renderHook } from "@testing-library/react";

import type { PhotoPage, PhotoQuery, StorageStatus } from "../api";
import type { Photo } from "../registry";

vi.mock("../api", () => ({
  listPhotos: vi.fn(),
  photoStatuses: vi.fn(),
}));

// Imported after the mock so these are the mock functions themselves.
import { listPhotos, photoStatuses } from "../api";
import { useLibraryQuery } from "../libraryQuery";

const mockListPhotos = vi.mocked(listPhotos);
const mockPhotoStatuses = vi.mocked(photoStatuses);

/** A promise this test resolves by hand. */
function deferred<T>() {
  let settle!: (value: T) => void;
  let fail!: (reason: unknown) => void;
  const promise = new Promise<T>((resolve, reject) => {
    settle = resolve;
    fail = reject;
  });
  return { promise, resolve: settle, reject: fail };
}

function photo(id: number): Photo {
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
  };
}

function page(ids: number[], total = ids.length): PhotoPage {
  return { photos: ids.map(photo), offset: 0, total };
}

/** Ids passed to the n-th `photoStatuses` call. */
function statusCallIds(n: number): number[] {
  return mockPhotoStatuses.mock.calls[n][0];
}

const EMPTY_QUERY: PhotoQuery = {};

beforeEach(() => {
  mockListPhotos.mockReset();
  mockPhotoStatuses.mockReset();
});

describe("useLibraryQuery — a late response from an older refresh", () => {
  it("cannot overwrite the badges a newer refresh already wrote", async () => {
    // Two refreshes. Both get their rows immediately; their badge fetches are held open
    // so this test decides the order they answer in.
    mockListPhotos
      .mockResolvedValueOnce(page([1, 2]))
      .mockResolvedValueOnce(page([3, 4]));
    const oldBadges = deferred<[number, StorageStatus][]>();
    const newBadges = deferred<[number, StorageStatus][]>();
    mockPhotoStatuses
      .mockReturnValueOnce(oldBadges.promise)
      .mockReturnValueOnce(newBadges.promise);

    const { result } = renderHook(() => useLibraryQuery(EMPTY_QUERY));
    // The grid is showing its first rows; from here on a refresh re-asks for this window.
    act(() => result.current.setVisibleRange(0, 2));

    await act(async () => {
      await result.current.refresh();
    });
    expect(statusCallIds(0)).toEqual([1, 2]);

    await act(async () => {
      await result.current.refresh();
    });
    expect(statusCallIds(1)).toEqual([3, 4]);

    // The newer refresh answers first and paints the grid.
    await act(async () => {
      newBadges.resolve([[3, "backedUp"]]);
      await newBadges.promise;
    });
    expect(result.current.statuses.get(3)).toBe("backedUp");

    // …and now the previous filter's badge fetch finally comes back. It describes photos
    // the grid is no longer showing; landing it would repaint the grid from a query the
    // user has already moved off.
    await act(async () => {
      oldBadges.resolve([
        [1, "missing"],
        [2, "missing"],
      ]);
      await oldBadges.promise;
    });

    expect(result.current.statuses.get(3)).toBe("backedUp");
    expect(result.current.statuses.has(1)).toBe(false);
    expect(result.current.statuses.has(2)).toBe(false);
    expect([...result.current.statuses.keys()]).toEqual([3]);
  });

  it("cannot replace the photo rows a newer refresh already wrote", async () => {
    const oldRows = deferred<PhotoPage>();
    const newRows = deferred<PhotoPage>();
    mockListPhotos
      .mockReturnValueOnce(oldRows.promise)
      .mockReturnValueOnce(newRows.promise);
    mockPhotoStatuses.mockResolvedValue([]);

    const { result } = renderHook(() => useLibraryQuery(EMPTY_QUERY));

    // Both in flight at once — the second overtakes the first.
    let first!: Promise<void>;
    let second!: Promise<void>;
    act(() => {
      first = result.current.refresh();
      second = result.current.refresh();
    });

    await act(async () => {
      newRows.resolve(page([3, 4], 2));
      await second;
    });
    expect(result.current.photos.map((p) => p.id)).toEqual([3, 4]);

    await act(async () => {
      oldRows.resolve(page([1, 2], 999));
      await first;
    });
    expect(result.current.photos.map((p) => p.id)).toEqual([3, 4]);
    expect(result.current.total).toBe(2);
  });

  it("reports a failed query to its caller without blanking the newer result", async () => {
    const oldRows = deferred<PhotoPage>();
    mockListPhotos
      .mockReturnValueOnce(oldRows.promise)
      .mockResolvedValueOnce(page([3, 4]));
    mockPhotoStatuses.mockResolvedValue([]);

    const { result } = renderHook(() => useLibraryQuery(EMPTY_QUERY));
    let first!: Promise<void>;
    act(() => {
      first = result.current.refresh();
    });
    await act(async () => {
      await result.current.refresh();
    });
    expect(result.current.photos.map((p) => p.id)).toEqual([3, 4]);

    // The failure reaches whoever asked (the shell puts it in the status bar) — but an
    // empty grid would read as "no photos match", so the rows are left alone.
    await act(async () => {
      oldRows.reject(new Error("catalog closed"));
      await expect(first).rejects.toThrow("catalog closed");
    });
    expect(result.current.photos.map((p) => p.id)).toEqual([3, 4]);
  });
});

describe("useLibraryQuery — badges follow the visible window", () => {
  it("fetches storage status for the reported rows, not for every matching photo", async () => {
    const ids = Array.from({ length: 1000 }, (_, i) => i + 1);
    mockListPhotos.mockResolvedValue(page(ids));
    mockPhotoStatuses.mockResolvedValue([]);

    const { result } = renderHook(() => useLibraryQuery(EMPTY_QUERY));
    await act(async () => {
      await result.current.refresh();
    });
    // Nothing is on screen yet, so nothing has been asked for.
    expect(mockPhotoStatuses).not.toHaveBeenCalled();

    act(() => result.current.setVisibleRange(0, 30));
    expect(statusCallIds(0)).toHaveLength(30);
    expect(statusCallIds(0)[0]).toBe(1);

    // Scrolling asks only for the rows it newly revealed.
    act(() => result.current.setVisibleRange(20, 60));
    expect(statusCallIds(1)).toHaveLength(30);
    expect(statusCallIds(1)[0]).toBe(31);

    // Scrolling back asks for nothing: those rows are already in hand.
    act(() => result.current.setVisibleRange(0, 30));
    expect(mockPhotoStatuses).toHaveBeenCalledTimes(2);
  });

  it("re-fetches the last reported window after a refresh cleared the badges", async () => {
    mockListPhotos.mockResolvedValue(page([1, 2, 3, 4]));
    mockPhotoStatuses.mockResolvedValue([[1, "localOnly"]]);

    const { result } = renderHook(() => useLibraryQuery(EMPTY_QUERY));
    await act(async () => {
      await result.current.refresh();
    });
    act(() => result.current.setVisibleRange(0, 2));
    expect(mockPhotoStatuses).toHaveBeenCalledTimes(1);

    // A refresh (a rating change, a scan commit) leaves the scroll position alone, so the
    // grid never reports again — the refresh itself has to re-ask.
    await act(async () => {
      await result.current.refresh();
    });
    expect(mockPhotoStatuses).toHaveBeenCalledTimes(2);
    expect(statusCallIds(1)).toEqual([1, 2]);
  });

  it("asks for a photo the grid never showed, on request", async () => {
    mockListPhotos.mockResolvedValue(page([1, 2, 3]));
    mockPhotoStatuses.mockResolvedValue([[3, "archived"]]);

    const { result } = renderHook(() => useLibraryQuery(EMPTY_QUERY));
    await act(async () => {
      await result.current.refresh();
    });
    await act(async () => {
      result.current.requestStatuses([3]);
    });
    expect(statusCallIds(0)).toEqual([3]);
    expect(result.current.statuses.get(3)).toBe("archived");
  });

  it("keeps the requested photo's badge across a refresh that cleared it", async () => {
    // The active photo is row 99 — far outside the two rows the grid is showing — and the
    // user rates it, which refreshes. Without re-asking, the inspector's storage line
    // would go blank until the selection changed.
    mockListPhotos.mockResolvedValue(page(Array.from({ length: 100 }, (_, i) => i + 1)));
    mockPhotoStatuses.mockResolvedValue([[99, "offline"]]);

    const { result } = renderHook(() => useLibraryQuery(EMPTY_QUERY));
    await act(async () => {
      await result.current.refresh();
    });
    act(() => result.current.setVisibleRange(0, 2));
    await act(async () => {
      result.current.requestStatuses([99]);
    });
    expect(result.current.statuses.get(99)).toBe("offline");

    await act(async () => {
      await result.current.refresh();
    });
    expect(statusCallIds(2)).toEqual([1, 2, 99]);
    expect(result.current.statuses.get(99)).toBe("offline");
  });
});

describe("useLibraryQuery — clear", () => {
  it("drops the rows and disowns a refresh still in flight", async () => {
    const inFlight = deferred<PhotoPage>();
    mockListPhotos
      .mockResolvedValueOnce(page([1, 2]))
      .mockReturnValueOnce(inFlight.promise);
    mockPhotoStatuses.mockResolvedValue([]);

    const { result } = renderHook(() => useLibraryQuery(EMPTY_QUERY));
    act(() => result.current.setVisibleRange(0, 2));
    await act(async () => {
      await result.current.refresh();
    });
    expect(result.current.photos).toHaveLength(2);

    let switching!: Promise<void>;
    act(() => {
      switching = result.current.refresh();
    });
    act(() => result.current.clear());
    expect(result.current.photos).toHaveLength(0);
    expect(result.current.total).toBe(0);
    expect(result.current.statuses.size).toBe(0);

    // The catalog that request belonged to is closed; its answer must not repopulate the
    // grid behind the switch.
    await act(async () => {
      inFlight.resolve(page([7, 8]));
      await switching;
    });
    expect(result.current.photos).toHaveLength(0);
  });
});
