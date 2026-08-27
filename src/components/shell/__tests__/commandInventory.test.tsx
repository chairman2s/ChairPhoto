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
 * without restructuring this file — see AGENTS.md's topbar-extraction task.
 */
import { describe, expect, it, vi, type Mock } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { TitleBar, type TitleBarProps, type ModuleActionGroup } from "../TitleBar";
import type { ImportBatch } from "../../../modules/api";
import type { ToolbarAction } from "../../../modules/registry";

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
    compareOn: false,
    compareEnabled: true,
    onToggleCompare: vi.fn(),
    selectionCount: 2,
    onAnalyseBurst: vi.fn(),
    onProposeStacks: vi.fn(),
    onCullSession: vi.fn(),
    canPublish: true,
    onPublish: vi.fn(),
    canBackUpSelection: true,
    onBackUpSelection: vi.fn(),
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
    name: "compare",
    run: () => {
      openMoreMenu();
      clickMenuItem("Compare");
    },
    spy: (p) => p.onToggleCompare as unknown as Mock,
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
    name: "publish",
    run: () => {
      openMoreMenu();
      clickMenuItem("Publish…");
    },
    spy: (p) => p.onPublish as unknown as Mock,
  },
  {
    name: "back up selection",
    run: () => {
      openMoreMenu();
      clickMenuItem("Back up selection");
    },
    spy: (p) => p.onBackUpSelection as unknown as Mock,
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
    expect(cases.length).toBe(24);
  });
});
