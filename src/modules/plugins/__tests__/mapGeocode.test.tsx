// @vitest-environment jsdom
/**
 * The inspector's single-photo "Geocode location" (#153, from the review of #148).
 *
 * A geocode whose location reached the catalog but not the sidecar comes back from
 * `geocode_to_iptc` as an error, worded by the backend ("Geocoded location stored in the
 * catalog, but not yet in the sidecar …"). It is not a failure: the status line shows it
 * without an "Error: " prefix, and the host re-reads the catalog, which did change.
 */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import type { ChairPhotoAPI } from "../../registry";
import { GEOCODE_PENDING_LEAD, GeocodePanelContent, singleGeocodeOutcome } from "../map";

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

const PENDING = `${GEOCODE_PENDING_LEAD} (sidecar is read-only); the repair pass will write it`;

describe("single-photo geocode", () => {
  it("shows a pending sidecar without an error prefix and refreshes the catalog", async () => {
    const api = fakeApi(() => Promise.reject(PENDING));
    render(<GeocodePanelContent api={api} />);
    fireEvent.click(screen.getByRole("button", { name: "Geocode location" }));
    await screen.findByText(PENDING);
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
    expect(singleGeocodeOutcome({ filled: true })).toEqual({ status: "Location fields filled.", changed: true });
    expect(singleGeocodeOutcome({ filled: false }).changed).toBe(false);
  });
});
