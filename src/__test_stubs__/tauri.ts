/**
 * Shared stub for all @tauri-apps/* imports used in tests.
 * Aliased in vitest.config.ts so every Tauri API import resolves here.
 * Exports the minimal surface that host.ts / api.ts call at module load time
 * or via the host-lifecycle methods we test; everything else is a no-op.
 */

// @tauri-apps/api/core
export const invoke = () => Promise.resolve(null);
export const convertFileSrc = (p: string) => `asset:///${p}`;

// @tauri-apps/api/app
export const getVersion = () => Promise.resolve("0.1.0");

// @tauri-apps/api/event
//
// Observable so tests can drive the host's `onEvent` adapter: `__events` records each
// subscription and counts unlisten calls. `emit` feeds a handler the Tauri-shaped
// `{ payload }` envelope, which the adapter is supposed to unwrap.
export const __events = {
  calls: [] as { event: string; handler: (e: { payload: unknown }) => void }[],
  unlistenCount: 0,
  reset() {
    this.calls.length = 0;
    this.unlistenCount = 0;
  },
  /** Deliver a payload to every handler registered for `event`. */
  emit(event: string, payload: unknown) {
    for (const c of this.calls) if (c.event === event) c.handler({ payload });
  },
};

export const listen = (event: string, handler: (e: { payload: unknown }) => void) => {
  const entry = { event, handler };
  __events.calls.push(entry);
  // Model unsubscribe for real: drop this registration so a later emit() cannot reach
  // the handler. A counter alone would let a broken Unsubscribe pass its own test.
  return Promise.resolve(() => {
    const i = __events.calls.indexOf(entry);
    if (i !== -1) __events.calls.splice(i, 1);
    __events.unlistenCount++;
  });
};

// @tauri-apps/plugin-dialog
export const open = () => Promise.resolve(null);

// @tauri-apps/plugin-deep-link
export const onOpenUrl = () => Promise.resolve(() => {});

// @tauri-apps/plugin-opener
export const openUrl = () => Promise.resolve(undefined);
export const revealItemInDir = () => Promise.resolve(undefined);
