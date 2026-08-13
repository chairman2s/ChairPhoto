// @vitest-environment jsdom
/**
 * Component tests for resolving an identity conflict from the panel (issue #33).
 *
 * The point of #33 is that a conflict had nowhere to go: the backend recorded it and no
 * user action existed. A backend-only fix would leave that complaint standing, so what has
 * to be true here is that the three decisions are REACHABLE, that each sends the action the
 * user actually chose, and that a refusal is shown rather than swallowed.
 *
 * Only the Tauri `invoke` boundary is mocked, routed by command name (the pattern from
 * `src/modules/plugins/__tests__/statistics.test.tsx`) — so these tests run through the real
 * `modules/api.ts` wrappers and therefore also pin the command name and the argument shape
 * the backend receives, not just the component's internal calls.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { IdentityDebtPanel } from "../IdentityDebtPanel";

/** Every invoke the panel made, in order — the assertion surface for "what reached the
 *  backend". Reset per test. */
let calls: Array<{ command: string; args: Record<string, unknown> }> = [];
/** What `resolve_identity_conflict` should do next: resolve with an outcome, or reject. */
let resolveBehavior: (args: Record<string, unknown>) => Promise<unknown> = async () => outcome();
/** The page the panel is currently served. */
let page: unknown[] = [];
let summary = { total: 1, conflicts: 1, dismissed: 0 };

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
          return Promise.resolve(page);
        case "list_volumes":
          return Promise.resolve([{ id: 1, name: "Library", basePath: "/photos", kind: "local" }]);
        case "resolve_identity_conflict":
          return resolveBehavior(args ?? {});
        case "repair_pending_identity":
          return Promise.resolve({ bound: 0, unreachable: 0, conflicts: 1, failed: 0 });
        default:
          return Promise.resolve(null);
      }
    },
  };
});

function outcome(over: Record<string, unknown> = {}) {
  return {
    action: "adopt",
    photoId: 7,
    catalogUuid: "uuid-from-the-file",
    previousSidecarUuid: "uuid-from-the-file",
    recheckedCopies: 0,
    sidecarBackup: null,
    ...over,
  };
}

function conflictedCopy(over: Record<string, unknown> = {}) {
  return {
    photoId: 7,
    path: "2026/03/DSC1.ARW",
    volumeId: 1,
    relativePath: "2026/03/DSC1.ARW",
    fields: [
      {
        field: "identifier",
        state: "conflict",
        attempts: 3,
        error: "sidecar carries a different identity (uuid-from-the-file); left untouched",
        lastAttemptAt: 1_700_000_000,
        dismissedAt: 0,
      },
    ],
    ...over,
  };
}

/** jsdom lays nothing out, so every element is 0×0. The panel's list is virtualized, and
 *  `@tanstack/virtual-core` measures the viewport with `offsetWidth`/`offsetHeight` (not
 *  `getBoundingClientRect`) — a 0px-high viewport yields zero virtual items, so without
 *  this every assertion below would be about an empty list rather than about the panel.
 *  Give the viewport a size; nothing else in the component depends on layout. */
function giveEveryElementASize() {
  for (const [prop, value] of [
    ["offsetHeight", 600],
    ["offsetWidth", 900],
  ] as const) {
    Object.defineProperty(HTMLElement.prototype, prop, { configurable: true, get: () => value });
  }
  return () => {
    delete (HTMLElement.prototype as unknown as Record<string, unknown>).offsetHeight;
    delete (HTMLElement.prototype as unknown as Record<string, unknown>).offsetWidth;
  };
}

let restoreSizes = () => {};

beforeEach(() => {
  calls = [];
  page = [conflictedCopy()];
  summary = { total: 1, conflicts: 1, dismissed: 0 };
  resolveBehavior = async () => outcome();
  restoreSizes = giveEveryElementASize();
});

afterEach(() => {
  cleanup();
  restoreSizes();
  vi.restoreAllMocks();
});

/** The last `resolve_identity_conflict` payload, or undefined if none was sent. */
function lastResolve() {
  const sent = calls.filter((c) => c.command === "resolve_identity_conflict");
  return sent.length > 0 ? sent[sent.length - 1].args : undefined;
}

describe("resolving a conflict from the panel", () => {
  it("offers exactly the three decisions for a conflicted copy — no default among them", async () => {
    render(<IdentityDebtPanel onClose={() => {}} />);
    await screen.findByRole("button", { name: "Adopt" });

    expect(screen.getByRole("button", { name: "Overwrite…" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Dismiss" })).toBeTruthy();
    // Nothing has been decided just by looking at the row.
    expect(lastResolve()).toBeUndefined();
  });

  it("sends the action the user picked, for the copy they picked it on", async () => {
    render(<IdentityDebtPanel onClose={() => {}} />);
    fireEvent.click(await screen.findByRole("button", { name: "Adopt" }));

    await waitFor(() => expect(lastResolve()).toBeTruthy());
    expect(lastResolve()).toEqual({
      photoId: 7,
      volumeId: 1,
      relativePath: "2026/03/DSC1.ARW",
      action: "adopt",
    });
    // Debt is per copy, so the identity of the copy — not just the photo — must be on the
    // wire: photoId alone would resolve the wrong file on a photo with several copies.
    expect(lastResolve()?.relativePath).toBe("2026/03/DSC1.ARW");
  });

  it("reports what happened from the outcome, not from the action requested", async () => {
    resolveBehavior = async () =>
      outcome({ action: "adopt", catalogUuid: "uuid-from-the-file", recheckedCopies: 2 });
    render(<IdentityDebtPanel onClose={() => {}} />);
    fireEvent.click(await screen.findByRole("button", { name: "Adopt" }));

    await screen.findByText(/Adopted uuid-from-the-file/);
    expect(screen.getByText(/2 other copies of this photo re-checked/)).toBeTruthy();
  });

  it("does not overwrite on the first click — the destructive one is confirmed", async () => {
    render(<IdentityDebtPanel onClose={() => {}} />);
    fireEvent.click(await screen.findByRole("button", { name: "Overwrite…" }));

    // Still nothing sent: the first click only asks.
    expect(lastResolve()).toBeUndefined();
    await screen.findByText(/Replace the identifier in the file\?/);

    fireEvent.click(screen.getByRole("button", { name: "Overwrite" }));
    await waitFor(() => expect(lastResolve()?.action).toBe("overwrite"));
  });

  it("lets the user back out of an overwrite without sending anything", async () => {
    render(<IdentityDebtPanel onClose={() => {}} />);
    fireEvent.click(await screen.findByRole("button", { name: "Overwrite…" }));
    fireEvent.click(await screen.findByRole("button", { name: "Cancel" }));

    await screen.findByRole("button", { name: "Overwrite…" });
    expect(lastResolve()).toBeUndefined();
  });

  it("shows the backend's refusal verbatim instead of swallowing it", async () => {
    // The refusal that matters most: adopting an identity another photo already holds. The
    // backend names the photo; the panel must not reduce that to "something went wrong".
    resolveBehavior = async () => {
      throw "invalid input: cannot adopt uuid-from-the-file: photo 42 (2025/01/OTHER.ARW) already holds that identity";
    };
    render(<IdentityDebtPanel onClose={() => {}} />);
    fireEvent.click(await screen.findByRole("button", { name: "Adopt" }));

    await screen.findByText(/photo 42 \(2025\/01\/OTHER\.ARW\) already holds that identity/);
    // And the row is still actionable — a refusal is not a dead end.
    expect(screen.getByRole("button", { name: "Adopt" })).toBeTruthy();
  });

  it("offers Restore, and only Restore, for a dismissed copy", async () => {
    page = [
      conflictedCopy({
        fields: [
          {
            field: "identifier",
            state: "dismissed",
            attempts: 3,
            error: "sidecar carries a different identity (uuid-from-the-file); left untouched",
            lastAttemptAt: 1_700_000_000,
            dismissedAt: 1_700_000_100,
          },
        ],
      }),
    ];
    summary = { total: 0, conflicts: 0, dismissed: 1 };
    render(<IdentityDebtPanel onClose={() => {}} />);

    fireEvent.click(await screen.findByRole("button", { name: "Restore" }));
    await waitFor(() => expect(lastResolve()?.action).toBe("restore"));
    expect(screen.queryByRole("button", { name: "Adopt" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Overwrite…" })).toBeNull();
  });

  it("offers no decision for a copy that needs a repair pass rather than one", async () => {
    // Unreachable/Unwritable are not questions for a person to answer — offering Adopt or
    // Overwrite there would invite a decision about an identity nobody has read.
    page = [
      conflictedCopy({
        fields: [
          {
            field: "identifier",
            state: "unwritable",
            attempts: 1,
            error: "sidecar write failed: read-only file system",
            lastAttemptAt: 1_700_000_000,
            dismissedAt: 0,
          },
        ],
      }),
    ];
    render(<IdentityDebtPanel onClose={() => {}} />);
    await screen.findByText(/read-only file system/);

    for (const name of ["Adopt", "Overwrite…", "Dismiss", "Restore"]) {
      expect(screen.queryByRole("button", { name })).toBeNull();
    }
  });

  it("asks the backend for dismissed copies only when the user asks for them", async () => {
    render(<IdentityDebtPanel onClose={() => {}} />);
    await screen.findByRole("button", { name: "Adopt" });
    const listArgs = () => {
      const listed = calls.filter((c) => c.command === "list_pending_identity");
      return listed.length > 0 ? listed[listed.length - 1].args : undefined;
    };
    expect(listArgs()?.includeDismissed).toBe(false);

    fireEvent.click(screen.getByLabelText(/Show dismissed/));
    await waitFor(() => expect(listArgs()?.includeDismissed).toBe(true));
  });
});
