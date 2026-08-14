import { defineConfig } from "vitest/config";
import { resolve } from "path";

// vitest.config.ts — runs the frontend unit tests (npx vitest / npm test).
//
// Tauri APIs (and some React hooks) are platform-only and unavailable in a
// plain Node/jsdom environment.  We alias every @tauri-apps/* import to a
// stub module so host.ts and api.ts load cleanly during tests. Only the
// pure-logic exports (satisfies, hostSatisfies, unmetRequirement, register,
// enableModule, …) are exercised; the Tauri-backed runtime paths are tested
// indirectly via those helpers.
//
// Environment (issue #57): the default stays "node" so the pure-logic suite (no DOM,
// no component render) keeps its fast, no-jsdom-overhead path. A test that renders a
// component needs jsdom instead — opt in per file with a leading
//   // @vitest-environment jsdom
// comment (a Vitest/Jest convention: the whole file, not just Testing Library's render(),
// switches environment). Vitest ships the jsdom adapter itself; only the `jsdom` package
// needs to be a devDependency, alongside `@testing-library/react` for rendering and
// querying. See src/modules/plugins/__tests__/*.test.tsx for the pattern — those files
// import "@testing-library/react", whose auto-cleanup registers via the `afterEach` global
// this config's `globals: true` already provides, so no extra setup file is needed.
//
// ⚠️ DISTINCT FILE PER SPECIFIER — DO NOT COLLAPSE THESE BACK TO ONE STUB (issue #64).
// `vi.mock(specifier, factory)` keys its registry on the specifier's *resolved module id*,
// not the specifier string. Before #64 every row below pointed at one shared file, so a
// test file that called `vi.mock("@tauri-apps/api/core", ...)` AND
// `vi.mock("@tauri-apps/api/event", ...)` was really registering two factories against one
// resolved path — only the last one took effect, and the *entire* module (not a merge) was
// replaced by it. There is no error: the test still runs, the earlier mock's exports are
// just gone, and the natural (wrong) conclusion is that the code under test is broken. It
// cost a real debugging cycle during #34 — see that test file's header and
// `src/components/__tests__/IdentityDebtPanel.repair.test.tsx`'s, which route multiple
// exports through a single `vi.mock` call specifically to dodge this. Giving every
// specifier its own resolved file makes that impossible by construction: two `vi.mock`
// calls naming two different specifiers below now always target two different paths. If
// you add a new @tauri-apps/* import, add its OWN stub file here — do not point it at an
// existing one just because the existing one already exports what you need.
export default defineConfig({
  test: {
    environment: "node",
    globals: true,
    include: ["src/**/*.test.ts", "src/**/*.test.tsx"],
    alias: {
      "@tauri-apps/api/core": resolve(__dirname, "src/__test_stubs__/tauri-api-core.ts"),
      "@tauri-apps/api/app": resolve(__dirname, "src/__test_stubs__/tauri-api-app.ts"),
      "@tauri-apps/api/event": resolve(__dirname, "src/__test_stubs__/tauri-api-event.ts"),
      "@tauri-apps/plugin-dialog": resolve(__dirname, "src/__test_stubs__/tauri-plugin-dialog.ts"),
      "@tauri-apps/plugin-deep-link": resolve(
        __dirname,
        "src/__test_stubs__/tauri-plugin-deep-link.ts",
      ),
      "@tauri-apps/plugin-opener": resolve(__dirname, "src/__test_stubs__/tauri-plugin-opener.ts"),
      // React useSyncExternalStore is fine in Node; only import side-effects matter.
    },
  },
});
