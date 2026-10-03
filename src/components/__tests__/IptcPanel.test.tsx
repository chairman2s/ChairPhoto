// @vitest-environment jsdom
/**
 * #148: an IPTC save whose sidecar write did not happen must not say "Saved to sidecar".
 *
 * Only the Tauri `invoke` boundary is mocked, routed by command name, so the test runs
 * through the real `modules/api.ts` wrapper and pins the `set_iptc` result the panel reads.
 */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { IptcPanel, iptcSaveStatus } from "../IptcPanel";
import type { IptcSaveOutcome } from "../../modules/api";

/** What `set_iptc` answers next. */
let saveOutcome: IptcSaveOutcome | null = { sidecar: "written", reason: null };

vi.mock("@tauri-apps/api/core", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@tauri-apps/api/core")>();
  return {
    ...actual,
    invoke: (command: string) => {
      switch (command) {
        case "get_iptc":
          return Promise.resolve({
            description: "",
            headline: "",
            title: "",
            creator: "",
            copyright: "",
            credit: "",
            source: "",
            city: "",
            state: "",
            country: "",
            countryCode: "",
          });
        case "set_iptc":
          return Promise.resolve(saveOutcome);
        default:
          return Promise.resolve(null);
      }
    },
  };
});

afterEach(() => {
  cleanup();
  saveOutcome = { sidecar: "written", reason: null };
});

describe("iptcSaveStatus", () => {
  it("claims the sidecar only when it was written", () => {
    expect(iptcSaveStatus({ sidecar: "written", reason: null })).toBe("Saved to sidecar");
    expect(iptcSaveStatus({ sidecar: "unchanged", reason: null })).toBe(
      "Saved (no sidecar change needed)",
    );
    // #223 F3: a write attempted and failed, whose debt was dismissed meanwhile, also
    // settles "unchanged" (nothing owed either way) but carries a reason — say the write
    // failed, not that none was needed.
    expect(iptcSaveStatus({ sidecar: "unchanged", reason: "read-only" })).toBe(
      "Saved to catalog; the sidecar write failed (read-only)",
    );
    expect(iptcSaveStatus({ sidecar: "pending", reason: "read-only" })).toBe(
      "Saved to catalog; sidecar pending (read-only)",
    );
    expect(iptcSaveStatus({ sidecar: "pending", reason: null })).toBe(
      "Saved to catalog; sidecar pending",
    );
    // A backend older than #148 answered nothing: it said nothing about the sidecar.
    expect(iptcSaveStatus(null)).toBe("Saved to catalog");
  });
});

describe("IptcPanel", () => {
  async function saveHeadline(value: string) {
    const { container } = render(<IptcPanel photoId={1} />);
    const headline = container.querySelectorAll("input.tag-input")[0] as HTMLInputElement;
    await waitFor(() => expect(headline).toBeTruthy());
    fireEvent.change(headline, { target: { value } });
    fireEvent.click(screen.getByText("Save IPTC"));
    return container;
  }

  it("says the sidecar is pending when the save reached only the catalog", async () => {
    saveOutcome = { sidecar: "pending", reason: "sidecar does not parse" };
    const container = await saveHeadline("Fjord");
    await waitFor(() =>
      expect(container.querySelector(".iptc-status")?.textContent).toBe(
        "Saved to catalog; sidecar pending (sidecar does not parse)",
      ),
    );
    expect(container.textContent).not.toContain("Saved to sidecar");
    // The catalog has the values: the form is clean again.
    expect((screen.getByText("Save IPTC") as HTMLButtonElement).disabled).toBe(true);
  });

  it("says saved to sidecar when it was", async () => {
    const container = await saveHeadline("Fjord");
    await waitFor(() =>
      expect(container.querySelector(".iptc-status")?.textContent).toBe("Saved to sidecar"),
    );
  });
});
