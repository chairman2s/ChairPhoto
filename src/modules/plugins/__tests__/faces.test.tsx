// @vitest-environment jsdom
/**
 * Component tests for the Faces inspector panel's batch confirm (issue #68).
 *
 * The bug: with several photos of the same person selected, confirming a suggested face
 * only ever marked the *active* photo — the panel read `getActivePhotoId()` and nothing
 * else, so the other selected photos were silently untouched. The fix adds a second action
 * that confirms the person across the whole selection, and the behaviour worth pinning is
 * what makes that action honest rather than merely bulk:
 *
 *  - it sends the *person tag* and the *selected photo ids*, not the face id — a batch
 *    confirm is a statement about a person across photos, not about one face row;
 *  - it only appears when more than one photo is selected, so the single-photo flow is
 *    unchanged;
 *  - the active photo is never left out of its own panel's batch, even if the host's
 *    selection list somehow omits it;
 *  - the reported counts distinguish confirmed / already-confirmed / never-suggested
 *    photos, because "confirmed on all 9" would be a lie when 3 had no suggestion.
 *
 * `FacesInspectorPanel` is not exported; it is reached the way the host reaches it, by
 * running the module's `onLoad` and rendering the inspector panel it registers.
 */
import { describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import type { ReactNode } from "react";

import { facesModule } from "../faces";
import type { ChairPhotoAPI, Photo } from "../../registry";

/** A photo carrying only the fields the panel and the host contract need. */
function photo(id: number): Photo {
  return {
    id,
    uuid: `uuid-${id}`,
    path: `${id}.NEF`,
    rating: 0,
    label: "",
    pickState: "none",
    captureTime: null,
    width: null,
    height: null,
    cameraModel: null,
    lens: null,
    aperture: null,
    shutterSpeed: null,
    iso: null,
    metadataReady: 1,
    sharpness: null,
    sharpnessMethod: null,
  } as Photo;
}

/** One suggested face for "Alice" (tag 42) on the active photo. */
const SUGGESTED_ALICE = {
  id: 900,
  photoId: 3,
  bbox: { x: 0.1, y: 0.1, w: 0.2, h: 0.2 },
  detectConfidence: 0.99,
  personTagId: 42,
  personName: "Alice",
  state: "suggested",
  matchConfidence: 0.91,
  source: "match",
};

function makeApi(overrides: Partial<ChairPhotoAPI> = {}): ChairPhotoAPI {
  const base: ChairPhotoAPI = {
    getSelectedPhotos: () => [],
    getActivePhotoId: () => 3,
    getActiveVersionId: () => null,
    getEditingTag: () => null,
    listTags: () => Promise.resolve([]),
    assignTag: () => Promise.resolve(),
    recordPublication: () => Promise.resolve(),
    listPublications: () => Promise.resolve([]),
    deletePublication: () => Promise.resolve(),
    invoke: ((command: string) =>
      command === "faces_for_photo"
        ? Promise.resolve([SUGGESTED_ALICE])
        : Promise.resolve(null)) as ChairPhotoAPI["invoke"],
    getSetting: () => Promise.resolve(null),
    setSetting: () => Promise.resolve(),
    getEditRecord: () => Promise.resolve(null),
    setEditRecord: () => Promise.resolve(),
    registerPanel: () => {},
    registerAction: () => {},
    registerPublishTarget: () => {},
    registerSettingsPanel: () => {},
    registerMainView: () => {},
    registerEditRenderer: () => {},
    showToast: () => {},
    notifyChange: () => {},
    filterByTag: () => {},
    selectPhoto: () => {},
    selectPhotoSilent: () => {},
    getFilterContext: () => ({ tagId: null, albumId: null, batchId: null }),
  };
  return { ...base, ...overrides };
}

/** Run the module's `onLoad` and return the inspector panel's rendered element. */
function inspectorPanel(api: ChairPhotoAPI): ReactNode {
  let node: ReactNode = null;
  const withRegistration = makeApi({
    ...api,
    registerPanel: (panel) => {
      if (panel.id !== "faces-inspector") return;
      if (!panel.render) throw new Error("the faces-inspector panel lost its render()");
      node = panel.render();
    },
  });
  facesModule.onLoad(withRegistration);
  if (!node) throw new Error("the module registered no faces-inspector panel");
  return node;
}

/** The batch-confirm chip, whose label carries the selection size. */
function batchButton(): Promise<HTMLButtonElement> {
  return waitFor(
    () => screen.getByRole("button", { name: /confirm on \d+/ }) as HTMLButtonElement,
  );
}

describe("FacesInspectorPanel — confirming a person across a multi-selection", () => {
  it("offers no batch action for a single selected photo", async () => {
    const api = makeApi({ getSelectedPhotos: () => [photo(3)] });
    render(inspectorPanel(api) as React.ReactElement);

    // The per-face confirm is the whole story when one photo is selected.
    await screen.findByRole("button", { name: "✓ confirm" });
    expect(screen.queryByRole("button", { name: /confirm on \d+/ })).toBeNull();
  });

  it("confirms the person on every selected photo, by tag and photo ids", async () => {
    const invoke = vi.fn((command: string) =>
      command === "faces_for_photo"
        ? Promise.resolve([SUGGESTED_ALICE])
        : Promise.resolve({
            photosConfirmed: 3,
            facesConfirmed: 3,
            photosAlreadyConfirmed: 0,
            photosWithoutSuggestion: 0,
          }),
    ) as unknown as ChairPhotoAPI["invoke"];
    const showToast = vi.fn();
    const api = makeApi({
      getSelectedPhotos: () => [photo(1), photo(2), photo(3)],
      invoke,
      showToast,
    });

    render(inspectorPanel(api) as React.ReactElement);
    const button = await batchButton();
    await act(async () => { fireEvent.click(button); });

    // The person tag and the selection — never the face id, which names one photo only.
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("faces_accept_person", {
        photoIds: [1, 2, 3],
        tagId: 42,
      }),
    );
    expect(showToast).toHaveBeenCalledWith(
      "Confirmed Alice on 3 of 3 selected photos.",
    );
  });

  it("includes the panel's own photo even if the selection omits it", async () => {
    const invoke = vi.fn((command: string) =>
      command === "faces_for_photo"
        ? Promise.resolve([SUGGESTED_ALICE])
        : Promise.resolve({
            photosConfirmed: 2,
            facesConfirmed: 2,
            photosAlreadyConfirmed: 0,
            photosWithoutSuggestion: 0,
          }),
    ) as unknown as ChairPhotoAPI["invoke"];
    const api = makeApi({
      // Active photo 3 is missing from the selection list.
      getSelectedPhotos: () => [photo(1), photo(2)],
      getActivePhotoId: () => 3,
      invoke,
    });

    render(inspectorPanel(api) as React.ReactElement);
    const button = await batchButton();
    expect(button.textContent).toBe("✓✓ confirm on 3");

    await act(async () => { fireEvent.click(button); });
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("faces_accept_person", {
        photoIds: [1, 2, 3],
        tagId: 42,
      }),
    );
  });

  it("reports the photos that had nothing to confirm rather than counting them as done", async () => {
    const showToast = vi.fn();
    const api = makeApi({
      getSelectedPhotos: () => [photo(1), photo(2), photo(3)],
      invoke: ((command: string) =>
        command === "faces_for_photo"
          ? Promise.resolve([SUGGESTED_ALICE])
          : Promise.resolve({
              photosConfirmed: 1,
              facesConfirmed: 2,
              photosAlreadyConfirmed: 1,
              photosWithoutSuggestion: 1,
            })) as ChairPhotoAPI["invoke"],
      showToast,
    });

    render(inspectorPanel(api) as React.ReactElement);
    const button = await batchButton();
    await act(async () => { fireEvent.click(button); });

    await waitFor(() =>
      expect(showToast).toHaveBeenCalledWith(
        "Confirmed Alice on 1 of 3 selected photos (2 faces) — " +
          "1 already confirmed, 1 had no suggestion for Alice.",
      ),
    );
  });

  it("says so when the batch confirmed nothing", async () => {
    const showToast = vi.fn();
    const api = makeApi({
      getSelectedPhotos: () => [photo(1), photo(2)],
      getActivePhotoId: () => 1,
      invoke: ((command: string) =>
        command === "faces_for_photo"
          ? Promise.resolve([{ ...SUGGESTED_ALICE, photoId: 1 }])
          : Promise.resolve({
              photosConfirmed: 0,
              facesConfirmed: 0,
              photosAlreadyConfirmed: 0,
              photosWithoutSuggestion: 2,
            })) as ChairPhotoAPI["invoke"],
      showToast,
    });

    render(inspectorPanel(api) as React.ReactElement);
    const button = await batchButton();
    await act(async () => { fireEvent.click(button); });

    await waitFor(() =>
      expect(showToast).toHaveBeenCalledWith(
        "Nothing to confirm on the 2 selected photos — 2 had no suggestion for Alice.",
      ),
    );
  });

  it("keeps the user informed when the batch command fails", async () => {
    const showToast = vi.fn();
    const api = makeApi({
      getSelectedPhotos: () => [photo(1), photo(2), photo(3)],
      invoke: ((command: string) =>
        command === "faces_for_photo"
          ? Promise.resolve([SUGGESTED_ALICE])
          : Promise.reject(new Error("no catalog is open"))) as ChairPhotoAPI["invoke"],
      showToast,
    });

    render(inspectorPanel(api) as React.ReactElement);
    const button = await batchButton();
    await act(async () => { fireEvent.click(button); });

    await waitFor(() =>
      expect(showToast).toHaveBeenCalledWith(
        "Could not confirm Alice on the selected photos.",
      ),
    );
    // The button is usable again — a failed batch must not leave it stuck on "confirming…".
    await waitFor(() => expect(button.disabled).toBe(false));
    expect(button.textContent).toBe("✓✓ confirm on 3");
  });
});
