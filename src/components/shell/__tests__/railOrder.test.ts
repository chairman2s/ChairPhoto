import { describe, expect, it } from "vitest";

import { railOrder } from "../railOrder";
import type { MainView } from "../../../modules/registry";

function view(id: string): MainView {
  return { id, label: id };
}

describe("railOrder", () => {
  it("sorts preferred ids into map, people, statistics, tag-graph regardless of input order", () => {
    const views = [view("tag-graph"), view("statistics"), view("people"), view("map")];
    expect(railOrder(views).map((v) => v.id)).toEqual([
      "map",
      "people",
      "statistics",
      "tag-graph",
    ]);
  });

  it("appends views with an unlisted id after the preferred ones, in their input order", () => {
    const views = [view("custom-b"), view("map"), view("custom-a"), view("people")];
    expect(railOrder(views).map((v) => v.id)).toEqual(["map", "people", "custom-b", "custom-a"]);
  });

  it("appends only unlisted ids, in input order, when none is preferred", () => {
    const views = [view("z"), view("a"), view("m")];
    expect(railOrder(views).map((v) => v.id)).toEqual(["z", "a", "m"]);
  });

  it("returns an empty array for an empty input", () => {
    expect(railOrder([])).toEqual([]);
  });

  it("does not mutate its input", () => {
    const views = [view("tag-graph"), view("map")];
    const copy = [...views];
    railOrder(views);
    expect(views).toEqual(copy);
  });
});
