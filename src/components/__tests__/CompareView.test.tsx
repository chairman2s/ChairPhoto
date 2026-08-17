// @vitest-environment jsdom
/**
 * Compare view: the screen where one frame is chosen over its neighbours.
 *
 * Two properties are worth pinning here, because both are invisible to a rendering test
 * that only checks "the panes appeared":
 *
 *  - **The panes share one pan/zoom.** That is the entire reason the view exists — two
 *    frames at different zoom levels cannot be compared. It works by `ZoomableImage`
 *    becoming controlled, so the regression to guard is a pane quietly falling back to
 *    its own internal transform, which looks fine until you zoom.
 *  - **"Keep this" names the frame it was pressed on.** Promotion rejects every other
 *    compared frame, so an off-by-one here silently rejects the keeper.
 *
 * The mixed-dimensions warning is also asserted: at equal zoom, frames of unequal pixel
 * size do not show the same crop, and a comparison that looks aligned but isn't is worse
 * than one that admits it.
 */

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { CompareView, MAX_PANES } from "../CompareView";
import type { Photo } from "../../modules/registry";

// The protocol URLs resolve to native handlers that jsdom has no idea about; the component
// under test cares about layout and wiring, not about bytes arriving.
vi.mock("../../modules/previewCache", () => ({
  previewUrl: (id: number) => `preview://${id}`,
  zoomUrl: (id: number) => `zoom://${id}`,
  isVideoPath: () => false,
}));

function photo(id: number, over: Partial<Photo> = {}): Photo {
  return {
    id,
    uuid: `uuid-${id}`,
    path: `DSC_${id}.ARW`,
    rating: 0,
    label: "",
    pickState: "none",
    captureTime: null,
    width: 6000,
    height: 4000,
    ...over,
  } as Photo;
}

afterEach(cleanup);

describe("CompareView", () => {
  it("renders one pane per compared frame", () => {
    render(
      <CompareView
        photos={[photo(1), photo(2), photo(3)]}
        focusedId={1}
        softThreshold={null}
        onFocus={() => {}}
        onKeep={() => {}}
        onExit={() => {}}
      />,
    );
    expect(screen.getByText("DSC_1.ARW")).toBeTruthy();
    expect(screen.getByText("DSC_2.ARW")).toBeTruthy();
    expect(screen.getByText("DSC_3.ARW")).toBeTruthy();
    expect(screen.getAllByRole("button", { name: /keep this/i })).toHaveLength(3);
  });

  it("gives every pane the SAME transform, so zoom is shared rather than per-pane", () => {
    const { container } = render(
      <CompareView
        photos={[photo(1), photo(2)]}
        focusedId={1}
        softThreshold={null}
        onFocus={() => {}}
        onKeep={() => {}}
        onExit={() => {}}
      />,
    );
    const imgs = Array.from(container.querySelectorAll("img.zoom-img")) as HTMLElement[];
    expect(imgs).toHaveLength(2);

    // Zoom one pane. A wheel event on either container must move both, because the
    // transform lives in CompareView and is handed to each ZoomableImage as a prop.
    const panes = container.querySelectorAll(".zoom-container");
    fireEvent.wheel(panes[0], { deltaY: -100, clientX: 100, clientY: 100 });

    const transforms = imgs.map((i) => i.style.transform);
    expect(transforms[0]).toBe(transforms[1]);
    expect(transforms[0]).not.toContain("scale(1)");
  });

  it("keeps the frame whose own button was pressed, not the focused one", () => {
    const onKeep = vi.fn();
    render(
      <CompareView
        photos={[photo(1), photo(2), photo(3)]}
        focusedId={1}
        softThreshold={null}
        onFocus={() => {}}
        onKeep={onKeep}
        onExit={() => {}}
      />,
    );
    // Press the THIRD pane's button while the FIRST is focused: promotion must follow the
    // button, or clicking "keep" on a frame would reject that very frame.
    fireEvent.click(screen.getAllByRole("button", { name: /keep this/i })[2]);
    expect(onKeep).toHaveBeenCalledWith(3);
  });

  it("focuses a pane on pointer-down, so a pan gesture also moves the keys' target", () => {
    const onFocus = vi.fn();
    const { container } = render(
      <CompareView
        photos={[photo(1), photo(2)]}
        focusedId={1}
        softThreshold={null}
        onFocus={onFocus}
        onKeep={() => {}}
        onExit={() => {}}
      />,
    );
    fireEvent.mouseDown(container.querySelectorAll(".compare-pane")[1]);
    expect(onFocus).toHaveBeenCalledWith(2);
  });

  it("warns when the frames have different pixel dimensions", () => {
    const { rerender, container } = render(
      <CompareView
        photos={[photo(1), photo(2)]}
        focusedId={1}
        softThreshold={null}
        onFocus={() => {}}
        onKeep={() => {}}
        onExit={() => {}}
      />,
    );
    // Same size: no warning, because the panes really do show the same crop.
    expect(container.querySelector(".compare-warn")).toBeNull();

    rerender(
      <CompareView
        photos={[photo(1), photo(2, { width: 4000, height: 3000 })]}
        focusedId={1}
        softThreshold={null}
        onFocus={() => {}}
        onKeep={() => {}}
        onExit={() => {}}
      />,
    );
    expect(container.querySelector(".compare-warn")).not.toBeNull();
  });

  it("marks the focused pane, and shows a rejected frame as rejected", () => {
    const { container } = render(
      <CompareView
        photos={[photo(1), photo(2, { pickState: "reject" })]}
        focusedId={2}
        softThreshold={null}
        onFocus={() => {}}
        onKeep={() => {}}
        onExit={() => {}}
      />,
    );
    const panes = container.querySelectorAll(".compare-pane");
    expect(panes[0].className).not.toContain("compare-pane-focused");
    expect(panes[1].className).toContain("compare-pane-focused");
    expect(panes[1].className).toContain("compare-pane-rejected");
    expect(screen.getByText("rejected")).toBeTruthy();
  });

  it("says what to do instead of rendering an empty grid", () => {
    render(
      <CompareView
        photos={[]}
        focusedId={null}
        softThreshold={null}
        onFocus={() => {}}
        onKeep={() => {}}
        onExit={() => {}}
      />,
    );
    expect(screen.getByText(/select two or more photos/i)).toBeTruthy();
  });

  it("caps at four panes — beyond that each frame is too small to judge", () => {
    expect(MAX_PANES).toBe(4);
  });
});
