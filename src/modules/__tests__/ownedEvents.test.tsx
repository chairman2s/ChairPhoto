// @vitest-environment jsdom
/**
 * Tests for the shared owned-listener utility (issue #13).
 *
 * These are the frontend half of "tests cover stale events, late listener registration and
 * status-slot clearing once through the shared module". The races are *forced*, not sampled:
 * every registration here is a promise this file resolves by hand, so "the panel unmounted
 * while the registration was in flight" happens on demand rather than when the scheduler
 * happens to allow it.
 *
 * What each group pins, and what breaks without it:
 *
 *  - late registration → a listener resolving after teardown must be stopped, not stored.
 *    Stored, it either outlives the panel with nobody holding its stopper, or it overwrites
 *    (and orphans) the subscription its replacement already installed.
 *  - owner-scoped release → a stale attempt tidying up must not free a replacement's
 *    listener, or the live run stops receiving progress.
 *  - job-id filtering → a superseded run keeps emitting after a newer one starts; without
 *    the filter its stragglers drive the panel's progress bar backwards.
 *  - terminal buffering → the terminal listener must be live before the start command, so a
 *    terminal event can arrive before the job id is known. Dropped, the panel sits in
 *    "indexing" forever.
 */
import { describe, expect, it, vi } from "vitest";
import { act, renderHook } from "@testing-library/react";

import {
  forJob,
  terminalBuffer,
  useOwnedListeners,
  useOwnedSubscription,
} from "../ownedEvents";
import type { Unsubscribe } from "../registry";

/** A registration whose resolution this test controls. */
function deferredSubscribe() {
  const stop = vi.fn();
  let settle!: (value: Unsubscribe | null) => void;
  let fail!: (reason: unknown) => void;
  const promise = new Promise<Unsubscribe | null>((resolve, reject) => {
    settle = resolve;
    fail = reject;
  });
  return {
    stop,
    subscribe: () => promise,
    /** Resolve with a live listener. */
    resolve: () => settle(stop),
    /** Resolve as "this host has no onEvent". */
    resolveUnsupported: () => settle(null),
    /** Reject, as a registration that failed. */
    reject: () => fail(new Error("registration rejected")),
  };
}

describe("useOwnedListeners — late registration", () => {
  it("stops a listener that resolves after the component unmounted", async () => {
    const reg = deferredSubscribe();
    const { result, unmount } = renderHook(() => useOwnedListeners());

    let outcome: Promise<string>;
    act(() => {
      outcome = result.current.attach("progress", Symbol("attempt"), reg.subscribe);
    });

    // The panel goes away while the registration is still in flight.
    unmount();
    await act(async () => {
      reg.resolve();
      await outcome;
    });

    expect(await outcome!).toBe("superseded");
    expect(reg.stop).toHaveBeenCalledTimes(1);
    expect(result.current.has("progress")).toBe(false);
  });

  it("stops a listener whose attempt lost the lease, without touching the replacement's", async () => {
    const stale = deferredSubscribe();
    const fresh = deferredSubscribe();
    const { result } = renderHook(() => useOwnedListeners());

    const staleToken = Symbol("stale");
    const freshToken = Symbol("fresh");
    let lease = staleToken;

    // The stale attempt registers, then a replacement takes the lease and installs first.
    let staleOutcome: Promise<string>;
    await act(async () => {
      staleOutcome = result.current.attach(
        "done",
        staleToken,
        stale.subscribe,
        () => lease === staleToken,
      );
      lease = freshToken;
      fresh.resolve();
      await result.current.attach("done", freshToken, fresh.subscribe, () => lease === freshToken);
    });
    expect(result.current.has("done")).toBe(true);

    // Only now does the stale registration land.
    await act(async () => {
      stale.resolve();
      await staleOutcome;
    });

    expect(await staleOutcome!).toBe("superseded");
    expect(stale.stop).toHaveBeenCalledTimes(1);
    expect(fresh.stop).not.toHaveBeenCalled();
    expect(result.current.has("done")).toBe(true);
  });

  it("tells an absent host API apart from a registration that failed", async () => {
    const missing = deferredSubscribe();
    const broken = deferredSubscribe();
    const { result } = renderHook(() => useOwnedListeners());

    let unsupported: string | undefined;
    let failed: string | undefined;
    await act(async () => {
      const a = result.current.attach("a", Symbol("a"), missing.subscribe);
      const b = result.current.attach("b", Symbol("b"), broken.subscribe);
      missing.resolveUnsupported();
      broken.reject();
      unsupported = await a;
      failed = await b;
    });

    // The distinction is load-bearing: "unsupported" means update the app, "failed" means
    // try again. Neither installs anything.
    expect(unsupported).toBe("unsupported");
    expect(failed).toBe("failed");
    expect(result.current.has("a")).toBe(false);
    expect(result.current.has("b")).toBe(false);
  });
});

describe("useOwnedListeners — owner-scoped release", () => {
  it("refuses to release a slot owned by a different attempt", async () => {
    const reg = deferredSubscribe();
    const { result } = renderHook(() => useOwnedListeners());
    const owner = Symbol("owner");

    await act(async () => {
      reg.resolve();
      await result.current.attach("progress", owner, reg.subscribe);
    });

    expect(result.current.release("progress", Symbol("someone-else"))).toBe(false);
    expect(reg.stop).not.toHaveBeenCalled();
    expect(result.current.has("progress")).toBe(true);

    expect(result.current.release("progress", owner)).toBe(true);
    expect(reg.stop).toHaveBeenCalledTimes(1);
    expect(result.current.has("progress")).toBe(false);
    // Releasing twice is a no-op, not a double stop.
    expect(result.current.release("progress", owner)).toBe(false);
    expect(reg.stop).toHaveBeenCalledTimes(1);
  });

  it("releases whatever is there when no owner is named, and on unmount", async () => {
    const progress = deferredSubscribe();
    const done = deferredSubscribe();
    const { result, unmount } = renderHook(() => useOwnedListeners());

    await act(async () => {
      progress.resolve();
      done.resolve();
      await result.current.attach("progress", Symbol("a"), progress.subscribe);
      await result.current.attach("done", Symbol("b"), done.subscribe);
    });

    // The run ending releases regardless of which attempt installed it.
    expect(result.current.release("progress")).toBe(true);
    expect(progress.stop).toHaveBeenCalledTimes(1);

    // The panel going away releases the rest, even though its attempt never tidied up.
    unmount();
    expect(done.stop).toHaveBeenCalledTimes(1);
  });
});

describe("useOwnedSubscription", () => {
  it("stops a registration that resolves after the effect was cleaned up", async () => {
    const reg = deferredSubscribe();
    const { unmount } = renderHook(() => useOwnedSubscription(reg.subscribe, []));

    unmount();
    await act(async () => {
      reg.resolve();
      await reg.subscribe();
    });

    expect(reg.stop).toHaveBeenCalledTimes(1);
  });

  it("stops the listener on cleanup when the registration resolved first", async () => {
    const reg = deferredSubscribe();
    const { unmount } = renderHook(() => useOwnedSubscription(reg.subscribe, []));

    await act(async () => {
      reg.resolve();
      await reg.subscribe();
    });
    expect(reg.stop).not.toHaveBeenCalled();

    unmount();
    expect(reg.stop).toHaveBeenCalledTimes(1);
  });

  it("swallows a rejected registration instead of raising out of cleanup", async () => {
    const reg = deferredSubscribe();
    const { unmount } = renderHook(() => useOwnedSubscription(reg.subscribe, []));

    await act(async () => {
      reg.reject();
      await reg.subscribe().catch(() => {});
    });

    expect(() => unmount()).not.toThrow();
  });
});

describe("forJob — stale events", () => {
  it("passes everything through until a job is adopted, then only that job's", () => {
    const seen: number[] = [];
    let job: number | null = null;
    const handler = forJob<{ job: number; done: number }>(
      () => job,
      (e) => seen.push(e.done),
    );

    // No job adopted yet: the first events of the run we are about to adopt must not be
    // thrown away.
    handler({ job: 7, done: 1 });
    job = 7;
    handler({ job: 7, done: 2 });
    // A superseded run's straggler. Without the filter this drives the bar backwards.
    handler({ job: 6, done: 99 });
    handler({ job: 7, done: 3 });

    expect(seen).toEqual([1, 2, 3]);
  });

  it("re-reads the job id per event, so one listener can follow a newer adoption", () => {
    const seen: number[] = [];
    let job: number | null = 7;
    const handler = forJob<{ job: number; done: number }>(
      () => job,
      (e) => seen.push(e.done),
    );

    handler({ job: 8, done: 1 }); // not ours yet
    job = 8; // reattach adopted the newer run that superseded the observed one
    handler({ job: 8, done: 2 });

    expect(seen).toEqual([2]);
  });
});

describe("terminalBuffer", () => {
  it("holds a terminal event that arrives before the job id is known, and replays it", () => {
    const finished: number[] = [];
    let job: number | null = null;
    const buffer = terminalBuffer<{ job: number; ok: boolean }>(() => job);
    const handler = buffer.handler((e) => finished.push(e.job));

    // The listener is live before the start command returns — this is the window.
    handler({ job: 4, ok: true });
    expect(finished).toEqual([]);

    // The command finally answers with the job id we started.
    job = 4;
    const ours = buffer.buffered(4);
    expect(ours).toEqual({ job: 4, ok: true });
    // ...and the panel replays it, which is what keeps it from freezing in "indexing".
    if (ours) finished.push(ours.job);
    expect(finished).toEqual([4]);
  });

  it("does not replay another run's buffered terminal event", () => {
    let job: number | null = null;
    const buffer = terminalBuffer<{ job: number; ok: boolean }>(() => job);
    const handler = buffer.handler(() => {
      throw new Error("a buffered event must not reach the handler");
    });

    handler({ job: 3, ok: true }); // a run that ended before ours started
    job = 4;
    expect(buffer.buffered(4)).toBeUndefined();
    expect(buffer.buffered(3)).toEqual({ job: 3, ok: true });
  });

  it("drops a superseded run's terminal event once a job is adopted", () => {
    const finished: number[] = [];
    let job: number | null = 5;
    const buffer = terminalBuffer<{ job: number; ok: boolean }>(() => job);
    const handler = buffer.handler((e) => finished.push(e.job));

    handler({ job: 4, ok: true }); // the superseded run finishing
    handler({ job: 5, ok: true });

    expect(finished).toEqual([5]);
  });
});
