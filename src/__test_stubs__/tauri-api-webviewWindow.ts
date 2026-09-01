/**
 * Test stub for `@tauri-apps/api/webviewWindow`. Aliased in `vitest.config.ts` (one file
 * per `@tauri-apps/*` specifier — see the alias block there for why).
 *
 * Observable so tests can drive the host's `openLoupe`: `__windows` records every window
 * constructed and every focus, and `open` names the labels `getByLabel` should report as
 * already open.
 */

export const __windows = {
  created: [] as { label: string; options: Record<string, unknown> }[],
  focused: [] as string[],
  open: new Set<string>(),
  reset() {
    this.created.length = 0;
    this.focused.length = 0;
    this.open.clear();
  },
};

export class WebviewWindow {
  label: string;

  constructor(label: string, options: Record<string, unknown> = {}) {
    this.label = label;
    __windows.created.push({ label, options });
  }

  static getByLabel(label: string): Promise<WebviewWindow | null> {
    if (!__windows.open.has(label)) return Promise.resolve(null);
    // An existing window, not a construction — so it is not recorded as created.
    const w = Object.create(WebviewWindow.prototype) as WebviewWindow;
    w.label = label;
    return Promise.resolve(w);
  }

  setFocus(): Promise<void> {
    __windows.focused.push(this.label);
    return Promise.resolve();
  }
}
