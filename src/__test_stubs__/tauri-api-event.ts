/**
 * Test stub for `@tauri-apps/api/event`. Aliased in `vitest.config.ts`.
 *
 * One file per `@tauri-apps/*` specifier (issue #64): `vi.mock()` keys its factory
 * registry on the resolved module id, so if two specifiers shared one file, mocking both
 * in the same test would silently collapse to a single registration. See the alias block
 * in `vitest.config.ts` for the full explanation.
 *
 * Observable so tests can drive the host's `onEvent` adapter: `__events` records each
 * subscription and counts unlisten calls. `emit` feeds a handler the Tauri-shaped
 * `{ payload }` envelope, which the adapter is supposed to unwrap.
 */

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
