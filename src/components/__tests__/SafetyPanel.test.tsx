// @vitest-environment jsdom
/**
 * B1's value is not the counts, it is that the counts are honest. So what is pinned here
 * is the disclosure, not the layout:
 *
 *  - a count you can act on offers the action, and "show me" filters to *the same
 *    predicate* the count used — a panel that says 13 and then shows you a different set
 *    is worse than either number alone.
 *  - `stale` is presented as a floor while sidecars remain unchecked, because freshness is
 *    only ever true as of the last scan.
 *  - the panel never claims to see more than the catalog does. Redundancy inside a device
 *    and any off-site backup are invisible to it, and it says so — that caveat is what
 *    makes "safe" an honest word here rather than a false green light.
 *  - zero is stated as a fact, not hidden: "every photo has a copy at home" is the
 *    sentence the panel exists to be able to say.
 */
import { describe, expect, it, vi, beforeEach } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";

import { SafetySection } from "../SafetyPanel";
import type { SafetySummary } from "../../modules/api";

const calls: { command: string; args: Record<string, unknown> }[] = [];
let respond: () => Promise<unknown> = () => Promise.resolve(summary());

vi.mock("@tauri-apps/api/core", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@tauri-apps/api/core")>();
  return {
    ...actual,
    invoke: (command: string, args: Record<string, unknown>) => {
      calls.push({ command, args: args ?? {} });
      if (command === "library_safety_summary") return respond();
      return Promise.resolve(null);
    },
  };
});

function summary(over: Partial<SafetySummary> = {}): SafetySummary {
  return {
    missing: 0,
    atRisk: 13,
    unverified: 164_185,
    stale: 0,
    safe: 895,
    oldestAtRisk: Math.floor(Date.now() / 1000) - 90 * 86_400,
    companionsChecked: 0,
    companionsUnchecked: 0,
    ...over,
  };
}

const text = () => (document.body.textContent ?? "").replace(/\s+/g, " ");

beforeEach(() => {
  calls.length = 0;
  respond = () => Promise.resolve(summary());
});

describe("SafetySection", () => {
  it("shows each bucket with its count", async () => {
    render(<SafetySection />);

    await waitFor(() => expect(text()).toContain("At risk"));
    expect(text()).toContain("13");
    expect(text()).toContain("164,185");
    expect(text()).toContain("895");
  });

  it("says how long the oldest at-risk photo has waited, not just how many", async () => {
    render(<SafetySection />);

    await waitFor(() => expect(text()).toMatch(/waiting 3 months/));
  });

  it("filters to the same bucket the count came from", async () => {
    const onShowTier = vi.fn();
    render(<SafetySection onShowTier={onShowTier} />);

    await waitFor(() => expect(screen.getAllByText("Show me").length).toBeGreaterThan(0));
    fireEvent.click(screen.getAllByText("Show me")[0]);

    expect(onShowTier).toHaveBeenCalledWith("atRisk");
  });

  it("offers no action on a bucket that is already zero", async () => {
    respond = () => Promise.resolve(summary({ atRisk: 0, stale: 0 }));
    render(<SafetySection onShowTier={vi.fn()} />);

    await waitFor(() => expect(text()).toMatch(/Every photo has a copy at home/));
    expect(screen.queryByText("Show me")).toBeNull();
  });

  it("presents stale as a floor while sidecars are unchecked", async () => {
    respond = () => Promise.resolve(summary({ stale: 4, companionsUnchecked: 900 }));
    render(<SafetySection />);

    await waitFor(() => expect(text()).toMatch(/as of the last scan/));
    expect(text()).toContain("900 carried sidecars have not been looked at");
    expect(text()).toMatch(/a floor rather than a total/);
  });

  it("drops the floor caveat once every sidecar has been checked", async () => {
    respond = () =>
      Promise.resolve(summary({ stale: 4, companionsChecked: 900, companionsUnchecked: 0 }));
    render(<SafetySection />);

    await waitFor(() => expect(text()).toContain("Edits not carried home"));
    expect(text()).not.toMatch(/a floor rather than a total/);
  });

  it("never claims to see more than the catalog can", async () => {
    // Home is a redundant array that is itself backed up off-site, and neither fact is
    // visible here. Without this sentence, "safe" would be a stronger word than earned.
    render(<SafetySection />);

    await waitFor(() => expect(text()).toMatch(/volumes ChairPhoto can see/));
    expect(text()).toMatch(/off-site backup, are invisible/);
    expect(text()).toMatch(/as far as this catalog knows/);
  });

  it("says plainly when there is nothing in the catalog", async () => {
    respond = () =>
      Promise.resolve(
        summary({ atRisk: 0, unverified: 0, safe: 0, stale: 0, missing: 0, oldestAtRisk: null }),
      );
    render(<SafetySection />);

    await waitFor(() => expect(text()).toMatch(/No photos in the catalog yet/));
  });

  it("hides the no-copy-anywhere row when it is empty, since it is not a normal state", async () => {
    render(<SafetySection />);
    await waitFor(() => expect(text()).toContain("At risk"));
    expect(text()).not.toContain("No copy anywhere");

    respond = () => Promise.resolve(summary({ missing: 2 }));
    render(<SafetySection />);
    await waitFor(() => expect(text()).toContain("No copy anywhere"));
  });

  it("surfaces a failure instead of rendering zeroes that look like good news", async () => {
    respond = () => Promise.reject("catalog is closed");
    render(<SafetySection />);

    await waitFor(() => expect(text()).toContain("catalog is closed"));
    expect(text()).not.toContain("At risk");
  });
});
