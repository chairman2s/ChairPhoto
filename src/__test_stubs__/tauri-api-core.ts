/**
 * Test stub for `@tauri-apps/api/core`. Aliased in `vitest.config.ts`.
 *
 * One file per `@tauri-apps/*` specifier (issue #64): `vi.mock()` keys its factory
 * registry on the resolved module id, so if two specifiers shared one file, mocking both
 * in the same test would silently collapse to a single registration. See the alias block
 * in `vitest.config.ts` for the full explanation.
 */

export const invoke = () => Promise.resolve(null);
export const convertFileSrc = (p: string) => `asset:///${p}`;
