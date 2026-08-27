// @vitest-environment jsdom
/**
 * The docked inspector shell (Inspector.tsx): thin chrome — serif filename header with
 * the label swatch and a hide button, the four-tab row, and the body slot. What is worth
 * pinning is the delegation contract, since the shell renders none of the content itself:
 *
 *  - each tab click reports the tab's id (App owns the state and its persistence);
 *  - exactly the active tab is styled/marked selected;
 *  - the hide button fires (App collapses the column, same as the TitleBar checkbox);
 *  - the header names the photo (basename + colour-label swatch, both from the row);
 *  - the `quickTags` node renders only while the tags tab is active — the other tabs
 *    must unmount it so its fetches never run off-screen.
 *
 * QuickTagGroups (same file) is the old QuickTagBar rehomed; its behavior — the virtual
 * "Recently used" group, group switching, assign/manage callbacks — is pinned below with
 * the same invoke stub the old bar's backend calls go through.
 */
import { describe, expect, it, vi, beforeEach } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";

import {
  Inspector,
  QuickTagGroups,
  INSPECTOR_TABS,
  type InspectorProps,
  type InspectorTab,
} from "../Inspector";
import type { Photo, Tag } from "../../../modules/registry";

// QuickTagGroups fetches its groups/members through modules/api → invoke.
const responses = new Map<string, unknown>();
vi.mock("@tauri-apps/api/core", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@tauri-apps/api/core")>();
  return {
    ...actual,
    invoke: (command: string) =>
      responses.has(command) ? Promise.resolve(responses.get(command)) : Promise.resolve([]),
  };
});

function makePhoto(over: Partial<Photo> = {}): Photo {
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

function makeTag(id: number, name: string, fullPath = name): Tag {
  return {
    id,
    uuid: `tag-${id}`,
    name,
    fullPath,
    parentId: null,
    description: "",
    autoRule: null,
    private: false,
  };
}

function buildProps(): InspectorProps {
  return {
    tab: "details",
    onTab: vi.fn(),
    onHide: vi.fn(),
    photo: makePhoto(),
    children: <div>tab-content-stub</div>,
    quickTags: <div>quick-tags-stub</div>,
  };
}

beforeEach(() => {
  responses.clear();
});

describe("Inspector shell", () => {
  it.each(INSPECTOR_TABS.map((t) => [t] as [InspectorTab]))(
    "clicking the %s tab reports that tab",
    (t) => {
      const props = buildProps();
      render(<Inspector {...props} />);
      fireEvent.click(screen.getByRole("tab", { name: t }));
      expect(props.onTab).toHaveBeenCalledTimes(1);
      expect(props.onTab).toHaveBeenCalledWith(t);
    },
  );

  it("marks exactly the active tab selected and styled", () => {
    render(<Inspector {...buildProps()} tab="versions" />);
    const tabs = screen.getAllByRole("tab");
    expect(tabs).toHaveLength(4);
    const on = tabs.filter((t) => t.getAttribute("aria-selected") === "true");
    expect(on).toHaveLength(1);
    expect(on[0].textContent).toBe("versions");
    expect(on[0].classList.contains("on")).toBe(true);
    for (const t of tabs) {
      if (t !== on[0]) expect(t.classList.contains("on")).toBe(false);
    }
  });

  it("the hide button fires onHide exactly once", () => {
    const props = buildProps();
    render(<Inspector {...props} />);
    fireEvent.click(screen.getByRole("button", { name: "Hide the inspector" }));
    expect(props.onHide).toHaveBeenCalledTimes(1);
  });

  it("the header shows the photo's basename and its colour-label swatch", () => {
    render(<Inspector {...buildProps()} photo={makePhoto({ label: "Green" })} />);
    const h3 = document.querySelector(".insp-head h3");
    expect(h3?.textContent).toBe("_DSC8177.ARW");
    expect(h3?.getAttribute("title")).toBe("2026/08/_DSC8177.ARW");
    const swatch = screen.getByTitle("Green label");
    expect(swatch.classList.contains("insp-swatch")).toBe(true);
    // Stored meaning, not theme colour: the canonical Green from COLOR_LABELS.
    expect((swatch as HTMLElement).style.background).toBe("rgb(16, 185, 129)");
  });

  it("shows no swatch for an unlabelled photo, and an empty header for none", () => {
    const { unmount } = render(<Inspector {...buildProps()} />);
    expect(document.querySelector(".insp-swatch")).toBeNull();
    unmount();
    render(<Inspector {...buildProps()} photo={null} />);
    expect(document.querySelector(".insp-head h3")?.textContent).toBe("");
    expect(document.querySelector(".insp-swatch")).toBeNull();
  });

  it("renders the children slot on every tab", () => {
    for (const t of INSPECTOR_TABS) {
      const { unmount } = render(<Inspector {...buildProps()} tab={t} />);
      expect(screen.getByText("tab-content-stub")).toBeTruthy();
      unmount();
    }
  });

  it("renders quickTags on the tags tab only", () => {
    for (const t of INSPECTOR_TABS) {
      const { unmount } = render(<Inspector {...buildProps()} tab={t} />);
      if (t === "tags") expect(screen.getByText("quick-tags-stub")).toBeTruthy();
      else expect(screen.queryByText("quick-tags-stub")).toBeNull();
      unmount();
    }
  });
});

describe("QuickTagGroups", () => {
  function buildQtgProps() {
    return {
      reloadKey: 0,
      selectionCount: 1,
      onAssign: vi.fn(),
      onManage: vi.fn(),
    };
  }

  it("lists the virtual Recently-used group first, then the fetched groups", async () => {
    responses.set("list_tag_groups", [{ id: 1, name: "Wedding set" }]);
    responses.set("recently_used_tags", [makeTag(11, "Oslo", "Places/Oslo")]);
    render(<QuickTagGroups {...buildQtgProps()} />);
    await waitFor(() => expect(screen.getByText("Wedding set")).toBeTruthy());
    expect(screen.getByText("Recently used")).toBeTruthy();
    // Recently used is the default active group; its members are on screen.
    await waitFor(() => expect(screen.getByText("Oslo")).toBeTruthy());
  });

  it("switching group fetches and shows that group's members", async () => {
    responses.set("list_tag_groups", [{ id: 1, name: "Wedding set" }]);
    responses.set("get_group_members", [makeTag(21, "Bride", "People/Bride")]);
    render(<QuickTagGroups {...buildQtgProps()} />);
    await waitFor(() => expect(screen.getByText("Wedding set")).toBeTruthy());
    fireEvent.click(screen.getByText("Wedding set"));
    await waitFor(() => expect(screen.getByText("Bride")).toBeTruthy());
  });

  it("clicking a tag fires onAssign with the tag id", async () => {
    responses.set("recently_used_tags", [makeTag(11, "Oslo", "Places/Oslo")]);
    const props = buildQtgProps();
    render(<QuickTagGroups {...props} />);
    await waitFor(() => expect(screen.getByText("Oslo")).toBeTruthy());
    fireEvent.click(screen.getByText("Oslo"));
    expect(props.onAssign).toHaveBeenCalledTimes(1);
    expect(props.onAssign).toHaveBeenCalledWith(11);
  });

  it("the groups button fires onManage", async () => {
    const props = buildQtgProps();
    render(<QuickTagGroups {...props} />);
    fireEvent.click(screen.getByTitle("Create/edit tag groups"));
    expect(props.onManage).toHaveBeenCalledTimes(1);
  });

  it("says so when there are no recently used tags yet", async () => {
    render(<QuickTagGroups {...buildQtgProps()} />);
    await waitFor(() =>
      expect(screen.getByText(/No recently used tags yet/)).toBeTruthy(),
    );
  });
});
