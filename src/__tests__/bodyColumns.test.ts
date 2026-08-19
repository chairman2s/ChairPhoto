/**
 * The `.body` grid's columns must be placed explicitly (issue #71).
 *
 * `.body` declares three tracks — left panel, grid, inspector — and App.tsx overrides their
 * widths inline, collapsing a hidden panel's track to `0px`. But it *removes* the hidden
 * panel's element rather than emptying it, so the track count stays three while the child
 * count drops. With no explicit placement, CSS grid auto-places what is left in DOM order:
 * hiding the left panel put `<main className="grid-wrap">` into track 1 — the `0px` one — and
 * the grid rendered nothing at all, at any scroll offset, on any selection.
 *
 * Measured in headless Chromium against the real `App.css` before the fix: with the left
 * panel hidden, `.grid-scroll`'s `clientWidth` was `0` (and its `clientHeight` a healthy 789,
 * which is what ruled out the height-collapse theory). Afterwards, 1276.
 *
 * What this test can and cannot do: jsdom implements no layout, so nothing here measures a
 * pixel — `clientWidth` is always 0 and a rendering test would pass either way. It asserts
 * the *source invariant* whose violation caused the bug, which is the half that can rot
 * silently. The pixel evidence above came from a real engine, by hand.
 *
 * Scanning source is blunt, but the failure it catches is exactly the blunt kind: a column
 * added to `.body`, or a `grid-column` dropped as "redundant" because the layout looks right
 * while every panel happens to be visible.
 */

import { describe, expect, it } from "vitest";

const APP_CSS = (
  import.meta.glob("../App.css", { query: "?raw", eager: true, import: "default" }) as Record<
    string,
    string
  >
)["../App.css"];

const APP_TSX = (
  import.meta.glob("../App.tsx", { query: "?raw", eager: true, import: "default" }) as Record<
    string,
    string
  >
)["../App.tsx"];

/**
 * Every direct column of `.body`, with the track it must occupy. Hard-coded on purpose: a
 * new column belongs in this list *and* needs its own `grid-column`, and having to add it
 * here is the reminder. Keep in sync with the children of `<div className="body">` in
 * App.tsx.
 */
const COLUMNS: { selector: string; gridColumn: string }[] = [
  { selector: ".leftcol", gridColumn: "1" },
  { selector: ".grid-wrap", gridColumn: "2" },
  { selector: ".rightcol", gridColumn: "3" },
  // Develop hides the inspector and spans both remaining tracks.
  { selector: ".develop-wrap", gridColumn: "2 / 4" },
];

/** The declaration block of a top-level rule, e.g. `.leftcol { … }`. */
function ruleBody(selector: string): string {
  // Anchored at a line start so `.grid-wrap` cannot match `.develop-wrap`'s block or a
  // descendant selector like `.leftcol .tag-panel`.
  const re = new RegExp(`^\\${selector}\\s*\\{([^}]*)\\}`, "m");
  const match = APP_CSS.match(re);
  if (!match) throw new Error(`App.css has no top-level rule for ${selector}`);
  return match[1];
}

describe(".body columns", () => {
  it.each(COLUMNS)("places $selector in track $gridColumn explicitly", ({ selector, gridColumn }) => {
    const declared = ruleBody(selector).match(/grid-column:\s*([^;]+);/);
    expect(declared, `${selector} must declare grid-column, or auto-placement decides`).not
      .toBeNull();
    expect(declared![1].trim()).toBe(gridColumn);
  });

  it("keeps .develop-wrap after .grid-wrap, which its override depends on", () => {
    // `<main>` carries both classes; equal specificity means source order decides which
    // grid-column wins. Reordering the file would silently put develop back in one track.
    expect(APP_CSS.indexOf("\n.develop-wrap {")).toBeGreaterThan(APP_CSS.indexOf("\n.grid-wrap {"));
  });

  it("still declares three tracks, which the placements above index into", () => {
    expect(ruleBody(".body")).toMatch(/grid-template-columns:\s*250px\s+1fr\s+300px;/);
    // The inline override that collapses a hidden panel's track — same three tracks.
    expect(APP_TSX).toMatch(/gridTemplateColumns:\s*`\$\{[^`]*\}px 1fr \$\{[\s\S]*?\}px`/);
  });

  it("still renders the side columns conditionally, which is what makes placement load-bearing", () => {
    // If these ever become unconditional, the bug's trigger is gone — but so is the reason
    // this test exists, and that should be a deliberate edit rather than a silent one.
    expect(APP_TSX).toMatch(/\{!leftHidden && \(/);
    expect(APP_TSX).toMatch(/\{!inDevelop && !rightHidden && \(/);
  });
});
