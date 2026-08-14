// @vitest-environment jsdom
/**
 * Component tests for the identity repair pass's job lifecycle in the debt panel (#34).
 *
 * Before #34 the pass was one awaited `invoke` with no id, no progress, no cancel, and no
 * way to survive the modal closing. It is now a job like every other long-running one, and
 * the behaviour that has to be true here is the behaviour that costs the user something when
 * it is wrong:
 *
 *  - a pass reports progress while it runs, and its RESULT comes from the terminal event,
 *    not from the command (which only returns a job id);
 *  - a superseded pass's stragglers are ignored rather than driving the counter backwards;
 *  - a terminal event that beats the job id is replayed, not dropped — dropped, the panel
 *    sits in "Repairing…" for a pass that has already finished;
 *  - Cancel reaches the backend and does NOT pretend the pass has already stopped: the pass
 *    still has to reach its next copy, and its own (partial, `aborted`) summary is the
 *    honest report;
 *  - a pass already running is re-attached to on mount, because a pass over a 74k-row queue
 *    on a NAS outlives one open/close of this modal;
 *  - an idle backend costs nothing — no listener, no error;
 *  - a host that cannot deliver the terminal event fails closed rather than starting a pass
 *    it could never see finish.
 *
 * Both Tauri boundaries are mocked: `invoke` routed by command name, and `listen` recorded so
 * every event is delivered by hand. Nothing is raced — each interleaving is forced. Running
 * through the real `modules/api.ts` wrappers means these also pin the command and event NAMES
 * the backend publishes, not just the component's internal calls.
 *
 * **Two `vi.mock` calls, one per specifier.** Issue #64: `vitest.config.ts` used to alias
 * every `@tauri-apps/*` specifier to one shared stub file, so `vi.mock("@tauri-apps/api/core")`
 * and `vi.mock("@tauri-apps/api/event")` named the same resolved module — the second silently
 * replaced the first, and `invoke` went back to the stub's no-op while every assertion here
 * read an empty call log. Fixed by giving each specifier its own resolved file (see the alias
 * block in `vitest.config.ts`), so `invoke` and `listen` are now mocked independently below —
 * each `vi.mock` call names a distinct path and both take effect.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { IdentityDebtPanel } from "../IdentityDebtPanel";
import type { IdentityRepairDone, IdentityRepairSummary } from "../../modules/api";

/** Every invoke the panel made, in order. */
let calls: Array<{ command: string; args: Record<string, unknown> }> = [];
/** What `identity_repair_status` reports — `null` is an idle backend. */
let runningPass: { job: number; done: number; total: number } | null = null;
/** What `repair_pending_identity` resolves to (a job id), and when. */
let startPass: () => Promise<number> = async () => 7;
/** Live `listen` handlers by event name, so tests can emit by hand. */
let handlers: Map<string, Array<(e: { payload: unknown }) => void>> = new Map();
/** Unlisten functions handed out per event name, to assert release. */
let stops: Map<string, ReturnType<typeof vi.fn>> = new Map();
/** Event names whose registration must reject (an older host / a failed registration). */
let listenRejects = new Set<string>();

vi.mock("@tauri-apps/api/core", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@tauri-apps/api/core")>();
  return {
    ...actual,
    invoke: (command: string, args: Record<string, unknown>) => {
      calls.push({ command, args: args ?? {} });
      switch (command) {
        case "summarize_pending_identity":
          return Promise.resolve({ total: 3, conflicts: 0, dismissed: 0 });
        case "list_pending_identity":
          return Promise.resolve([]);
        case "list_volumes":
          return Promise.resolve([]);
        case "identity_repair_status":
          return Promise.resolve(runningPass);
        case "repair_pending_identity":
          return startPass();
        case "identity_repair_cancel":
          return Promise.resolve(null);
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
      if (listenRejects.has(event)) return Promise.reject(new Error("no event bridge"));
      const list = handlers.get(event) ?? [];
      list.push(handler);
      handlers.set(event, list);
      const stop = vi.fn(() => {
        handlers.set(event, (handlers.get(event) ?? []).filter((h) => h !== handler));
      });
      stops.set(event, stop);
      return Promise.resolve(stop);
    },
  };
});

/** Deliver `payload` to every live listener for `event`, inside `act`. */
async function emit(event: string, payload: unknown) {
  await act(async () => {
    for (const h of [...(handlers.get(event) ?? [])]) h({ payload });
  });
}

function summary(over: Partial<IdentityRepairSummary> = {}): IdentityRepairSummary {
  return {
    bound: 0,
    unreachable: 0,
    conflicts: 0,
    failed: 0,
    superseded: 0,
    total: 3,
    aborted: false,
    ...over,
  };
}

function done(over: Partial<IdentityRepairDone> = {}): IdentityRepairDone {
  return { ok: true, job: 7, summary: summary(), error: null, ...over };
}

/** jsdom lays nothing out, and the panel's list is virtualized — see the note in
 *  `IdentityDebtPanel.actions.test.tsx`. The list is empty in this file, but the panel still
 *  renders it, so give the viewport a size for consistency. */
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
  handlers = new Map();
  stops = new Map();
  listenRejects = new Set();
  runningPass = null;
  startPass = async () => 7;
  restoreSizes = giveEveryElementASize();
});

afterEach(() => {
  cleanup();
  restoreSizes();
  vi.restoreAllMocks();
});

/** The Start button, once the summary has loaded and enabled it — it is disabled until the
 *  panel knows there is debt to repair, so clicking it earlier does nothing at all. */
async function enabledStartButton() {
  const start = await screen.findByRole("button", { name: "Start repair pass" });
  await waitFor(() => expect((start as HTMLButtonElement).disabled).toBe(false));
  return start;
}

/** Render, wait for the initial loads, then press Start and wait until the pass is adopted. */
async function startAPass() {
  render(<IdentityDebtPanel onClose={() => {}} />);
  const start = await enabledStartButton();
  await act(async () => {
    fireEvent.click(start);
  });
  await screen.findByRole("button", { name: "Cancel" });
}

function commandNames() {
  return calls.map((c) => c.command);
}

describe("starting a repair pass", () => {
  it("subscribes to both event streams before asking the backend to start", async () => {
    // A pass over an already-clean queue finishes before the command's promise resolves, so
    // a listener installed afterwards would miss the only terminal event there will ever be.
    let listenedBeforeStart: string[] = [];
    startPass = async () => {
      listenedBeforeStart = [...handlers.keys()];
      return 7;
    };
    await startAPass();

    expect(listenedBeforeStart).toContain("identity:repair_done");
    expect(listenedBeforeStart).toContain("identity:repair_progress");
  });

  it("follows progress from the event stream, and takes its result from the terminal event", async () => {
    await startAPass();
    expect(commandNames()).toContain("repair_pending_identity");

    await emit("identity:repair_progress", { done: 120, total: 74488, job: 7 });
    expect(screen.getByText("Repairing… 120 of 74488")).toBeTruthy();

    await emit("identity:repair_done", done({ summary: summary({ bound: 2, conflicts: 1 }) }));
    await waitFor(() =>
      expect(screen.getByText(/bound 2 · still unreachable 0 · conflict 1/)).toBeTruthy(),
    );
    // The pass is over: no Cancel, and Start is available again.
    expect(screen.queryByRole("button", { name: "Cancel" })).toBeNull();
    expect(screen.queryByText(/Repairing… /)).toBeNull();
  });

  it("reloads the queue when the pass ends, because repaired copies have left it", async () => {
    await startAPass();
    const before = commandNames().filter((c) => c === "list_pending_identity").length;

    await emit("identity:repair_done", done());

    await waitFor(() =>
      expect(commandNames().filter((c) => c === "list_pending_identity").length).toBeGreaterThan(
        before,
      ),
    );
    expect(commandNames().filter((c) => c === "summarize_pending_identity").length).toBeGreaterThan(
      1,
    );
  });

  it("ignores a superseded pass's stragglers", async () => {
    await startAPass();
    await emit("identity:repair_progress", { done: 5, total: 100, job: 7 });
    expect(screen.getByText("Repairing… 5 of 100")).toBeTruthy();

    // An older pass (job 6) is still emitting after this one was adopted. Its numbers must
    // not drive the counter — least of all backwards.
    await emit("identity:repair_progress", { done: 99, total: 100, job: 6 });
    expect(screen.getByText("Repairing… 5 of 100")).toBeTruthy();

    // Nor may its terminal event end the pass this panel is following.
    await emit("identity:repair_done", done({ job: 6, summary: summary({ bound: 99 }) }));
    expect(screen.getByRole("button", { name: "Cancel" })).toBeTruthy();
    expect(screen.queryByText(/bound 99/)).toBeNull();
  });

  it("replays a terminal event that arrived before the job id was known", async () => {
    // The window is unavoidable: the listener has to be live before the command, so a fast
    // pass's terminal event can land while the command's promise is still pending. Dropped,
    // the panel sits in "Repairing…" for a pass that has already finished.
    let resolveStart: (job: number) => void = () => {};
    startPass = () => new Promise<number>((res) => (resolveStart = res));

    render(<IdentityDebtPanel onClose={() => {}} />);
    const start = await enabledStartButton();
    await act(async () => {
      fireEvent.click(start);
    });
    await waitFor(() => expect(handlers.get("identity:repair_done")?.length).toBe(1));

    await emit("identity:repair_done", done({ job: 7, summary: summary({ bound: 3 }) }));
    await act(async () => {
      resolveStart(7);
    });

    await waitFor(() => expect(screen.getByText(/bound 3/)).toBeTruthy());
    expect(screen.queryByRole("button", { name: "Cancel" })).toBeNull();
  });

  it("shows the backend's error when the pass could not run, rather than a result", async () => {
    await startAPass();
    await emit(
      "identity:repair_done",
      done({ ok: false, error: "couldn't open catalog connection: disk I/O error" }),
    );

    await waitFor(() => expect(screen.getByText(/disk I\/O error/)).toBeTruthy());
    expect(screen.queryByText(/Finished — bound/)).toBeNull();
    expect(screen.queryByRole("button", { name: "Cancel" })).toBeNull();
  });

  it("fails closed when the terminal event cannot be delivered at all", async () => {
    // AGENTS.md: "a required terminal signal must fail closed with an explicit state".
    // Starting a pass this panel could never see finish is worse than not starting one.
    listenRejects.add("identity:repair_done");
    render(<IdentityDebtPanel onClose={() => {}} />);
    const start = await enabledStartButton();
    await act(async () => {
      fireEvent.click(start);
    });

    await waitFor(() => expect(screen.getByText(/could not report when it finished/)).toBeTruthy());
    expect(commandNames()).not.toContain("repair_pending_identity");
    expect(screen.queryByRole("button", { name: "Cancel" })).toBeNull();
  });
});

describe("cancelling a repair pass", () => {
  it("reaches the backend and waits for the pass's own terminal report", async () => {
    await startAPass();
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    });

    expect(commandNames()).toContain("identity_repair_cancel");
    // Still running: the pass has to reach its next copy first. Claiming it had already
    // stopped would be a state the backend has not reported.
    expect(screen.getByRole("button", { name: "Cancel" })).toBeTruthy();

    // Its own summary is the report, and it says the counters are partial.
    await emit(
      "identity:repair_done",
      done({ summary: summary({ bound: 2, total: 74488, aborted: true }) }),
    );
    await waitFor(() => expect(screen.getByText(/Stopped after 2 of 74488/)).toBeTruthy());
  });

  it("offers Cancel only while a pass is running", async () => {
    render(<IdentityDebtPanel onClose={() => {}} />);
    await screen.findByRole("button", { name: "Start repair pass" });
    expect(screen.queryByRole("button", { name: "Cancel" })).toBeNull();
  });
});

describe("re-attaching to a pass in flight", () => {
  it("adopts a pass that was already running when the panel mounted", async () => {
    // The modal is not a lifetime a 74k-row pass over a NAS can be tied to. A panel that
    // reopened as if nothing were happening would invite a second pass over the same queue.
    runningPass = { job: 42, done: 900, total: 74488 };
    render(<IdentityDebtPanel onClose={() => {}} />);

    await screen.findByRole("button", { name: "Cancel" });
    expect(screen.getByText("Repairing… 900 of 74488")).toBeTruthy();
    expect(commandNames()).not.toContain("repair_pending_identity");

    // And it follows THAT job, not whatever id a fresh start would have had.
    await emit("identity:repair_progress", { done: 1000, total: 74488, job: 42 });
    expect(screen.getByText("Repairing… 1000 of 74488")).toBeTruthy();
    await emit("identity:repair_done", done({ job: 42, summary: summary({ bound: 900 }) }));
    await waitFor(() => expect(screen.getByText(/bound 900/)).toBeTruthy());
  });

  it("installs nothing, and reports nothing, when the backend is idle", async () => {
    runningPass = null;
    render(<IdentityDebtPanel onClose={() => {}} />);
    await screen.findByRole("button", { name: "Start repair pass" });

    await waitFor(() => expect(commandNames()).toContain("identity_repair_status"));
    expect(handlers.get("identity:repair_done") ?? []).toHaveLength(0);
    expect(handlers.get("identity:repair_progress") ?? []).toHaveLength(0);
    expect(screen.queryByRole("button", { name: "Cancel" })).toBeNull();
  });

  it("does not blame a pass that is not running when the event bridge is unavailable", async () => {
    // The status query has to come BEFORE the listeners, not after. A mount that subscribes
    // first and asks second still lands on the fail-closed path when the bridge is down —
    // and then a panel opened over a perfectly idle backend greets the user with "the pass
    // could not report when it finished" about a pass that never existed.
    //
    // This is what discriminates the two orderings observably: with the probe first, an idle
    // backend means no registration is ever attempted, so there is nothing to fail.
    runningPass = null;
    listenRejects.add("identity:repair_done");
    render(<IdentityDebtPanel onClose={() => {}} />);
    await screen.findByRole("button", { name: "Start repair pass" });
    await waitFor(() => expect(commandNames()).toContain("identity_repair_status"));

    expect(screen.queryByText(/could not report when it finished/)).toBeNull();
    expect(screen.queryByRole("button", { name: "Cancel" })).toBeNull();
  });
});

describe("listener ownership", () => {
  it("releases both listeners when the pass ends", async () => {
    await startAPass();
    expect(handlers.get("identity:repair_done")).toHaveLength(1);
    expect(handlers.get("identity:repair_progress")).toHaveLength(1);

    await emit("identity:repair_done", done());

    await waitFor(() => expect(handlers.get("identity:repair_done")).toHaveLength(0));
    expect(handlers.get("identity:repair_progress")).toHaveLength(0);
    expect(stops.get("identity:repair_done")).toHaveBeenCalled();
    expect(stops.get("identity:repair_progress")).toHaveBeenCalled();
  });

  it("releases them on unmount too, so a closed panel leaves nothing running", async () => {
    await startAPass();
    cleanup();

    expect(handlers.get("identity:repair_done")).toHaveLength(0);
    expect(handlers.get("identity:repair_progress")).toHaveLength(0);
  });

  // The fourth listener failure mode — a registration that RESOLVES after the panel has
  // unmounted, which must be stopped rather than stored — is not re-tested here. It is a
  // property of `useOwnedListeners` itself, not of this panel's use of it, and
  // `src/modules/__tests__/ownedEvents.test.tsx` forces it directly by parking the
  // registration. Re-staging it through this component would test the same code twice while
  // making the interleaving harder to force.
});
