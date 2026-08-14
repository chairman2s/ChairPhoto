// @vitest-environment jsdom
/**
 * Component test for the Statistics panel re-rendering on a host channel notify
 * (issue #57). #53 recorded that `useHostFilterContext()` was asserted only at the
 * channel layer — "setFilterContext() notifies only the filterContext channel (M9)" in
 * src/modules/__tests__/host.test.ts (line ~760 as of this writing; the issue's own
 * ":746" pointed a few tests earlier, at an unrelated throwing-listener case — line
 * numbers drift) — but "the Statistics panel re-renders when the scope changes" was not
 * testable because nothing could render a component. This is that missing layer.
 *
 * The panel is exercised through the real host wiring (`register` + `enableModule`, the
 * same pattern host.test.ts uses to capture a module's injected `ChairPhotoAPI`) rather
 * than a hand-built fake, so `api.getFilterContext()` reads the same host state
 * `setFilterContext()` (the App → host sink) writes — exactly what production does. Only
 * the Tauri `invoke` boundary is mocked, routed by command name.
 */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";

import {
  __resetForTests,
  enableModule,
  grantPermissions,
  mainViews,
  register,
  setFilterContext,
} from "../../host";
import { ModuleContent } from "../../ModuleContent";
import { statisticsModule } from "../statistics";

vi.mock("@tauri-apps/api/core", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@tauri-apps/api/core")>();
  return {
    ...actual,
    invoke: (command: string) => {
      if (command === "list_tags") return Promise.resolve(tagsFixture());
      if (command === "catalog_stats") return Promise.resolve(statsFixture());
      return Promise.resolve(null);
    },
  };
});

// Plain `function` declarations, not `const`: vi.mock() factories run before the rest of
// this module's top-level code (they're hoisted above the imports), so a `const` fixture
// referenced above would still be in its temporal dead zone. Function declarations hoist
// their body too, so calling these from inside the factory is safe.
function tagsFixture() {
  return [
    {
      id: 42,
      uuid: "tag-uuid-1",
      name: "Sunset",
      fullPath: "Sunset",
      parentId: null,
      description: "",
      autoRule: null,
      private: false,
      photoCount: 3,
    },
  ];
}

function statsFixture() {
  return {
    totalPhotos: 5,
    withCaptureTime: 5,
    firstMonth: "2024-01",
    lastMonth: "2024-01",
    timeline: [["2024-01", 5]],
    hours: new Array(24).fill(0),
    weekdays: new Array(7).fill(0),
    topTags: [],
    cameras: [],
    lenses: [],
    focalLengths: [],
    ratings: new Array(6).fill(0),
    topDays: [],
    invalidDates: 0,
  };
}

afterEach(() => {
  cleanup();
  __resetForTests();
  vi.restoreAllMocks();
});

describe("Statistics panel — re-renders on a host filter-context notify (issue #53)", () => {
  it("shows the scope chip after setFilterContext(), with no prop change from the test", async () => {
    register(statisticsModule);
    // Statistics declares `catalog_stats` (#48), and the host refuses to enable a module
    // whose declaration has not been granted — so the grant the Modules panel records after
    // permission review has to be recorded here too. Nothing about this test is a special
    // case: `enableModule` would refuse in production the same way.
    grantPermissions("statistics", false);
    enableModule("statistics", false);

    const view = mainViews().find((v) => v.id === "statistics");
    if (!view) throw new Error("statistics module did not register a main view");

    // Rendered once, through the same adapter App uses for a main view, so this covers the
    // real path rather than a direct `view.render()` the app no longer makes. The element
    // itself never changes — only a host channel notify can make the panel show something
    // different from here on.
    const { container } = render(<ModuleContent view={view} />);

    // The Photos stat card, scoped by class since "5" alone also matches an SVG axis label.
    await waitFor(() =>
      expect(container.querySelector(".st-stat-num")?.textContent).toBe("5"),
    );
    expect(screen.queryByText(/scoped to tag/i)).toBeNull();

    // The same call the Library sidebar makes when the user narrows to a tag.
    setFilterContext({ tagId: 42, albumId: null, batchId: null });

    await waitFor(() => expect(screen.getByText(/scoped to tag/i)).toBeTruthy());
  });
});
