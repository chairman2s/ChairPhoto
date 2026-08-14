// @vitest-environment jsdom
/**
 * The rendering ABI (issue #46): an external module draws through `mount(el)` / `unmount(el)`
 * and never touches the host's React instance; a bundled module keeps drawing through
 * `render(): ReactNode`.
 *
 * Both paths are exercised through the real wiring — `register` + `enableModule` to get the
 * module's injected `ChairPhotoAPI`, `panelsForSlot("inspector")` to read the contribution
 * back, and `<ModuleContent>` (what PhotoInspector/TagEditor/App/PublishDialog render) to
 * draw it — rather than through a hand-built fake, so a break in the adapter shows up here.
 *
 * The last case loads `examples/modules/hello/index.js` itself, by URL, the way `host.ts`
 * loads an external module (a dynamic `import()` of a resolved path). That file is plain JS
 * with no React import and no bundler step, so if it renders here it renders for the same
 * reason it renders in the app.
 */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";

import {
  __resetForTests,
  enableModule,
  panelsForSlot,
  register,
  setSelection,
} from "../host";
import { ModuleContent } from "../ModuleContent";
import type { ChairPhotoAPI, ChairPhotoModule, Photo } from "../registry";

afterEach(() => {
  cleanup();
  __resetForTests();
  vi.restoreAllMocks();
});

/** The one inspector panel `id` registered, drawn through the production adapter. */
function renderInspectorPanel(id: string) {
  const panel = panelsForSlot("inspector").find((p) => p.id === id);
  if (!panel) throw new Error(`no inspector panel "${id}" registered`);
  return render(<ModuleContent view={panel} />);
}

function photo(id: number): Photo {
  return {
    id,
    uuid: `uuid-${id}`,
    path: `p${id}.raf`,
    rating: 0,
    label: "",
    pickState: "none",
    captureTime: null,
    width: null,
    height: null,
    cameraModel: null,
    lens: null,
    aperture: null,
    shutterSpeed: null,
    iso: null,
    metadataReady: 1,
    sharpness: null,
    sharpnessMethod: null,
    burstFlag: null,
  };
}

describe("ModuleContent — the DOM path external modules target", () => {
  it("mount(el) puts a module's own DOM in the slot, with no React on the module side", () => {
    const mount = vi.fn((el: HTMLElement) => {
      const p = document.createElement("p");
      p.textContent = "drawn by an external module";
      el.append(p);
    });
    register({
      id: "dom-mod",
      name: "DOM",
      version: "1.0.0",
      onLoad: (api: ChairPhotoAPI) =>
        api.registerPanel({ id: "dom-panel", label: "DOM", slot: "inspector", mount }),
    });
    enableModule("dom-mod", false);

    renderInspectorPanel("dom-panel");

    expect(mount).toHaveBeenCalledTimes(1);
    expect(screen.getByText("drawn by an external module")).toBeTruthy();
    // The host gives the module one element to own and nothing else.
    expect(mount.mock.calls[0][0].className).toBe("module-mount");
  });

  it("unmount(el) runs and the host empties the element, so a remount starts clean", () => {
    const unmount = vi.fn();
    register({
      id: "dom-mod",
      name: "DOM",
      version: "1.0.0",
      onLoad: (api: ChairPhotoAPI) =>
        api.registerPanel({
          id: "dom-panel",
          label: "DOM",
          slot: "inspector",
          mount: (el) => el.append(document.createTextNode("content")),
          unmount,
        }),
    });
    enableModule("dom-mod", false);

    const view = render(<ModuleContent view={panelsForSlot("inspector")[0]} />);
    const el = view.container.querySelector(".module-mount") as HTMLElement;
    expect(el.textContent).toBe("content");

    view.unmount();

    expect(unmount).toHaveBeenCalledTimes(1);
    expect(unmount.mock.calls[0][0]).toBe(el);
    // The host clears it even though this module's unmount() left the DOM alone — a module
    // that forgets cleanup cannot leak into the next mount.
    expect(el.childNodes.length).toBe(0);
  });

  it("a throwing mount() is logged, not propagated into the slot's render", () => {
    const logged = vi.spyOn(console, "error").mockImplementation(() => {});
    register({
      id: "bad-mod",
      name: "Bad",
      version: "1.0.0",
      onLoad: (api: ChairPhotoAPI) =>
        api.registerPanel({
          id: "bad-panel",
          label: "Bad",
          slot: "inspector",
          mount: () => {
            throw new Error("boom");
          },
        }),
    });
    enableModule("bad-mod", false);

    expect(() => renderInspectorPanel("bad-panel")).not.toThrow();
    expect(logged).toHaveBeenCalled();
  });
});

describe("ModuleContent — the React path bundled modules keep using", () => {
  it("render() output lands in the slot with no wrapper element around it", () => {
    register({
      id: "react-mod",
      name: "React",
      version: "1.0.0",
      onLoad: (api: ChairPhotoAPI) =>
        api.registerPanel({
          id: "react-panel",
          label: "React",
          slot: "inspector",
          render: () => <p className="from-react">drawn by a bundled module</p>,
        }),
    });
    enableModule("react-mod", false);

    const { container } = renderInspectorPanel("react-panel");

    // Same DOM the slot produced before the adapter existed: the <p> is the direct child,
    // and the adapter contributes no `.module-mount` div on this path.
    expect(container.firstElementChild?.className).toBe("from-react");
    expect(container.querySelector(".module-mount")).toBeNull();
  });

  it("render() wins when a contribution supplies both paths", () => {
    const mount = vi.fn();
    register({
      id: "both-mod",
      name: "Both",
      version: "1.0.0",
      onLoad: (api: ChairPhotoAPI) =>
        api.registerPanel({
          id: "both-panel",
          label: "Both",
          slot: "inspector",
          render: () => <span>react wins</span>,
          mount,
        }),
    });
    enableModule("both-mod", false);

    renderInspectorPanel("both-panel");

    expect(screen.getByText("react wins")).toBeTruthy();
    expect(mount).not.toHaveBeenCalled();
  });
});

describe("the shipped example module (examples/modules/hello)", () => {
  it("renders into the inspector slot and repaints on a selection change", async () => {
    // A dynamic import of the example's entrypoint, the same call host.ts makes on an
    // external module — nothing about this file is compiled into the app bundle.
    const imported = (await import(
      // @ts-expect-error — plain JS outside `src` and outside tsconfig's `include`: this is
      // the file an external author copies, deliberately untyped and unbuilt.
      "../../../examples/modules/hello/index.js"
    )) as { default: ChairPhotoModule };
    const hello = imported.default;

    expect(hello.id).toBe("hello");
    // Registered as external but without the manifest's minHostVersion floor: this test is
    // about what the module draws, and `hostVersion` is "" outside initHost, which
    // hostSatisfies conservatively treats as unmet (covered in host.test.ts).
    register(hello, { external: true });
    enableModule("hello", false);

    renderInspectorPanel("hello-panel");
    expect(screen.getByText("No photo selected.")).toBeTruthy();

    // The host's own selection sink — what App calls when the user clicks a thumbnail.
    setSelection([photo(7), photo(8)], 7);

    expect(screen.getByText("2 photo(s) selected — active id 7.")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Say hello" })).toBeTruthy();
  });
});
