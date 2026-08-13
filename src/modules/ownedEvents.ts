/**
 * Owned event subscriptions — the frontend half of the job-ownership protocol.
 *
 * AGENTS.md: "Event listeners are resources: give each async registration an owner, stop late
 * registrations, and release only the listener owned by that attempt." Registration through
 * `ChairPhotoAPI.onEvent` (or `api.ts`'s `listen` wrappers) is asynchronous, so between the
 * call and its resolution the component can unmount, a newer attempt can take over, or the
 * job being followed can end. Each of those needs a different answer, and getting one wrong
 * is either a leaked listener nobody can stop or a live listener orphaned by its replacement.
 *
 * Smart Tagging had all of that care written out by hand — owned listener refs, a
 * single-flight lease token, buffered terminal events, job-id filtering. This module is that
 * care, generalized (issue #13), so a panel expresses the policy instead of re-deriving it.
 *
 * Nothing here imports app internals: only React and the module contract's `Unsubscribe`
 * type. A module may use it without reaching outside its boundary, and so may the host.
 *
 * The four pieces, matching the four failure modes:
 *
 * | Piece | Answers |
 * |---|---|
 * | {@link useOwnedSubscription} | "this listener lives exactly as long as the effect" |
 * | {@link useOwnedListeners} | "which attempt owns this listener, and is it still wanted?" |
 * | {@link forJob} | "is this progress event from the run we are following?" |
 * | {@link terminalBuffer} | "the terminal event arrived before we knew our job id" |
 */
import { useCallback, useEffect, useMemo, useRef } from "react";
import type { Unsubscribe } from "./registry";

/**
 * A registration attempt: resolves to the stopper, or to `null` when the host cannot deliver
 * this event at all (`ChairPhotoAPI.onEvent` is an optional member, so a module's wrapper
 * resolves `null` on an older host). A rejection means the host supports events but *this*
 * registration failed — a different outcome, and callers routinely need to tell them apart.
 */
export type Subscribe = () => Promise<Unsubscribe | null>;

/**
 * What {@link OwnedListeners.attach} did.
 *
 * - `installed` — live and stored under the slot; release it with
 *   {@link OwnedListeners.release}.
 * - `unsupported` — the host has no `onEvent`. Nothing was installed. For a cosmetic stream
 *   this only means no progress bar; for a required terminal signal the caller must fail
 *   closed (AGENTS.md: "a required terminal signal must fail closed with an explicit state").
 * - `failed` — registration rejected. Retrying may work.
 * - `superseded` — it resolved after the attempt stopped owning the component (unmount, a
 *   newer attempt, or the job it was for having ended). The listener was **stopped**, not
 *   stored: storing it would either leak it past unmount or orphan the replacement's.
 */
export type AttachOutcome = "installed" | "unsupported" | "failed" | "superseded";

/** Named owned listener slots for one component. See {@link useOwnedListeners}. */
export interface OwnedListeners {
  /** Whether the component is still mounted. */
  isMounted(): boolean;
  /** Whether `slot` currently holds a live listener. */
  has(slot: string): boolean;
  /**
   * Register, then either install under `slot` or stop the result.
   *
   * `stillWanted` runs **after** the await and decides whether this attempt may still install.
   * Pass the attempt's full predicate — its lease, the job id it was for, whatever else —
   * not just "am I mounted"; mountedness is checked here anyway. Omitting it means "mounted
   * is enough".
   *
   * Installing replaces whatever `slot` held **without** stopping it, matching the callers
   * that install at most one listener per slot per run. Release first if that is not true of
   * yours.
   */
  attach(
    slot: string,
    owner: symbol,
    subscribe: Subscribe,
    stillWanted?: () => boolean,
  ): Promise<AttachOutcome>;
  /**
   * Stop and forget `slot`. With an `owner`, only if that attempt installed it — a stale
   * attempt tidying up must not free a replacement's subscription. Without one it releases
   * whatever is there, which is what the *run* ending (or the panel unmounting) means.
   *
   * Returns whether it actually released, so a caller can keep render state (a "progress is
   * live" flag) in step without a second ownership check.
   */
  release(slot: string, owner?: symbol): boolean;
  /** Stop and forget every slot. The panel going away, not one attempt tidying up. */
  releaseAll(): void;
}

/**
 * Own a component's event listeners by name, each tagged with the attempt that installed it.
 *
 * Everything is refs, not state: a second same-tick click, or a registration resolving mid
 * teardown, must be excluded *synchronously*. State has not updated by then.
 *
 * Unmount releases every slot, so a listener installed by an attempt that never got to clean
 * up after itself still goes away.
 */
export function useOwnedListeners(): OwnedListeners {
  const held = useRef(new Map<string, { owner: symbol; stop: Unsubscribe }>());
  const mounted = useRef(true);

  const isMounted = useCallback(() => mounted.current, []);

  const has = useCallback((slot: string) => held.current.has(slot), []);

  const release = useCallback((slot: string, owner?: symbol) => {
    const cur = held.current.get(slot);
    if (!cur) return false;
    if (owner !== undefined && cur.owner !== owner) return false;
    held.current.delete(slot);
    cur.stop();
    return true;
  }, []);

  const releaseAll = useCallback(() => {
    const all = [...held.current.values()];
    held.current.clear();
    for (const cur of all) cur.stop();
  }, []);

  const attach = useCallback(
    async (
      slot: string,
      owner: symbol,
      subscribe: Subscribe,
      stillWanted?: () => boolean,
    ): Promise<AttachOutcome> => {
      let stop: Unsubscribe | null | undefined;
      try {
        stop = await subscribe();
      } catch {
        stop = undefined; // distinct from `null`: the host has events, this attempt failed
      }
      // Ownership is checked before the outcome is reported, and the listener is stopped
      // rather than stored — a registration that resolves after teardown belongs to nobody.
      if (!mounted.current || (stillWanted && !stillWanted())) {
        stop?.();
        return "superseded";
      }
      if (stop === null) return "unsupported";
      if (stop === undefined) return "failed";
      held.current.set(slot, { owner, stop });
      return "installed";
    },
    [],
  );

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      releaseAll();
    };
  }, [releaseAll]);

  return useMemo(
    () => ({ isMounted, has, attach, release, releaseAll }),
    [isMounted, has, attach, release, releaseAll],
  );
}

/**
 * Subscribe for exactly the lifetime of an effect.
 *
 * The whole-component case: no attempts, no job ids, one listener that lives while the effect
 * does. It still owns its async registration — one that resolves after cleanup is stopped
 * rather than left running — and it swallows a rejected registration instead of raising an
 * unhandled rejection out of a cleanup function.
 *
 * `deps` is passed straight to `useEffect`, so the usual rule applies: whatever `subscribe`
 * closes over and can change belongs in it.
 */
export function useOwnedSubscription(
  subscribe: Subscribe,
  deps: React.DependencyList,
): void {
  useEffect(() => {
    let stale = false;
    let stop: Unsubscribe | null = null;
    subscribe().then(
      (s) => {
        if (stale) s?.();
        else stop = s;
      },
      () => {},
    );
    return () => {
      stale = true;
      stop?.();
      stop = null;
    };
    // The caller owns the dependency list; `subscribe` is re-created per render by design.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);
}

/**
 * A backend event that says which run it belongs to. Every job family stamps this (the Rust
 * payloads declare `job: u64`, never an `Option`), because a superseded run's stragglers keep
 * arriving after a newer one has started.
 */
export interface JobScopedEvent {
  job: number;
}

/**
 * Drop events from a run we are not following.
 *
 * Events pass through while `currentJob()` is `null` — no job has been adopted yet, so there
 * is nothing to compare against and the alternative would be discarding the first events of
 * the run we are about to adopt. Once a job id exists, only that run's events get through.
 *
 * `currentJob` is read per event rather than captured, so one live listener keeps following
 * whichever job the panel ends up adopting.
 */
export function forJob<T extends JobScopedEvent>(
  currentJob: () => number | null,
  handle: (event: T) => void,
): (event: T) => void {
  return (event: T) => {
    const job = currentJob();
    if (job != null && event.job !== job) return;
    handle(event);
  };
}

/**
 * Holds terminal events that arrive before a job id is adopted, so none is lost in the window
 * between "listener is live" and "we know which run we are following".
 *
 * The window is real and unavoidable: the terminal listener has to be installed *before* the
 * command that starts the run, or a fast run finishes unobserved and the panel sits in
 * "indexing" forever. Handling such an event immediately is no better — it would run the
 * finish path against state the attempt has not populated yet, clearing nothing, and the
 * continuation would then enter "indexing" for a run that had already ended.
 *
 * Buffering starts at registration, so this prevents a frozen panel, not a lost result: an
 * event emitted before the listener existed is gone either way.
 */
export interface TerminalBuffer<T extends JobScopedEvent> {
  /**
   * Wrap the terminal handler. Before a job is adopted the event is buffered and `handle` is
   * not called; after, only the adopted run's event reaches it.
   */
  handler(handle: (event: T) => void): (event: T) => void;
  /**
   * The buffered terminal event for `job`, if it arrived before adoption. Call this straight
   * after adopting, and replay what it returns.
   */
  buffered(job: number): T | undefined;
}

/**
 * A {@link TerminalBuffer} over `currentJob` — the same "which run are we following" source
 * {@link forJob} reads, so the progress and terminal streams can never disagree about it.
 */
export function terminalBuffer<T extends JobScopedEvent>(
  currentJob: () => number | null,
): TerminalBuffer<T> {
  const pending: T[] = [];
  return {
    handler: (handle) => (event: T) => {
      const job = currentJob();
      if (job === null) {
        pending.push(event);
        return;
      }
      if (event.job !== job) return;
      handle(event);
    },
    buffered: (job) => pending.find((e) => e.job === job),
  };
}
