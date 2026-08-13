// @vitest-environment jsdom
/**
 * Component tests for the Smart Tagging inspector panel's job/listener lifecycle.
 *
 * The panel was migrated onto the shared owned-listener utility (issue #13). That is a
 * refactor, so the point of these tests is that the *observable* behaviour survived it — and
 * the behaviour worth pinning is precisely the care the module had written out by hand:
 *
 *  - stale events: a superseded run keeps emitting `smarttags:progress` after a newer run is
 *    adopted; the panel must ignore its stragglers rather than let them drive the bar.
 *  - late listener registration: the panel can unmount while `onEvent` is still resolving.
 *    That listener must be stopped, not left running with nobody holding its stopper.
 *  - terminal buffering: the terminal listener has to be live *before* the start command, so
 *    a fast run's `smarttags:index_done` can arrive before the job id is known. Dropped, the
 *    panel sits in "indexing" forever.
 *  - failing closed: a host that cannot deliver the terminal event must refuse to start a run
 *    at all, rather than start one it could never see finish.
 *
 * None of that had a test before — nothing could render this panel until the jsdom harness
 * (issue #57). Every event here is delivered by hand through a captured `onEvent` handler and
 * every promise resolved on demand, so each interleaving is forced rather than raced.
 *
 * `SimilarTagsPanel` is not exported; it is reached the way the host reaches it, by running
 * the module's `onLoad` and rendering the inspector panel it registers.
 */
import { describe, expect, it, vi } from "vitest";
import { act, render, screen, waitFor } from "@testing-library/react";
import type { ReactNode } from "react";

import { smartTaggingModule } from "../smartTagging";
import type { ChairPhotoAPI, Unsubscribe } from "../../registry";

/** A model that is present, so the panel renders its index UI rather than a download prompt. */
const READY_MODEL = {
  ready: true,
  model: { key: "clip", present: true, path: "/models/clip.onnx", custom: false, detail: null },
};

type EventHandler = (payload: unknown) => void;

/** Records every `onEvent` registration so a test can deliver events by hand. */
function eventBus() {
  const handlers = new Map<string, EventHandler[]>();
  const stops: Record<string, ReturnType<typeof vi.fn>> = {};
  return {
    handlers,
    stops,
    onEvent: (<T,>(event: string, handler: (payload: T) => void): Promise<Unsubscribe> => {
      const list = handlers.get(event) ?? [];
      list.push(handler as EventHandler);
      handlers.set(event, list);
      const stop = vi.fn(() => {
        handlers.set(event, (handlers.get(event) ?? []).filter((h) => h !== handler));
      });
      stops[event] = stop;
      return Promise.resolve(stop as Unsubscribe);
    }) as ChairPhotoAPI["onEvent"],
    /** Deliver `payload` to every live listener for `event`. */
    emit(event: string, payload: unknown) {
      for (const h of [...(handlers.get(event) ?? [])]) h(payload);
    },
    live(event: string) {
      return (handlers.get(event) ?? []).length;
    },
  };
}

function makeApi(overrides: Partial<ChairPhotoAPI> = {}): ChairPhotoAPI {
  const base: ChairPhotoAPI = {
    getSelectedPhotos: () => [],
    getActivePhotoId: () => 1,
    getActiveVersionId: () => null,
    getEditingTag: () => null,
    listTags: () => Promise.resolve([]),
    assignTag: () => Promise.resolve(),
    recordPublication: () => Promise.resolve(),
    listPublications: () => Promise.resolve([]),
    deletePublication: () => Promise.resolve(),
    invoke: (() => Promise.resolve(null)) as ChairPhotoAPI["invoke"],
    getSetting: () => Promise.resolve(null),
    setSetting: () => Promise.resolve(),
    getEditRecord: () => Promise.resolve(null),
    setEditRecord: () => Promise.resolve(),
    registerPanel: () => {},
    registerAction: () => {},
    registerPublishTarget: () => {},
    registerSettingsPanel: () => {},
    registerMainView: () => {},
    registerEditRenderer: () => {},
    showToast: () => {},
    notifyChange: () => {},
    filterByTag: () => {},
    selectPhoto: () => {},
    selectPhotoSilent: () => {},
    getFilterContext: () => ({ tagId: null, albumId: null, batchId: null }),
  };
  return { ...base, ...overrides };
}

/** Run the module's `onLoad` and return the inspector panel's rendered element. */
function similarTagsPanel(api: ChairPhotoAPI): ReactNode {
  let node: ReactNode = null;
  const withRegistration = makeApi({
    ...api,
    registerPanel: (panel) => {
      if (panel.id === "smarttags-similar") node = panel.render();
    },
  });
  smartTaggingModule.onLoad(withRegistration);
  if (!node) throw new Error("the module registered no smarttags-similar panel");
  return node;
}

/** The Index button, whose label carries the panel's whole job state. */
function indexButton(): HTMLButtonElement {
  return screen.getByRole("button", { name: /^Index|^Indexing/ }) as HTMLButtonElement;
}

describe("SimilarTagsPanel — reattaching to a running job", () => {
  it("adopts the running job and ignores a superseded run's progress", async () => {
    const bus = eventBus();
    const api = makeApi({
      onEvent: bus.onEvent,
      invoke: ((command: string) => {
        if (command === "smarttags_model_status") return Promise.resolve(READY_MODEL);
        if (command === "smarttags_load_suggestions") return Promise.resolve([]);
        if (command === "smarttags_index_status")
          return Promise.resolve({ job: 7, done: 1, total: 10 });
        return Promise.resolve(null);
      }) as ChairPhotoAPI["invoke"],
    });

    render(similarTagsPanel(api) as React.ReactElement);

    // The reattach adopts job 7 and shows its snapshot: 1 of 10.
    await waitFor(() => expect(indexButton().textContent).toBe("Indexing… 10%"));

    // A straggler from the run job 7 superseded. Without job-id filtering this would drive
    // the bar to 99% for a run nobody is following.
    await act(async () => {
      bus.emit("smarttags:progress", { job: 6, done: 99, total: 100 });
    });
    expect(indexButton().textContent).toBe("Indexing… 10%");

    // Our own run's progress does move it.
    await act(async () => {
      bus.emit("smarttags:progress", { job: 7, done: 5, total: 10 });
    });
    expect(indexButton().textContent).toBe("Indexing… 50%");

    // And only our run's terminal event ends it.
    await act(async () => {
      bus.emit("smarttags:index_done", {
        ok: true,
        done: 5,
        total: 10,
        offline: 0,
        failed: 0,
        aborted: true,
        job: 6,
        error: null,
      });
    });
    expect(indexButton().textContent).toBe("Indexing… 50%");

    await act(async () => {
      bus.emit("smarttags:index_done", {
        ok: true,
        done: 10,
        total: 10,
        offline: 0,
        failed: 0,
        aborted: false,
        job: 7,
        error: null,
      });
    });
    await waitFor(() => expect(indexButton().textContent).toBe("Index"));
    expect(screen.getByText(/Indexing complete: 10 photos processed/)).toBeTruthy();
  });

  it("stays idle, and releases its listener, when the backend reports no running job", async () => {
    const bus = eventBus();
    const api = makeApi({
      onEvent: bus.onEvent,
      invoke: ((command: string) => {
        if (command === "smarttags_model_status") return Promise.resolve(READY_MODEL);
        if (command === "smarttags_load_suggestions") return Promise.resolve([]);
        if (command === "smarttags_index_status") return Promise.resolve(null);
        return Promise.resolve(null);
      }) as ChairPhotoAPI["invoke"],
    });

    render(similarTagsPanel(api) as React.ReactElement);

    await waitFor(() => expect(indexButton().disabled).toBe(false));
    expect(indexButton().textContent).toBe("Index");
    // Confirmed idle is the one outcome that returns to an enabled panel — and the terminal
    // listener the preflight installed must not be left behind.
    await waitFor(() => expect(bus.live("smarttags:index_done")).toBe(0));
  });
});

describe("SimilarTagsPanel — late listener registration", () => {
  it("stops a terminal registration that resolves after the panel unmounted", async () => {
    const stop = vi.fn();
    let settle!: (u: Unsubscribe) => void;
    const pending = new Promise<Unsubscribe>((resolve) => {
      settle = resolve;
    });
    const api = makeApi({
      onEvent: ((event: string) =>
        event === "smarttags:index_done"
          ? pending
          : Promise.resolve(vi.fn() as Unsubscribe)) as ChairPhotoAPI["onEvent"],
      invoke: ((command: string) => {
        if (command === "smarttags_model_status") return Promise.resolve(READY_MODEL);
        if (command === "smarttags_load_suggestions") return Promise.resolve([]);
        if (command === "smarttags_index_status")
          return Promise.resolve({ job: 3, done: 0, total: 4 });
        return Promise.resolve(null);
      }) as ChairPhotoAPI["invoke"],
    });

    const { unmount } = render(similarTagsPanel(api) as React.ReactElement);

    // The reattach is parked inside its terminal-listener registration.
    await waitFor(() => expect(indexButton()).toBeTruthy());
    unmount();

    // Only now does the registration land. It belongs to nobody: it must be stopped, not
    // stored — stored, nothing would ever be able to stop it.
    await act(async () => {
      settle(stop as Unsubscribe);
      await pending;
    });
    expect(stop).toHaveBeenCalledTimes(1);
  });
});

describe("SimilarTagsPanel — starting a run", () => {
  it("replays a terminal event that arrived before the job id was known", async () => {
    const bus = eventBus();
    let startJob!: (job: number) => void;
    const started = new Promise<number>((resolve) => {
      startJob = resolve;
    });
    const api = makeApi({
      onEvent: bus.onEvent,
      invoke: ((command: string) => {
        if (command === "smarttags_model_status") return Promise.resolve(READY_MODEL);
        if (command === "smarttags_load_suggestions") return Promise.resolve([]);
        if (command === "smarttags_index_status") return Promise.resolve(null);
        if (command === "smarttags_index_photos") return started;
        return Promise.resolve(null);
      }) as ChairPhotoAPI["invoke"],
    });

    render(similarTagsPanel(api) as React.ReactElement);
    await waitFor(() => expect(indexButton().disabled).toBe(false));

    await act(async () => {
      indexButton().click();
    });
    await waitFor(() => expect(bus.live("smarttags:index_done")).toBe(1));

    // The run finishes before `smarttags_index_photos` has even answered — the window the
    // buffer exists for. Nothing may be shown yet; the panel does not know its job id.
    await act(async () => {
      bus.emit("smarttags:index_done", {
        ok: true,
        done: 2,
        total: 2,
        offline: 0,
        failed: 0,
        aborted: false,
        job: 9,
        error: null,
      });
    });
    expect(screen.queryByText(/Indexing complete/)).toBeNull();

    // The start command finally answers with that same job id, and the buffered event is
    // replayed. Without the buffer the panel would sit in "Indexing…" forever.
    await act(async () => {
      startJob(9);
      await started;
    });
    await waitFor(() => expect(screen.getByText(/Indexing complete: 2 photos/)).toBeTruthy());
    expect(indexButton().textContent).toBe("Index");
  });

  it("refuses to start when the host cannot deliver the terminal event", async () => {
    const invoke = vi.fn((command: string) => {
      if (command === "smarttags_model_status") return Promise.resolve(READY_MODEL);
      if (command === "smarttags_load_suggestions") return Promise.resolve([]);
      if (command === "smarttags_index_status") return Promise.resolve(null);
      return Promise.resolve(null);
    });
    // No `onEvent` at all — an older host. The terminal signal is required, so this must
    // fail closed rather than start a run it could never see finish.
    const api = makeApi({
      onEvent: undefined,
      invoke: invoke as unknown as ChairPhotoAPI["invoke"],
    });

    render(similarTagsPanel(api) as React.ReactElement);
    await waitFor(() => expect(indexButton().disabled).toBe(false));

    await act(async () => {
      indexButton().click();
    });

    await waitFor(() =>
      expect(screen.getByText(/this build cannot report when indexing finishes/)).toBeTruthy(),
    );
    expect(invoke.mock.calls.some(([c]) => c === "smarttags_index_photos")).toBe(false);
    expect(indexButton().textContent).toBe("Index");
  });
});
