// @vitest-environment jsdom
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

const { offloadPhoto } = vi.hoisted(() => ({ offloadPhoto: vi.fn() }));
vi.mock("../../modules/api", () => ({
  availableEditors: vi.fn().mockResolvedValue([]),
  backupPhoto: vi.fn(),
  cancelRapidraw: vi.fn(),
  createTag: vi.fn(),
  developInEditor: vi.fn(),
  editInRapidraw: vi.fn(),
  enqueueOperation: vi.fn(),
  getPhoto: vi.fn(),
  getPhotoTags: vi.fn().mockResolvedValue([]),
  getSetting: vi.fn().mockResolvedValue(null),
  importDeveloped: vi.fn(),
  listPublications: vi.fn().mockResolvedValue([]),
  listStackChildren: vi.fn().mockResolvedValue([]),
  listVersions: vi.fn().mockResolvedValue([]),
  offloadPhoto,
  onRapidrawProgress: vi.fn().mockResolvedValue(() => {}),
  rapidrawAvailable: vi.fn().mockResolvedValue(false),
  restorePhoto: vi.fn(),
  setLabel: vi.fn(),
  setPickState: vi.fn(),
  setRating: vi.fn(),
  setSetting: vi.fn(),
  suggestTagsByTime: vi.fn().mockResolvedValue([]),
  unstackPhoto: vi.fn(),
}));

import { PhotoInspector } from "../PhotoInspector";

describe("PhotoInspector storage outcome", () => {
  beforeAll(() => {
    const values = new Map<string, string>();
    Object.defineProperty(globalThis, "localStorage", {
      configurable: true,
      value: {
        getItem: (key: string) => values.get(key) ?? null,
        setItem: (key: string, value: string) => values.set(key, value),
      },
    });
  });
  beforeEach(() => {
    localStorage.setItem("inspector.section.storage", "1");
    offloadPhoto.mockReset();
  });

  it("renders the whole selected moment from the IPC report total", async () => {
    offloadPhoto.mockResolvedValue({
      freed: [1, 2, 3, 4],
      skipped: [{ photoId: 7, reason: "no verified backup yet" }],
      total: 7,
      sidecarBackupsLeft: 0,
    });
    render(
      <PhotoInspector
        photo={{ id: 1, path: "one.raw", rating: 0, pickState: 0, label: null } as never}
        status="backedUp"
        allTags={[]}
        activeVersionId={null}
        canEditVersions={false}
        clipboardCount={0}
        selectionCount={1}
        onChanged={vi.fn()}
        onSelectVersion={vi.fn()}
        onEditVersion={vi.fn()}
        onCopyTags={vi.fn()}
        onPasteTags={vi.fn()}
        onAssignTag={vi.fn()}
        onRemoveTag={vi.fn()}
        onRotate={vi.fn()}
        onViewPhoto={vi.fn()}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "Offload local" }));
    await waitFor(() =>
      expect(screen.getByText("Freed 4 of 7 — no verified backup yet")).toBeTruthy(),
    );
  });
});
