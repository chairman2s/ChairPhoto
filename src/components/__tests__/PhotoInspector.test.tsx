// @vitest-environment jsdom
/**
 * The two load-bearing invariants of the tabbed PhotoInspector, each pinned at the level
 * where its violation would be silent:
 *
 * 1. React hook order. PhotoInspector renders one of four tabs, but every hook must stay
 *    unconditional at the top of the component — only the returned JSX branches by `tab`.
 *    A hook moved into a tab branch would change the hook count between renders and
 *    corrupt all inspector state, and nothing would fail loudly at review time. jsdom
 *    cannot cheaply mount the full component across tabs (it drags the whole Tauri
 *    surface with it, and no PhotoInspector mount harness exists), so — like
 *    bodyColumns.test.ts — this pins the *source* invariant: no hook call appears after
 *    the first `tab ===` branch.
 *
 * 2. Section's collapsed body is UNMOUNTED, not hidden. Several section bodies fetch on
 *    mount (IPTC, Metadata, the stack), so "collapsed" must mean "never rendered" — a
 *    CSS-hidden body would fire those fetches for every photo navigation. Pinned
 *    behaviorally against the exported Section itself: a probe child's render function
 *    must not run while collapsed, must run when expanded, and the open state must
 *    persist under `inspector.section.<id>`.
 */
import { describe, expect, it, vi, beforeEach } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { Section } from "../PhotoInspector";

// Section persists open/collapsed state to localStorage. Node ≥22's experimental global
// `localStorage` can win the race against jsdom's own Storage under vitest but throw on
// every call (no backing file configured) — same shim as commandInventory.test.tsx.
if (typeof (globalThis as { localStorage?: Storage }).localStorage?.clear !== "function") {
  const backing = new Map<string, string>();
  const memoryStorage: Storage = {
    getItem: (key) => (backing.has(key) ? (backing.get(key) as string) : null),
    setItem: (key, value) => void backing.set(key, String(value)),
    removeItem: (key) => void backing.delete(key),
    clear: () => backing.clear(),
    key: (index) => Array.from(backing.keys())[index] ?? null,
    get length() {
      return backing.size;
    },
  };
  Object.defineProperty(globalThis, "localStorage", {
    value: memoryStorage,
    configurable: true,
    writable: true,
  });
}

const PI_TSX = (
  import.meta.glob("../PhotoInspector.tsx", {
    query: "?raw",
    eager: true,
    import: "default",
  }) as Record<string, string>
)["../PhotoInspector.tsx"];

describe("hook order: every hook precedes the first tab branch", () => {
  it("has tab branches to speak of (the invariant's precondition)", () => {
    for (const t of ["details", "tags", "versions", "publish"]) {
      expect(PI_TSX, `missing branch for the ${t} tab`).toContain(`tab === "${t}" && (`);
    }
  });

  it("calls no hook after the first `tab ===`", () => {
    const i = PI_TSX.indexOf("tab ===");
    expect(i).toBeGreaterThan(-1);
    // Sanity: the hooks really are above the branch point, not absent altogether.
    expect(PI_TSX.slice(0, i)).toMatch(/\buseState\(/);
    expect(PI_TSX.slice(0, i)).toMatch(/\buseEffect\(/);
    const after = PI_TSX.slice(i);
    const stray = after.match(/\buse[A-Z]\w*\s*\(/);
    expect(
      stray,
      `hook call after the first tab branch: ${stray?.[0] ?? ""} — hooks must stay unconditional at the top`,
    ).toBeNull();
  });
});

describe("Section: a collapsed body is unmounted", () => {
  const head = (container: HTMLElement) => container.querySelector(".field-head")!;

  beforeEach(() => {
    localStorage.clear();
  });

  it("does not render the body while collapsed (the default)", () => {
    const renders = vi.fn();
    function Probe() {
      renders();
      return <div>probe-body</div>;
    }
    render(
      <Section id="t1" label="Test section">
        <Probe />
      </Section>,
    );
    // Unmounted, not hidden: the child's render function never ran, so a fetch-on-mount
    // body cannot have fetched.
    expect(renders).not.toHaveBeenCalled();
    expect(screen.queryByText("probe-body")).toBeNull();
  });

  it("mounts the body on expand and unmounts it again on collapse", () => {
    const renders = vi.fn();
    function Probe() {
      renders();
      return <div>probe-body</div>;
    }
    const { container } = render(
      <Section id="t1" label="Test section">
        <Probe />
      </Section>,
    );
    fireEvent.click(head(container));
    expect(renders).toHaveBeenCalled();
    expect(screen.getByText("probe-body")).toBeTruthy();

    renders.mockClear();
    fireEvent.click(head(container));
    expect(screen.queryByText("probe-body")).toBeNull();
    expect(renders).not.toHaveBeenCalled();
  });

  it("persists open state under inspector.section.<id> and honours it on remount", () => {
    const { container, unmount } = render(
      <Section id="t2" label="Test section">
        <div>probe-body</div>
      </Section>,
    );
    fireEvent.click(head(container));
    expect(localStorage.getItem("inspector.section.t2")).toBe("1");
    unmount();

    const second = render(
      <Section id="t2" label="Test section">
        <div>probe-body</div>
      </Section>,
    );
    expect(screen.getByText("probe-body")).toBeTruthy();
    // And the collapse writes back too.
    fireEvent.click(head(second.container));
    expect(localStorage.getItem("inspector.section.t2")).toBe("0");
    expect(screen.queryByText("probe-body")).toBeNull();
  });

  it("shows the summary only on the collapsed row", () => {
    const { container } = render(
      <Section id="t3" label="Test section" summary="42">
        <div>probe-body</div>
      </Section>,
    );
    expect(screen.getByText("42")).toBeTruthy();
    fireEvent.click(head(container));
    expect(screen.queryByText("42")).toBeNull();
  });
});
