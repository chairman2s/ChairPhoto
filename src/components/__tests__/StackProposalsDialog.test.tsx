// @vitest-environment jsdom
/**
 * A proposal collapses frames out of the grid, so what is worth pinning is the review
 * contract, not the layout:
 *
 *  - proposing never stacks. Opening the dialog must not send an apply.
 *  - accepting sends the *chosen* keeper, including after the reviewer overrides it, and
 *    sends every member — the backend drops the keeper from its own member list.
 *  - each group is accepted on its own; accepting one leaves the rest untouched, and a
 *    skipped group is never sent at all.
 *  - the consequences of accepting are on screen before the button that causes them:
 *    stacks that will be re-homed onto the keeper, and frames the rule could not weigh.
 *  - what the pass left out (already-stacked photos, groups past the cap) is stated. A
 *    silent omission reads as "there was nothing else".
 *  - the engine's stated reason is withdrawn once the reviewer picks a different keeper —
 *    it explains the engine's choice, not theirs.
 */
import { describe, expect, it, vi, beforeEach } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";

import { StackProposalsDialog } from "../StackProposalsDialog";
import type { ProposalFrame, StackProposal, StackProposals } from "../../modules/api";

const calls: { command: string; args: Record<string, unknown> }[] = [];
let proposals: () => Promise<unknown> = () => Promise.resolve(result());
/** What `apply_stack_proposal` should do next; replaced per test. */
let applyBehavior: () => Promise<unknown> = () => Promise.resolve({ stacked: 1, absorbed: 0 });

vi.mock("@tauri-apps/api/core", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@tauri-apps/api/core")>();
  return {
    ...actual,
    convertFileSrc: (id: string) => `thumb://${id}`,
    invoke: (command: string, args: Record<string, unknown>) => {
      calls.push({ command, args: args ?? {} });
      if (command === "propose_stacks") return proposals();
      if (command === "apply_stack_proposal") return applyBehavior();
      return Promise.resolve(null);
    },
  };
});

function frame(over: Partial<ProposalFrame> = {}): ProposalFrame {
  return {
    photoId: 1,
    fileName: "DSC_0001.NEF",
    sharpness: 100,
    rating: 0,
    burstFlag: null,
    hammingDistance: 0,
    childCount: 0,
    isKeeper: false,
    ...over,
  };
}

function proposal(over: Partial<StackProposal> = {}): StackProposal {
  return {
    keeperId: 2,
    reason: "sharpest frame (300)",
    spanSecs: 4,
    maxDistance: 3,
    unscored: 0,
    absorbedChildren: 0,
    members: [
      frame({ photoId: 1, fileName: "DSC_0001.NEF", sharpness: 100 }),
      frame({ photoId: 2, fileName: "DSC_0002.NEF", sharpness: 300, isKeeper: true }),
      frame({ photoId: 3, fileName: "DSC_0003.NEF", sharpness: 90 }),
    ],
    ...over,
  };
}

function result(over: Partial<StackProposals> = {}): StackProposals {
  return {
    proposals: [proposal()],
    considered: 12,
    skippedStacked: 0,
    truncated: false,
    timeGapSecs: 15,
    hammingThreshold: 10,
    ...over,
  };
}

const text = () => (document.body.textContent ?? "").replace(/\s+/g, " ");
const applies = () => calls.filter((c) => c.command === "apply_stack_proposal");

beforeEach(() => {
  calls.length = 0;
  proposals = () => Promise.resolve(result());
  applyBehavior = () => Promise.resolve({ stacked: 1, absorbed: 0 });
});

describe("StackProposalsDialog", () => {
  it("proposes without stacking anything", async () => {
    render(<StackProposalsDialog photoIds={[1, 2, 3]} onClose={() => {}} onApplied={() => {}} />);

    await waitFor(() => expect(screen.getByText(/3 frames/)).toBeTruthy());
    expect(calls.map((c) => c.command)).toEqual(["propose_stacks"]);
    expect(calls[0].args).toEqual({ photoIds: [1, 2, 3] });
  });

  it("stacks the group under the engine's keeper when it is not overridden", async () => {
    const onApplied = vi.fn();
    render(<StackProposalsDialog photoIds={[1, 2, 3]} onClose={() => {}} onApplied={onApplied} />);

    await waitFor(() => expect(screen.getByText(/Stack 2 under this/)).toBeTruthy());
    fireEvent.click(screen.getByText(/Stack 2 under this/));

    await waitFor(() => expect(applies().length).toBe(1));
    expect(applies()[0].args).toEqual({ keeperId: 2, memberIds: [1, 2, 3] });
    expect(onApplied).toHaveBeenCalled();
  });

  it("sends the keeper the reviewer picked, not the one proposed", async () => {
    render(<StackProposalsDialog photoIds={[1]} onClose={() => {}} onApplied={() => {}} />);

    await waitFor(() => expect(screen.getByTitle(/DSC_0003.NEF — click to keep/)).toBeTruthy());
    fireEvent.click(screen.getByTitle(/DSC_0003.NEF — click to keep/));
    fireEvent.click(screen.getByText(/Stack 2 under this/));

    await waitFor(() => expect(applies().length).toBe(1));
    expect(applies()[0].args).toEqual({ keeperId: 3, memberIds: [1, 2, 3] });
  });

  it("withdraws the engine's reason once a different keeper is chosen", async () => {
    render(<StackProposalsDialog photoIds={[1]} onClose={() => {}} onApplied={() => {}} />);

    await waitFor(() => expect(text()).toContain("keeper: sharpest frame (300)"));
    fireEvent.click(screen.getByTitle(/DSC_0001.NEF — click to keep/));

    expect(text()).toContain("keeper chosen by hand");
    expect(text()).not.toContain("sharpest frame (300)");
  });

  it("accepts one group at a time and never sends a skipped one", async () => {
    proposals = () =>
      Promise.resolve(
        result({
          proposals: [
            proposal({ keeperId: 2 }),
            proposal({
              keeperId: 5,
              members: [
                frame({ photoId: 4, fileName: "DSC_0004.NEF" }),
                frame({ photoId: 5, fileName: "DSC_0005.NEF", isKeeper: true }),
              ],
            }),
          ],
        }),
      );
    render(<StackProposalsDialog photoIds={[1]} onClose={() => {}} onApplied={() => {}} />);

    await waitFor(() => expect(screen.getAllByText(/Skip/).length).toBe(2));
    // Skip the first group, accept the second.
    fireEvent.click(screen.getAllByText(/Skip/)[0]);
    await waitFor(() => expect(screen.getAllByText(/Skip/).length).toBe(1));
    fireEvent.click(screen.getByText(/Stack 1 under this/));

    await waitFor(() => expect(applies().length).toBe(1));
    expect(applies()[0].args).toEqual({ keeperId: 5, memberIds: [4, 5] });
  });

  it("removes an accepted group from the list and counts it", async () => {
    render(<StackProposalsDialog photoIds={[1]} onClose={() => {}} onApplied={() => {}} />);

    await waitFor(() => expect(screen.getByText(/Stack 2 under this/)).toBeTruthy());
    fireEvent.click(screen.getByText(/Stack 2 under this/));

    await waitFor(() => expect(text()).toContain("Stacked 1 group this session"));
    expect(screen.queryByText(/Stack 2 under this/)).toBeNull();
  });

  it("shows what accepting will move before the button that moves it", async () => {
    proposals = () =>
      Promise.resolve(result({ proposals: [proposal({ absorbedChildren: 2, unscored: 1 })] }));
    render(<StackProposalsDialog photoIds={[1]} onClose={() => {}} onApplied={() => {}} />);

    await waitFor(() => expect(text()).toMatch(/will move onto the keeper/));
    expect(text()).toContain("2 photos stacked under these frames");
    expect(text()).toMatch(/1 frame has no sharpness score/);
    expect(text()).toMatch(/not weighed in picking the keeper/);
  });

  it("states what the pass left out", async () => {
    proposals = () => Promise.resolve(result({ skippedStacked: 4, truncated: true }));
    render(<StackProposalsDialog photoIds={[1]} onClose={() => {}} onApplied={() => {}} />);

    await waitFor(() => expect(text()).toMatch(/already-stacked photos were left out/));
    expect(text()).toMatch(/run again after these/i);
  });

  it("says plainly when there is nothing to group", async () => {
    proposals = () => Promise.resolve(result({ proposals: [], considered: 40 }));
    render(<StackProposalsDialog photoIds={[1]} onClose={() => {}} onApplied={() => {}} />);

    await waitFor(() => expect(text()).toMatch(/No groups in 40 photos/));
    // And no promise about stacking, since nothing would be stacked.
    expect(text()).not.toContain("Nothing is deleted.");
  });

  it("surfaces a refused apply instead of pretending the group collapsed", async () => {
    // The backend refuses a keeper that is itself stacked under another photo.
    applyBehavior = () => Promise.reject("the keeper is itself stacked under another photo");
    const onApplied = vi.fn();
    render(<StackProposalsDialog photoIds={[1]} onClose={() => {}} onApplied={onApplied} />);

    await waitFor(() => expect(screen.getByText(/Stack 2 under this/)).toBeTruthy());
    fireEvent.click(screen.getByText(/Stack 2 under this/));

    await waitFor(() => expect(text()).toContain("the keeper is itself stacked"));
    expect(onApplied).not.toHaveBeenCalled();
    // The group is still listed and still acceptable: it did not collapse.
    expect(screen.getByText(/Stack 2 under this/)).toBeTruthy();
    expect(text()).not.toContain("Stacked 1 group this session");
  });
});
