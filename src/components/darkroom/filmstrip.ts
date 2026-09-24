// The Darkroom filmstrip's pure parts: which photos to render around the current one,
// and where a step lands. The strip follows the Library's current order and filter.

/** How many photos either side of the current one the strip renders. */
export const STRIP_RADIUS = 40;

/** The slice of `ids` the strip renders: up to `radius` either side of `currentId`. */
export function windowAround(ids: number[], currentId: number, radius = STRIP_RADIUS): { start: number; ids: number[] } {
  const i = ids.indexOf(currentId);
  if (i === -1) return { start: 0, ids: ids.slice(0, radius * 2 + 1) };
  const start = Math.max(0, i - radius);
  return { start, ids: ids.slice(start, i + radius + 1) };
}

/** The photo a step of `delta` lands on, or null at either end (no wrap-around). */
export function stepTarget(ids: number[], currentId: number, delta: number): number | null {
  const i = ids.indexOf(currentId);
  if (i === -1) return null;
  return ids[i + delta] ?? null;
}

/** Whether a key event belongs to a control that uses arrow keys itself. */
export function arrowsBelongToTarget(t: EventTarget | null): boolean {
  const el = t as HTMLElement | null;
  if (!el || !el.tagName) return false;
  if (el.isContentEditable) return true;
  return el.tagName === "INPUT" || el.tagName === "TEXTAREA" || el.tagName === "SELECT";
}
