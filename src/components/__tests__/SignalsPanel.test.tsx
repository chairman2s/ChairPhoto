// @vitest-environment jsdom
/**
 * The point of C6 is that a badge stops being an assertion you have to trust, so what is
 * worth pinning is the *disclosure*, not the layout:
 *
 *  - the numbers a flag was actually derived from reach the screen — median, cutoff, rank,
 *    and the frame that won — rather than a sentence restating the rule.
 *  - a stored flag the recomputation no longer reaches is called out as stale. Showing only
 *    the fresh verdict would silently contradict the badge still on the tile, and quietly
 *    swapping in the stored one would hide that the analysis is out of date.
 *  - a burst too long to be seen whole says so, so a rank is never read as final.
 *  - an unhashed frame shows no distance. A `0` there would read as "identical to this
 *    photo", which is the opposite of what an absent hash means.
 *  - the panel does not fetch for the previous photo after the id changes.
 */
import { describe, expect, it, vi, beforeEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";

import { SignalsPanel } from "../SignalsPanel";
import type { BurstSignal, ClusterFrame, PhotoSignals } from "../../modules/api";

const calls: { command: string; args: Record<string, unknown> }[] = [];
let respond: (args: Record<string, unknown>) => Promise<unknown> = () =>
  Promise.resolve(signals());

vi.mock("@tauri-apps/api/core", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@tauri-apps/api/core")>();
  return {
    ...actual,
    invoke: (command: string, args: Record<string, unknown>) => {
      calls.push({ command, args: args ?? {} });
      if (command === "explain_photo_signals") return respond(args ?? {});
      return Promise.resolve(null);
    },
  };
});

function frame(over: Partial<ClusterFrame> = {}): ClusterFrame {
  return {
    photoId: 1,
    fileName: "DSC_0001.NEF",
    sharpness: 100,
    rating: 0,
    verdict: null,
    hammingDistance: 0,
    isSubject: false,
    ...over,
  };
}

function burst(over: Partial<BurstSignal> = {}): BurstSignal {
  return {
    clusterSize: 3,
    timeGroupSize: 3,
    scored: 3,
    rank: 3,
    median: 100,
    cutoff: 60,
    softFraction: 0.6,
    verdict: "soft-in-burst",
    storedFlag: "soft-in-burst",
    stale: false,
    truncated: false,
    timeGapSecs: 15,
    hammingThreshold: 10,
    best: frame({ photoId: 2, fileName: "DSC_0002.NEF", sharpness: 140 }),
    frames: [
      frame({ photoId: 1, fileName: "DSC_0001.NEF", sharpness: 100 }),
      frame({
        photoId: 2,
        fileName: "DSC_0002.NEF",
        sharpness: 140,
        verdict: "sharpest-of-burst",
      }),
      frame({
        photoId: 3,
        fileName: "DSC_0003.NEF",
        sharpness: 10,
        verdict: "soft-in-burst",
        isSubject: true,
      }),
    ],
    ...over,
  };
}

function signals(over: Partial<PhotoSignals> = {}): PhotoSignals {
  return {
    photoId: 3,
    sharpness: { score: 10, method: "tile", softThreshold: 15, belowThreshold: true },
    burst: burst(),
    stack: { childCount: 0, parentId: null },
    versionCount: 0,
    ...over,
  };
}

/** The whole panel's text, whitespace-collapsed, for phrase assertions. */
const panelText = () => (document.body.textContent ?? "").replace(/\s+/g, " ");

beforeEach(() => {
  calls.length = 0;
  respond = () => Promise.resolve(signals());
});

describe("SignalsPanel", () => {
  it("shows the numbers the flag was derived from, not a restatement of the rule", async () => {
    render(<SignalsPanel photoId={3} />);

    await waitFor(() => expect(screen.getByText(/Cluster median/)).toBeTruthy());
    const text = panelText();

    expect(text).toContain("Frame 3 of 3");
    expect(text).toMatch(/Cluster median 100/);
    expect(text).toMatch(/soft below 60/);
    expect(text).toContain("60% of the median");
    expect(text).toContain("sharpest is DSC_0002.NEF");
    // The absolute reading, against the library threshold rather than the cluster.
    expect(text).toMatch(/10\.0 against a threshold of 15\.0/);
  });

  it("uses the configured fraction rather than a hardcoded 60%", async () => {
    respond = () =>
      Promise.resolve(signals({ burst: burst({ softFraction: 0.45, cutoff: 45 }) }));
    render(<SignalsPanel photoId={3} />);

    await waitFor(() => expect(panelText()).toContain("45% of the median"));
    expect(panelText()).not.toContain("60% of the median");
  });

  it("calls out a stored flag the recomputation no longer reaches", async () => {
    respond = () =>
      Promise.resolve(
        signals({
          burst: burst({ storedFlag: "soft-in-burst", verdict: null, stale: true }),
        }),
      );
    render(<SignalsPanel photoId={3} />);

    await waitFor(() => expect(panelText()).toMatch(/badge on this photo says/));
    const text = panelText();
    expect(text).toContain("Soft in burst");
    expect(text).toContain("no flag");
    expect(text).toMatch(/re-run burst analysis/i);
  });

  it("says when the burst was too long to see whole", async () => {
    respond = () => Promise.resolve(signals({ burst: burst({ truncated: true }) }));
    render(<SignalsPanel photoId={3} />);

    await waitFor(() => expect(panelText()).toMatch(/lower bounds/));
    // The count is marked as a floor in the line itself, not only in the warning.
    expect(panelText()).toContain("burst of 3+");
  });

  it("leaves an unhashed frame's distance blank rather than showing it as identical", async () => {
    respond = () =>
      Promise.resolve(
        signals({
          burst: burst({
            frames: [
              frame({ photoId: 1, hammingDistance: null }),
              frame({ photoId: 3, fileName: "DSC_0003.NEF", isSubject: true }),
            ],
          }),
        }),
      );
    render(<SignalsPanel photoId={3} />);

    await waitFor(() => expect(screen.getByText("DSC_0003.NEF")).toBeTruthy());
    const row = screen.getByText("DSC_0001.NEF").closest("tr")!;
    const cells = Array.from(row.querySelectorAll("td")).map((c) => c.textContent);
    expect(cells).toContain("—");
    expect(cells).not.toContain("0");
  });

  it("reports a photo that belongs to no burst instead of an empty cluster", async () => {
    respond = () =>
      Promise.resolve(
        signals({
          burst: burst({
            clusterSize: 1,
            timeGroupSize: 1,
            scored: 1,
            rank: 1,
            verdict: null,
            storedFlag: null,
            best: null,
            frames: [frame({ photoId: 3, isSubject: true })],
          }),
        }),
      );
    render(<SignalsPanel photoId={3} />);

    await waitFor(() => expect(panelText()).toMatch(/Not part of a burst/));
    expect(panelText()).toContain("no frame within 15s");
  });

  it("says nothing has been measured rather than rendering empty sections", async () => {
    respond = () =>
      Promise.resolve(
        signals({ sharpness: null, burst: null, versionCount: 0, stack: { childCount: 0, parentId: null } }),
      );
    render(<SignalsPanel photoId={7} />);

    await waitFor(() => expect(panelText()).toMatch(/has not been scored, hashed or stacked/));
  });

  it("asks the backend once, for the photo it was given", async () => {
    render(<SignalsPanel photoId={42} />);

    await waitFor(() => expect(calls.length).toBe(1));
    expect(calls[0]).toEqual({
      command: "explain_photo_signals",
      args: { photoId: 42 },
    });
  });

  it("does not render a slow answer that arrives after the photo changed", async () => {
    let resolveFirst: (v: unknown) => void = () => {};
    respond = (args) =>
      args.photoId === 1
        ? new Promise((r) => {
            resolveFirst = r;
          })
        : Promise.resolve(signals({ photoId: 2, burst: burst({ clusterSize: 9 }) }));

    const { rerender } = render(<SignalsPanel photoId={1} />);
    rerender(<SignalsPanel photoId={2} />);
    await waitFor(() => expect(panelText()).toContain("burst of 9"));

    // The first photo's answer lands late, carrying a burst of 3.
    resolveFirst(signals({ photoId: 1, burst: burst({ clusterSize: 3 }) }));
    await new Promise((r) => setTimeout(r, 0));

    expect(panelText()).toContain("burst of 9");
    expect(panelText()).not.toContain("burst of 3");
  });
});
