// @vitest-environment jsdom
/**
 * Component tests for the owed-IPTC list in the identity-debt panel (#153).
 *
 * #148 records IPTC a sidecar is still owed, and the panel only counted it: a photo on
 * deliberately read-only media owed forever, with no way to see which photo owed what or to
 * stop owing it. What has to be true here: each owing photo is listed with its fields and
 * last error; Dismiss and Retry reach the backend for THAT photo, with the UUID (and, for
 * Dismiss, the generation) the row was read with — the guard against an id that names
 * another photo now, or a debt a newer save added; the answer is reported as the backend
 * gave it; and the counts are re-read, the panel's and the host's badge.
 *
 * Only the Tauri `invoke` boundary is mocked, routed by command name, so these run through
 * the real `modules/api.ts` wrappers and pin the command names and argument shapes.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";

import { IdentityDebtPanel, owedActionMessage } from "../IdentityDebtPanel";
import type { OwedIptc } from "../../modules/api";

let calls: Array<{ command: string; args: Record<string, unknown> }> = [];
let owed: OwedIptc[] = [];
let summary = { total: 0, conflicts: 0, dismissed: 0, iptcOwed: 2 };
let dismissAnswer: boolean | Error = true;
let retryAnswer: unknown = { sidecar: "written", reason: null };

vi.mock("@tauri-apps/api/core", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@tauri-apps/api/core")>();
  return {
    ...actual,
    invoke: (command: string, args: Record<string, unknown>) => {
      calls.push({ command, args: args ?? {} });
      switch (command) {
        case "summarize_pending_identity":
          return Promise.resolve(summary);
        case "list_pending_identity":
          return Promise.resolve([]);
        case "list_owed_iptc":
          return Promise.resolve(owed);
        case "list_volumes":
          return Promise.resolve([]);
        case "dismiss_owed_iptc":
          return dismissAnswer instanceof Error ? Promise.reject(dismissAnswer.message) : Promise.resolve(dismissAnswer);
        case "retry_owed_iptc":
          return Promise.resolve(retryAnswer);
        default:
          return Promise.resolve(null);
      }
    },
  };
});

function row(over: Partial<OwedIptc> = {}): OwedIptc {
  return {
    photoId: 7,
    uuid: "uuid-7",
    path: "2026/03/DSC7.ARW",
    fields: ["Title", "City"],
    attempts: 2,
    error: "sidecar is read-only",
    lastAttemptAt: 1_700_000_000,
    queuedAt: 1_699_000_000,
    generation: 4,
    ...over,
  };
}

beforeEach(() => {
  calls = [];
  owed = [row(), row({ photoId: 9, uuid: "uuid-9", path: "2026/03/DSC9.ARW", fields: ["Creator"], error: "", attempts: 0, generation: 1 })];
  summary = { total: 0, conflicts: 0, dismissed: 0, iptcOwed: 2 };
  dismissAnswer = true;
  retryAnswer = { sidecar: "written", reason: null };
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

const sent = (command: string) => calls.filter((c) => c.command === command);

describe("the owed-IPTC list", () => {
  it("lists each owing photo with its fields and last error", async () => {
    render(<IdentityDebtPanel onClose={() => {}} />);
    const first = await screen.findByTestId("owed-row-7");
    expect(within(first).getByText("2026/03/DSC7.ARW")).toBeTruthy();
    expect(within(first).getByText("Title, City")).toBeTruthy();
    expect(within(first).getByText("sidecar is read-only")).toBeTruthy();
    expect(within(screen.getByTestId("owed-row-9")).getByText("Creator")).toBeTruthy();
    expect(sent("list_owed_iptc")[0].args).toEqual({ limit: 100, offset: 0 });
  });

  it("is not shown when nothing is owed", async () => {
    owed = [];
    summary = { ...summary, iptcOwed: 0 };
    render(<IdentityDebtPanel onClose={() => {}} />);
    await waitFor(() => expect(sent("list_owed_iptc").length).toBe(1));
    await screen.findByText(/No identity debt/);
    expect(screen.queryByTestId("owed-iptc")).toBeNull();
  });

  it("Dismiss sends the row's photo, UUID and generation, then re-reads the counts", async () => {
    const onCountsChanged = vi.fn();
    render(<IdentityDebtPanel onClose={() => {}} onCountsChanged={onCountsChanged} />);
    const target = await screen.findByTestId("owed-row-9");
    const summariesBefore = sent("summarize_pending_identity").length;
    const listsBefore = sent("list_owed_iptc").length;
    owed = [owed[0]];
    summary = { ...summary, iptcOwed: 1 };
    fireEvent.click(within(target).getByRole("button", { name: "Dismiss" }));

    await screen.findByText(/^Dismissed\./);
    expect(sent("dismiss_owed_iptc")).toEqual([
      { command: "dismiss_owed_iptc", args: { photoId: 9, uuid: "uuid-9", generation: 1 } },
    ]);
    expect(sent("retry_owed_iptc")).toEqual([]);
    expect(sent("summarize_pending_identity").length).toBeGreaterThan(summariesBefore);
    expect(sent("list_owed_iptc").length).toBeGreaterThan(listsBefore);
    expect(onCountsChanged).toHaveBeenCalledTimes(1);
    await waitFor(() => expect(screen.queryByTestId("owed-row-9")).toBeNull());
  });

  it("a Dismiss the backend refused (a newer save owes) says so and is not reported as done", async () => {
    dismissAnswer = false;
    render(<IdentityDebtPanel onClose={() => {}} />);
    fireEvent.click(within(await screen.findByTestId("owed-row-7")).getByRole("button", { name: "Dismiss" }));
    await screen.findByText(/^Not dismissed: this photo's IPTC changed/);
    expect(screen.queryByText(/^Dismissed\./)).toBeNull();
  });

  it("Retry sends the row's photo and UUID and reports what the backend answered", async () => {
    const onCountsChanged = vi.fn();
    retryAnswer = { sidecar: "pending", reason: "no reachable copy of photo 7" };
    render(<IdentityDebtPanel onClose={() => {}} onCountsChanged={onCountsChanged} />);
    fireEvent.click(within(await screen.findByTestId("owed-row-7")).getByRole("button", { name: "Retry" }));

    await screen.findByText("Still pending (no reachable copy of photo 7).");
    expect(sent("retry_owed_iptc")).toEqual([{ command: "retry_owed_iptc", args: { photoId: 7, uuid: "uuid-7" } }]);
    expect(sent("dismiss_owed_iptc")).toEqual([]);
    expect(onCountsChanged).toHaveBeenCalledTimes(1);
  });

  it("a refusal is shown verbatim and changes no count", async () => {
    const onCountsChanged = vi.fn();
    dismissAnswer = new Error("This photo is no longer in the catalog");
    render(<IdentityDebtPanel onClose={() => {}} onCountsChanged={onCountsChanged} />);
    fireEvent.click(within(await screen.findByTestId("owed-row-7")).getByRole("button", { name: "Dismiss" }));
    await screen.findByText("This photo is no longer in the catalog");
    expect(onCountsChanged).not.toHaveBeenCalled();
  });
});

describe("owedActionMessage", () => {
  it("states each answer from the backend's result", () => {
    expect(owedActionMessage({ dismissed: true })).toMatch(/^Dismissed\./);
    expect(owedActionMessage({ dismissed: false })).toMatch(/^Not dismissed/);
    expect(owedActionMessage({ retried: { sidecar: "written", reason: null } })).toBe("Written to the sidecar.");
    expect(owedActionMessage({ retried: { sidecar: "unchanged", reason: null } })).toMatch(/^Nothing was owed/);
    expect(owedActionMessage({ retried: { sidecar: "pending", reason: null } })).toBe("Still pending.");
    expect(owedActionMessage({ retried: null })).toBe("Still pending.");
  });
});
