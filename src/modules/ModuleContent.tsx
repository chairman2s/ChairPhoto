// The host-side adapter for the two module rendering paths.
//
// A bundled module is compiled with the app, shares its React instance, and contributes a
// `render(): ReactNode`. An external module is imported from disk, has no route to that
// React instance, and contributes `mount(el)` / `unmount(el)` instead — a DOM handle, so
// nothing in the ABI names React (see `ModuleMount` in registry.ts and
// docs/module-capabilities.md § "Rendering ABI").
//
// Every slot renders its contribution through <ModuleContent>, which picks the path:
// `render` if present (bundled), otherwise `mount`. The React path is a pass-through —
// <ModuleContent> emits no DOM of its own for it, so a bundled module's output lands in
// exactly the same place it did before this adapter existed. The DOM path gets one plain
// `<div class="module-mount">` to own.

import { useEffect, useMemo, useRef } from "react";
import type { ReactNode } from "react";
import type { ModuleMount, SettingsPanel, ToolbarAction } from "./registry";

/** A contribution that draws either way: React `render()` or DOM `mount()`. */
export interface ModuleRenderable extends ModuleMount {
  render?(): ReactNode;
}

/** True if `view` can draw at all (a contribution with neither path renders nothing). */
export function canRender(view: ModuleRenderable | undefined | null): boolean {
  return !!view && (typeof view.render === "function" || typeof view.mount === "function");
}

/**
 * Render a module contribution through whichever path it supplies. `render` wins when a
 * contribution has both, so a bundled module is never routed through the DOM adapter.
 *
 * Uses no hooks itself: which branch it takes depends on `view`'s shape, and a hook behind
 * that condition would break the rules of hooks if a module ever swapped shape. The DOM
 * branch is a separate component, so React unmounts/remounts cleanly if that happens.
 */
export function ModuleContent({
  view,
  className,
}: {
  view: ModuleRenderable;
  className?: string;
}): ReactNode {
  if (typeof view.render === "function") return view.render();
  if (typeof view.mount === "function") return <ModuleMountPoint view={view} className={className} />;
  return null;
}

/**
 * Give an external module a DOM element and let it draw.
 *
 * The effect owns the element for exactly as long as React keeps it mounted: `mount` on
 * attach, `unmount` then `replaceChildren()` on detach. Emptying the element is the host's
 * job on purpose — it makes `mount` safe to call again (React StrictMode mounts, unmounts
 * and remounts every effect in development), and it means a module that forgets `unmount`
 * still cannot leak its DOM into the next mount.
 *
 * Both calls run inside try/catch for the same reason `host.ts` wraps `onLoad`: an
 * external module is untrusted code (docs/plugin-system.md § "Trust model"), and a throw
 * from it must not take down the panel, inspector or window rendering it.
 */
function ModuleMountPoint({
  view,
  className,
}: {
  view: ModuleMount;
  className?: string;
}) {
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    try {
      view.mount?.(el);
    } catch (e) {
      console.error("[modules] mount() threw:", e);
    }
    return () => {
      try {
        view.unmount?.(el);
      } catch (e) {
        console.error("[modules] unmount() threw:", e);
      }
      el.replaceChildren();
    };
  }, [view]);

  return <div ref={ref} className={className ? `module-mount ${className}` : "module-mount"} />;
}

/**
 * A module's settings section. Bundled modules register a React thunk; external modules
 * register a `ModuleMount`. Both reach `ModuleContent` as one `ModuleRenderable`.
 *
 * The thunk case is wrapped in `useMemo` so the object identity stays stable across
 * re-renders — `ModuleMountPoint`'s effect keys on it, and a fresh object every render
 * would remount an external module's DOM on every parent render.
 */
export function ModuleSettings({ panel }: { panel: SettingsPanel }): ReactNode {
  const view = useMemo<ModuleRenderable>(
    () => (typeof panel === "function" ? { render: panel } : panel),
    [panel],
  );
  return <ModuleContent view={view} />;
}

/** True if this toolbar action opens a modal (either path) rather than firing on click. */
export function isModalAction(action: ToolbarAction): boolean {
  return typeof action.render === "function" || typeof action.mount === "function";
}

/**
 * A toolbar action's modal body. `ToolbarAction` is the one contribution whose draw call
 * takes an argument (`close`), so it gets its own adapter that binds it — memoised on
 * `[action, close]` so an external module's modal mounts once, not once per parent render.
 */
export function ModuleActionModal({
  action,
  close,
}: {
  action: ToolbarAction;
  close: () => void;
}): ReactNode {
  const view = useMemo<ModuleRenderable>(
    () => ({
      render: action.render ? () => action.render!(close) : undefined,
      mount: action.mount ? (el: HTMLElement) => action.mount!(el, close) : undefined,
      unmount: action.unmount ? (el: HTMLElement) => action.unmount!(el) : undefined,
    }),
    [action, close],
  );
  return <ModuleContent view={view} />;
}
