// @vitest-environment jsdom
/**
 * Permission review at enable time (#48), through the panel the user actually clicks.
 *
 * The acceptance criterion is "enabling a module that requests capabilities surfaces what it
 * is asking for", and the layer below cannot show that: `enableModule` refusing an ungranted
 * module proves the gate holds, not that the user is ever told why or given a way through.
 * The interesting failure modes are all in this component —
 *
 *  - the toggle grants by itself, which is precisely the "side effect of toggling" the issue
 *    exists to stop;
 *  - the dialog appears but the module is enabled anyway behind it;
 *  - Cancel leaves the module enabled, or leaves the checkbox looking enabled;
 *  - a module with nothing to ask for is made to nag anyway.
 *
 * Rendered through the real `ModulesSection` against the real host registry, so "enabled"
 * below means the host actually ran the module's `onLoad`, not that a local flag flipped.
 */

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { ModulesSection } from "../ModulesPanel";
import {
  __resetForTests,
  grantedPermissions,
  listModules,
  register,
  setToastSink,
} from "../../modules/host";
import type { ChairPhotoModule } from "../../modules/registry";

// The panel asks the backend where modules install; nothing else here touches Tauri.
vi.mock("@tauri-apps/api/core", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@tauri-apps/api/core")>();
  return { ...actual, invoke: () => Promise.resolve("/tmp/modules") };
});

function makeModule(id: string, overrides: Partial<ChairPhotoModule> = {}): ChairPhotoModule {
  return {
    id,
    name: overrides.name ?? id,
    version: "1.0.0",
    permissions: overrides.permissions,
    onLoad: overrides.onLoad ?? (() => {}),
  };
}

const enabled = (id: string) => !!listModules().find((m) => m.id === id)?.enabled;

/** The "enabled" checkbox of the row for `name`. Rows are `<div>`s, so this walks up from
 *  the module's name to the row and takes its checkbox. */
function toggleFor(name: string): HTMLInputElement {
  const row = screen.getByText(name, { exact: false }).closest(".module-row");
  const box = row?.querySelector('input[type="checkbox"]');
  if (!box) throw new Error(`no enable toggle for "${name}"`);
  return box as HTMLInputElement;
}

beforeEach(() => {
  setToastSink(() => {});
  vi.spyOn(console, "warn").mockImplementation(() => {});
  vi.spyOn(console, "error").mockImplementation(() => {});
});

afterEach(() => {
  cleanup();
  __resetForTests();
  setToastSink(() => {});
  vi.restoreAllMocks();
});

describe("enabling a module that requests capabilities", () => {
  it("shows what it is asking for instead of enabling it", () => {
    const onLoad = vi.fn();
    register(
      makeModule("asker", {
        name: "Asker",
        permissions: { commands: ["faces_index_photos", "faces_for_photo"] },
        onLoad,
      }),
    );
    render(<ModulesSection />);

    fireEvent.click(toggleFor("Asker"));

    // The list is in front of the user…
    const dialog = screen.getByRole("dialog", { name: "Permissions for Asker" });
    expect(dialog.textContent).toContain("faces_index_photos");
    expect(dialog.textContent).toContain("faces_for_photo");

    // …and nothing has been granted or run behind it. This is the assertion that fails if
    // the toggle ever goes back to enabling first and asking afterwards.
    expect(onLoad).not.toHaveBeenCalled();
    expect(enabled("asker")).toBe(false);
    expect(grantedPermissions("asker")).toEqual([]);
  });

  it("grants and enables only when the user allows it", () => {
    const onLoad = vi.fn();
    register(
      makeModule("allower", {
        name: "Allower",
        permissions: { commands: ["catalog_stats"] },
        onLoad,
      }),
    );
    render(<ModulesSection />);

    fireEvent.click(toggleFor("Allower"));
    fireEvent.click(screen.getByRole("button", { name: "Allow and enable" }));

    expect(grantedPermissions("allower")).toEqual(["catalog_stats"]);
    expect(onLoad).toHaveBeenCalledTimes(1);
    expect(enabled("allower")).toBe(true);
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(toggleFor("Allower").checked).toBe(true);
  });

  it("leaves the module off — and the checkbox unchecked — when the user cancels", () => {
    const onLoad = vi.fn();
    register(
      makeModule("canceller", {
        name: "Canceller",
        permissions: { commands: ["catalog_stats"] },
        onLoad,
      }),
    );
    render(<ModulesSection />);

    fireEvent.click(toggleFor("Canceller"));
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));

    expect(screen.queryByRole("dialog")).toBeNull();
    expect(grantedPermissions("canceller")).toEqual([]);
    expect(onLoad).not.toHaveBeenCalled();
    expect(enabled("canceller")).toBe(false);
    // A controlled checkbox left visually on after a refused enable is its own bug.
    expect(toggleFor("Canceller").checked).toBe(false);
  });

  it("does not interrupt a module that asks for nothing", () => {
    const onLoad = vi.fn();
    register(makeModule("quiet", { name: "Quiet", onLoad }));
    render(<ModulesSection />);

    fireEvent.click(toggleFor("Quiet"));

    expect(screen.queryByRole("dialog")).toBeNull();
    expect(onLoad).toHaveBeenCalledTimes(1);
    expect(enabled("quiet")).toBe(true);
  });

  it("does not re-ask on the next enable, but does list the grant", () => {
    register(
      makeModule("repeat", { name: "Repeat", permissions: { commands: ["catalog_stats"] } }),
    );
    render(<ModulesSection />);

    fireEvent.click(toggleFor("Repeat"));
    fireEvent.click(screen.getByRole("button", { name: "Allow and enable" }));
    fireEvent.click(toggleFor("Repeat")); // off
    fireEvent.click(toggleFor("Repeat")); // on again

    expect(screen.queryByRole("dialog")).toBeNull();
    expect(enabled("repeat")).toBe(true);
    // The grant stays visible in the row, so consent given once is still inspectable —
    // the standing objection to reviewing up front is that the decision then disappears.
    expect(screen.getByText(/Backend access: 1 command/)).toBeTruthy();
    expect(screen.getByText("catalog_stats")).toBeTruthy();
  });

  it("marks an unapproved module's row before the user touches the toggle", () => {
    register(
      makeModule("pending", { name: "Pending", permissions: { commands: ["a_cmd", "b_cmd"] } }),
    );
    render(<ModulesSection />);

    // Visible without interacting: a module sitting there unapproved should not look
    // identical to one that is simply switched off.
    expect(screen.getByText(/Backend access: 2 commands \(needs your approval\)/)).toBeTruthy();

    fireEvent.click(toggleFor("Pending"));
    expect(screen.getByRole("dialog", { name: "Permissions for Pending" })).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Allow and enable" }));
    // Approved: the row keeps the list but drops the call to action. The "declaration grew
    // since approval" case that produces the same marker mid-life is driven through
    // `initHost` in src/modules/__tests__/permissions.test.ts, where the persisted grant
    // can actually be made narrower than the declaration.
    expect(screen.queryByText(/needs your approval/)).toBeNull();
    expect(screen.getByText(/Backend access: 2 commands/)).toBeTruthy();
  });
});
