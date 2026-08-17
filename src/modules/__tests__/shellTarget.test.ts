import { describe, expect, it } from "vitest";

import { shellTarget } from "../shellTarget";
import type { Photo } from "../registry";

const photo = (id: number): Photo =>
  ({ id, uuid: `u${id}`, path: `DSC_${id}.ARW`, rating: 0, label: "", pickState: "none" }) as Photo;

describe("shellTarget", () => {
  it("follows the selection when Compare is closed", () => {
    const t = shellTarget({
      selected: photo(1),
      activeId: 1,
      compareFocus: null,
      activeEditJson: '{"exposure":1}',
    });
    expect(t.photo?.id).toBe(1);
    expect(t.broadcastId).toBe(1);
    expect(t.editJson).toBe('{"exposure":1}');
  });

  it("follows Compare's focused pane while comparing", () => {
    const t = shellTarget({
      selected: photo(1),
      activeId: 1,
      compareFocus: photo(3),
      activeEditJson: null,
    });
    expect(t.photo?.id).toBe(3);
    expect(t.broadcastId).toBe(3);
  });

  it("never sends the active photo's edit with a different frame", () => {
    // The bug this guards: `activeVersion` belongs to photo 1. Broadcasting it alongside
    // photo 3 would render photo 1's crop and tone curve over photo 3 in the pop-out —
    // wrong, and with nothing on screen to explain why.
    const t = shellTarget({
      selected: photo(1),
      activeId: 1,
      compareFocus: photo(3),
      activeEditJson: '{"exposure":1}',
    });
    expect(t.photo?.id).toBe(3);
    expect(t.editJson).toBeNull();
  });

  it("keeps the edit when the focused pane IS the active photo", () => {
    const t = shellTarget({
      selected: photo(1),
      activeId: 1,
      compareFocus: photo(1),
      activeEditJson: '{"exposure":1}',
    });
    expect(t.broadcastId).toBe(1);
    expect(t.editJson).toBe('{"exposure":1}');
  });

  it("falls back to the active id when the selected row is momentarily absent", () => {
    // Mid-refresh the session can hold an activeId whose row it no longer has; the pop-out
    // should keep showing that photo rather than blanking.
    const t = shellTarget({
      selected: null,
      activeId: 7,
      compareFocus: null,
      activeEditJson: '{"exposure":1}',
    });
    expect(t.photo).toBeNull();
    expect(t.broadcastId).toBe(7);
    expect(t.editJson).toBe('{"exposure":1}');
  });

  it("has nothing to show when nothing is selected", () => {
    const t = shellTarget({
      selected: null,
      activeId: null,
      compareFocus: null,
      activeEditJson: null,
    });
    expect(t.photo).toBeNull();
    expect(t.broadcastId).toBeNull();
    expect(t.editJson).toBeNull();
  });
});
