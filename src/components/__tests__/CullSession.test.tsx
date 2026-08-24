// @vitest-environment jsdom
/**
 * A cull session is a keyboard contract, so that is what is pinned — not the layout:
 *
 *  - it is the app's *existing* keymap. 0-5, p/x/u and the colour letters send the same
 *    commands they send in the grid; a second dialect of the same shortcuts would destroy
 *    the muscle memory that is the entire point of a session mode.
 *  - every decision advances, and ← goes back, so a mis-hit is correctable in place.
 *  - the cursor stops at the ends. A session that silently wraps makes "have I seen
 *    everything?" unanswerable.
 *  - the cursor is a *photo*, persisted to the catalog, and resuming says which happened:
 *    picked up where you left off, or that photo is not in this set.
 *  - the HUD shows what you just pressed, from the session's own record — the frozen row
 *    still carries the old rating.
 *  - the summary counts a photo you looked at and left alone as reviewed but not decided,
 *    because leaving it alone was the decision.
 */
import { describe, expect, it, vi, beforeEach } from "vitest";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { CullSession } from "../CullSession";
import type { Photo } from "../../modules/registry";

const calls: { command: string; args: Record<string, unknown> }[] = [];
/** What `get_setting` returns for the cull cursor; replaced per test. */
let storedCursor: string | null = null;

vi.mock("@tauri-apps/api/core", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@tauri-apps/api/core")>();
  return {
    ...actual,
    convertFileSrc: (id: string) => `preview://${id}`,
    invoke: (command: string, args: Record<string, unknown>) => {
      calls.push({ command, args: args ?? {} });
      if (command === "get_setting") return Promise.resolve(storedCursor);
      if (command === "set_setting") return Promise.resolve(null);
      return Promise.resolve(null);
    },
  };
});

function photo(id: number, over: Partial<Photo> = {}): Photo {
  return {
    id,
    uuid: `uuid-${id}`,
    path: `2024/DSC_${String(id).padStart(4, "0")}.NEF`,
    rating: 0,
    label: "",
    pickState: "none",
    captureTime: "2024-01-01T12:00:00",
    width: 6000,
    height: 4000,
    cameraModel: null,
    lens: null,
    aperture: null,
    shutterSpeed: null,
    iso: null,
    metadataReady: 1,
    sharpness: null,
    sharpnessMethod: null,
    burstFlag: null,
    ...over,
  };
}

const set = (n: number) => Array.from({ length: n }, (_, i) => photo(i + 1));
const text = () => (document.body.textContent ?? "").replace(/\s+/g, " ");
const press = (key: string) => fireEvent.keyDown(window, { key });
const sent = (command: string) => calls.filter((c) => c.command === command);

/** Render and wait for the cursor lookup to resolve. */
async function open(photos: Photo[], onExit = vi.fn()) {
  render(<CullSession photos={photos} onExit={onExit} />);
  await waitFor(() => expect(text()).toMatch(/1 \/|\d+ \/ \d+/));
  return onExit;
}

beforeEach(() => {
  calls.length = 0;
  storedCursor = null;
});

describe("CullSession", () => {
  it("sends the same commands the grid's culling keys send", async () => {
    await open(set(4));

    press("3");
    await waitFor(() => expect(sent("set_rating").length).toBe(1));
    expect(sent("set_rating")[0].args).toEqual({ photoId: 1, rating: 3 });

    press("x");
    await waitFor(() => expect(sent("set_pick_state").length).toBe(1));
    expect(sent("set_pick_state")[0].args).toEqual({ photoId: 2, pickState: "reject" });

    press("g");
    await waitFor(() => expect(sent("set_label").length).toBe(1));
    expect(sent("set_label")[0].args).toEqual({ photoId: 3, label: "Green" });
  });

  it("advances on a decision and steps back on the arrow", async () => {
    await open(set(3));

    expect(text()).toContain("1 / 3");
    press("p");
    await waitFor(() => expect(text()).toContain("2 / 3"));

    press("ArrowLeft");
    await waitFor(() => expect(text()).toContain("1 / 3"));
    // Back on the first photo, a correction lands on *it*, not on the one advanced past.
    press("u");
    await waitFor(() => expect(sent("set_pick_state").length).toBe(2));
    expect(sent("set_pick_state")[1].args).toEqual({ photoId: 1, pickState: "none" });
  });

  it("moves on without deciding on space, and sends nothing", async () => {
    await open(set(3));

    press(" ");
    await waitFor(() => expect(text()).toContain("2 / 3"));
    expect(sent("set_rating")).toHaveLength(0);
    expect(sent("set_pick_state")).toHaveLength(0);
    expect(sent("set_label")).toHaveLength(0);
  });

  it("stops at both ends rather than wrapping", async () => {
    await open(set(2));

    press("ArrowLeft");
    expect(text()).toContain("1 / 2");

    press("ArrowRight");
    await waitFor(() => expect(text()).toContain("2 / 2"));
    press("ArrowRight");
    expect(text()).toContain("2 / 2");
    expect(text()).toMatch(/End of set/);
  });

  it("shows the decision just made, not the row it was given", async () => {
    // The frozen row still says 2 stars; the HUD must show the 5 that was just pressed.
    await open([photo(1, { rating: 2 }), photo(2)]);

    expect(text()).toContain("★★");
    press("5");
    await waitFor(() => expect(text()).toContain("2 / 2"));

    press("ArrowLeft");
    await waitFor(() => expect(text()).toContain("1 / 2"));
    expect(text()).toContain("★★★★★");
  });

  it("resumes at the stored photo and says so", async () => {
    storedCursor = "3";
    await open(set(5));

    await waitFor(() => expect(text()).toContain("3 / 5"));
    expect(text()).toMatch(/Resumed where you left off/);
  });

  it("says when the stored photo is not in this set instead of resuming silently", async () => {
    storedCursor = "999";
    await open(set(3));

    await waitFor(() => expect(text()).toMatch(/isn't in this set/));
    expect(text()).toContain("1 / 3");
  });

  it("starts at the beginning with no note when there is no stored cursor", async () => {
    await open(set(3));

    expect(text()).toContain("1 / 3");
    expect(text()).not.toMatch(/Resumed where/);
    expect(text()).not.toMatch(/isn't in this set/);
  });

  it("persists the cursor as a photo id when the session ends", async () => {
    await open(set(4));

    press("ArrowRight");
    await waitFor(() => expect(text()).toContain("2 / 4"));
    press("Escape");

    await waitFor(() => expect(sent("set_setting").length).toBeGreaterThan(0));
    const writes = sent("set_setting");
    expect(writes[writes.length - 1].args).toEqual({
      key: "cull.cursor.photo_id",
      value: "2",
    });
  });

  it("counts a photo left alone as reviewed but not decided", async () => {
    const onExit = await open(set(4));

    press("p"); // decide photo 1
    await waitFor(() => expect(text()).toContain("2 / 4"));
    press(" "); // look at photo 2, leave it
    await waitFor(() => expect(text()).toContain("3 / 4"));
    press("Escape");

    await waitFor(() => expect(text()).toMatch(/Session over/));
    expect(text()).toContain("3 of 4 photos reviewed");
    expect(text()).toContain("1 still to go");
    expect(text()).toMatch(/2 left as they were/);

    fireEvent.click(screen.getByText("Back to the grid"));
    expect(onExit).toHaveBeenCalledWith(
      expect.objectContaining({ visited: 3, decided: 1, picked: 1, remaining: 1 }),
    );
  });

  it("opens and closes the key help without ending the session", async () => {
    const onExit = await open(set(3));

    press("h");
    await waitFor(() => expect(text()).toContain("pick · reject · clear"));

    // Escape closes the help first — a reflex Escape must not end the session.
    press("Escape");
    await waitFor(() => expect(text()).not.toContain("pick · reject · clear"));
    expect(onExit).not.toHaveBeenCalled();
    expect(text()).toContain("1 / 3");
  });

  it("keeps consecutive keypresses on consecutive photos", async () => {
    // The mode is judged on how fast it feels, so keys arrive faster than React repaints:
    // both of these keydowns run before either advance has rendered. A handler reading the
    // rendered cursor would put both decisions on photo 1 and step past photo 2 unseen.
    await open(set(4));

    await act(async () => {
      press("3");
      press("x");
    });

    await waitFor(() => expect(sent("set_pick_state").length).toBe(1));
    expect(sent("set_rating")[0].args).toEqual({ photoId: 1, rating: 3 });
    expect(sent("set_pick_state")[0].args).toEqual({ photoId: 2, pickState: "reject" });
    expect(text()).toContain("3 / 4");
  });

  it("takes a decision back off the photo when the write fails", async () => {
    // A HUD showing a rating the catalog never received is worse than a visible error.
    await open([photo(1), photo(2)]);

    const core = await import("@tauri-apps/api/core");
    const spy = vi
      .spyOn(core, "invoke")
      .mockImplementation((command: string) =>
        command === "set_rating" ? Promise.reject("disk is read-only") : Promise.resolve(null),
      );

    press("4");
    await waitFor(() => expect(text()).toMatch(/Not saved/));
    expect(text()).toContain("disk is read-only");

    // Back on the photo, the stars it never got are not shown.
    press("ArrowLeft");
    await waitFor(() => expect(text()).toContain("1 / 2"));
    expect(text()).not.toContain("★★★★");
    spy.mockRestore();
  });

  it("does not act on keys typed into an input", async () => {
    await open(set(3));

    const input = document.createElement("input");
    document.body.appendChild(input);
    await act(async () => {
      fireEvent.keyDown(input, { key: "5" });
    });

    expect(sent("set_rating")).toHaveLength(0);
    expect(text()).toContain("1 / 3");
    input.remove();
  });
});
