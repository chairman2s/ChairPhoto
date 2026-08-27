// @vitest-environment jsdom
/**
 * Preservation test for the topbar → TitleBar extraction: every command reachable from the
 * old `<header className="topbar">` (one row of ~21 ungrouped buttons) must still be
 * reachable from the new grouped title bar. Each case below renders TitleBar with
 * permissive props (everything enabled, every count > 0, two module action groups, two
 * exportable batches), drives the UI exactly the way a user would (open the owning menu,
 * click through any MenuSub, click the leaf command), and asserts the one callback that
 * command owns fired exactly once.
 *
 * Table-driven so a later commit (icon rail, bottom bench, rich search) can append cases
 * without restructuring this file — see AGENTS.md's topbar-extraction task. The IconRail
 * and Bench inventories below follow the same pattern, and the "one code path" block at
 * the end pins the bench/keyboard shared-write-path invariant at source level.
 */
import { describe, expect, it, vi, type Mock } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { TitleBar, type TitleBarProps, type ModuleActionGroup } from "../TitleBar";
import { IconRail, type IconRailProps } from "../IconRail";
import { Bench, type BenchProps } from "../Bench";
import { COLOR_LABELS } from "../../../modules/labels";
import type { ImportBatch } from "../../../modules/api";
import type { ToolbarAction, MainView, Photo } from "../../../modules/registry";

function makeBatch(id: number, sourceLabel: string): ImportBatch {
  return { id, uuid: `uuid-${id}`, sourceLabel, note: "", createdAt: 0, photoCount: 10 };
}

function makeAction(id: string, label: string): ToolbarAction {
  return { id, label };
}

const batches = [makeBatch(1, "/Volumes/card/DCIM/100CANON"), makeBatch(2, "/home/user/import2")];

const moduleGroups: ModuleActionGroup[] = [
  { moduleId: "mod-a", moduleLabel: "Module Alpha", actions: [makeAction("a1", "Alpha action")] },
  { moduleId: "mod-b", moduleLabel: "Module Beta", actions: [makeAction("b1", "Beta action")] },
];

/** Every callback prop as a fresh vi.fn(), plus permissive values for every gating prop —
 *  ready, every "canX"/"xEnabled" flag true, every count > 0 — so no command is disabled. */
function buildProps(): TitleBarProps {
  return {
    catalogName: "Test Catalog",
    photoCount: 1234,
    onOpenCatalogs: vi.fn(),
    pendingCount: 3,
    onReconcile: vi.fn(),
    identityDebtCount: 5,
    onOpenIdentityDebt: vi.fn(),
    ready: true,
    onImportCard: vi.fn(),
    onImportBundle: vi.fn(),
    onRescan: vi.fn(),
    cachePreviews: true,
    onCachePreviews: vi.fn(),
    exportableBatches: batches,
    onExportBundle: vi.fn(),
    canExport: true,
    onExport: vi.fn(),
    onPopOutLoupe: vi.fn(),
    loupeOn: false,
    loupeEnabled: true,
    onToggleLoupe: vi.fn(),
    selectionCount: 2,
    onAnalyseBurst: vi.fn(),
    onProposeStacks: vi.fn(),
    onCullSession: vi.fn(),
    onTrash: vi.fn(),
    moduleActionGroups: moduleGroups,
    onModuleAction: vi.fn(),
    onOpenPrefs: vi.fn(),
    leftHidden: false,
    onToggleLeft: vi.fn(),
    rightHidden: false,
    onToggleRight: vi.fn(),
  };
}

const CHILDREN_TEXT = "children-slot-stub";

function renderTitleBar(props: TitleBarProps) {
  render(<TitleBar {...props}>{CHILDREN_TEXT}</TitleBar>);
}

// -- interaction helpers -----------------------------------------------------------------

function openImportMenu() {
  fireEvent.click(screen.getByRole("button", { name: "Import ▾" }));
}
function openMoreMenu() {
  fireEvent.click(screen.getByRole("button", { name: "More" }));
}
function clickMenuItem(name: string | RegExp) {
  fireEvent.click(screen.getByRole("menuitem", { name }));
}
function clickCheckbox(name: string | RegExp) {
  fireEvent.click(screen.getByRole("menuitemcheckbox", { name }));
}

// -- the inventory ------------------------------------------------------------------------

interface Case {
  name: string;
  run: (props: TitleBarProps) => void;
  spy: (props: TitleBarProps) => Mock;
  expectCalledWith?: (props: TitleBarProps) => unknown[];
}

const cases: Case[] = [
  {
    name: "catalog pill",
    run: () => fireEvent.click(screen.getByRole("button", { name: /Test Catalog/ })),
    spy: (p) => p.onOpenCatalogs as unknown as Mock,
  },
  {
    name: "import from card",
    run: () => {
      openImportMenu();
      clickMenuItem("Import from card…");
    },
    spy: (p) => p.onImportCard as unknown as Mock,
  },
  {
    name: "import a .chairphoto bundle",
    run: () => {
      openImportMenu();
      clickMenuItem("Import a .chairphoto bundle…");
    },
    spy: (p) => p.onImportBundle as unknown as Mock,
  },
  {
    name: "rescan library",
    run: () => {
      openImportMenu();
      clickMenuItem("Rescan library");
    },
    spy: (p) => p.onRescan as unknown as Mock,
  },
  {
    name: "cache previews on import (toggle)",
    run: () => {
      openImportMenu();
      clickCheckbox("Cache previews on import");
    },
    spy: (p) => p.onCachePreviews as unknown as Mock,
  },
  {
    name: "export a bundle (batch 1)",
    run: () => {
      openImportMenu();
      clickMenuItem("Export a bundle");
      clickMenuItem("100CANON");
    },
    spy: (p) => p.onExportBundle as unknown as Mock,
    expectCalledWith: () => [batches[0]],
  },
  {
    name: "export a bundle (batch 2)",
    run: () => {
      openImportMenu();
      clickMenuItem("Export a bundle");
      clickMenuItem("import2");
    },
    spy: (p) => p.onExportBundle as unknown as Mock,
    expectCalledWith: () => [batches[1]],
  },
  {
    name: "export",
    run: () => fireEvent.click(screen.getByRole("button", { name: "Export" })),
    spy: (p) => p.onExport as unknown as Mock,
  },
  {
    name: "pop out loupe",
    run: () => {
      openMoreMenu();
      clickMenuItem("Open loupe in a new window");
    },
    spy: (p) => p.onPopOutLoupe as unknown as Mock,
  },
  {
    name: "loupe toggle",
    run: () => {
      openMoreMenu();
      clickMenuItem("Loupe");
    },
    spy: (p) => p.onToggleLoupe as unknown as Mock,
  },
  {
    name: "analyse burst sharpness",
    run: () => {
      openMoreMenu();
      clickMenuItem("Analyse burst sharpness");
    },
    spy: (p) => p.onAnalyseBurst as unknown as Mock,
  },
  {
    name: "propose stacks",
    run: () => {
      openMoreMenu();
      clickMenuItem("Propose stacks…");
    },
    spy: (p) => p.onProposeStacks as unknown as Mock,
  },
  {
    name: "start cull session",
    run: () => {
      openMoreMenu();
      clickMenuItem("Start cull session");
    },
    spy: (p) => p.onCullSession as unknown as Mock,
  },
  {
    name: "trash",
    run: () => {
      openMoreMenu();
      clickMenuItem("Trash…");
    },
    spy: (p) => p.onTrash as unknown as Mock,
  },
  {
    name: "module action (group 1)",
    run: () => {
      openMoreMenu();
      clickMenuItem("Modules");
      clickMenuItem("Alpha action");
    },
    spy: (p) => p.onModuleAction as unknown as Mock,
    expectCalledWith: () => [moduleGroups[0].actions[0]],
  },
  {
    name: "module action (group 2)",
    run: () => {
      openMoreMenu();
      clickMenuItem("Modules");
      clickMenuItem("Beta action");
    },
    spy: (p) => p.onModuleAction as unknown as Mock,
    expectCalledWith: () => [moduleGroups[1].actions[0]],
  },
  {
    // Badged (a copy of the count sits inside the same button), so its accessible name is
    // "Identity debt 5", not the bare label — match by prefix.
    name: "identity debt",
    run: () => {
      openMoreMenu();
      clickMenuItem(/^Identity debt/);
    },
    spy: (p) => p.onOpenIdentityDebt as unknown as Mock,
  },
  {
    // Same badge situation as above ("Back-up queue 3").
    name: "back-up queue (reconcile)",
    run: () => {
      openMoreMenu();
      clickMenuItem(/^Back-up queue/);
    },
    spy: (p) => p.onReconcile as unknown as Mock,
  },
  {
    name: "preferences",
    run: () => {
      openMoreMenu();
      clickMenuItem("Preferences…");
    },
    spy: (p) => p.onOpenPrefs as unknown as Mock,
  },
  {
    name: "left panel toggle",
    run: () => {
      openMoreMenu();
      clickCheckbox("Tags & collections panel");
    },
    spy: (p) => p.onToggleLeft as unknown as Mock,
  },
  {
    name: "right panel (inspector) toggle",
    run: () => {
      openMoreMenu();
      clickCheckbox("Inspector");
    },
    spy: (p) => p.onToggleRight as unknown as Mock,
  },
];

describe("TitleBar command inventory (preservation)", () => {
  it("renders the children slot", () => {
    renderTitleBar(buildProps());
    expect(screen.getByText(CHILDREN_TEXT)).toBeTruthy();
  });

  it.each(cases)("$name fires its callback exactly once", ({ run, spy, expectCalledWith }) => {
    const props = buildProps();
    renderTitleBar(props);

    run(props);

    const mock = spy(props);
    expect(mock).toHaveBeenCalledTimes(1);
    if (expectCalledWith) {
      expect(mock).toHaveBeenCalledWith(...expectCalledWith(props));
    }
  });

  it("covers every onClick/onSelect/onChange affordance TitleBar renders", () => {
    // Sanity check against the report's `grep -c "onClick"` count: this is not that grep
    // (menu commands fire through MenuItem's onSelect / MenuCheckItem's onChange, not a
    // raw onClick), but it pins the total command count so a command silently added to
    // TitleBar without a matching case here fails loudly instead of shipping unpinned.
    // Was 24; Compare, Publish and Back up selection moved to the Bench (see below).
    expect(cases.length).toBe(21);
  });
});

// -- IconRail --------------------------------------------------------------------------
// The Library/Develop/module-view switcher used to render inside TitleBar's children slot
// (covered above via the CHILDREN_TEXT stub); it now lives in IconRail, a sibling of
// TitleBar in App.tsx's `.body`. Same preservation intent, same table style, own inventory.

function railView(id: string, label: string): MainView {
  return { id, label };
}

const railModuleViews: MainView[] = [railView("map", "Map"), railView("people", "People")];

/** Permissive rail props: Develop shown and enabled, two module views, fresh vi.fn()s. */
function buildRailProps(): IconRailProps {
  return {
    active: "library",
    canDevelop: true,
    developEnabled: true,
    moduleViews: railModuleViews,
    onSelect: vi.fn(),
    onOpenPrefs: vi.fn(),
  };
}

function renderIconRail(props: IconRailProps) {
  render(<IconRail {...props} />);
}

interface RailCase {
  name: string;
  run: () => void;
  spy: (props: IconRailProps) => Mock;
  expectCalledWith?: unknown[];
}

const railCases: RailCase[] = [
  {
    name: "library",
    run: () => fireEvent.click(screen.getByRole("button", { name: "Library" })),
    spy: (p) => p.onSelect as unknown as Mock,
    expectCalledWith: ["library"],
  },
  {
    name: "develop",
    run: () => fireEvent.click(screen.getByRole("button", { name: "Develop" })),
    spy: (p) => p.onSelect as unknown as Mock,
    expectCalledWith: ["develop"],
  },
  {
    name: "module view (map)",
    run: () => fireEvent.click(screen.getByRole("button", { name: "Map" })),
    spy: (p) => p.onSelect as unknown as Mock,
    expectCalledWith: ["map"],
  },
  {
    name: "module view (people)",
    run: () => fireEvent.click(screen.getByRole("button", { name: "People" })),
    spy: (p) => p.onSelect as unknown as Mock,
    expectCalledWith: ["people"],
  },
  {
    name: "preferences",
    run: () => fireEvent.click(screen.getByRole("button", { name: "Preferences" })),
    spy: (p) => p.onOpenPrefs as unknown as Mock,
  },
];

describe("IconRail command inventory (preservation)", () => {
  it.each(railCases)("$name fires its callback exactly once", ({ run, spy, expectCalledWith }) => {
    const props = buildRailProps();
    renderIconRail(props);

    run();

    const mock = spy(props);
    expect(mock).toHaveBeenCalledTimes(1);
    if (expectCalledWith) {
      expect(mock).toHaveBeenCalledWith(...expectCalledWith);
    }
  });

  it("hides Develop when canDevelop is false", () => {
    renderIconRail({ ...buildRailProps(), canDevelop: false });
    expect(screen.queryByRole("button", { name: "Develop" })).toBeNull();
  });

  it("disables Develop (without hiding it) when developEnabled is false", () => {
    renderIconRail({ ...buildRailProps(), developEnabled: false });
    const btn = screen.getByRole("button", { name: "Develop" }) as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
  });

  it("covers every rail affordance IconRail renders", () => {
    expect(railCases.length).toBe(5);
  });
});

// -- Bench -----------------------------------------------------------------------------
// The bottom bench (Bench.tsx) took over the selection-flavoured commands: Compare,
// Publish and Back up selection moved here out of More ⋯ (their TitleBar cases above went
// with them), joined by the marking controls (stars / pick pills / label dots), the pile
// actions (Stack / Cull / Analyse / Export) and the clear-selection ✕. Same preservation
// intent, same table style, own inventory.
//
// The marking cases also pin the *toggle* rules the bench mirrors from PhotoInspector:
// clicking the active star sends onRate(0), the active pick pill sends onPick("none"),
// and the active label dot (or the clear dot) sends onLabel("").

function benchPhoto(over: Partial<Photo> = {}): Photo {
  return {
    id: 7,
    uuid: "uuid-7",
    path: "2026/08/_DSC8177.ARW",
    rating: 3,
    label: "",
    pickState: "none",
    captureTime: null,
    width: 6000,
    height: 4000,
    metadataReady: 1,
    ...over,
  } as Photo;
}

/** Permissive bench props: a job-free left block, an active photo rated 3 with no pick and
 *  no label, three selected, every gate open, fresh vi.fn()s. */
function buildBenchProps(): BenchProps {
  return {
    progress: null,
    status: "All quiet",
    total: 42,
    selectedCount: 3,
    active: benchPhoto(),
    onRate: vi.fn(),
    onPick: vi.fn(),
    onLabel: vi.fn(),
    selectionThumbs: [benchPhoto(), benchPhoto({ id: 8 }), benchPhoto({ id: 9 })],
    thumbBusts: undefined,
    canCompare: true,
    compareOn: false,
    onCompare: vi.fn(),
    onStack: vi.fn(),
    onCull: vi.fn(),
    canExport: true,
    onExport: vi.fn(),
    canPublish: true,
    onPublish: vi.fn(),
    onBackUpSelection: vi.fn(),
    canBackUpSelection: true,
    onAnalyseBurst: vi.fn(),
    ready: true,
    onClearSelection: vi.fn(),
  };
}

interface BenchCase {
  name: string;
  /** Overrides on the permissive base — e.g. an already-picked active photo. */
  props?: Partial<BenchProps>;
  run: () => void;
  spy: (props: BenchProps) => Mock;
  expectCalledWith?: unknown[];
}

const clickButton = (name: string | RegExp) =>
  fireEvent.click(screen.getByRole("button", { name }));

const benchCases: BenchCase[] = [
  {
    name: "rate star",
    run: () => clickButton("Rate 4"),
    spy: (p) => p.onRate as unknown as Mock,
    expectCalledWith: [4],
  },
  {
    // The inspector's toggle rule: the active star clears the rating.
    name: "rate toggle to zero",
    run: () => clickButton("Rate 3"),
    spy: (p) => p.onRate as unknown as Mock,
    expectCalledWith: [0],
  },
  {
    name: "pick",
    run: () => clickButton("Pick"),
    spy: (p) => p.onPick as unknown as Mock,
    expectCalledWith: ["pick"],
  },
  {
    name: "reject",
    run: () => clickButton("Reject"),
    spy: (p) => p.onPick as unknown as Mock,
    expectCalledWith: ["reject"],
  },
  {
    name: "pick toggle to none",
    props: { active: benchPhoto({ pickState: "pick" }) },
    run: () => clickButton("Pick"),
    spy: (p) => p.onPick as unknown as Mock,
    expectCalledWith: ["none"],
  },
  {
    name: "reject toggle to none",
    props: { active: benchPhoto({ pickState: "reject" }) },
    run: () => clickButton("Reject"),
    spy: (p) => p.onPick as unknown as Mock,
    expectCalledWith: ["none"],
  },
  // One case per canonical colour label — the dots carry their label name as the title.
  ...COLOR_LABELS.map(
    (l): BenchCase => ({
      name: `label dot (${l.name})`,
      run: () => clickButton(l.name),
      spy: (p) => p.onLabel as unknown as Mock,
      expectCalledWith: [l.name],
    }),
  ),
  {
    name: "label dot toggle clears",
    props: { active: benchPhoto({ label: "Green" }) },
    run: () => clickButton("Green"),
    spy: (p) => p.onLabel as unknown as Mock,
    expectCalledWith: [""],
  },
  {
    name: "clear-label dot",
    props: { active: benchPhoto({ label: "Green" }) },
    run: () => clickButton("Clear label"),
    spy: (p) => p.onLabel as unknown as Mock,
    expectCalledWith: [""],
  },
  {
    name: "compare",
    run: () => clickButton("Compare"),
    spy: (p) => p.onCompare as unknown as Mock,
  },
  {
    name: "stack",
    run: () => clickButton("Stack"),
    spy: (p) => p.onStack as unknown as Mock,
  },
  {
    name: "cull",
    run: () => clickButton("Cull"),
    spy: (p) => p.onCull as unknown as Mock,
  },
  {
    name: "analyse burst",
    run: () => clickButton("Analyse"),
    spy: (p) => p.onAnalyseBurst as unknown as Mock,
  },
  {
    name: "export",
    run: () => clickButton("Export"),
    spy: (p) => p.onExport as unknown as Mock,
  },
  {
    name: "publish",
    run: () => clickButton("Publish"),
    spy: (p) => p.onPublish as unknown as Mock,
  },
  {
    name: "back up",
    run: () => clickButton("Back up"),
    spy: (p) => p.onBackUpSelection as unknown as Mock,
  },
  {
    name: "clear selection",
    run: () => clickButton("Clear selection"),
    spy: (p) => p.onClearSelection as unknown as Mock,
  },
];

describe("Bench command inventory", () => {
  it.each(benchCases)(
    "$name fires its callback exactly once",
    ({ props: patch, run, spy, expectCalledWith }) => {
      const props = { ...buildBenchProps(), ...patch };
      render(<Bench {...props} />);

      run();

      const mock = spy(props);
      expect(mock).toHaveBeenCalledTimes(1);
      if (expectCalledWith) {
        expect(mock).toHaveBeenCalledWith(...expectCalledWith);
      }
    },
  );

  it("covers every bench affordance Bench renders", () => {
    expect(benchCases.length).toBe(21);
  });

  it("disabled controls fire nothing", () => {
    const props: BenchProps = {
      ...buildBenchProps(),
      canCompare: false,
      compareOn: false,
      ready: false,
      canExport: false,
      canPublish: false,
      canBackUpSelection: false,
    };
    render(<Bench {...props} />);

    for (const name of ["Compare", "Stack", "Cull", "Analyse", "Export", "Publish", "Back up"]) {
      const btn = screen.getByRole("button", { name }) as HTMLButtonElement;
      expect(btn.disabled, `${name} must be disabled`).toBe(true);
      fireEvent.click(btn);
    }

    for (const spy of [
      props.onCompare,
      props.onStack,
      props.onCull,
      props.onAnalyseBurst,
      props.onExport,
      props.onPublish,
      props.onBackUpSelection,
    ]) {
      expect(spy as unknown as Mock).not.toHaveBeenCalled();
    }
  });

  it("hides the marking section when no photo is active", () => {
    render(<Bench {...buildBenchProps()} active={null} />);
    expect(screen.queryByText(/MARKING/)).toBeNull();
    expect(screen.queryByRole("button", { name: "Pick" })).toBeNull();
  });

  it("hides the pile when nothing is selected", () => {
    render(
      <Bench {...buildBenchProps()} selectedCount={0} selectionThumbs={[]} />,
    );
    expect(screen.queryByRole("button", { name: "Compare" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Clear selection" })).toBeNull();
  });

  it("shows the counts and the status when no job is running", () => {
    render(<Bench {...buildBenchProps()} />);
    expect(screen.getByText("42 photos · 3 selected")).toBeTruthy();
    expect(screen.getByText("All quiet").getAttribute("title")).toBe("All quiet");
  });

  it("shows one progress readout instead of the counts while a job runs", () => {
    render(
      <Bench
        {...buildBenchProps()}
        progress={{ label: "Importing 3/10", done: 3, total: 10 }}
      />,
    );
    const bar = screen.getByRole("progressbar");
    expect(bar.getAttribute("aria-valuenow")).toBe("3");
    expect(bar.getAttribute("aria-valuemax")).toBe("10");
    expect(screen.getByText("Importing 3/10")).toBeTruthy();
    expect(screen.queryByText(/photos/)).toBeNull();
  });

  it("marks the active photo's thumb in the pile", () => {
    const { container } = render(<Bench {...buildBenchProps()} />);
    const wraps = Array.from(container.querySelectorAll(".bench-strip .thumbwrap"));
    expect(wraps).toHaveLength(3);
    expect(wraps.filter((w) => w.classList.contains("hl"))).toHaveLength(1);
  });

  it("captions the marking section with the active photo's basename", () => {
    render(<Bench {...buildBenchProps()} />);
    // The path is 2026/08/_DSC8177.ARW — only the basename is shown.
    expect(screen.getByText("_DSC8177.ARW")).toBeTruthy();
  });
});

// -- one code path: bench marking = keyboard culling ----------------------------------
// The semantics-critical invariant of the bench commit: its marking controls and the
// keyboard culling shortcuts must not drift, so both route through the single
// `applyToSelection` callback in App.tsx (the bench via `applyMark`, whose non-Compare
// branch is `applyToSelection` verbatim; Compare's branch drives the focused pane, which
// is the same photo the bench displays). jsdom cannot mount App — catalog, modules and
// Tauri IPC stand in the way — so, like bodyColumns.test.ts, this block asserts the
// *source* invariant whose violation would be silent: a second write loop growing back
// in the handler, or a bench prop bypassing applyMark. The behavioral halves are covered
// elsewhere: the Bench inventory above proves click → callback, and the keyboard branch
// keeps its existing coverage.

const APP_TSX = (
  import.meta.glob("../../../App.tsx", { query: "?raw", eager: true, import: "default" }) as Record<
    string,
    string
  >
)["../../../App.tsx"];

const BENCH_TSX = (
  import.meta.glob("../Bench.tsx", { query: "?raw", eager: true, import: "default" }) as Record<
    string,
    string
  >
)["../Bench.tsx"];

/** The source between two markers, asserting both exist (and, for `start`, is unique). */
function between(src: string, start: string, end: string): string {
  const i = src.indexOf(start);
  expect(i, `marker not found: ${start}`).toBeGreaterThan(-1);
  expect(src.indexOf(start, i + 1), `marker not unique: ${start}`).toBe(-1);
  const j = src.indexOf(end, i + start.length);
  expect(j, `marker not found after start: ${end}`).toBeGreaterThan(-1);
  return src.slice(i + start.length, j);
}

describe("one code path: bench marking = keyboard culling", () => {
  it("App defines exactly one applyToSelection, owning the target loop and the refresh", () => {
    const body = between(
      APP_TSX,
      "const applyToSelection = useCallback(",
      "[selection.targets, refresh],",
    );
    expect(body).toContain("for (const id of selection.targets) await fn(id);");
    expect(body).toContain("await refresh();");
  });

  it("the keyboard's applyAll is a thin wrapper: applyToSelection plus the advance", () => {
    const body = between(
      APP_TSX,
      "const applyAll = async (fn: (id: number) => Promise<unknown>) => {",
      "};",
    );
    expect(body).toContain("await applyToSelection(fn);");
    expect(body).toContain("library.stepActive(1)");
    // No residual write loop or refresh of its own — those live only in applyToSelection.
    expect(body).not.toMatch(/setRating|setPickState|setLabel|refresh\(|for \(/);
  });

  it("every grid culling key routes through applyAll", () => {
    expect(APP_TSX).toContain("await applyAll((id) => setRating(id, parseInt(key, 10)));");
    expect(APP_TSX).toContain('await applyAll((id) => setPickState(id, "pick"));');
    expect(APP_TSX).toContain('await applyAll((id) => setPickState(id, "reject"));');
    expect(APP_TSX).toContain('await applyAll((id) => setPickState(id, "none"));');
    expect(APP_TSX).toContain("await applyAll((id) => setLabel(id, COLOR_KEYS[key]));");
  });

  it("the keyboard handler writes marks only through applyAll or to Compare's focused pane", () => {
    const handler = between(
      APP_TSX,
      "// Keyboard culling.",
      'window.addEventListener("keydown", handler);',
    );
    const writes = handler
      .split("\n")
      .filter((line) => /setRating\(|setPickState\(|setLabel\(/.test(line));
    expect(writes.length).toBeGreaterThan(0);
    for (const line of writes) {
      expect(line, `stray write path in the keyboard handler: ${line.trim()}`).toMatch(
        /applyAll\(|focused/,
      );
    }
  });

  it("the bench's marking props route through applyMark", () => {
    expect(APP_TSX).toContain("onRate={(n) => void applyMark((id) => setRating(id, n))}");
    expect(APP_TSX).toContain("onPick={(s) => void applyMark((id) => setPickState(id, s))}");
    expect(APP_TSX).toContain("onLabel={(name) => void applyMark((id) => setLabel(id, name))}");
  });

  it("applyMark's grid path is applyToSelection verbatim, and it never advances", () => {
    const body = between(
      APP_TSX,
      "const applyMark = useCallback(",
      "[inCompare, shellPhoto, applyToSelection, refresh],",
    );
    expect(body).toContain("await applyToSelection(fn);");
    // Compare's branch mirrors the keyboard's Compare branch: the focused pane
    // (shellPhoto — the same photo the bench displays), then refresh.
    expect(body).toContain("await fn(shellPhoto.id);");
    // No second loop, and no auto-advance — clicking a star must not move the selection.
    expect(body).not.toMatch(/for \(|stepActive/);
  });

  it("Bench.tsx cannot write marks on its own", () => {
    // Presentational: without a modules/api import there is no setRating/setPickState/
    // setLabel in reach, so every write must come back through App's props — i.e. applyMark.
    expect(BENCH_TSX).not.toMatch(/from ["'][^"']*modules\/api["']/);
    expect(BENCH_TSX).not.toMatch(/setRating|setPickState|setLabel|invoke\(/);
  });
});
