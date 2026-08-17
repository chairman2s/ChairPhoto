// @vitest-environment jsdom
/**
 * `ZoomableImage` became optionally controlled so Compare could drive several panes from
 * one transform. That refactor touched the component the loupe, the pop-out loupe and
 * Develop all render — none of which had a test.
 *
 * These cover the UNCONTROLLED path specifically: the one every pre-existing caller uses,
 * and the one that had to keep behaving exactly as before. The reset-on-photo-change
 * effect is the sharp edge — it is now conditional (`if (!controlled)`), so the way to
 * break the loupe without noticing is to make the reset apply only to the controlled case.
 */

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";

import { ZoomableImage } from "../ZoomableImage";

vi.mock("../../modules/previewCache", () => ({
  previewUrl: (id: number) => `preview://${id}`,
  zoomUrl: (id: number) => `zoom://${id}`,
  isVideoPath: () => false,
}));

const img = (c: HTMLElement) => c.querySelector("img.zoom-img") as HTMLElement;
const pane = (c: HTMLElement) => c.querySelector(".zoom-container") as HTMLElement;

afterEach(cleanup);

describe("ZoomableImage (uncontrolled — the loupe's path)", () => {
  it("starts at fit", () => {
    const { container } = render(<ZoomableImage photoId={1} />);
    expect(img(container).style.transform).toBe("translate(0px, 0px) scale(1)");
  });

  it("zooms itself on wheel, with no caller involvement", () => {
    const { container } = render(<ZoomableImage photoId={1} />);
    fireEvent.wheel(pane(container), { deltaY: -100, clientX: 50, clientY: 50 });
    expect(img(container).style.transform).not.toBe("translate(0px, 0px) scale(1)");
  });

  it("resets to fit when the photo changes", () => {
    // Stepping to the next photo in the loupe must not inherit the previous frame's zoom:
    // the new image would open showing an arbitrary corner.
    const { container, rerender } = render(<ZoomableImage photoId={1} />);
    fireEvent.wheel(pane(container), { deltaY: -100, clientX: 50, clientY: 50 });
    expect(img(container).style.transform).not.toBe("translate(0px, 0px) scale(1)");

    rerender(<ZoomableImage photoId={2} />);
    expect(img(container).style.transform).toBe("translate(0px, 0px) scale(1)");
  });

  it("does not zoom below fit", () => {
    const { container } = render(<ZoomableImage photoId={1} />);
    fireEvent.wheel(pane(container), { deltaY: 100, clientX: 50, clientY: 50 });
    expect(img(container).style.transform).toBe("translate(0px, 0px) scale(1)");
  });
});

describe("ZoomableImage (controlled — Compare's path)", () => {
  it("renders the caller's transform and reports gestures instead of applying them", () => {
    const onViewChange = vi.fn();
    const { container } = render(
      <ZoomableImage
        photoId={1}
        view={{ scale: 2, tx: 10, ty: 20 }}
        onViewChange={onViewChange}
      />,
    );
    expect(img(container).style.transform).toBe("translate(10px, 20px) scale(2)");

    fireEvent.wheel(pane(container), { deltaY: -100, clientX: 50, clientY: 50 });
    expect(onViewChange).toHaveBeenCalled();
    // The transform on screen is still the prop: a controlled component must not move
    // itself, or the panes would drift out of sync with each other.
    expect(img(container).style.transform).toBe("translate(10px, 20px) scale(2)");
  });

  it("leaves the caller's transform alone when the photo changes", () => {
    // The mirror of the uncontrolled reset: Compare holds zoom across a swap on purpose,
    // and a reset here would fight it.
    const onViewChange = vi.fn();
    const view = { scale: 2, tx: 10, ty: 20 };
    const { container, rerender } = render(
      <ZoomableImage photoId={1} view={view} onViewChange={onViewChange} />,
    );
    rerender(<ZoomableImage photoId={2} view={view} onViewChange={onViewChange} />);
    expect(img(container).style.transform).toBe("translate(10px, 20px) scale(2)");
    expect(onViewChange).not.toHaveBeenCalled();
  });
});
