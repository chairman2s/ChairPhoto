// @vitest-environment jsdom
/**
 * The identity-debt panel across a catalog switch (#164).
 *
 * Before #164 the panel had no catalog-switch handling: catalog A's identity queue and
 * owed-IPTC list stayed on screen after a switch to B, and their actions carried only ids,
 * paths, UUIDs and generations. When B is a byte copy of A those are all equal, so a
 * Dismiss of A's row cleared B's debt. What has to be true now:
 *
 *  - the panel captures the open catalog's identity when it opens, reads nothing before it
 *    is known, and passes it to every read and every action — so the backend refuses any of
 *    them once another catalog is open, even before `catalog:switched` arrives (a switch
 *    publishes the new catalog first);
 *  - the identity is the one captured at open: the backend's current identity changing
 *    under the panel does not move the panel's actions onto the new catalog;
 *  - `catalog:switched` closes the panel, and an unmounted panel holds no listener.
 *
 * Both Tauri boundaries are mocked (one `vi.mock` per specifier; see vitest.config.ts):
 * `invoke` routed by command name, and `listen` recorded so the event is delivered by hand.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { IdentityDebtPanel } from "../IdentityDebtPanel";

const CATALOG_CHANGED = "The catalog changed since this was read";

let calls: Array<{ command: string; args: Record<string, unknown> }> = [];
/** What `get_catalog_identity` answers now — the open catalog. */
let openCatalog = "catalog-a";
/** Held when a test needs the identity to arrive late. */
let identityGate: Promise<void> = Promise.resolve();
let handlers: Map<string, Array<(e: { payload: unknown }) => void>> = new Map();

/** The backend's binding: a read or action carrying another catalog's identity is refused. */
function bound<T>(args: Record<string, unknown>, answer: T): Promise<T> {
  if (args.catalog !== undefined && args.catalog !== openCatalog) return Promise.reject(CATALOG_CHANGED);
  return Promise.resolve(answer);
}

vi.mock("@tauri-apps/api/core", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@tauri-apps/api/core")>();
  return {
    ...actual,
    invoke: (command: string, rawArgs: Record<string, unknown>) => {
      const args = rawArgs ?? {};
      calls.push({ command, args });
      switch (command) {
        case "get_catalog_identity": {
          const now = openCatalog;
          return identityGate.then(() => now);
        }
        case "summarize_pending_identity":
          return bound(args, { total: 1, conflicts: 1, dismissed: 0, iptcOwed: 1 });
        case "list_pending_identity":
          return bound(args, [conflictedCopy()]);
        case "list_owed_iptc":
          return bound(args, [owedRow()]);
        case "list_volumes":
          return Promise.resolve([]);
        case "resolve_identity_conflict":
          return bound(args, {
            action: args.action,
            photoId: 7,
            catalogUuid: "u",
            previousSidecarUuid: "",
            recheckedCopies: 0,
            sidecarBackup: null,
          });
        case "dismiss_owed_iptc":
          return bound(args, "dismissed");
        case "retry_owed_iptc":
          return bound(args, { sidecar: "written", reason: null });
        default:
          return Promise.resolve(null);
      }
    },
  };
});

vi.mock("@tauri-apps/api/event", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@tauri-apps/api/event")>();
  return {
    ...actual,
    listen: (event: string, handler: (e: { payload: unknown }) => void) => {
      handlers.set(event, [...(handlers.get(event) ?? []), handler]);
      return Promise.resolve(() => {
        handlers.set(event, (handlers.get(event) ?? []).filter((h) => h !== handler));
      });
    },
  };
});

function conflictedCopy() {
  return {
    photoId: 7,
    path: "2026/03/DSC7.ARW",
    volumeId: 1,
    relativePath: "2026/03/DSC7.ARW",
    fields: [
      {
        field: "identifier",
        state: "conflict",
        attempts: 1,
        error: "sidecar carries a different identity",
        lastAttemptAt: 0,
        dismissedAt: 0,
      },
    ],
  };
}

function owedRow() {
  return {
    photoId: 7,
    uuid: "uuid-7",
    path: "2026/03/DSC7.ARW",
    fields: ["Title"],
    attempts: 1,
    error: "",
    lastAttemptAt: 0,
    queuedAt: 0,
    generation: 3,
  };
}

/** jsdom lays nothing out; the queue is virtualized (see IdentityDebtPanel.actions.test.tsx). */
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
  openCatalog = "catalog-a";
  identityGate = Promise.resolve();
  handlers = new Map();
  restoreSizes = giveEveryElementASize();
});

afterEach(() => {
  cleanup();
  restoreSizes();
  vi.restoreAllMocks();
});

const sent = (command: string) => calls.filter((c) => c.command === command);
const READS = ["summarize_pending_identity", "list_pending_identity", "list_owed_iptc"];

/** The panel open on catalog A, both lists drawn. */
async function openOnA(onClose: () => void = () => {}) {
  const view = render(<IdentityDebtPanel onClose={onClose} />);
  await screen.findByRole("button", { name: "Adopt" });
  await screen.findByTestId("owed-row-7");
  return view;
}

/** The switch publishes B; `catalog:switched` has not reached the panel. */
function switchToCopyUndelivered() {
  openCatalog = "catalog-b";
}

describe("the identity-debt panel is bound to the catalog it opened on (#164)", () => {
  it("reads nothing before the catalog's identity is known, then binds every read to it", async () => {
    let open = () => {};
    identityGate = new Promise((r) => (open = r));
    render(<IdentityDebtPanel onClose={() => {}} />);
    await waitFor(() => expect(sent("get_catalog_identity")).toHaveLength(1));
    for (const read of READS) expect(sent(read), read).toEqual([]);

    await act(async () => open());
    await screen.findByRole("button", { name: "Adopt" });
    for (const read of READS) {
      expect(sent(read).length, read).toBeGreaterThan(0);
      for (const c of sent(read)) expect(c.args.catalog, read).toBe("catalog-a");
    }
  });

  it("every action carries the catalog its row was read from", async () => {
    await openOnA();
    fireEvent.click(screen.getByRole("button", { name: "Dismiss", description: /Changes neither/ }));
    await waitFor(() => expect(sent("resolve_identity_conflict")).toHaveLength(1));
    fireEvent.click(await screen.findByRole("button", { name: "Retry" }));
    await waitFor(() => expect(sent("retry_owed_iptc")).toHaveLength(1));
    fireEvent.click(await screen.findByRole("button", { name: "Dismiss", description: /Stop owing/ }));
    await waitFor(() => expect(sent("dismiss_owed_iptc")).toHaveLength(1));
    for (const action of ["resolve_identity_conflict", "retry_owed_iptc", "dismiss_owed_iptc"]) {
      expect(sent(action)[0].args.catalog, action).toBe("catalog-a");
    }
  });

  it("a click after a switch, before catalog:switched, is refused rather than reaching the copy", async () => {
    await openOnA();
    switchToCopyUndelivered();
    fireEvent.click(screen.getByRole("button", { name: "Dismiss", description: /Changes neither/ }));
    expect(await screen.findByText(CATALOG_CHANGED)).toBeTruthy();
    // The panel did not re-capture the identity: the action went out bound to A.
    expect(sent("get_catalog_identity")).toHaveLength(1);
    expect(sent("resolve_identity_conflict")[0].args.catalog).toBe("catalog-a");

    fireEvent.click(screen.getByRole("button", { name: "Dismiss", description: /Stop owing/ }));
    await waitFor(() => expect(sent("dismiss_owed_iptc")).toHaveLength(1));
    expect(sent("dismiss_owed_iptc")[0].args.catalog).toBe("catalog-a");
    await waitFor(() => expect(screen.getAllByText(CATALOG_CHANGED).length).toBeGreaterThan(1));
  });

  it("catalog:switched closes the panel", async () => {
    const onClose = vi.fn();
    await openOnA(onClose);
    expect(handlers.get("catalog:switched")?.length).toBe(1);
    await act(async () => {
      for (const h of handlers.get("catalog:switched") ?? []) h({ payload: "/b.chairphoto" });
    });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("an unmounted panel holds no catalog:switched listener", async () => {
    const view = await openOnA();
    view.unmount();
    expect(handlers.get("catalog:switched") ?? []).toEqual([]);
  });
});
