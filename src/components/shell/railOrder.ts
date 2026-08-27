// Ordering for the icon rail's module items (see IconRail.tsx). Pure — no state, no host
// access — so App.tsx can call it directly on `mainViews()` and the result is a plain
// prop.
import type { MainView } from "../../modules/registry";

/** Ids that get a fixed spot at the front of the rail, in this order. A view whose id
 *  isn't listed here appends after these, in the order it arrived in. */
const PREFERRED_ORDER = ["map", "people", "statistics", "tag-graph"];

/** Orders module main views for the icon rail: preferred ids first (in the fixed order
 *  above), then every other view, in input order. Does not mutate `views`. */
export function railOrder(views: MainView[]): MainView[] {
  const preferred: MainView[] = [];
  for (const id of PREFERRED_ORDER) {
    const v = views.find((view) => view.id === id);
    if (v) preferred.push(v);
  }
  const rest = views.filter((view) => !PREFERRED_ORDER.includes(view.id));
  return [...preferred, ...rest];
}
