/**
 * `computeCols` is the ResizeObserver-driven column count CatalogGrid uses to lay out its
 * virtualized rows (see CatalogGrid.tsx's `setScrollEl`) — the same
 * `repeat(auto-fill, minmax(tileMin, 1fr))` arithmetic CSS grid would use, pulled out as a
 * pure function so its boundary behaviour (and the 1-column floor a narrow/unmeasured
 * container hits) can be pinned directly, without mounting the virtualizer or a DOM.
 */
import { describe, expect, it } from "vitest";
import { computeCols } from "../CatalogGrid";

describe("computeCols", () => {
  it("floors at 1 column when nothing has been measured yet (width <= 0)", () => {
    expect(computeCols(0, 160, 3)).toBe(1);
    expect(computeCols(-40, 160, 3)).toBe(1);
  });

  it("floors at 1 column when the container is narrower than one tile", () => {
    expect(computeCols(100, 160, 3)).toBe(1);
  });

  it("fits exactly N columns at the boundary width, and N-1 one px short", () => {
    // 2 columns need width >= 2*tileMin + 1*gap.
    const twoColBoundary = 2 * 160 + 3;
    expect(computeCols(twoColBoundary, 160, 3)).toBe(2);
    expect(computeCols(twoColBoundary - 1, 160, 3)).toBe(1);
  });

  it("matches CatalogGrid's current GAP=3 tile density", () => {
    expect(computeCols(326, 160, 3)).toBe(2);
    expect(computeCols(322, 160, 3)).toBe(1);
  });

  it("scales down as tileMin grows (thumb-size slider dragged toward its max)", () => {
    expect(computeCols(1000, 160, 3)).toBe(6);
    expect(computeCols(1000, 320, 3)).toBe(3);
  });

  it("scales up as tileMin shrinks (thumb-size slider dragged toward its min)", () => {
    expect(computeCols(1000, 120, 3)).toBe(8);
  });
});
