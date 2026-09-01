// Radial hierarchical edge bundling (Holten 2006) for the Tag Graph.
//
// Every tag with photos gets a slot on a ring, ordered by a depth-first walk of the tag
// hierarchy so children sit next to their parent and each top-level community is a
// contiguous arc. Internal hierarchy nodes are placed at inner radii by height (d3's
// cluster layout), and an edge between two ring nodes is routed through their lowest
// common ancestor, then smoothed with a straightened B-spline — so edges between the
// same two families share a path and read as one bundle.
//
// Pure layout, no rendering: the component paints the result onto its canvas.

export interface BundleNode {
  id: string;
  kind: "tag" | "camera" | "photo";
  /** "Animals/Bird/Seagull" for tags; the model name for cameras. */
  fullPath: string;
  count: number;
}

export interface RingPlacement {
  /** Radians, canvas convention (0 = 3 o'clock, increasing clockwise). */
  angle: number;
  x: number;
  y: number;
}

export interface BundleGroup {
  /** Top-level community name, or CAMERA_GROUP for the cameras arc. */
  name: string;
  /** Angular span of the group's slots, radians. */
  a0: number;
  a1: number;
  /** Sum of member counts — the ordering key. */
  total: number;
}

export interface BundleLayout {
  R: number;
  /** Ring node ids in angular order. */
  order: string[];
  placement: Map<string, RingPlacement>;
  /** Groups in ring order. */
  groups: BundleGroup[];
  /** Angular width of one ring slot, radians. */
  slot: number;
  /** Ring node ids by count, descending — label priority. */
  labelOrder: string[];
  maxCount: number;
  /**
   * Control polyline for an edge between two ring nodes: up from `a` to their lowest
   * common ancestor and down to `b`. Null if either id is not on the ring.
   */
  path(a: string, b: string): [number, number][] | null;
}

/** The synthetic top-level group cameras hang under. */
export const CAMERA_GROUP = "__camera";

/**
 * A tag path relative to a branch root: "" for the root itself, "Bird/Seagull" for a
 * descendant, null when the path is outside the branch. Segment-exact — "Animals" does
 * not contain "AnimalsX".
 */
export function relativeToBranch(path: string, rootPath: string): string | null {
  if (path === rootPath) return "";
  return path.startsWith(`${rootPath}/`) ? path.slice(rootPath.length + 1) : null;
}

/** The parent path of a tag path, or null at the top level. */
export function parentPath(path: string): string | null {
  const i = path.lastIndexOf("/");
  return i === -1 ? null : path.slice(0, i);
}

// Slot gaps: between sibling subtrees and between top-level groups (in slot widths).
const SIBLING_GAP = 0.35;
const GROUP_GAP = 2.5;

interface TreeNode {
  name: string;
  parent: TreeNode | null;
  children: TreeNode[];
  /** Graph node this tree node stands for (internal tags too; see `selfLeaf`). */
  node: BundleNode | null;
  /** Set on leaves only: the ring node drawn at this position. */
  ring: BundleNode | null;
  total: number;
  height: number;
  angle: number;
  x: number;
  y: number;
}

const mk = (name: string, parent: TreeNode | null): TreeNode => ({
  name,
  parent,
  children: [],
  node: null,
  ring: null,
  total: 0,
  height: 0,
  angle: 0,
  x: 0,
  y: 0,
});

const segments = (n: BundleNode): string[] =>
  n.kind === "camera" ? [CAMERA_GROUP, n.fullPath] : n.fullPath.split("/");

export function buildBundleLayout(nodes: BundleNode[], R: number): BundleLayout {
  const root = mk("", null);

  // 1. Trie over paths. Order of first appearance is the tie-break for every sort.
  for (const n of nodes) {
    let t = root;
    for (const seg of segments(n)) {
      let c = t.children.find((ch) => ch.name === seg);
      if (!c) {
        c = mk(seg, t);
        t.children.push(c);
      }
      t = c;
    }
    t.node = n;
  }

  // 2. A tag with photos AND children gets a "self" leaf in front of its children, so
  //    it takes a ring slot next to them while the tree node stays internal (the
  //    bundle waypoint for its subtree).
  const walk = (t: TreeNode) => {
    for (const c of t.children) walk(c);
    if (t.node) {
      if (t.children.length === 0) t.ring = t.node;
      else {
        const self = mk(t.name, t);
        self.node = t.node;
        self.ring = t.node;
        t.children.unshift(self);
      }
    }
  };
  walk(root);

  // 3. Subtree totals and heights, bottom-up; then order children by total (self
  //    leaf first) so each arc reads big-to-small.
  const measure = (t: TreeNode): void => {
    if (t.ring) {
      t.total = t.ring.count;
      t.height = 0;
      return;
    }
    t.total = 0;
    t.height = 0;
    for (const c of t.children) {
      measure(c);
      t.total += c.total;
      t.height = Math.max(t.height, c.height + 1);
    }
    t.children.sort((a, b) => {
      if (a.ring && a.node === t.node) return -1;
      if (b.ring && b.node === t.node) return 1;
      return b.total - a.total;
    });
  };
  measure(root);

  // 4. Ring slots: depth-first leaf order, with gaps where the parent changes.
  const leaves: TreeNode[] = [];
  collectLeaves(root, leaves);

  const topOf = (t: TreeNode): TreeNode => {
    let n = t;
    while (n.parent && n.parent !== root) n = n.parent;
    return n;
  };

  const cursorFor: number[] = [];
  let cursor = 0;
  let prev: TreeNode | null = null;
  for (const leaf of leaves) {
    if (prev) {
      if (topOf(prev) !== topOf(leaf)) cursor += GROUP_GAP;
      else if (prev.parent !== leaf.parent) cursor += SIBLING_GAP;
    }
    cursorFor.push(cursor);
    cursor += 1;
    prev = leaf;
  }
  // Close the ring with a group gap so the first and last groups don't touch.
  const totalSlots = leaves.length ? cursor + GROUP_GAP : 1;
  const slot = (Math.PI * 2) / totalSlots;
  const start = -Math.PI / 2; // first group begins at 12 o'clock

  const placement = new Map<string, RingPlacement>();
  const order: string[] = [];
  leaves.forEach((leaf, i) => {
    const angle = start + (cursorFor[i] + 0.5) * slot;
    leaf.angle = angle;
    leaf.x = R * Math.cos(angle);
    leaf.y = R * Math.sin(angle);
    placement.set(leaf.ring!.id, { angle, x: leaf.x, y: leaf.y });
    order.push(leaf.ring!.id);
  });

  // 5. Internal nodes: angle between first and last child, radius by height so every
  //    subtree ends on the ring (d3.cluster). The root sits at the centre.
  const rootH = Math.max(1, root.height);
  const place = (t: TreeNode) => {
    if (t.ring) return;
    for (const c of t.children) place(c);
    if (t.children.length) {
      t.angle = (t.children[0].angle + t.children[t.children.length - 1].angle) / 2;
    }
    const radius = (1 - t.height / rootH) * R;
    t.x = radius * Math.cos(t.angle);
    t.y = radius * Math.sin(t.angle);
  };
  place(root);
  root.x = 0;
  root.y = 0;

  // 6. Groups: one arc per top-level child that has ring slots.
  const groups: BundleGroup[] = [];
  for (const g of root.children) {
    const own: TreeNode[] = [];
    collectLeaves(g, own);
    if (!own.length) continue;
    groups.push({
      name: g.name,
      a0: own[0].angle - slot / 2,
      a1: own[own.length - 1].angle + slot / 2,
      total: g.total,
    });
  }

  const leafById = new Map<string, TreeNode>();
  for (const leaf of leaves) leafById.set(leaf.ring!.id, leaf);

  const path = (a: string, b: string): [number, number][] | null => {
    const la = leafById.get(a);
    const lb = leafById.get(b);
    if (!la || !lb) return null;
    const up: TreeNode[] = [];
    for (let t: TreeNode | null = la; t; t = t.parent) up.push(t);
    const upSet = new Set(up);
    const down: TreeNode[] = [];
    let lca: TreeNode | null = null;
    for (let t: TreeNode | null = lb; t; t = t.parent) {
      if (upSet.has(t)) {
        lca = t;
        break;
      }
      down.push(t);
    }
    if (!lca) return null;
    const pts: [number, number][] = [];
    for (const t of up) {
      pts.push([t.x, t.y]);
      if (t === lca) break;
    }
    for (let i = down.length - 1; i >= 0; i--) pts.push([down[i].x, down[i].y]);
    return pts;
  };

  let maxCount = 1;
  for (const leaf of leaves) maxCount = Math.max(maxCount, leaf.ring!.count);
  const labelOrder = [...order].sort(
    (x, y) => leafById.get(y)!.ring!.count - leafById.get(x)!.ring!.count,
  );

  return { R, order, placement, groups, slot, labelOrder, maxCount, path };
}

/** Ring leaves under `t`, depth-first — the ring order. */
function collectLeaves(t: TreeNode, out: TreeNode[]) {
  if (t.ring) out.push(t);
  for (const c of t.children) collectLeaves(c, out);
}

/** The subset of the canvas path API the bundle spline emits. */
export interface PathSink {
  moveTo(x: number, y: number): void;
  lineTo(x: number, y: number): void;
  bezierCurveTo(x1: number, y1: number, x2: number, y2: number, x: number, y: number): void;
}

/**
 * Append the bundled curve through `pts` to `sink`. This is d3-shape's curveBundle
 * (straighten each control point toward the chord by `1 - beta`) feeding curveBasis
 * (a uniform cubic B-spline through the straightened points), transcribed so the
 * module needs no dependency for two small routines.
 */
export function bundlePath(sink: PathSink, pts: [number, number][], beta: number): void {
  const j = pts.length - 1;
  if (j < 1) return;
  const [x0, y0] = pts[0];
  const dx = pts[j][0] - x0;
  const dy = pts[j][1] - y0;

  // curveBasis state machine.
  let bx0 = NaN;
  let by0 = NaN;
  let bx1 = NaN;
  let by1 = NaN;
  let state = 0;
  const bezier = (x: number, y: number) => {
    sink.bezierCurveTo(
      (2 * bx0 + bx1) / 3,
      (2 * by0 + by1) / 3,
      (bx0 + 2 * bx1) / 3,
      (by0 + 2 * by1) / 3,
      (bx0 + 4 * bx1 + x) / 6,
      (by0 + 4 * by1 + y) / 6,
    );
  };
  const point = (x: number, y: number) => {
    switch (state) {
      case 0:
        state = 1;
        sink.moveTo(x, y);
        break;
      case 1:
        state = 2;
        break;
      case 2:
        state = 3;
        sink.lineTo((5 * bx0 + bx1) / 6, (5 * by0 + by1) / 6);
        bezier(x, y);
        break;
      default:
        bezier(x, y);
    }
    bx0 = bx1;
    bx1 = x;
    by0 = by1;
    by1 = y;
  };

  for (let i = 0; i <= j; i++) {
    const t = i / j;
    point(
      beta * pts[i][0] + (1 - beta) * (x0 + t * dx),
      beta * pts[i][1] + (1 - beta) * (y0 + t * dy),
    );
  }
  // lineEnd
  if (state === 3) bezier(bx1, by1);
  if (state >= 2) sink.lineTo(bx1, by1);
}
