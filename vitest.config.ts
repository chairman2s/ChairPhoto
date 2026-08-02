import { defineConfig } from "vitest/config";
import { resolve } from "path";

// vitest.config.ts — runs the frontend unit tests (npx vitest / npm test).
//
// Tauri APIs (and some React hooks) are platform-only and unavailable in a
// plain Node/jsdom environment.  We alias every @tauri-apps/* import to a
// centralised stub module so host.ts and api.ts load cleanly during tests.
// Only the pure-logic exports (satisfies, hostSatisfies, unmetRequirement,
// register, enableModule, …) are exercised; the Tauri-backed runtime paths
// are tested indirectly via those helpers.

export default defineConfig({
  test: {
    environment: "node",
    globals: true,
    include: ["src/**/*.test.ts", "src/**/*.test.tsx"],
    alias: {
      // Tauri runtime stubs — one file covers all @tauri-apps/* imports.
      "@tauri-apps/api/core": resolve(__dirname, "src/__test_stubs__/tauri.ts"),
      "@tauri-apps/api/app": resolve(__dirname, "src/__test_stubs__/tauri.ts"),
      "@tauri-apps/api/event": resolve(__dirname, "src/__test_stubs__/tauri.ts"),
      "@tauri-apps/plugin-dialog": resolve(__dirname, "src/__test_stubs__/tauri.ts"),
      "@tauri-apps/plugin-deep-link": resolve(__dirname, "src/__test_stubs__/tauri.ts"),
      "@tauri-apps/plugin-opener": resolve(__dirname, "src/__test_stubs__/tauri.ts"),
      // React useSyncExternalStore is fine in Node; only import side-effects matter.
    },
  },
});
