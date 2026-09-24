// The Darkroom filmstrip's window and stepping (pure).
import { describe, expect, it } from "vitest";
import { arrowsBelongToTarget, stepTarget, windowAround } from "../filmstrip";

const ids = Array.from({ length: 100 }, (_, i) => i + 1);

describe("windowAround", () => {
  it("centres on the current photo, clipped at the ends", () => {
    expect(windowAround(ids, 50, 3)).toEqual({ start: 46, ids: [47, 48, 49, 50, 51, 52, 53] });
    expect(windowAround(ids, 1, 3)).toEqual({ start: 0, ids: [1, 2, 3, 4] });
    expect(windowAround(ids, 100, 3)).toEqual({ start: 96, ids: [97, 98, 99, 100] });
  });
  it("shows the start of the list when the current photo is not in it", () => {
    expect(windowAround(ids, 999, 2).ids).toEqual([1, 2, 3, 4, 5]);
  });
});

describe("stepTarget", () => {
  it("steps within the list and stops at the ends", () => {
    expect(stepTarget(ids, 50, 1)).toBe(51);
    expect(stepTarget(ids, 50, -1)).toBe(49);
    expect(stepTarget(ids, 100, 1)).toBeNull();
    expect(stepTarget(ids, 1, -1)).toBeNull();
    expect(stepTarget(ids, 999, 1)).toBeNull();
  });
});

describe("arrowsBelongToTarget", () => {
  it("leaves arrows to sliders, fields and selects", () => {
    const el = (tagName: string, extra: object = {}) => ({ tagName, isContentEditable: false, ...extra }) as unknown as EventTarget;
    expect(arrowsBelongToTarget(el("INPUT"))).toBe(true); // range sliders included
    expect(arrowsBelongToTarget(el("TEXTAREA"))).toBe(true);
    expect(arrowsBelongToTarget(el("SELECT"))).toBe(true);
    expect(arrowsBelongToTarget(el("DIV", { isContentEditable: true }))).toBe(true);
    expect(arrowsBelongToTarget(el("BUTTON"))).toBe(false);
    expect(arrowsBelongToTarget(null)).toBe(false);
  });
});
