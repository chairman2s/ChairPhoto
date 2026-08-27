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
import { CommandPill, type CommandPillProps } from "../CommandPill";
import { CollectionBrowser, type CollectionBrowserProps } from "../CollectionBrowser";
import { Inspector, type InspectorProps } from "../Inspector";
import { PhotoInspector } from "../../PhotoInspector";
import { COLOR_LABELS } from "../../../modules/labels";
import type { ImportBatch } from "../../../modules/api";
import type { ToolbarAction, MainView, Photo } from "../../../modules/registry";

// CollectionBrowser's sections persist open/collapsed state to localStorage. Node ≥22's
// experimental global `localStorage` can win the race against jsdom's own Storage under
// vitest but throw "is not a function" on every call (no backing file configured) — see
// src/theme/__tests__/theme.test.ts's comment on the same issue. Detect the broken
// accessor once and substitute a real in-memory Storage.
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
    // Was 21; Trash moved to CollectionBrowser's Library section (see below).
    expect(cases.length).toBe(20);
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

// -- CollectionBrowser -------------------------------------------------------------------
// CollectionBrowser.tsx merged the four leftcol panels (TagPanel, AlbumsPanel,
// SmartAlbumsPanel, BatchesPanel) into one scrollable browser and added a Library
// section on top (All photos / Trash) — Trash's case here replaces the "trash" case that
// used to live in the TitleBar table above, since the entry point moved off the More ⋯
// menu. AlbumsPanel/SmartAlbumsPanel/BatchesPanel fetch via `invoke` on mount; the
// CommandPill block below already stubs `@tauri-apps/api/core`'s `invoke` file-wide (vi.mock
// hoists), so rendering them here is safe without a per-block mock.

/** Permissive browser props: an active "all photos" scope, a known trash count, empty
 *  pass-through panels, fresh vi.fn()s everywhere. */
function buildBrowserProps(): CollectionBrowserProps {
  return {
    isAllScope: true,
    onSelectAll: vi.fn(),
    trashCount: 5,
    onOpenTrash: vi.fn(),
    tagPanel: {
      tags: [],
      activeTagId: null,
      onSelectTag: vi.fn(),
      onEditTag: vi.fn(),
      onMoveTag: vi.fn(),
      onSetPrivate: vi.fn(),
      onTagsChanged: vi.fn(),
      selectedPhotoIds: [],
      onStatus: vi.fn(),
    },
    albumsPanel: {
      activeAlbumId: null,
      onSelectAlbum: vi.fn(),
      selectionCount: 0,
      onAddSelection: vi.fn(async () => {}),
      reloadKey: 0,
    },
    smartAlbumsPanel: {
      activeSmartAlbumId: null,
      onSelectSmartAlbum: vi.fn(),
      onEditRule: vi.fn(),
      reloadKey: 0,
    },
    batchesPanel: {
      activeBatchId: null,
      onSelectBatch: vi.fn(),
      onExportBatch: vi.fn(),
      reloadKey: 0,
    },
  };
}

interface BrowserCase {
  name: string;
  props?: Partial<CollectionBrowserProps>;
  run: () => void;
  spy: (props: CollectionBrowserProps) => Mock;
}

// TagPanel keeps its own "All photos" root row (clears just the tag scope — see
// TagPanel.tsx's `tag-filterrow`), so `getByRole("button", { name: "All photos" })` would
// find two matches once the (default-open) tags section renders it alongside the
// browser's own Library row. `.brow-li` is unique to the Library section's two rows
// (index 0 = All photos, 1 = Trash), so query by that instead of by accessible name.
const browserRow = (index: 0 | 1) => document.querySelectorAll(".brow-li")[index];

const browserCases: BrowserCase[] = [
  {
    name: "all photos",
    run: () => fireEvent.click(browserRow(0)),
    spy: (p) => p.onSelectAll as unknown as Mock,
  },
  {
    name: "trash",
    run: () => fireEvent.click(browserRow(1)),
    spy: (p) => p.onOpenTrash as unknown as Mock,
  },
];

describe("CollectionBrowser command inventory (preservation)", () => {
  it.each(browserCases)("$name fires its callback exactly once", ({ props: patch, run, spy }) => {
    const props = { ...buildBrowserProps(), ...patch };
    render(<CollectionBrowser {...props} />);

    run();

    const mock = spy(props);
    expect(mock).toHaveBeenCalledTimes(1);
  });

  it("covers every Library-section affordance CollectionBrowser renders", () => {
    expect(browserCases.length).toBe(2);
  });
});

// The full behavioral suite (active-state marking, trash-count display, section
// collapse/expand + persistence, collapsed-body unmounting, pass-through panel props)
// lives in CollectionBrowser.test.tsx alongside the component; this file only pins the
// two Library-section commands' reachability, matching the other tables above.

// -- CommandPill -------------------------------------------------------------------------
// The floating command pill (CommandPill.tsx) absorbed the old `<FilterBar>` row that used
// to sit directly under TitleBar — culling seg, colour-label dots, removable scope chips,
// and the facet/camera/lens/storage/sort pickers (now behind a "+Filter" menu) — plus the
// smart-album chip and thumbnail-size slider FilterBar never had. Same preservation intent,
// same table style, own inventory.
//
// Unlike TitleBar/IconRail/Bench, CommandPill resolves a few names (album/smart-album/batch)
// and option lists (facets/cameras/lenses) via `invoke` on mount. Every case below asserts
// through a chip/item's `title` or static label rather than an async-resolved display name,
// so none of them need to wait on that fetch — the mock just needs to not reject.

vi.mock("@tauri-apps/api/core", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@tauri-apps/api/core")>();
  return {
    ...actual,
    invoke: () => Promise.resolve([]),
  };
});

/** Permissive pill props: an empty scope (no chips) and fresh vi.fn()s. Individual cases
 *  patch in whatever scope value their chip needs. */
function buildPillProps(): CommandPillProps {
  return {
    filters: ["all", "unrated", "pick", "reject", "edited"],
    filter: "all",
    onFilter: vi.fn(),
    activeTagLabel: null,
    onClearTag: vi.fn(),
    activeAlbumId: null,
    onClearAlbum: vi.fn(),
    activeSmartAlbumId: null,
    onClearSmartAlbum: vi.fn(),
    activeBatchId: null,
    onClearBatch: vi.fn(),
    activeFacets: [],
    onToggleFacet: vi.fn(),
    storageTier: "all",
    onStorageTier: vi.fn(),
    photoSort: "date",
    onPhotoSort: vi.fn(),
    activeCamera: null,
    onCamera: vi.fn(),
    activeLens: null,
    onLens: vi.fn(),
    activeLabels: [],
    onToggleLabel: vi.fn(),
    reloadKey: 0,
    thumbSize: 160,
    onThumbSize: vi.fn(),
  };
}

const openFilterMenu = () => fireEvent.click(screen.getByRole("button", { name: "＋ Filter" }));

interface PillCase {
  name: string;
  /** Overrides on the permissive base — e.g. an active tag, so its chip renders. */
  props?: Partial<CommandPillProps>;
  run: () => void;
  spy: (props: CommandPillProps) => Mock;
  expectCalledWith?: unknown[];
}

const pillCases: PillCase[] = [
  {
    name: "seg: All",
    run: () => fireEvent.click(screen.getByRole("button", { name: "All" })),
    spy: (p) => p.onFilter as unknown as Mock,
    expectCalledWith: ["all"],
  },
  {
    name: "seg: Unrated",
    run: () => fireEvent.click(screen.getByRole("button", { name: "Unrated" })),
    spy: (p) => p.onFilter as unknown as Mock,
    expectCalledWith: ["unrated"],
  },
  {
    name: "seg: Picks",
    run: () => fireEvent.click(screen.getByRole("button", { name: "Picks" })),
    spy: (p) => p.onFilter as unknown as Mock,
    expectCalledWith: ["pick"],
  },
  {
    name: "seg: Rejects",
    run: () => fireEvent.click(screen.getByRole("button", { name: "Rejects" })),
    spy: (p) => p.onFilter as unknown as Mock,
    expectCalledWith: ["reject"],
  },
  {
    name: "seg: Edited",
    run: () => fireEvent.click(screen.getByRole("button", { name: "Edited" })),
    spy: (p) => p.onFilter as unknown as Mock,
    expectCalledWith: ["edited"],
  },
  // One case per canonical colour label, same pattern as the bench's dots below.
  ...COLOR_LABELS.map(
    (l): PillCase => ({
      name: `label dot (${l.name})`,
      run: () => fireEvent.click(screen.getByTitle(`${l.name} label`)),
      spy: (p) => p.onToggleLabel as unknown as Mock,
      expectCalledWith: [l.name],
    }),
  ),
  {
    name: "label dot (none)",
    run: () => fireEvent.click(screen.getByTitle("No label")),
    spy: (p) => p.onToggleLabel as unknown as Mock,
    expectCalledWith: [""],
  },
  {
    name: "chip: tag",
    props: { activeTagLabel: "Portraits" },
    run: () => fireEvent.click(screen.getByTitle("Clear tag filter")),
    spy: (p) => p.onClearTag as unknown as Mock,
  },
  {
    name: "chip: album",
    props: { activeAlbumId: 3 },
    run: () => fireEvent.click(screen.getByTitle("Clear album filter")),
    spy: (p) => p.onClearAlbum as unknown as Mock,
  },
  {
    name: "chip: smart album",
    props: { activeSmartAlbumId: 4 },
    run: () => fireEvent.click(screen.getByTitle("Clear smart album filter")),
    spy: (p) => p.onClearSmartAlbum as unknown as Mock,
  },
  {
    name: "chip: batch",
    props: { activeBatchId: 5 },
    run: () => fireEvent.click(screen.getByTitle("Clear batch filter")),
    spy: (p) => p.onClearBatch as unknown as Mock,
  },
  {
    name: "chip: facet",
    props: { activeFacets: ["has-gps"] },
    run: () => fireEvent.click(screen.getByTitle("Remove facet filter")),
    spy: (p) => p.onToggleFacet as unknown as Mock,
    expectCalledWith: ["has-gps"],
  },
  {
    name: "chip: camera",
    props: { activeCamera: "Canon EOS R5" },
    run: () => fireEvent.click(screen.getByTitle("Clear camera filter")),
    spy: (p) => p.onCamera as unknown as Mock,
    expectCalledWith: [null],
  },
  {
    name: "chip: lens",
    props: { activeLens: "50mm" },
    run: () => fireEvent.click(screen.getByTitle("Clear lens filter")),
    spy: (p) => p.onLens as unknown as Mock,
    expectCalledWith: [null],
  },
  {
    name: "chip: storage tier",
    props: { storageTier: "nas" },
    run: () => fireEvent.click(screen.getByTitle("Clear storage filter")),
    spy: (p) => p.onStorageTier as unknown as Mock,
    expectCalledWith: ["all"],
  },
  {
    name: "+Filter: Any camera",
    run: () => {
      openFilterMenu();
      fireEvent.click(screen.getByRole("menuitem", { name: "Camera" }));
      fireEvent.click(screen.getByRole("menuitem", { name: "Any camera" }));
    },
    spy: (p) => p.onCamera as unknown as Mock,
    expectCalledWith: [null],
  },
  {
    name: "+Filter: Any lens",
    run: () => {
      openFilterMenu();
      fireEvent.click(screen.getByRole("menuitem", { name: "Lens" }));
      fireEvent.click(screen.getByRole("menuitem", { name: "Any lens" }));
    },
    spy: (p) => p.onLens as unknown as Mock,
    expectCalledWith: [null],
  },
  {
    name: "+Filter: Storage All",
    run: () => {
      openFilterMenu();
      fireEvent.click(screen.getByRole("menuitemcheckbox", { name: "All" }));
    },
    spy: (p) => p.onStorageTier as unknown as Mock,
    expectCalledWith: ["all"],
  },
  {
    name: "+Filter: Storage On disk",
    run: () => {
      openFilterMenu();
      fireEvent.click(screen.getByRole("menuitemcheckbox", { name: "On disk" }));
    },
    spy: (p) => p.onStorageTier as unknown as Mock,
    expectCalledWith: ["local"],
  },
  {
    name: "+Filter: Storage NAS only",
    run: () => {
      openFilterMenu();
      fireEvent.click(screen.getByRole("menuitemcheckbox", { name: "NAS only" }));
    },
    spy: (p) => p.onStorageTier as unknown as Mock,
    expectCalledWith: ["nas"],
  },
  {
    name: "+Filter: Sort Date",
    run: () => {
      openFilterMenu();
      fireEvent.click(screen.getByRole("menuitemcheckbox", { name: "Date" }));
    },
    spy: (p) => p.onPhotoSort as unknown as Mock,
    expectCalledWith: ["date"],
  },
  {
    name: "+Filter: Sort Least sharp first",
    run: () => {
      openFilterMenu();
      fireEvent.click(screen.getByRole("menuitemcheckbox", { name: "Least sharp first" }));
    },
    spy: (p) => p.onPhotoSort as unknown as Mock,
    expectCalledWith: ["sharpness_asc"],
  },
  {
    name: "+Filter: Sort Sharpest first",
    run: () => {
      openFilterMenu();
      fireEvent.click(screen.getByRole("menuitemcheckbox", { name: "Sharpest first" }));
    },
    spy: (p) => p.onPhotoSort as unknown as Mock,
    expectCalledWith: ["sharpness_desc"],
  },
  {
    name: "thumbnail size slider",
    run: () =>
      fireEvent.change(screen.getByLabelText("Thumbnail size"), { target: { value: "240" } }),
    spy: (p) => p.onThumbSize as unknown as Mock,
    expectCalledWith: [240],
  },
];

describe("CommandPill command inventory (preservation)", () => {
  it.each(pillCases)(
    "$name fires its callback exactly once",
    ({ props: patch, run, spy, expectCalledWith }) => {
      const props = { ...buildPillProps(), ...patch };
      render(<CommandPill {...props} />);

      run();

      const mock = spy(props);
      expect(mock).toHaveBeenCalledTimes(1);
      if (expectCalledWith) {
        expect(mock).toHaveBeenCalledWith(...expectCalledWith);
      }
    },
  );

  it("covers every affordance CommandPill renders", () => {
    expect(pillCases.length).toBe(28);
  });

  it("renders no scope chips when the scope is empty", () => {
    render(<CommandPill {...buildPillProps()} />);
    expect(screen.queryByText("✕")).toBeNull();
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

// -- Inspector -------------------------------------------------------------------------
// The docked inspector column (shell/Inspector.tsx) replaced the plain PhotoInspector +
// bottom QuickTagBar stack in `.rightcol`: a header (filename + label swatch + hide
// button) and a four-tab row now stand between the user and the tab content. Same
// preservation intent, same table style, own inventory. The tab content's own commands
// keep their coverage elsewhere (the Bench/keyboard own marking; the panels their
// suites) — the new chrome here is the tab row and the hide button, plus the publish
// tab's Publish… entry point pinned right after.

function buildInspectorProps(): InspectorProps {
  return {
    tab: "details",
    onTab: vi.fn(),
    onHide: vi.fn(),
    photo: benchPhoto(),
    children: <div>inspector-children-stub</div>,
    quickTags: null,
  };
}

interface InspectorCase {
  name: string;
  run: () => void;
  spy: (props: InspectorProps) => Mock;
  expectCalledWith?: unknown[];
}

const inspectorCases: InspectorCase[] = [
  ...(["details", "tags", "versions", "publish"] as const).map(
    (t): InspectorCase => ({
      name: `tab: ${t}`,
      run: () => fireEvent.click(screen.getByRole("tab", { name: t })),
      spy: (p) => p.onTab as unknown as Mock,
      expectCalledWith: [t],
    }),
  ),
  {
    name: "hide the inspector",
    run: () => fireEvent.click(screen.getByRole("button", { name: "Hide the inspector" })),
    spy: (p) => p.onHide as unknown as Mock,
  },
];

describe("Inspector command inventory", () => {
  it.each(inspectorCases)(
    "$name fires its callback exactly once",
    ({ run, spy, expectCalledWith }) => {
      const props = buildInspectorProps();
      render(<Inspector {...props} />);

      run();

      const mock = spy(props);
      expect(mock).toHaveBeenCalledTimes(1);
      if (expectCalledWith) {
        expect(mock).toHaveBeenCalledWith(...expectCalledWith);
      }
    },
  );

  it("covers every affordance the inspector shell renders", () => {
    expect(inspectorCases.length).toBe(5);
  });
});

// The publish tab's "Publish…" button lives in PhotoInspector (the tab content), wired
// through the optional onPublish prop — App passes it exactly when the Bench's canPublish
// gate (an active grid selection) is open, so an absent prop renders it disabled. The
// file-wide invoke stub above lets the tab's PublishedPanel mount against an empty
// catalog.

function buildPhotoInspectorProps() {
  return {
    tab: "publish" as const,
    photo: benchPhoto(),
    onChanged: vi.fn(),
    allTags: [],
    status: null,
    activeVersionId: null,
    onSelectVersion: vi.fn(),
    canEditVersions: false,
    onEditVersion: vi.fn(),
    clipboardCount: 0,
    selectionCount: 1,
    onCopyTags: vi.fn(),
    onPasteTags: vi.fn(),
    onAssignTag: vi.fn(),
    onRemoveTag: vi.fn(),
    onRotate: vi.fn(),
    onViewPhoto: vi.fn(),
  };
}

describe("PhotoInspector publish entry point", () => {
  it("the publish tab's Publish… button fires onPublish exactly once", async () => {
    const onPublish = vi.fn();
    render(<PhotoInspector {...buildPhotoInspectorProps()} onPublish={onPublish} />);
    // Let the panel's stubbed fetches settle (empty catalog → the empty state).
    await screen.findByText("Not published yet");

    fireEvent.click(screen.getByRole("button", { name: "Publish…" }));
    expect(onPublish).toHaveBeenCalledTimes(1);
  });

  it("renders Publish… disabled when onPublish is absent (canPublish gate closed)", async () => {
    render(<PhotoInspector {...buildPhotoInspectorProps()} />);
    await screen.findByText("Not published yet");

    const btn = screen.getByRole("button", { name: "Publish…" }) as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
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
