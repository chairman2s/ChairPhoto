// @vitest-environment jsdom
/**
 * CommandPill absorbed every one of FilterBar's internals (culling seg, colour-label dots,
 * scope chips, facet/camera/lens/storage/sort pickers) plus the smart-album chip and the
 * thumbnail-size slider FilterBar never had (see AGENTS.md's command-pill task). These
 * tests exercise the presentational contract directly — each affordance fires exactly the
 * callback it owns — rather than mocking any backend beyond the `invoke` calls the internal
 * album/facet/camera/lens/batch/smart-album name lookups make on mount (same pattern as
 * SafetyPanel.test.tsx: mock `@tauri-apps/api/core`'s `invoke`, route by command name).
 */
import { describe, expect, it, vi, type Mock } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { CommandPill, type CommandPillProps } from "../CommandPill";
import { COLOR_LABELS } from "../../../modules/labels";
import type { Album, Facet, ImportBatch, SmartAlbum } from "../../../modules/api";

const facets: Facet[] = [{ key: "has-gps", label: "Has GPS" }];
const cameras = ["Canon EOS R5"];
const lenses = ["RF 50mm F1.2L"];
const albums: Album[] = [{ id: 3, uuid: "a3", name: "Vacation", note: "", photoCount: 10 }];
const smartAlbums: SmartAlbum[] = [
  { id: 4, uuid: "s4", name: "Best of 2026", ruleJson: "{}", photoCount: 5 },
];
const batches: ImportBatch[] = [
  { id: 5, uuid: "b5", sourceLabel: "/Volumes/card/DCIM/100CANON", note: "", createdAt: 0, photoCount: 20 },
];

vi.mock("@tauri-apps/api/core", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@tauri-apps/api/core")>();
  return {
    ...actual,
    invoke: (command: string, args?: Record<string, unknown>) => {
      switch (command) {
        case "list_facets":
          return Promise.resolve(facets);
        case "distinct_photo_values":
          return Promise.resolve(args?.kind === "camera" ? cameras : lenses);
        case "list_albums":
          return Promise.resolve(albums);
        case "list_import_batches":
          return Promise.resolve(batches);
        case "list_smart_albums":
          return Promise.resolve(smartAlbums);
        default:
          return Promise.resolve(null);
      }
    },
  };
});

/** Every callback prop as a fresh vi.fn(), plus an empty scope (no chips) by default. */
function buildProps(): CommandPillProps {
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

function openFilterMenu() {
  fireEvent.click(screen.getByRole("button", { name: "＋ Filter" }));
}

// -- culling seg -----------------------------------------------------------------------

describe("CommandPill culling seg", () => {
  it.each([
    ["All", "all"],
    ["Unrated", "unrated"],
    ["Picks", "pick"],
    ["Rejects", "reject"],
    ["Edited", "edited"],
  ])("%s fires onFilter(%s)", (label, value) => {
    const props = buildProps();
    render(<CommandPill {...props} />);
    fireEvent.click(screen.getByRole("button", { name: label }));
    expect(props.onFilter).toHaveBeenCalledWith(value);
  });

  it("marks the active filter with the on class", () => {
    render(<CommandPill {...buildProps()} filter="pick" />);
    expect((screen.getByRole("button", { name: "Picks" })).className).toContain("on");
    expect((screen.getByRole("button", { name: "All" })).className).not.toContain("on");
  });
});

// -- label dots --------------------------------------------------------------------------

describe("CommandPill label dots", () => {
  it.each(COLOR_LABELS.map((l) => l.name))("%s dot toggles onToggleLabel", (name) => {
    const props = buildProps();
    render(<CommandPill {...props} />);
    fireEvent.click(screen.getByTitle(`${name} label`));
    expect(props.onToggleLabel).toHaveBeenCalledWith(name);
  });

  it("the none-dot toggles onToggleLabel('')", () => {
    const props = buildProps();
    render(<CommandPill {...props} />);
    fireEvent.click(screen.getByTitle("No label"));
    expect(props.onToggleLabel).toHaveBeenCalledWith("");
  });
});

// -- scope chips -----------------------------------------------------------------------
// Table-driven (mirrors commandInventory.test.tsx's style): one row per removable chip
// kind, asserting its ✕ fires exactly the clear callback that chip owns.

interface ChipCase {
  name: string;
  patch: Partial<CommandPillProps>;
  chipName: RegExp;
  spy: (p: CommandPillProps) => Mock;
  expectCalledWith?: unknown[];
}

const chipCases: ChipCase[] = [
  {
    name: "tag",
    patch: { activeTagLabel: "Portraits" },
    chipName: /Tag: Portraits/,
    spy: (p) => p.onClearTag as unknown as Mock,
  },
  {
    name: "album",
    patch: { activeAlbumId: 3 },
    chipName: /Album:/,
    spy: (p) => p.onClearAlbum as unknown as Mock,
  },
  {
    name: "smart album",
    patch: { activeSmartAlbumId: 4 },
    chipName: /Smart album:/,
    spy: (p) => p.onClearSmartAlbum as unknown as Mock,
  },
  {
    name: "batch",
    patch: { activeBatchId: 5 },
    chipName: /Batch:/,
    spy: (p) => p.onClearBatch as unknown as Mock,
  },
  {
    name: "facet",
    patch: { activeFacets: ["has-gps"] },
    chipName: /Has GPS/,
    spy: (p) => p.onToggleFacet as unknown as Mock,
    expectCalledWith: ["has-gps"],
  },
  {
    name: "camera",
    patch: { activeCamera: "Canon EOS R5" },
    chipName: /Camera: Canon EOS R5/,
    spy: (p) => p.onCamera as unknown as Mock,
    expectCalledWith: [null],
  },
  {
    name: "lens",
    patch: { activeLens: "RF 50mm F1.2L" },
    chipName: /Lens: RF 50mm F1.2L/,
    spy: (p) => p.onLens as unknown as Mock,
    expectCalledWith: [null],
  },
  {
    name: "storage tier",
    patch: { storageTier: "nas" },
    chipName: /NAS only/,
    spy: (p) => p.onStorageTier as unknown as Mock,
    expectCalledWith: ["all"],
  },
];

describe("CommandPill scope chips", () => {
  it.each(chipCases)(
    "$name chip's ✕ fires its clear callback",
    async ({ patch, chipName, spy, expectCalledWith }) => {
      const props = { ...buildProps(), ...patch };
      render(<CommandPill {...props} />);

      const chip = await screen.findByRole("button", { name: chipName });
      fireEvent.click(chip);

      const mock = spy(props);
      expect(mock).toHaveBeenCalledTimes(1);
      if (expectCalledWith) expect(mock).toHaveBeenCalledWith(...expectCalledWith);
    },
  );

  it("covers every scope chip kind CommandPill renders", () => {
    expect(chipCases.length).toBe(8);
  });

  it("renders no chips when the scope is empty", () => {
    render(<CommandPill {...buildProps()} />);
    expect(screen.queryByText("✕")).toBeNull();
  });
});

// -- +Filter menu: facets / camera / lens / storage / sort -----------------------------

describe("CommandPill +Filter menu", () => {
  it("a facet CheckItem toggles onToggleFacet", async () => {
    const props = buildProps();
    render(<CommandPill {...props} />);
    openFilterMenu();
    const item = await screen.findByRole("menuitemcheckbox", { name: "Has GPS" });
    fireEvent.click(item);
    expect(props.onToggleFacet).toHaveBeenCalledWith("has-gps");
  });

  it("picking a camera fires onCamera", async () => {
    const props = buildProps();
    render(<CommandPill {...props} />);
    openFilterMenu();
    fireEvent.click(screen.getByRole("menuitem", { name: "Camera" }));
    const item = await screen.findByRole("menuitem", { name: "Canon EOS R5" });
    fireEvent.click(item);
    expect(props.onCamera).toHaveBeenCalledWith("Canon EOS R5");
  });

  it("picking a lens fires onLens", async () => {
    const props = buildProps();
    render(<CommandPill {...props} />);
    openFilterMenu();
    fireEvent.click(screen.getByRole("menuitem", { name: "Lens" }));
    const item = await screen.findByRole("menuitem", { name: "RF 50mm F1.2L" });
    fireEvent.click(item);
    expect(props.onLens).toHaveBeenCalledWith("RF 50mm F1.2L");
  });

  it("'Any camera' fires onCamera(null)", () => {
    const props = buildProps();
    render(<CommandPill {...props} />);
    openFilterMenu();
    fireEvent.click(screen.getByRole("menuitem", { name: "Camera" }));
    fireEvent.click(screen.getByRole("menuitem", { name: "Any camera" }));
    expect(props.onCamera).toHaveBeenCalledWith(null);
  });

  it("a storage item fires onStorageTier", () => {
    const props = buildProps();
    render(<CommandPill {...props} />);
    openFilterMenu();
    fireEvent.click(screen.getByRole("menuitemcheckbox", { name: "On disk" }));
    expect(props.onStorageTier).toHaveBeenCalledWith("local");
  });

  it("marks the active storage tier checked", () => {
    render(<CommandPill {...buildProps()} storageTier="local" />);
    openFilterMenu();
    expect(
      screen.getByRole("menuitemcheckbox", { name: "On disk" }).getAttribute("aria-checked"),
    ).toBe("true");
  });

  it("a sort item fires onPhotoSort", () => {
    const props = buildProps();
    render(<CommandPill {...props} />);
    openFilterMenu();
    fireEvent.click(screen.getByRole("menuitemcheckbox", { name: "Sharpest first" }));
    expect(props.onPhotoSort).toHaveBeenCalledWith("sharpness_desc");
  });

  it("marks the active sort checked", () => {
    render(<CommandPill {...buildProps()} photoSort="sharpness_asc" />);
    openFilterMenu();
    expect(
      screen.getByRole("menuitemcheckbox", { name: "Least sharp first" }).getAttribute("aria-checked"),
    ).toBe("true");
  });
});

// -- thumbnail-size slider ---------------------------------------------------------------

describe("CommandPill thumbnail-size slider", () => {
  it("fires onThumbSize with the new value", () => {
    const props = buildProps();
    render(<CommandPill {...props} />);
    const slider = screen.getByLabelText("Thumbnail size") as HTMLInputElement;
    fireEvent.change(slider, { target: { value: "240" } });
    expect(props.onThumbSize).toHaveBeenCalledWith(240);
  });

  it("is bound to the thumbSize prop and clamped to [120, 320] step 8", () => {
    render(<CommandPill {...buildProps()} thumbSize={200} />);
    const slider = screen.getByLabelText("Thumbnail size") as HTMLInputElement;
    expect(slider.value).toBe("200");
    expect(slider.min).toBe("120");
    expect(slider.max).toBe("320");
    expect(slider.step).toBe("8");
  });
});
