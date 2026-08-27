// Tracks whether the window currently matches a media query, for the shell's ≤1024px
// responsive layout (App.tsx: the side panels move from persistent columns to transient
// overlays below that width). Pure hook — no App-specific knowledge — so it is reusable
// wherever a component needs to branch on viewport width.
import { useSyncExternalStore } from "react";

const DEFAULT_QUERY = "(max-width: 1024px)";

// `matchMedia` is unavailable during SSR and in this repo's default jsdom test
// environment (see vitest.config.ts) — window exists there, but `window.matchMedia` is
// `undefined`, not a stub. Every entry point below treats that identically to "no
// window at all": default to `false` (desktop-first) rather than throwing.
function hasMatchMedia(): boolean {
  return typeof window !== "undefined" && typeof window.matchMedia === "function";
}

function subscribe(query: string) {
  return (onStoreChange: () => void) => {
    if (!hasMatchMedia()) return () => {};
    const mql = window.matchMedia(query);
    mql.addEventListener("change", onStoreChange);
    return () => mql.removeEventListener("change", onStoreChange);
  };
}

function getSnapshot(query: string) {
  return () => (hasMatchMedia() ? window.matchMedia(query).matches : false);
}

const getServerSnapshot = () => false;

/** Whether `query` currently matches (default: the shell's narrow breakpoint,
 *  ≤1024px). Re-renders on every `change` event the media query fires, so a window
 *  resize or a monitor swap flips every consumer without polling. */
export function useNarrow(query: string = DEFAULT_QUERY): boolean {
  return useSyncExternalStore(subscribe(query), getSnapshot(query), getServerSnapshot);
}
