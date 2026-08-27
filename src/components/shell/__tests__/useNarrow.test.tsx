// @vitest-environment jsdom
/**
 * useNarrow: matchMedia + useSyncExternalStore, guarded for environments where
 * `window.matchMedia` doesn't exist at all — SSR, and this repo's default jsdom test
 * setup (vitest.config.ts stubs no browser APIs beyond what jsdom itself provides, and
 * jsdom does not implement matchMedia). The guard is the whole point of the hook: it must
 * default to `false` rather than throw, and pick the real value back up the moment a
 * `matchMedia` shows up.
 */
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, renderHook } from "@testing-library/react";

import { useNarrow } from "../useNarrow";

type ChangeListener = (e: MediaQueryListEvent) => void;

/** A minimal MediaQueryList stub. Real MediaQueryLists have no way to synthesize a
 *  resize, so this records the one `change` listener the hook registers and exposes
 *  `set()` to flip `matches` and fire it by hand. */
function stubMatchMedia(initialMatches: boolean) {
  let matches = initialMatches;
  let listener: ChangeListener | null = null;
  const removeEventListener = vi.fn(() => {
    listener = null;
  });
  const addEventListener = vi.fn((_event: "change", cb: ChangeListener) => {
    listener = cb;
  });
  const matchMedia = vi.fn(
    () =>
      ({
        get matches() {
          return matches;
        },
        addEventListener,
        removeEventListener,
      }) as unknown as MediaQueryList,
  );
  return {
    matchMedia,
    removeEventListener,
    set: (next: boolean) => {
      matches = next;
      listener?.({ matches } as MediaQueryListEvent);
    },
  };
}

const originalMatchMedia = window.matchMedia;

afterEach(() => {
  window.matchMedia = originalMatchMedia;
});

describe("useNarrow", () => {
  it("returns false when matchMedia is unavailable", () => {
    // @ts-expect-error simulating the environment this hook is guarded for
    window.matchMedia = undefined;
    const { result } = renderHook(() => useNarrow());
    expect(result.current).toBe(false);
  });

  it("reflects the query's initial matches value, using the 1024px default", () => {
    const stub = stubMatchMedia(true);
    window.matchMedia = stub.matchMedia;
    const { result } = renderHook(() => useNarrow());
    expect(result.current).toBe(true);
    expect(stub.matchMedia).toHaveBeenCalledWith("(max-width: 1024px)");
  });

  it("honors a custom query string", () => {
    const stub = stubMatchMedia(false);
    window.matchMedia = stub.matchMedia;
    renderHook(() => useNarrow("(max-width: 600px)"));
    expect(stub.matchMedia).toHaveBeenCalledWith("(max-width: 600px)");
  });

  it("updates when the query's change event fires", () => {
    const stub = stubMatchMedia(false);
    window.matchMedia = stub.matchMedia;
    const { result } = renderHook(() => useNarrow());
    expect(result.current).toBe(false);

    act(() => stub.set(true));
    expect(result.current).toBe(true);

    act(() => stub.set(false));
    expect(result.current).toBe(false);
  });

  it("unsubscribes on unmount", () => {
    const stub = stubMatchMedia(false);
    window.matchMedia = stub.matchMedia;
    const { unmount } = renderHook(() => useNarrow());
    expect(stub.removeEventListener).not.toHaveBeenCalled();
    unmount();
    expect(stub.removeEventListener).toHaveBeenCalledWith("change", expect.any(Function));
  });
});
