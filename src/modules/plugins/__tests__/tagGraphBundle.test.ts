// Layout invariants for the Tag Graph's radial edge bundling: ring order follows the
// hierarchy, groups are contiguous arcs, and edge paths route through the tree.
import { describe, expect, it } from "vitest";
import {
  buildBundleLayout,
  bundlePath,
  CAMERA_GROUP,
  parentPath,
  relativeToBranch,
  type BundleNode,
  type PathSink,
} from "../tagGraphBundle";

describe("relativeToBranch / parentPath", () => {
  it("is empty for the root itself and relative for descendants", () => {
    expect(relativeToBranch("Animals", "Animals")).toBe("");
    expect(relativeToBranch("Animals/Bird", "Animals")).toBe("Bird");
    expect(relativeToBranch("Animals/Bird/Seagull", "Animals")).toBe("Bird/Seagull");
    expect(relativeToBranch("Animals/Bird/Seagull", "Animals/Bird")).toBe("Seagull");
  });

  it("is null outside the branch, and segment-exact", () => {
    expect(relativeToBranch("Places/Oslo", "Animals")).toBeNull();
    expect(relativeToBranch("AnimalsX/Bird", "Animals")).toBeNull();
    expect(relativeToBranch("Animal", "Animals")).toBeNull();
  });

  it("parentPath climbs one level and stops at the top", () => {
    expect(parentPath("Animals/Bird/Seagull")).toBe("Animals/Bird");
    expect(parentPath("Animals/Bird")).toBe("Animals");
    expect(parentPath("Animals")).toBeNull();
  });
});

const tag = (fullPath: string, count: number): BundleNode => ({
  id: `t:${fullPath}`,
  kind: "tag",
  fullPath,
  count,
});
const camera = (model: string, count: number): BundleNode => ({
  id: `c:${model}`,
  kind: "camera",
  fullPath: model,
  count,
});

const R = 100;

describe("buildBundleLayout — ring order", () => {
  it("places every node on the ring, children right after their parent, self slot first", () => {
    const layout = buildBundleLayout(
      [
        tag("Animals", 5), // has children AND photos → self slot
        tag("Animals/Bird", 20),
        tag("Animals/Bird/Seagull", 15),
        tag("Animals/Bird/Dove", 3),
        tag("Animals/Dog", 30),
      ],
      R,
    );
    // The self slot leads its subtree; siblings order by subtree total, so Bird
    // (20 + 15 + 3 = 38) comes before Dog (30).
    expect(layout.order).toEqual([
      "t:Animals",
      "t:Animals/Bird",
      "t:Animals/Bird/Seagull",
      "t:Animals/Bird/Dove",
      "t:Animals/Dog",
    ]);
    expect(layout.placement.size).toBe(5);
    for (const id of layout.order) {
      const p = layout.placement.get(id)!;
      expect(Math.hypot(p.x, p.y)).toBeCloseTo(R, 6);
    }
  });

  it("orders groups by total and keeps angles increasing from 12 o'clock", () => {
    const layout = buildBundleLayout(
      [tag("Small/a", 1), tag("Big/a", 50), tag("Big/b", 40), tag("Mid/a", 10)],
      R,
    );
    expect(layout.groups.map((g) => g.name)).toEqual(["Big", "Mid", "Small"]);
    const angles = layout.order.map((id) => layout.placement.get(id)!.angle);
    for (let i = 1; i < angles.length; i++) expect(angles[i]).toBeGreaterThan(angles[i - 1]);
    expect(angles[0]).toBeGreaterThan(-Math.PI / 2);
    expect(angles[angles.length - 1]).toBeLessThan((3 * Math.PI) / 2);
  });

  it("leaves a wider gap between groups than between sibling subtrees", () => {
    const layout = buildBundleLayout(
      [tag("A/x/1", 1), tag("A/y/1", 1), tag("B/z/1", 1)],
      R,
    );
    const a = (id: string) => layout.placement.get(id)!.angle;
    const siblingGap = a("t:A/y/1") - a("t:A/x/1");
    const groupGap = a("t:B/z/1") - a("t:A/y/1");
    expect(siblingGap).toBeGreaterThan(layout.slot);
    expect(groupGap).toBeGreaterThan(siblingGap);
  });

  it("gives groups contiguous, non-overlapping arcs that cover their members", () => {
    const layout = buildBundleLayout(
      [tag("A/1", 3), tag("A/2", 2), tag("B/1", 9), tag("B/2", 1), tag("C", 1)],
      R,
    );
    for (let i = 1; i < layout.groups.length; i++) {
      expect(layout.groups[i].a0).toBeGreaterThan(layout.groups[i - 1].a1);
    }
    for (const id of layout.order) {
      const p = layout.placement.get(id)!;
      const groupName = id.slice(2).split("/")[0];
      const g = layout.groups.find((x) => x.name === groupName)!;
      expect(p.angle).toBeGreaterThan(g.a0);
      expect(p.angle).toBeLessThan(g.a1);
    }
  });

  it("puts cameras in their own group", () => {
    const layout = buildBundleLayout([tag("A/1", 1), camera("X100", 7)], R);
    expect(layout.groups.map((g) => g.name)).toEqual([CAMERA_GROUP, "A"]);
    expect(layout.placement.has("c:X100")).toBe(true);
  });

  it("orders labels by count and reports the maximum", () => {
    const layout = buildBundleLayout([tag("A/1", 3), tag("B/1", 30), tag("A/2", 10)], R);
    expect(layout.labelOrder).toEqual(["t:B/1", "t:A/2", "t:A/1"]);
    expect(layout.maxCount).toBe(30);
  });

  it("handles an empty input", () => {
    const layout = buildBundleLayout([], R);
    expect(layout.order).toEqual([]);
    expect(layout.groups).toEqual([]);
    expect(layout.path("a", "b")).toBeNull();
  });
});

describe("buildBundleLayout — edge paths", () => {
  const layout = buildBundleLayout(
    [tag("A/x/1", 1), tag("A/x/2", 1), tag("A/y/1", 1), tag("B/1", 1)],
    R,
  );

  it("routes siblings through their parent, which sits inside the ring", () => {
    const pts = layout.path("t:A/x/1", "t:A/x/2")!;
    expect(pts).toHaveLength(3);
    expect(pts[0]).toEqual([layout.placement.get("t:A/x/1")!.x, layout.placement.get("t:A/x/1")!.y]);
    expect(pts[2]).toEqual([layout.placement.get("t:A/x/2")!.x, layout.placement.get("t:A/x/2")!.y]);
    const parentR = Math.hypot(pts[1][0], pts[1][1]);
    expect(parentR).toBeGreaterThan(0);
    expect(parentR).toBeLessThan(R);
  });

  it("routes cousins through the shared ancestor, not the root", () => {
    const pts = layout.path("t:A/x/1", "t:A/y/1")!;
    // x/1 → x → A → y → y/1
    expect(pts).toHaveLength(5);
    for (const [px, py] of pts.slice(1, -1)) expect(Math.hypot(px, py)).toBeGreaterThan(0);
  });

  it("routes across groups through the centre", () => {
    const pts = layout.path("t:A/x/1", "t:B/1")!;
    // x/1 → x → A → root → B → B/1
    expect(pts).toHaveLength(6);
    expect(pts[3]).toEqual([0, 0]);
  });

  it("returns null for ids that are not on the ring", () => {
    expect(layout.path("t:A/x/1", "nope")).toBeNull();
    expect(layout.path("t:A", "t:B/1")).toBeNull(); // "A" has no photos, so no slot
  });
});

describe("bundlePath", () => {
  type Op = ["M" | "L" | "C", number[]];
  const record = (): { ops: Op[]; sink: PathSink } => {
    const ops: Op[] = [];
    return {
      ops,
      sink: {
        moveTo: (x, y) => ops.push(["M", [x, y]]),
        lineTo: (x, y) => ops.push(["L", [x, y]]),
        bezierCurveTo: (...a) => ops.push(["C", a]),
      },
    };
  };

  it("starts at the first point and ends at the last", () => {
    const { ops, sink } = record();
    bundlePath(sink, [[0, 0], [10, 40], [50, 50], [100, 0]], 0.85);
    expect(ops[0]).toEqual(["M", [0, 0]]);
    const last = ops[ops.length - 1];
    expect(last[0]).toBe("L");
    expect(last[1][0]).toBeCloseTo(100, 6);
    expect(last[1][1]).toBeCloseTo(0, 6);
    expect(ops.some((o) => o[0] === "C")).toBe(true);
  });

  it("with beta 0 straightens every control point onto the chord", () => {
    const { ops, sink } = record();
    bundlePath(sink, [[0, 0], [10, 90], [60, -30], [100, 100]], 0);
    // Every emitted coordinate must be collinear with the chord (0,0)→(100,100).
    for (const [, a] of ops) {
      for (let i = 0; i < a.length; i += 2) expect(a[i + 1]).toBeCloseTo(a[i], 6);
    }
  });

  it("emits nothing for fewer than two points", () => {
    const { ops, sink } = record();
    bundlePath(sink, [[1, 1]], 0.85);
    expect(ops).toEqual([]);
  });
});
