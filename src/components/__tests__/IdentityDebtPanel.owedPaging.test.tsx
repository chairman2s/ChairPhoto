// @vitest-environment jsdom
/**
 * Review of #153, L1 (its probe P4): after a Retry or Dismiss, the panel re-reads the owed
 * list's CURRENT page, not the page it was on when the button was pressed. A Retry can wait
 * for the sidecar's write turn; the user may page on meanwhile, and the action's re-read must
 * not put page 1's rows back under page 2's label.
 */
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { IdentityDebtPanel } from "../IdentityDebtPanel";
import type { OwedIptc } from "../../modules/api";

const lists: number[] = [];
let releaseRetry: (v: unknown) => void = () => {};

function row(id: number): OwedIptc {
  return {
    photoId: id,
    uuid: `u${id}`,
    path: `p${id}.ARW`,
    fields: ["Title"],
    attempts: 0,
    error: "",
    lastAttemptAt: 0,
    queuedAt: 0,
    generation: 1,
  };
}

vi.mock("@tauri-apps/api/core", async (orig) => {
  const actual = await orig<typeof import("@tauri-apps/api/core")>();
  return {
    ...actual,
    invoke: (command: string, args: Record<string, unknown>) => {
      switch (command) {
        case "summarize_pending_identity":
          return Promise.resolve({ total: 0, conflicts: 0, dismissed: 0, iptcOwed: 101 });
        case "list_owed_iptc": {
          const offset = args.offset as number;
          lists.push(offset);
          const n = offset === 0 ? 100 : 1;
          return Promise.resolve(Array.from({ length: n }, (_, i) => row(offset + i + 1)));
        }
        case "retry_owed_iptc":
          return new Promise((r) => (releaseRetry = r));
        case "list_pending_identity":
        case "list_volumes":
          return Promise.resolve([]);
        default:
          return Promise.resolve(null);
      }
    },
  };
});

afterEach(cleanup);

describe("an owed-IPTC action and paging", () => {
  it("a page change while a Retry waits is kept when the Retry's re-read lands", async () => {
    render(<IdentityDebtPanel onClose={() => {}} />);
    fireEvent.click(within(await screen.findByTestId("owed-row-1", {}, { timeout: 10_000 })).getByRole("button", { name: "Retry" }));
    fireEvent.click(screen.getByRole("button", { name: "Next →" }));
    await screen.findByTestId("owed-row-101", {}, { timeout: 10_000 });

    await act(async () => releaseRetry({ sidecar: "written", reason: null }));
    // The answer and the action's re-read are issued in one step: once the answer shows,
    // the re-read has been sent, and its page lands right after.
    await screen.findByText("Written to the sidecar.", {}, { timeout: 10_000 });
    await act(async () => {
      await new Promise((r) => setTimeout(r, 50));
    });
    expect(lists[lists.length - 1]).toBe(100);
    expect(screen.queryByTestId("owed-row-101")).toBeTruthy();
    expect(screen.queryByTestId("owed-row-1")).toBeNull();
    expect(screen.getByText(/^Showing 101–101/)).toBeTruthy();
  }, 30_000); // two 100-row renders in jsdom: slow under the full suite's load
});
