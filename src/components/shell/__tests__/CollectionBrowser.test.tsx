// @vitest-environment jsdom
/**
 * CollectionBrowser.tsx merges the four panels that used to stack in `.leftcol` — TagPanel,
 * AlbumsPanel, SmartAlbumsPanel, BatchesPanel — into one scrollable browser, adding a new
 * Library section (All photos / Trash) on top. This is a rehost, not a rewrite: the four
 * panels keep their own fetching/modals/context menus/drag/drop/quick-search, so these tests
 * exercise the wrapper's own contract — the Library rows, the collapsible sections, and that
 * each pass-through panel actually receives (and renders with) the prop bag it's given.
 *
 * AlbumsPanel/SmartAlbumsPanel/BatchesPanel each fetch their list via `invoke` on mount
 * (list_albums/list_smart_albums/list_import_batches) — mocked to resolve empty, same pattern
 * as commandInventory.test.tsx's CommandPill block and CommandPill.test.tsx itself.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { CollectionBrowser, type CollectionBrowserProps } from "../CollectionBrowser";
import type { TagWithCount } from "../../../modules/api";

vi.mock("@tauri-apps/api/core", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@tauri-apps/api/core")>();
  return {
    ...actual,
    invoke: () => Promise.resolve([]),
  };
});

// Node ≥22's experimental global `localStorage` can win the race against jsdom's own Storage
// under vitest but throw "is not a function" on every call (no backing file configured) — see
// src/theme/__tests__/theme.test.ts's comment on the same issue. Detect the broken accessor
// once and substitute a real in-memory Storage so the persistence assertions below exercise
// actual reads/writes rather than passing vacuously against a silently-swallowed throw.
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

beforeEach(() => {
  localStorage.clear();
});

function tag(id: number, name: string, over: Partial<TagWithCount> = {}): TagWithCount {
  return {
    id,
    uuid: `uuid-${id}`,
    name,
    fullPath: name,
    parentId: null,
    description: "",
    autoRule: null,
    private: false,
    photoCount: 0,
    ...over,
  };
}

/** Permissive props: an active "all photos" scope, a known trash count, empty pass-through
 *  panels, fresh vi.fn()s everywhere. */
function buildProps(): CollectionBrowserProps {
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

// TagPanel keeps its own "All photos" root row (clears just the tag scope, unconditionally
// rendered — see TagPanel.tsx's `tag-filterrow`), so a role/name query for "All photos" would
// find two matches once the (default-open) tags section renders alongside the browser's own
// Library row. `.brow-li` is unique to the Library section's two rows (0 = All photos,
// 1 = Trash), so tests below query by that instead of by accessible name.
function libraryRows(container: HTMLElement): [Element, Element] {
  const rows = container.querySelectorAll(".brow-li");
  return [rows[0], rows[1]];
}

describe("CollectionBrowser: Library section", () => {
  it("All photos fires onSelectAll", () => {
    const props = buildProps();
    const { container } = render(<CollectionBrowser {...props} />);
    const [allPhotos] = libraryRows(container);

    fireEvent.click(allPhotos);

    expect(props.onSelectAll).toHaveBeenCalledTimes(1);
  });

  it("marks All photos active when isAllScope is true", () => {
    const { container } = render(<CollectionBrowser {...buildProps()} isAllScope={true} />);
    const [allPhotos] = libraryRows(container);
    expect(allPhotos.className).toMatch(/\bon\b/);
  });

  it("does not mark All photos active when a scope is set", () => {
    const { container } = render(<CollectionBrowser {...buildProps()} isAllScope={false} />);
    const [allPhotos] = libraryRows(container);
    expect(allPhotos.className).not.toMatch(/\bon\b/);
  });

  it("Trash fires onOpenTrash", () => {
    const props = buildProps();
    const { container } = render(<CollectionBrowser {...props} />);
    const [, trash] = libraryRows(container);

    fireEvent.click(trash);

    expect(props.onOpenTrash).toHaveBeenCalledTimes(1);
  });

  it("shows the trash count when known", () => {
    const { container } = render(<CollectionBrowser {...buildProps()} trashCount={38} />);
    const [, trash] = libraryRows(container);
    expect(trash.querySelector(".n")?.textContent).toBe("38");
  });

  it("hides the trash count when unknown (null)", () => {
    const { container } = render(<CollectionBrowser {...buildProps()} trashCount={null} />);
    const [, trash] = libraryRows(container);
    expect(trash.querySelector(".n")).toBeNull();
    expect(trash.textContent).toBe("Trash");
  });
});

describe("CollectionBrowser: collapsible sections", () => {
  it("default open: every pass-through panel's empty state is visible", () => {
    render(<CollectionBrowser {...buildProps()} />);
    expect(screen.getByText("No albums yet")).toBeTruthy();
    expect(screen.getByText("No smart albums yet")).toBeTruthy();
    expect(screen.getByText("No imports yet")).toBeTruthy();
  });

  it("collapsing a section unmounts its body", () => {
    render(<CollectionBrowser {...buildProps()} />);
    expect(screen.getByText("No albums yet")).toBeTruthy();

    // The header button's accessible name concatenates the caret glyph and the label
    // ("▾albums"), so target it via its label text rather than an exact role name.
    fireEvent.click(screen.getByText("albums").closest("button")!);

    expect(screen.queryByText("No albums yet")).toBeNull();
    // Its neighbours are untouched.
    expect(screen.getByText("No smart albums yet")).toBeTruthy();
  });

  it("expanding a collapsed section remounts its body", () => {
    render(<CollectionBrowser {...buildProps()} />);
    const header = () => screen.getByText("albums").closest("button")!;

    fireEvent.click(header()); // collapse
    expect(screen.queryByText("No albums yet")).toBeNull();
    fireEvent.click(header()); // expand
    expect(screen.getByText("No albums yet")).toBeTruthy();
  });

  it("persists a collapsed section across remounts via localStorage", () => {
    const { unmount } = render(<CollectionBrowser {...buildProps()} />);
    fireEvent.click(screen.getByText("albums").closest("button")!);
    expect(localStorage.getItem("panel.section.albums")).toBe("0");
    unmount();

    render(<CollectionBrowser {...buildProps()} />);
    // A fresh mount re-reads localStorage, so the collapse survives remount.
    expect(screen.queryByText("No albums yet")).toBeNull();
  });

  it("persists an expanded section across remounts via localStorage", () => {
    localStorage.setItem("panel.section.batches", "0");
    const { unmount } = render(<CollectionBrowser {...buildProps()} />);
    expect(screen.queryByText("No imports yet")).toBeNull();

    fireEvent.click(screen.getByText("import batches").closest("button")!);
    expect(localStorage.getItem("panel.section.batches")).toBe("1");
    unmount();

    render(<CollectionBrowser {...buildProps()} />);
    expect(screen.getByText("No imports yet")).toBeTruthy();
  });

  it("defaults an unvisited section to open (no localStorage entry yet)", () => {
    expect(localStorage.getItem("panel.section.tags")).toBeNull();
    render(<CollectionBrowser {...buildProps()} />);
    // TagPanel's own header ("Tags") stays visible per the per-panel header decision —
    // see CollectionBrowser.tsx's file header — so it's a reliable open/closed signal.
    expect(screen.getByText("Tags")).toBeTruthy();
  });

  it("the Library section has no disclosure control (always visible, not collapsible)", () => {
    const { container } = render(<CollectionBrowser {...buildProps()} />);
    // Its fixed header renders as a plain div, not a <button> — unlike every other
    // section's `.brow-gh`.
    const fixed = container.querySelector(".brow-gh-fixed");
    expect(fixed?.tagName).toBe("DIV");
  });
});

describe("CollectionBrowser: pass-through panels", () => {
  it("TagPanel renders the rows the tags prop is given", () => {
    render(
      <CollectionBrowser
        {...buildProps()}
        tagPanel={{
          ...buildProps().tagPanel,
          tags: [tag(1, "Portraits"), tag(2, "Landscapes")],
        }}
      />,
    );
    expect(screen.getByText("Portraits")).toBeTruthy();
    expect(screen.getByText("Landscapes")).toBeTruthy();
  });

  it("TagPanel's onSelectTag prop still fires when a tag row is clicked", () => {
    const onSelectTag = vi.fn();
    render(
      <CollectionBrowser
        {...buildProps()}
        tagPanel={{ ...buildProps().tagPanel, tags: [tag(1, "Portraits")], onSelectTag }}
      />,
    );
    fireEvent.click(screen.getByText("Portraits"));
    expect(onSelectTag).toHaveBeenCalledWith(1);
  });

  it("AlbumsPanel's onSelectAlbum prop is reachable through the wrapper", () => {
    // No fetched albums (invoke resolves []), but the panel's own root affordances (the
    // add button in its still-visible header) prove it mounted with its props intact.
    render(<CollectionBrowser {...buildProps()} />);
    expect(screen.getByTitle("New album")).toBeTruthy();
  });

  it("BatchesPanel's own header is rendered (hidden only by CSS, not unmounted)", () => {
    // BatchesPanel has no add affordance, so its `.panel-header` is hidden via
    // `.brow-section-hide-header .panel-header { display: none }` in App.css rather than
    // removed — jsdom doesn't apply external stylesheets, so the node is still queryable
    // here; this asserts the wrapper didn't drop BatchesPanel's markup outright.
    render(<CollectionBrowser {...buildProps()} />);
    expect(screen.getByText("Import batches")).toBeTruthy();
  });
});
