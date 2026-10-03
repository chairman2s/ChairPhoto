// @vitest-environment jsdom
/**
 * The inspector's single-photo "Geocode location" (#153, from the reviews of #148 and #153).
 *
 * `geocode_to_iptc` answers `{ filled, sidecar, reason }`. A location that reached the
 * catalog but not the sidecar is `filled: true, sidecar: "pending"` — not an error: the
 * status line has no "Error: " prefix, and the host re-reads the catalog, which did change.
 * The outcome is typed, so nothing here depends on how the backend words a message.
 */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import type { ChairPhotoAPI } from "../../registry";
import { GeocodePanelContent, singleGeocodeOutcome } from "../map";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

/** Just the members the panel uses. */
function fakeApi(answer: () => Promise<unknown>) {
  const api = {
    getActivePhotoId: () => 3,
    invoke: vi.fn(answer),
    notifyChange: vi.fn(),
  };
  return api as typeof api & ChairPhotoAPI;
}

describe("single-photo geocode", () => {
  it("shows a pending sidecar without an error prefix and refreshes the catalog", async () => {
    const api = fakeApi(() =>
      Promise.resolve({ filled: true, sidecar: "pending", reason: "sidecar is read-only" }),
    );
    render(<GeocodePanelContent api={api} />);
    fireEvent.click(screen.getByRole("button", { name: "Geocode location" }));
    await screen.findByText(
      "Geocoded location stored in the catalog, but not yet in the sidecar (sidecar is read-only); the repair pass will write it.",
    );
    expect(screen.queryByText(/^Error:/)).toBeNull();
    expect(api.invoke).toHaveBeenCalledWith("geocode_to_iptc", { photoId: 3 });
    expect(api.notifyChange).toHaveBeenCalledTimes(1);
  });

  it("keeps the prefix for a real failure, and refreshes nothing", async () => {
    const api = fakeApi(() => Promise.reject("geocode: HTTP 500"));
    render(<GeocodePanelContent api={api} />);
    fireEvent.click(screen.getByRole("button", { name: "Geocode location" }));
    await screen.findByText("Error: geocode: HTTP 500");
    expect(api.notifyChange).not.toHaveBeenCalled();
  });

  it("refreshes after a fill, not after a no-op", () => {
    expect(singleGeocodeOutcome({ filled: true, sidecar: "written", reason: null })).toEqual({
      status: "Location fields filled.",
      changed: true,
    });
    expect(singleGeocodeOutcome({ filled: false, sidecar: "unchanged", reason: null }).changed).toBe(false);
  });
});
