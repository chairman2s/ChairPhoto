// @vitest-environment jsdom
/**
 * Component tests for SendToDevicePanel (issue #57 — component-test harness).
 *
 * Two things the pure-logic suite could never exercise because nothing could render a
 * component (see AGENTS.md-linked issue #57, and 6d1066e's own commit message, which
 * recorded this exact gap):
 *
 *  - "drive an input, assert the output": typing a manual IP/port and clicking Send must
 *    reach the backend with exactly what was typed, and the panel must show it sent.
 *  - the 6d1066e race: `discover()` now runs on mount and resolves ~5s later. If the user
 *    types a manual address while that scan is still in flight, `manualIpRef` must stop the
 *    resolving scan from auto-selecting a discovered device out from under the typed address
 *    — otherwise the send silently goes to the wrong device. That guard is the reason this
 *    panel isn't a one-line change; see git show 6d1066e.
 *
 * `../../api`'s `listVersions` is mocked so the panel doesn't depend on the Tauri stub's
 * command-agnostic `null` response (which would leave `versions` non-array and crash the
 * `.map()` in render). Everything else the panel needs is a hand-built `ChairPhotoAPI` —
 * SendToDevicePanel takes it as a prop directly, so no module registration is needed here.
 */
import { afterEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";

import { SendToDevicePanel } from "../SendToDevicePanel";
import type { ChairPhotoAPI } from "../../registry";

vi.mock("../../api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../api")>();
  return { ...actual, listVersions: () => Promise.resolve([]) };
});

afterEach(() => {
  vi.restoreAllMocks();
});

/** A minimal ChairPhotoAPI: one selected/active photo, no publish preflight. Override
 *  `invoke` per test to control discovery/send. */
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

describe("SendToDevicePanel — driving an input and asserting the output", () => {
  it("sends to the manually typed address and port, and shows the result", async () => {
    const invoke = vi.fn((command: string, _args?: Record<string, unknown>) => {
      if (command === "localsend_discover") return Promise.resolve([]);
      if (command === "localsend_send") return Promise.resolve({ sent: 1, failed: 0 });
      return Promise.resolve(null);
    });
    const api = makeApi({ invoke: invoke as unknown as ChairPhotoAPI["invoke"] });

    render(<SendToDevicePanel api={api} />);

    // Mount-scan resolves with no devices — the manual-IP path is the only way to send.
    await waitFor(() => expect(screen.getByText(/no devices found/i)).toBeTruthy());

    fireEvent.change(screen.getByPlaceholderText(/manual ip/i), {
      target: { value: "10.0.0.55" },
    });
    fireEvent.change(screen.getByPlaceholderText(/port/i), {
      target: { value: "9000" },
    });

    const sendButton = screen.getByRole("button", { name: /send/i }) as HTMLButtonElement;
    expect(sendButton.disabled).toBe(false);
    fireEvent.click(sendButton);

    // Output asserted two ways: what the panel shows, and what it actually sent.
    await waitFor(() => expect(screen.getByText(/Sent 1/)).toBeTruthy());

    const sendCall = invoke.mock.calls.find(([command]) => command === "localsend_send");
    expect(sendCall).toBeTruthy();
    const args = sendCall?.[1] as { device: { ip: string; port: number } };
    expect(args.device).toMatchObject({ ip: "10.0.0.55", port: 9000, protocol: "", fingerprint: "" });
  });
});

describe("SendToDevicePanel — the mount-scan race (regression: 6d1066e)", () => {
  it("does not let a resolving discovery scan steal the selection from a typed address", async () => {
    let resolveDiscover!: (devices: unknown[]) => void;
    const discoverPromise = new Promise<unknown[]>((resolve) => {
      resolveDiscover = resolve;
    });
    const invoke = vi.fn((command: string, _args?: Record<string, unknown>) => {
      if (command === "localsend_discover") return discoverPromise;
      if (command === "localsend_send") return Promise.resolve({ sent: 1, failed: 0 });
      return Promise.resolve(null);
    });
    const api = makeApi({ invoke: invoke as unknown as ChairPhotoAPI["invoke"] });

    render(<SendToDevicePanel api={api} />);

    // The mount effect has started `discover()`; its localsend_discover call is still
    // pending, exactly the ~5s window 6d1066e's commit message describes.
    await waitFor(() => expect(screen.getByText(/scanning the network/i)).toBeTruthy());

    // The user types an address while the scan is still in flight.
    fireEvent.change(screen.getByPlaceholderText(/manual ip/i), {
      target: { value: "10.0.0.55" },
    });

    // The scan now resolves and finds a real device. Without the `manualIpRef` guard added
    // in 6d1066e, this would auto-select it and silently override the typed address.
    resolveDiscover([
      {
        alias: "Living Room TV",
        ip: "192.168.1.50",
        port: 53317,
        protocol: "http",
        fingerprint: "abc123",
      },
    ]);
    await waitFor(() => expect(screen.queryByText(/scanning the network/i)).toBeNull());

    const sendButton = screen.getByRole("button", { name: /send/i }) as HTMLButtonElement;
    expect(sendButton.disabled).toBe(false);
    fireEvent.click(sendButton);

    await waitFor(() => {
      const sendCall = invoke.mock.calls.find(([command]) => command === "localsend_send");
      expect(sendCall).toBeTruthy();
    });
    const sendCall = invoke.mock.calls.find(([command]) => command === "localsend_send");
    const args = sendCall?.[1] as { device: { ip: string } };

    // The genuine wrong-target send #57 called out: this must go to the typed address,
    // never to the device the resolving scan found.
    expect(args.device.ip).toBe("10.0.0.55");
    expect(args.device.ip).not.toBe("192.168.1.50");
  });
});
