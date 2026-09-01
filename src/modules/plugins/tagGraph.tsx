// Tag Graph module: the tag vocabulary as a radial edge-bundled graph — tags on a ring
// grouped by community, co-occurrence edges bundled through the hierarchy. Frontend-only;
// data comes from the `library_graph` command. See docs/tag-graph.md.
//
// The scene is painted onto a single <canvas>: layout, pan/zoom, and hover mutate refs
// and set a dirty flag that one rAF loop repaints. React re-renders only on real state
// changes (selection, panel toggles) — never per frame or per pointermove.
import { useEffect, useMemo, useRef, useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import {
  forceCenter,
  forceCollide,
  forceLink,
  forceManyBody,
  forceSimulation,
  forceX,
  forceY,
  type Simulation,
  type SimulationLinkDatum,
  type SimulationNodeDatum,
} from "d3-force";
import type { ChairPhotoAPI, ChairPhotoModule, LoupeCard } from "../registry";
import {
  buildBundleLayout,
  bundlePath,
  CAMERA_GROUP,
  parentPath,
  relativeToBranch,
  type BundleLayout,
} from "./tagGraphBundle";
import "./tagGraph.css";

// The Graph module: a full-surface view that visualizes the library as a graph.
//  • Communities — radial edge bundling: tags (and cameras) sit on a ring, grouped by
//    their top-level community; co-occurrence and camera↔tag edges curve through the
//    hierarchy in bundles (from the `library_graph` command). Deterministic — no physics.
//    Selecting a node opens an inspector; it does NOT navigate directly.
//  • Photo ↔ tag — a force-directed bipartite graph of photos and their tags (legacy view).
// Pure frontend (reads core catalog commands), so it needs no Cargo feature.

type Mode = "community" | "bipartite";
type Kind = "tag" | "camera" | "photo";

interface GNode extends SimulationNodeDatum {
  id: string; // "t<id>" | "c<idx>" | "p<id>"
  kind: Kind;
  refId: number;
  label: string; // leaf label for display
  fullPath: string; // full tag path (tags) or model (cameras)
  count: number;
  community: string; // top-level path segment (tags) / "__camera" / "__photo"
  r: number;
}
interface GLink extends SimulationLinkDatum<GNode> {
  weight: number;
  kind: "cooc" | "hierarchy" | "camera" | "bipartite";
}

/** Per-frame paint context: the view transform, canvas size, and resolved theme colours. */
interface PaintEnv {
  k: number;
  x: number;
  y: number;
  cw: number;
  ch: number;
  dpr: number;
  font: string;
  txtColor: string;
  haloColor: string;
  borderColor: string;
}

interface LibraryGraphData {
  tags: { id: number; label: string; count: number }[];
  cameras: { id: number; label: string; count: number }[];
  coocEdges: [number, number, number][];
  hierarchyEdges: [number, number][];
  cameraEdges: [number, number, number][];
}

interface RawNode {
  id: number;
  label: string;
  count: number;
}

const leaf = (path: string) => path.split("/").pop() || path;
const topLevel = (path: string) => path.split("/")[0] || path;
const endId = (e: GLink["source"]) => (typeof e === "object" ? (e as GNode).id : (e as string));

// Inside a branch, the ring is laid out from paths relative to the branch root, so its
// direct children become the arcs. The root tag itself takes a slot under its own name.
const layoutPath = (path: string, label: string, branch: string): string => {
  const rel = relativeToBranch(path, branch);
  return rel == null ? path : rel === "" ? label : rel;
};

// Community colour palette (families), cycled in size order. Amber is reserved for cameras
// so their arc is unmistakable.
const PALETTE = [
  "#3B82F6",
  "#8B5CF6",
  "#10B981",
  "#EC4899",
  "#14B8A6",
  "#F97316",
];
const CAMERA_COLOR = "#F59E0B";
const PHOTO_COLOR = "#64748B";

const tagRadius = (count: number) => 4 + Math.sqrt(count) * 2.2;

// Photo-node radius in the bipartite graph — large enough to hold a thumbnail, and used
// for collision so the layout reserves space whether thumbnails are shown or not.
const PHOTO_R = 13;

// Bipartite mode keeps only this many photos (the most-tagged ones). A six-figure library
// would otherwise hand d3-force more nodes than any layout or renderer can animate.
const BIPARTITE_PHOTO_CAP = 1500;

const NODE_STROKE = "rgba(15, 23, 42, 0.6)";

// Radial bundling geometry. The ring lives in graph units and the view scales it; every
// piece of chrome (dots, labels, group arcs) is drawn in screen pixels so it stays
// legible at any zoom.
const RING_R = 400;
const LABEL_EXTENT = 100; // px reserved outside the ring for tag labels
const LABEL_MAX_CHARS = 16;
const BUNDLE_BETA = 0.85; // 1 = follow the hierarchy exactly, 0 = straight chords

const truncate = (s: string) =>
  s.length > LABEL_MAX_CHARS ? `${s.slice(0, LABEL_MAX_CHARS - 1)}…` : s;

/** Normalize an angle difference into (-π, π]. */
const angleDiff = (a: number, b: number) => {
  const d = (a - b) % (Math.PI * 2);
  return d > Math.PI ? d - Math.PI * 2 : d <= -Math.PI ? d + Math.PI * 2 : d;
};

function GraphView({ api }: { api: ChairPhotoAPI }) {
  const [mode, setMode] = useState<Mode>("community");
  const [loaded, setLoaded] = useState<{
    nodes: GNode[];
    links: GLink[];
    /** Every tag path → id, including parents with no direct photos (which are not nodes). */
    tagIdByPath: Map<string, number>;
  } | null>(null);
  // The tag path the ring is scoped to (community mode), or null for the whole library.
  const [branch, setBranch] = useState<string | null>(null);

  // The graph on screen: the whole library, or one tag's branch — the tag and its
  // descendants plus any cameras, with `community` re-rooted to the branch's direct
  // children so the arcs, colours, and the Communities list are the branch's families.
  // Node objects are shared with `loaded` (positions live on them); only `community`
  // is rewritten, and restored when the scope lifts.
  const graph = useMemo(() => {
    if (!loaded) return null;
    if (!branch || mode !== "community") {
      for (const n of loaded.nodes) if (n.kind === "tag") n.community = topLevel(n.fullPath);
      return loaded;
    }
    const keep = new Set<string>();
    const nodes = loaded.nodes.filter((n) => {
      if (n.kind === "camera") {
        keep.add(n.id);
        return true;
      }
      if (n.kind !== "tag") return false;
      const rel = relativeToBranch(n.fullPath, branch);
      if (rel == null) return false;
      n.community = rel === "" ? n.label : topLevel(rel);
      keep.add(n.id);
      return true;
    });
    const links = loaded.links.filter(
      (l) => keep.has(endId(l.source)) && keep.has(endId(l.target)),
    );
    return { nodes, links };
  }, [loaded, branch, mode]);
  const [error, setError] = useState("");
  const [selected, setSelected] = useState<string | null>(null);
  const [thumbs, setThumbs] = useState(false); // show photo thumbnails (bipartite)
  // Photos dropped by BIPARTITE_PHOTO_CAP — surfaced in the status strip.
  const [photoOverflow, setPhotoOverflow] = useState(0);

  // Left-panel controls. Node types start OFF: the canvas begins empty and the user
  // opts in per type — a 1,400-node hairball is a worse default than a blank slate,
  // and the layout keeps settling in the background so nodes appear already placed.
  const [visible, setVisible] = useState<Record<Kind, boolean>>({
    tag: false,
    camera: false,
    photo: false,
  });
  const [activeCommunity, setActiveCommunity] = useState<string | null>(null);
  const [linkThreshold, setLinkThreshold] = useState(0);
  const [frozen, setFrozen] = useState(false);
  const [isolate, setIsolate] = useState<string | null>(null); // selected node whose nbhd is isolated

  const canvasRef = useRef<HTMLCanvasElement>(null);
  const simRef = useRef<Simulation<GNode, GLink> | null>(null);
  // Hot-path state lives in refs: mutate + set `dirty`, and the rAF loop repaints.
  const viewRef = useRef({ k: 1, x: 0, y: 0 });
  const hoverRef = useRef<string | null>(null);
  const dirtyRef = useRef(true);
  const drawRef = useRef<() => void>(() => {});
  const thumbCache = useRef(new Map<number, HTMLImageElement>());
  // Bundle mode: the un-highlighted edges as cached Path2D buckets (rebuilt only when the
  // layout, link set, or isolation changes — hover repaints just restroke them), the ids
  // labelled in the last paint (label-zone hit-testing), and which graph was auto-fitted.
  const edgeCacheRef = useRef<{
    bundle: BundleLayout;
    links: GLink[];
    isolate: Set<string> | null;
    buckets: { path: Path2D; color: string; alpha: number }[];
  } | null>(null);
  const labelledRef = useRef<string[]>([]);
  const fittedRef = useRef<object | null>(null);

  // Communities (tag families) by total photo count, descending — the left-panel list
  // and the ring order share this ordering.
  const communities = useMemo(() => {
    if (!graph) return [] as { name: string; count: number }[];
    const totals = new Map<string, number>();
    for (const n of graph.nodes) {
      if (n.kind !== "tag") continue;
      totals.set(n.community, (totals.get(n.community) ?? 0) + n.count);
    }
    return Array.from(totals.entries())
      .map(([name, count]) => ({ name, count }))
      .sort((a, b) => b.count - a.count);
  }, [graph]);

  // Community → colour. The palette cycles down the size-ordered list, which is also the
  // ring order, so neighbouring arcs differ.
  const communityColor = useMemo(() => {
    const m = new Map<string, string>();
    communities.forEach((c, i) => m.set(c.name, PALETTE[i % PALETTE.length]));
    return m;
  }, [communities]);

  // Tag ids per community — the edge focus set when a community is active.
  const communityMembers = useMemo(() => {
    const m = new Map<string, Set<string>>();
    graph?.nodes.forEach((n) => {
      if (n.kind !== "tag") return;
      (m.get(n.community) ?? m.set(n.community, new Set()).get(n.community)!).add(n.id);
    });
    return m;
  }, [graph]);

  const nodeColor = (n: GNode): string => {
    if (n.kind === "camera") return CAMERA_COLOR;
    if (n.kind === "photo") return PHOTO_COLOR;
    return communityColor.get(n.community) ?? PALETTE[0];
  };

  // Load + shape the data whenever the mode changes.
  useEffect(() => {
    let alive = true;
    setError("");
    setLoaded(null);
    setBranch(null);
    setSelected(null);
    setIsolate(null);
    setPhotoOverflow(0);
    const load = async () => {
      try {
        if (mode === "community") {
          const g = await api.invoke<LibraryGraphData>("library_graph");
          const tagNodes: GNode[] = g.tags.map((n) => ({
            id: `t${n.id}`,
            kind: "tag",
            refId: n.id,
            label: leaf(n.label),
            fullPath: n.label,
            count: n.count,
            community: topLevel(n.label),
            r: tagRadius(n.count),
          }));
          const cameraNodes: GNode[] = g.cameras.map((n) => ({
            id: `c${n.id}`,
            kind: "camera",
            refId: n.id,
            label: n.label,
            fullPath: n.label,
            count: n.count,
            community: "__camera",
            r: tagRadius(n.count),
          }));
          const nodes = [...tagNodes, ...cameraNodes];
          // Only keep edges whose BOTH endpoints are actual nodes. The backend's node
          // list holds tags with ≥1 direct photo, but hierarchy edges span the whole
          // tag tree (a parent like "Animals" may have no direct photos) — d3's
          // forceLink throws "node not found" on a dangling endpoint, crashing the view.
          const ids = new Set(nodes.map((n) => n.id));
          const links: GLink[] = [
            ...g.coocEdges.map(([a, b, w]) => ({
              source: `t${a}`,
              target: `t${b}`,
              weight: w,
              kind: "cooc" as const,
            })),
            ...g.hierarchyEdges.map(([p, c]) => ({
              source: `t${p}`,
              target: `t${c}`,
              weight: 1,
              kind: "hierarchy" as const,
            })),
            ...g.cameraEdges.map(([ci, t, w]) => ({
              source: `c${ci}`,
              target: `t${t}`,
              weight: w,
              kind: "camera" as const,
            })),
          ].filter((l) => ids.has(l.source as string) && ids.has(l.target as string));
          // Every tag path → id, parents without direct photos included: they are not
          // nodes, but hierarchy edges name their ids, and a child's path names theirs.
          // Walk the edges until no parent is left unnamed (one pass per missing level).
          const pathById = new Map<number, string>(g.tags.map((n) => [n.id, n.label]));
          for (let grew = true; grew; ) {
            grew = false;
            for (const [p, c] of g.hierarchyEdges) {
              if (pathById.has(p)) continue;
              const childPath = pathById.get(c);
              const pp = childPath != null ? parentPath(childPath) : null;
              if (pp != null) {
                pathById.set(p, pp);
                grew = true;
              }
            }
          }
          const tagIdByPath = new Map<string, number>();
          pathById.forEach((path, id) => tagIdByPath.set(path, id));
          if (alive) setLoaded({ nodes, links, tagIdByPath });
        } else {
          const g = await api.invoke<{
            photos: RawNode[];
            tags: RawNode[];
            edges: [number, number][];
          }>("photo_tag_graph");
          // Keep only the most-tagged photos — a six-figure library would otherwise
          // feed the force layout far more nodes than it can animate. The status
          // strip reports the truncation.
          const keptPhotos = [...g.photos]
            .sort((a, b) => b.count - a.count)
            .slice(0, BIPARTITE_PHOTO_CAP);
          const keptIds = new Set(keptPhotos.map((p) => p.id));
          const edges = g.edges.filter(([p]) => keptIds.has(p));
          const keptTagIds = new Set(edges.map(([, t]) => t));
          const nodes: GNode[] = [
            ...g.tags
              .filter((n) => keptTagIds.has(n.id))
              .map((n) => ({
                id: `t${n.id}`,
                kind: "tag" as const,
                refId: n.id,
                label: leaf(n.label),
                fullPath: n.label,
                count: n.count,
                community: topLevel(n.label),
                r: tagRadius(n.count),
              })),
            ...keptPhotos.map((n) => ({
              id: `p${n.id}`,
              kind: "photo" as const,
              refId: n.id,
              label: n.label,
              fullPath: n.label,
              count: n.count,
              community: "__photo",
              r: PHOTO_R,
            })),
          ];
          const links: GLink[] = edges.map(([p, t]) => ({
            source: `p${p}`,
            target: `t${t}`,
            weight: 1,
            kind: "bipartite" as const,
          }));
          if (alive) {
            setPhotoOverflow(g.photos.length - keptPhotos.length);
            setLoaded({ nodes, links, tagIdByPath: new Map() });
          }
        }
      } catch (e) {
        if (alive) setError(String(e));
      }
    };
    load();
    return () => {
      alive = false;
    };
  }, [mode, api]);

  // The links actually fed to the sim: cooc edges below the strength threshold are dropped.
  const simLinks = useMemo(() => {
    if (!graph) return [] as GLink[];
    return graph.links.filter((l) => l.kind !== "cooc" || l.weight >= linkThreshold);
  }, [graph, linkThreshold]);

  // The links actually painted. The bundle draws hierarchy as ring adjacency, not edges.
  const drawLinks = useMemo(
    () => (mode === "community" ? simLinks.filter((l) => l.kind !== "hierarchy") : simLinks),
    [mode, simLinks],
  );

  // Radial bundling layout (community mode). Only the visible kinds take ring slots, so
  // toggling a type re-lays the ring. Ring placement is written onto the node objects:
  // hit-testing, fit, and the inspector all read n.x / n.y — the same mutable-position
  // convention d3-force uses in bipartite mode.
  const bundle = useMemo(() => {
    if (!graph || mode !== "community") return null;
    const shown = graph.nodes.filter((n) => visible[n.kind]);
    const layout = buildBundleLayout(
      branch
        ? shown.map((n) => ({
            ...n,
            fullPath: n.kind === "tag" ? layoutPath(n.fullPath, n.label, branch) : n.fullPath,
          }))
        : shown,
      RING_R,
    );
    for (const n of shown) {
      const p = layout.placement.get(n.id);
      if (p) {
        n.x = p.x;
        n.y = p.y;
      }
    }
    return layout;
  }, [graph, mode, visible, branch]);

  // Run / re-run the force simulation when the graph or the active link set changes
  // (bipartite mode only — the bundle layout is deterministic).
  useEffect(() => {
    simRef.current?.stop();
    if (!graph || mode !== "bipartite") return;
    // d3 throws (e.g. "node not found" on a dangling link) — surface it in the view's
    // error banner instead of letting the exception unmount the whole app.
    try {
      const sim = forceSimulation<GNode, GLink>(graph.nodes)
        // Settle in roughly half d3's default tick count — the layout is visually
        // stable long before alphaMin either way, and it halves time-to-readable.
        .alphaDecay(0.04)
        .force(
          "link",
          forceLink<GNode, GLink>(simLinks)
            .id((d) => d.id)
            .distance(28)
            .strength(0.4),
        )
        .force("charge", forceManyBody<GNode>().strength(-60))
        .force("collide", forceCollide<GNode>().radius((d) => d.r + 2))
        .force("center", forceCenter(0, 0))
        .force("x", forceX(0).strength(0.04))
        .force("y", forceY(0).strength(0.04));
      sim.on("tick", () => {
        dirtyRef.current = true;
      });
      simRef.current = sim;
      if (frozen) sim.stop();
    } catch (e) {
      setError(String(e));
      return;
    }
    return () => {
      simRef.current?.stop();
    };
    // frozen intentionally excluded — toggled via its own effect so a graph reload
    // doesn't unfreeze.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [graph, mode, simLinks]);

  // Freeze / unfreeze without rebuilding the sim.
  useEffect(() => {
    const sim = simRef.current;
    if (!sim) return;
    if (frozen) sim.stop();
    else sim.alpha(0.3).restart();
  }, [frozen]);

  // Neighbour set for hover highlighting & inspector (over ALL links).
  const neighbors = useMemo(() => {
    const m = new Map<string, Set<string>>();
    if (!graph) return m;
    for (const l of graph.links) {
      const s = typeof l.source === "object" ? (l.source as GNode).id : (l.source as string);
      const t = typeof l.target === "object" ? (l.target as GNode).id : (l.target as string);
      (m.get(s) ?? m.set(s, new Set()).get(s)!).add(t);
      (m.get(t) ?? m.set(t, new Set()).get(t)!).add(s);
    }
    return m;
  }, [graph]);

  const nodeById = useMemo(() => {
    const m = new Map<string, GNode>();
    graph?.nodes.forEach((n) => m.set(n.id, n));
    return m;
  }, [graph]);

  // Degree (over the full link set) — used for the "hub" chip and Links stat.
  const degree = useMemo(() => {
    const m = new Map<string, number>();
    neighbors.forEach((set, id) => m.set(id, set.size));
    return m;
  }, [neighbors]);

  // Top-decile degree threshold (for the "hub" chip).
  const hubThreshold = useMemo(() => {
    const ds = Array.from(degree.values()).sort((a, b) => a - b);
    if (!ds.length) return Infinity;
    return ds[Math.floor(ds.length * 0.9)] ?? Infinity;
  }, [degree]);

  // Hierarchy children count (for the Children stat).
  const childrenCount = useMemo(() => {
    const m = new Map<string, number>();
    if (!graph) return m;
    for (const l of graph.links) {
      if (l.kind !== "hierarchy") continue;
      const s = typeof l.source === "object" ? (l.source as GNode).id : (l.source as string);
      m.set(s, (m.get(s) ?? 0) + 1);
    }
    return m;
  }, [graph]);

  // Isolation neighbourhood: the selected node + its neighbours (or null = show all).
  const isolateSet = useMemo(() => {
    if (!isolate) return null;
    const s = new Set<string>([isolate]);
    neighbors.get(isolate)?.forEach((n) => s.add(n));
    return s;
  }, [isolate, neighbors]);

  // Is a node currently drawn?
  const nodeShown = (n: GNode): boolean => {
    if (!visible[n.kind]) return false;
    if (isolateSet && !isolateSet.has(n.id)) return false;
    return true;
  };

  // Selected node & its data.
  const selNode = selected ? nodeById.get(selected) ?? null : null;

  // Top neighbours by edge weight (for the CONNECTED chips).
  const connected = useMemo(() => {
    if (!selNode || !graph) return [] as { node: GNode; weight: number }[];
    const out: { node: GNode; weight: number }[] = [];
    for (const l of graph.links) {
      const s = typeof l.source === "object" ? (l.source as GNode) : nodeById.get(l.source as string);
      const t = typeof l.target === "object" ? (l.target as GNode) : nodeById.get(l.target as string);
      if (!s || !t) continue;
      if (s.id === selNode.id && t) out.push({ node: t, weight: l.weight });
      else if (t.id === selNode.id && s) out.push({ node: s, weight: l.weight });
    }
    // A pair can be linked by more than one edge kind — keep the strongest.
    const best = new Map<string, { node: GNode; weight: number }>();
    for (const e of out) {
      const prev = best.get(e.node.id);
      if (!prev || e.weight > prev.weight) best.set(e.node.id, e);
    }
    return Array.from(best.values())
      .sort((a, b) => b.weight - a.weight)
      .slice(0, 12);
  }, [selNode, graph, nodeById]);

  // The active community as a selectable thing of its own — the inspector's subject when
  // no node is selected. A community is a tag branch: a top-level family, or a sub-family
  // inside the focused branch. Its tag id is known even when that tag has no direct
  // photos (and so is not on the ring), which is what makes Filter and the loupe work
  // for "Animals" itself.
  const community = useMemo(() => {
    if (!activeCommunity || mode !== "community" || !graph) return null;
    const members = Array.from(communityMembers.get(activeCommunity) ?? [])
      .map((id) => nodeById.get(id))
      .filter((n): n is GNode => !!n)
      .sort((a, b) => b.count - a.count);
    const first = members[0];
    if (!first) return null;
    let path: string;
    if (!branch) path = topLevel(first.fullPath);
    else {
      const rel = relativeToBranch(first.fullPath, branch);
      if (rel == null) return null;
      path = rel === "" ? branch : `${branch}/${topLevel(rel)}`;
    }
    return {
      name: activeCommunity,
      path,
      tagId: loaded?.tagIdByPath.get(path) ?? null,
      color: communityColor.get(activeCommunity) ?? PALETTE[0],
      photos: communities.find((c) => c.name === activeCommunity)?.count ?? 0,
      members,
      // A one-tag family is already as focused as it gets.
      canFocus: path !== branch && members.length > 1,
    };
  }, [
    activeCommunity,
    mode,
    graph,
    communityMembers,
    nodeById,
    branch,
    loaded,
    communityColor,
    communities,
  ]);

  // Mirror the inspector to the pop-out loupe window — a second screen — when the host
  // can. The card carries the subject's numbers and a photo scope; the loupe fetches the
  // photos itself, so it can show a whole wall of them rather than the inspector's six.
  const loupeCard = useMemo<LoupeCard | null>(() => {
    if (selNode) {
      const isTag = selNode.kind === "tag";
      const links = degree.get(selNode.id) ?? 0;
      const stats: NonNullable<LoupeCard["stats"]> = [{ label: "Photos", value: selNode.count }];
      if (isTag) stats.push({ label: "Children", value: childrenCount.get(selNode.id) ?? 0 });
      stats.push({ label: "Links", value: links });
      return {
        title: selNode.label,
        subtitle: isTag ? selNode.fullPath : "Camera",
        color: nodeColor(selNode),
        chips: isTag
          ? [`Tag${links >= hubThreshold ? " · hub" : ""}`, `Community: ${selNode.community}`]
          : ["Camera"],
        stats,
        related: connected.map((c) => ({
          label: c.node.label,
          detail: String(c.weight),
          color: nodeColor(c.node),
        })),
        photos: isTag ? { tagId: selNode.refId } : { camera: selNode.fullPath },
      };
    }
    if (community) {
      return {
        title: community.name,
        subtitle: community.path,
        color: community.color,
        chips: [branch ? "Branch" : "Community"],
        stats: [
          { label: "Photos", value: community.photos },
          { label: "Tags", value: community.members.length },
        ],
        related: community.members.slice(0, 12).map((n) => ({
          label: n.label,
          detail: String(n.count),
          color: nodeColor(n),
        })),
        photos: community.tagId != null ? { tagId: community.tagId } : undefined,
      };
    }
    return null;
    // nodeColor is a plain closure over communityColor, which is the dep that matters.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selNode, degree, childrenCount, hubThreshold, connected, communityColor, community, branch]);
  useEffect(() => {
    api.showInLoupe?.(loupeCard);
  }, [api, loupeCard]);
  // Leaving the view hands the loupe back to the photo selection.
  useEffect(() => () => api.showInLoupe?.(null), [api]);

  // TOP PHOTOS thumbnails for the selected tag (via the core `list_photos` command).
  const [topPhotos, setTopPhotos] = useState<number[]>([]);
  useEffect(() => {
    let alive = true;
    setTopPhotos([]);
    if (!selNode || selNode.kind !== "tag") return;
    api
      // `list_photos` takes one typed query and answers with a window (issue #10), so the
      // six thumbnails are six rows fetched, not the tag's whole photo set.
      .invoke<{ photos: { id: number }[] }>("list_photos", {
        query: { tagId: selNode.refId, window: { offset: 0, limit: 6 } },
      })
      .then((page) => {
        if (alive) setTopPhotos(page.photos.map((p) => p.id));
      })
      .catch(() => {
        if (alive) setTopPhotos([]);
      });
    return () => {
      alive = false;
    };
  }, [selNode, api]);

  const markDirty = () => {
    dirtyRef.current = true;
  };

  // Screen → graph coordinates. The draw transform is translate(view) · scale(k) ·
  // translate(cw/2, ch/2), so the inverse subtracts the recentre too.
  const toGraph = (cx: number, cy: number) => {
    const rect = canvasRef.current!.getBoundingClientRect();
    const { k, x, y } = viewRef.current;
    return {
      x: (cx - rect.left - x) / k - rect.width / 2,
      y: (cy - rect.top - y) / k - rect.height / 2,
    };
  };

  // Node under the cursor. Bundle mode: nearest ring slot by angle, on the ring itself or
  // (further out) among the slots that currently carry a label. Force mode: topmost node
  // — they paint in array order, so scan from the end.
  const hitTest = (cx: number, cy: number): GNode | null => {
    if (!graph) return null;
    const p = toGraph(cx, cy);
    if (bundle) {
      const { k } = viewRef.current;
      const r = Math.hypot(p.x, p.y);
      const ringTol = 8 / k;
      if (r < bundle.R - ringTol || r > bundle.R + (LABEL_EXTENT + 12) / k) return null;
      const a = Math.atan2(p.y, p.x);
      const inLabelZone = r > bundle.R + ringTol;
      const tol = inLabelZone ? 6 / (r * k) : Math.max(bundle.slot / 2, 5 / (bundle.R * k));
      let best: GNode | null = null;
      let bestD = Infinity;
      for (const id of inLabelZone ? labelledRef.current : bundle.order) {
        const n = nodeById.get(id);
        const pl = bundle.placement.get(id);
        if (!n || !pl || !nodeShown(n)) continue;
        const d = Math.abs(angleDiff(a, pl.angle));
        if (d < bestD) {
          bestD = d;
          best = n;
        }
      }
      return bestD <= tol ? best : null;
    }
    for (let i = graph.nodes.length - 1; i >= 0; i--) {
      const n = graph.nodes[i];
      if (!nodeShown(n)) continue;
      const r = (n.kind === "photo" && !thumbs ? 5 : n.r) + 2;
      const dx = (n.x ?? 0) - p.x;
      const dy = (n.y ?? 0) - p.y;
      if (dx * dx + dy * dy <= r * r) return n;
    }
    return null;
  };

  // Pan / zoom — refs only, no React render per gesture frame.
  const zoomAt = (factor: number, cx?: number, cy?: number) => {
    const rect = canvasRef.current?.getBoundingClientRect();
    if (!rect) return;
    const mx = cx ?? rect.width / 2;
    const my = cy ?? rect.height / 2;
    const v = viewRef.current;
    const k = Math.min(Math.max(v.k * factor, 0.05), 6);
    viewRef.current = { k, x: mx - (k / v.k) * (mx - v.x), y: my - (k / v.k) * (my - v.y) };
    markDirty();
  };

  const fitView = () => {
    const rect = canvasRef.current?.getBoundingClientRect();
    if (!rect || !graph) return;
    if (bundle) {
      // Ring plus the label band, centred.
      const margin = LABEL_EXTENT + 28;
      const k = Math.min(
        Math.max((Math.min(rect.width, rect.height) / 2 - margin) / bundle.R, 0.05),
        6,
      );
      viewRef.current = {
        k,
        x: (rect.width / 2) * (1 - k),
        y: (rect.height / 2) * (1 - k),
      };
      markDirty();
      return;
    }
    let minX = Infinity;
    let minY = Infinity;
    let maxX = -Infinity;
    let maxY = -Infinity;
    for (const n of graph.nodes) {
      if (!nodeShown(n)) continue;
      minX = Math.min(minX, (n.x ?? 0) - n.r);
      maxX = Math.max(maxX, (n.x ?? 0) + n.r);
      minY = Math.min(minY, (n.y ?? 0) - n.r);
      maxY = Math.max(maxY, (n.y ?? 0) + n.r);
    }
    if (minX > maxX) return;
    const pad = 48;
    const k = Math.min(
      Math.max(
        Math.min((rect.width - pad) / (maxX - minX || 1), (rect.height - pad) / (maxY - minY || 1)),
        0.05,
      ),
      6,
    );
    const cx = (minX + maxX) / 2;
    const cy = (minY + maxY) / 2;
    viewRef.current = {
      k,
      x: rect.width / 2 - (cx + rect.width / 2) * k,
      y: rect.height / 2 - (cy + rect.height / 2) * k,
    };
    markDirty();
  };

  const resetView = () => {
    viewRef.current = { k: 1, x: 0, y: 0 };
    markDirty();
  };

  // Wheel zoom must preventDefault, and React registers wheel listeners passively —
  // attach directly. Reads only refs, so the empty dep list is safe.
  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      const rect = canvas.getBoundingClientRect();
      const v = viewRef.current;
      const mx = e.clientX - rect.left;
      const my = e.clientY - rect.top;
      const k = Math.min(Math.max(v.k * (e.deltaY < 0 ? 1.15 : 1 / 1.15), 0.05), 6);
      viewRef.current = { k, x: mx - (k / v.k) * (mx - v.x), y: my - (k / v.k) * (my - v.y) };
      dirtyRef.current = true;
    };
    canvas.addEventListener("wheel", onWheel, { passive: false });
    return () => canvas.removeEventListener("wheel", onWheel);
  }, []);

  // One pointerdown handler: a node starts a drag (a sub-4px drag = a click → select),
  // empty canvas deselects and pans.
  const onPointerDown = (e: React.PointerEvent) => {
    const n = hitTest(e.clientX, e.clientY);
    if (n && bundle) {
      // Ring slots are fixed — a press is a select, not a drag.
      setSelected(n.id);
      return;
    }
    if (n) {
      const sim = simRef.current;
      if (!frozen) sim?.alphaTarget(0.3).restart();
      const move = (ev: PointerEvent) => {
        const p = toGraph(ev.clientX, ev.clientY);
        n.fx = p.x;
        n.fy = p.y;
        if (frozen) {
          // The sim is stopped, so fx/fy alone won't move the node — move it directly.
          n.x = p.x;
          n.y = p.y;
          markDirty();
        }
      };
      const up = (ev: PointerEvent) => {
        window.removeEventListener("pointermove", move);
        window.removeEventListener("pointerup", up);
        sim?.alphaTarget(0);
        n.fx = null;
        n.fy = null;
        if (Math.hypot(ev.clientX - e.clientX, ev.clientY - e.clientY) < 4) {
          setSelected(n.id);
        }
      };
      window.addEventListener("pointermove", move);
      window.addEventListener("pointerup", up);
      return;
    }
    setSelected(null);
    const start = { mx: e.clientX, my: e.clientY, x: viewRef.current.x, y: viewRef.current.y };
    const move = (ev: PointerEvent) => {
      viewRef.current = {
        ...viewRef.current,
        x: start.x + (ev.clientX - start.mx),
        y: start.y + (ev.clientY - start.my),
      };
      markDirty();
    };
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  };

  const onPointerMove = (e: React.PointerEvent) => {
    if (e.buttons) return; // mid-gesture — the window listeners own the pointer
    const id = hitTest(e.clientX, e.clientY)?.id ?? null;
    if (id !== hoverRef.current) {
      hoverRef.current = id;
      if (canvasRef.current) canvasRef.current.style.cursor = id ? "pointer" : "grab";
      markDirty();
    }
  };

  const onPointerLeave = () => {
    if (hoverRef.current != null) {
      hoverRef.current = null;
      markDirty();
    }
  };

  // Scope the ring to a tag's branch (community mode). Community focus and isolation are
  // about the previous ring, so they lift; the selection stays — the tag is on the new ring.
  const focusBranch = (path: string | null) => {
    setBranch(path);
    setActiveCommunity(null);
    setIsolate(null);
    // The new ring has the same radius, so fitting now (before it is laid out) is exact.
    fitView();
  };

  // Escape deselects; with nothing selected it climbs one level out of a branch.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      if (selected) setSelected(null);
      else if (branch) focusBranch(parentPath(branch));
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // focusBranch only sets state.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selected, branch]);

  // Counts for the NODE TYPES panel.
  const kindCounts = useMemo(() => {
    const c: Record<Kind, number> = { tag: 0, camera: 0, photo: 0 };
    graph?.nodes.forEach((n) => (c[n.kind] += 1));
    return c;
  }, [graph]);

  const shownStats = useMemo(() => {
    if (!graph) return { nodes: 0, links: 0, communities: 0 };
    const shownNodes = graph.nodes.filter(nodeShown);
    const shownIds = new Set(shownNodes.map((n) => n.id));
    const shownLinks = drawLinks.filter((l) => {
      const s = typeof l.source === "object" ? (l.source as GNode).id : (l.source as string);
      const t = typeof l.target === "object" ? (l.target as GNode).id : (l.target as string);
      return shownIds.has(s) && shownIds.has(t);
    });
    return { nodes: shownNodes.length, links: shownLinks.length, communities: communities.length };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [graph, drawLinks, visible, isolateSet, communities.length]);

  const communityDimmed = (n: GNode) =>
    activeCommunity != null && n.kind === "tag" && n.community !== activeCommunity;

  // Screen-fixed tooltip beside a node (what the SVG <title> used to say).
  const drawTooltip = (
    ctx: CanvasRenderingContext2D,
    env: PaintEnv,
    n: GNode,
    sx: number,
    sy: number,
    rPx: number,
  ) => {
    const { dpr, cw, ch, font, txtColor, haloColor, borderColor } = env;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    const what = n.kind === "photo" ? "tag(s)" : "photo(s)";
    const text = `${n.fullPath} · ${n.count} ${what}`;
    ctx.font = `500 12px ${font}`;
    ctx.textAlign = "left";
    ctx.textBaseline = "alphabetic";
    const w = ctx.measureText(text).width;
    const tx = Math.min(Math.max(sx + 10, 4), Math.max(cw - w - 12, 4));
    const ty = Math.min(Math.max(sy - rPx - 10, 16), ch - 8);
    ctx.globalAlpha = 0.92;
    ctx.fillStyle = haloColor;
    ctx.fillRect(tx - 6, ty - 13, w + 12, 19);
    ctx.globalAlpha = 1;
    ctx.strokeStyle = borderColor;
    ctx.lineWidth = 1;
    ctx.strokeRect(tx - 6, ty - 13, w + 12, 19);
    ctx.fillStyle = txtColor;
    ctx.fillText(text, tx, ty + 1);
  };

  // ── Bundle painter (community mode) ─────────────────────────────
  // Edges are stroked in graph space from cached Path2D buckets; everything else — group
  // arcs, dots, labels — is drawn in screen pixels so it reads the same at any zoom.
  const drawBundle = (ctx: CanvasRenderingContext2D, layout: BundleLayout, env: PaintEnv) => {
    const { k, x, y, cw, ch, dpr, font, txtColor, haloColor } = env;
    const cxs = (cw / 2) * k + x; // screen position of the ring centre
    const cys = (ch / 2) * k + y;
    const Rs = layout.R * k;
    const hov = hoverRef.current;
    const hovNbrs = hov ? neighbors.get(hov) : undefined;

    // Focus: the node(s) whose edges light up. Hover beats selection beats community.
    let focus: Set<string> | null = null;
    if (hov) focus = new Set([hov]);
    else if (selected) focus = new Set([selected]);
    else if (activeCommunity) focus = communityMembers.get(activeCommunity) ?? null;

    // ── Edges, in graph space ──
    ctx.translate(x, y);
    ctx.scale(k, k);
    ctx.translate(cw / 2, ch / 2);
    let cache = edgeCacheRef.current;
    if (
      !cache ||
      cache.bundle !== layout ||
      cache.links !== drawLinks ||
      cache.isolate !== isolateSet
    ) {
      // One Path2D per (colour, alpha step). Alpha follows log weight so the strong
      // pairs read and the long tail stays a haze; colour follows the heavier end so a
      // bundle out of one family is one hue.
      let maxW = 1;
      for (const l of drawLinks) maxW = Math.max(maxW, l.weight);
      const lnMax = Math.log1p(maxW);
      const byKey = new Map<string, { path: Path2D; color: string; alpha: number }>();
      for (const l of drawLinks) {
        const s = nodeById.get(endId(l.source));
        const t = nodeById.get(endId(l.target));
        if (!s || !t || !nodeShown(s) || !nodeShown(t)) continue;
        const pts = layout.path(s.id, t.id);
        if (!pts) continue;
        const heavy = s.count >= t.count ? s : t;
        const color = l.kind === "camera" ? CAMERA_COLOR : nodeColor(heavy);
        const alpha = Math.round((0.05 + 0.35 * (Math.log1p(l.weight) / lnMax)) * 50) / 50;
        const key = `${color}|${alpha}`;
        let b = byKey.get(key);
        if (!b) byKey.set(key, (b = { path: new Path2D(), color, alpha }));
        bundlePath(b.path, pts, BUNDLE_BETA);
      }
      cache = {
        bundle: layout,
        links: drawLinks,
        isolate: isolateSet,
        buckets: Array.from(byKey.values()),
      };
      edgeCacheRef.current = cache;
    }
    ctx.lineWidth = 1 / k;
    ctx.lineCap = "round";
    const baseMul = focus ? 0.2 : 1;
    for (const b of cache.buckets) {
      ctx.globalAlpha = b.alpha * baseMul;
      ctx.strokeStyle = b.color;
      ctx.stroke(b.path);
    }
    if (focus) {
      // Lit edges, coloured by their far end so the picture says where a tag goes.
      const byColor = new Map<string, Path2D>();
      for (const l of drawLinks) {
        const s = nodeById.get(endId(l.source));
        const t = nodeById.get(endId(l.target));
        if (!s || !t) continue;
        const sIn = focus.has(s.id);
        const tIn = focus.has(t.id);
        if (!sIn && !tIn) continue;
        if (!nodeShown(s) || !nodeShown(t)) continue;
        const pts = layout.path(s.id, t.id);
        if (!pts) continue;
        const far = sIn ? t : s;
        const color = far.kind === "camera" ? CAMERA_COLOR : nodeColor(far);
        let p = byColor.get(color);
        if (!p) byColor.set(color, (p = new Path2D()));
        bundlePath(p, pts, BUNDLE_BETA);
      }
      ctx.lineWidth = 1.5 / k;
      ctx.globalAlpha = 0.85;
      for (const [color, p] of byColor) {
        ctx.strokeStyle = color;
        ctx.stroke(p);
      }
    }

    // ── Chrome, in screen space ──
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.lineCap = "butt";
    const groupColor = (name: string) =>
      name === CAMERA_GROUP ? CAMERA_COLOR : (communityColor.get(name) ?? PALETTE[0]);
    const groupDim = (name: string) =>
      activeCommunity != null && name !== CAMERA_GROUP && name !== activeCommunity;

    ctx.lineWidth = 3;
    for (const g of layout.groups) {
      ctx.globalAlpha = groupDim(g.name) ? 0.25 : 0.9;
      ctx.strokeStyle = groupColor(g.name);
      ctx.beginPath();
      ctx.arc(cxs, cys, Rs + 6, g.a0, g.a1);
      ctx.stroke();
    }

    const dimmed = (n: GNode) =>
      (hov != null && hov !== n.id && !hovNbrs?.has(n.id)) || communityDimmed(n);
    const TWO_PI = Math.PI * 2;
    let hovered: GNode | null = null;
    let hoveredR = 0;
    for (const id of layout.order) {
      const n = nodeById.get(id);
      const p = layout.placement.get(id);
      if (!n || !p || !nodeShown(n)) continue;
      const r = 1.5 + 4.5 * Math.sqrt(n.count / layout.maxCount);
      const sx = cxs + Rs * Math.cos(p.angle);
      const sy = cys + Rs * Math.sin(p.angle);
      if (id === hov) {
        hovered = n;
        hoveredR = r;
      }
      ctx.globalAlpha = dimmed(n) ? 0.18 : 1;
      ctx.beginPath();
      ctx.arc(sx, sy, r, 0, TWO_PI);
      ctx.fillStyle = nodeColor(n);
      ctx.fill();
      if (id === selected || id === hov) {
        ctx.strokeStyle = "#fff";
        ctx.lineWidth = 1.5;
        ctx.stroke();
      }
    }

    // Labels: a stable base set by count, placed greedily with an angular gap so they
    // never collide; then the hovered/selected node always, and its neighbours where
    // they fit.
    const minSep = 12 / Rs;
    const placed: number[] = [];
    const labelled: string[] = [];
    const labelledSet = new Set<string>();
    const tryLabel = (id: string, force: boolean) => {
      if (labelledSet.has(id)) return;
      const n = nodeById.get(id);
      const p = layout.placement.get(id);
      if (!n || !p || !nodeShown(n)) return;
      if (!force) for (const a of placed) if (Math.abs(a - p.angle) < minSep) return;
      placed.push(p.angle);
      labelled.push(id);
      labelledSet.add(id);
    };
    for (const id of layout.labelOrder) {
      if (labelled.length >= 500) break;
      tryLabel(id, false);
    }
    const emph = hov ?? selected;
    if (emph) {
      tryLabel(emph, true);
      neighbors.get(emph)?.forEach((nb) => tryLabel(nb, false));
    }
    labelledRef.current = labelled;

    ctx.textBaseline = "middle";
    ctx.lineWidth = 3;
    ctx.lineJoin = "round";
    for (const id of labelled) {
      const n = nodeById.get(id)!;
      const p = layout.placement.get(id)!;
      const isEmph = id === emph;
      const flip = Math.cos(p.angle) < 0;
      ctx.save();
      ctx.translate(cxs + Rs * Math.cos(p.angle), cys + Rs * Math.sin(p.angle));
      ctx.rotate(flip ? p.angle + Math.PI : p.angle);
      ctx.textAlign = flip ? "right" : "left";
      ctx.globalAlpha = dimmed(n) && !isEmph ? 0.3 : 1;
      ctx.font = `${isEmph ? 700 : 500} 11px ${font}`;
      const ox = flip ? -12 : 12;
      const text = truncate(n.label);
      ctx.strokeStyle = haloColor;
      ctx.strokeText(text, ox, 0);
      ctx.fillStyle = isEmph ? nodeColor(n) : txtColor;
      ctx.fillText(text, ox, 0);
      ctx.restore();
    }

    // Group names along the outside, only where the arc is long enough to carry them.
    ctx.font = `700 11px ${font}`;
    ctx.textAlign = "center";
    for (const g of layout.groups) {
      const name = g.name === CAMERA_GROUP ? "Cameras" : g.name;
      const w = ctx.measureText(name).width;
      if ((g.a1 - g.a0) * Rs < w + 10) continue;
      const mid = (g.a0 + g.a1) / 2;
      const rr = Rs + LABEL_EXTENT + 10;
      let rot = mid + Math.PI / 2;
      if (Math.cos(rot) < 0) rot += Math.PI;
      ctx.save();
      ctx.translate(cxs + rr * Math.cos(mid), cys + rr * Math.sin(mid));
      ctx.rotate(rot);
      ctx.globalAlpha = groupDim(g.name) ? 0.3 : 0.95;
      ctx.strokeStyle = haloColor;
      ctx.strokeText(name, 0, 0);
      ctx.fillStyle = groupColor(g.name);
      ctx.fillText(name, 0, 0);
      ctx.restore();
    }
    ctx.globalAlpha = 1;

    if (hovered) {
      const p = layout.placement.get(hovered.id)!;
      drawTooltip(
        ctx,
        env,
        hovered,
        cxs + Rs * Math.cos(p.angle),
        cys + Rs * Math.sin(p.angle),
        hoveredR,
      );
    }
  };

  // ── Frame painter ───────────────────────────────────────────────
  // Repaints the whole scene. Bundle mode hands off to drawBundle; force mode (bipartite)
  // batches links into one Path2D per (colour, width, dash, alpha) and draws one arc per
  // visible node — a few dozen stroke calls instead of 20k+ reconciled SVG elements.
  drawRef.current = () => {
    const canvas = canvasRef.current;
    const ctx = canvas?.getContext("2d");
    if (!canvas || !ctx) return;
    const dpr = window.devicePixelRatio || 1;
    const cw = canvas.clientWidth;
    const ch = canvas.clientHeight;
    if (canvas.width !== Math.round(cw * dpr) || canvas.height !== Math.round(ch * dpr)) {
      canvas.width = Math.round(cw * dpr);
      canvas.height = Math.round(ch * dpr);
    }
    const styles = getComputedStyle(canvas);
    const txtColor = styles.getPropertyValue("--txt").trim() || "#e2e8f0";
    const haloColor = styles.getPropertyValue("--canvas").trim() || "#0d1117";
    const borderColor = styles.getPropertyValue("--border").trim() || "#3a4150";
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, cw, ch);
    if (!graph) return;
    const { k, x, y } = viewRef.current;
    const env: PaintEnv = {
      k,
      x,
      y,
      cw,
      ch,
      dpr,
      font: styles.fontFamily,
      txtColor,
      haloColor,
      borderColor,
    };
    if (bundle) {
      drawBundle(ctx, bundle, env);
      return;
    }
    ctx.translate(x, y);
    ctx.scale(k, k);
    ctx.translate(cw / 2, ch / 2);

    // Viewport in graph coordinates, for culling.
    const vx0 = (0 - x) / k - cw / 2;
    const vy0 = (0 - y) / k - ch / 2;
    const vx1 = (cw - x) / k - cw / 2;
    const vy1 = (ch - y) / k - ch / 2;

    const hov = hoverRef.current;
    const hovNbrs = hov ? neighbors.get(hov) : undefined;

    type LinkBucket = { path: Path2D; color: string; width: number; dash: boolean; alpha: number };
    const base = new Map<string, LinkBucket>();
    const active = new Map<string, LinkBucket>();
    for (const l of drawLinks) {
      const s = typeof l.source === "object" ? (l.source as GNode) : nodeById.get(l.source as string);
      const t = typeof l.target === "object" ? (l.target as GNode) : nodeById.get(l.target as string);
      if (!s || !t || !nodeShown(s) || !nodeShown(t)) continue;
      const sx = s.x ?? 0;
      const sy = s.y ?? 0;
      const tx = t.x ?? 0;
      const ty = t.y ?? 0;
      if (
        Math.max(sx, tx) < vx0 ||
        Math.min(sx, tx) > vx1 ||
        Math.max(sy, ty) < vy0 ||
        Math.min(sy, ty) > vy1
      )
        continue;
      const isActive =
        (hov != null && (s.id === hov || t.id === hov)) ||
        (selected != null && (s.id === selected || t.id === selected));
      const color = l.kind === "camera" ? CAMERA_COLOR : nodeColor(s.kind === "camera" ? t : s);
      const alpha = isActive
        ? l.kind === "hierarchy" || l.kind === "camera"
          ? 0.7
          : 0.8
        : l.kind === "hierarchy"
          ? 0.12
          : l.kind === "camera"
            ? 0.2
            : 0.25;
      // Quantized to 0.5px so widths batch; visually indistinguishable from exact.
      const width = Math.round(Math.min(1 + Math.log2(l.weight + 1) * 0.6, 4) * 2) / 2;
      const dash = l.kind === "hierarchy";
      const key = `${color}|${width}|${dash}|${alpha}`;
      const map = isActive ? active : base;
      let b = map.get(key);
      if (!b) map.set(key, (b = { path: new Path2D(), color, width, dash, alpha }));
      b.path.moveTo(sx, sy);
      b.path.lineTo(tx, ty);
    }
    const strokeBuckets = (m: Map<string, LinkBucket>) => {
      for (const b of m.values()) {
        ctx.globalAlpha = b.alpha;
        ctx.strokeStyle = b.color;
        ctx.lineWidth = b.width;
        ctx.setLineDash(b.dash ? [3, 3] : []);
        ctx.stroke(b.path);
      }
    };
    strokeBuckets(base);
    strokeBuckets(active); // highlighted links paint on top
    ctx.setLineDash([]);

    // Labels are unreadable below ~6 screen px; skip them when zoomed far out.
    const labelsLegible = k * 11 >= 6;
    const TWO_PI = Math.PI * 2;
    let hovered: GNode | null = null;

    for (const n of graph.nodes) {
      if (!nodeShown(n)) continue;
      const nx = n.x ?? 0;
      const ny = n.y ?? 0;
      if (n.id === hov) hovered = n;
      if (nx + n.r < vx0 || nx - n.r > vx1 || ny + n.r < vy0 || ny - n.r > vy1) continue;
      const dim = (hov != null && hov !== n.id && !hovNbrs?.has(n.id)) || communityDimmed(n);
      const isSel = selected === n.id;
      ctx.globalAlpha = dim ? 0.18 : 1;
      if (n.kind === "photo" && thumbs) {
        let img = thumbCache.current.get(n.refId);
        if (!img) {
          img = new Image();
          img.onload = () => {
            dirtyRef.current = true;
          };
          img.src = convertFileSrc(String(n.refId), "thumb");
          thumbCache.current.set(n.refId, img);
        }
        if (img.complete && img.naturalWidth > 0) {
          ctx.save();
          ctx.beginPath();
          ctx.arc(nx, ny, n.r, 0, TWO_PI);
          ctx.clip();
          ctx.drawImage(img, nx - n.r, ny - n.r, n.r * 2, n.r * 2);
          ctx.restore();
        }
        ctx.beginPath();
        ctx.arc(nx, ny, n.r, 0, TWO_PI);
        ctx.strokeStyle = borderColor;
        ctx.lineWidth = 1.5;
        ctx.stroke();
      } else {
        ctx.beginPath();
        ctx.arc(nx, ny, n.kind === "photo" ? 5 : n.r, 0, TWO_PI);
        ctx.fillStyle = nodeColor(n);
        ctx.fill();
        ctx.strokeStyle = isSel ? "#fff" : NODE_STROKE;
        ctx.lineWidth = isSel ? 1.5 : 1;
        ctx.stroke();
      }
      if (isSel) {
        ctx.globalAlpha = 0.9;
        ctx.beginPath();
        ctx.arc(nx, ny, n.r + 3, 0, TWO_PI);
        ctx.strokeStyle = "#fff";
        ctx.lineWidth = 2;
        ctx.stroke();
        ctx.globalAlpha = dim ? 0.18 : 1;
      }
      if (n.kind !== "photo" && (isSel || n.id === hov || (labelsLegible && n.r > 7))) {
        ctx.font = `500 11px ${styles.fontFamily}`;
        ctx.textBaseline = "alphabetic";
        ctx.lineWidth = 3;
        ctx.strokeStyle = haloColor;
        ctx.strokeText(n.label, nx + n.r + 3, ny + 3);
        ctx.fillStyle = txtColor;
        ctx.fillText(n.label, nx + n.r + 3, ny + 3);
      }
    }
    ctx.globalAlpha = 1;

    if (hovered) {
      drawTooltip(
        ctx,
        env,
        hovered,
        ((hovered.x ?? 0) + cw / 2) * k + x,
        ((hovered.y ?? 0) + ch / 2) * k + y,
        hovered.r * k,
      );
    }
  };

  // Any committed render may have changed the scene (selection, filters, new data).
  useEffect(() => {
    dirtyRef.current = true;
  });

  // Fit the ring the first time it has something on it for this graph.
  useEffect(() => {
    if (!bundle || !graph || bundle.order.length === 0 || fittedRef.current === graph) return;
    fittedRef.current = graph;
    fitView();
    // fitView reads refs and `bundle`; nothing else it closes over should re-trigger it.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [bundle, graph]);

  // The repaint loop: at most one paint per frame, and only when something changed.
  useEffect(() => {
    let raf = 0;
    const loop = () => {
      if (dirtyRef.current) {
        dirtyRef.current = false;
        drawRef.current();
      }
      raf = requestAnimationFrame(loop);
    };
    raf = requestAnimationFrame(loop);
    return () => cancelAnimationFrame(raf);
  }, []);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const ro = new ResizeObserver(() => {
      dirtyRef.current = true;
    });
    ro.observe(canvas);
    return () => ro.disconnect();
  }, []);

  return (
    <div className="tg-root">
      {/* ── Left panel ─────────────────────────────────────────────── */}
      <div className="tg-left">
        {mode === "community" && branch && (
          <div className="tg-crumbs" aria-label="Branch">
            <button className="tg-crumb" onClick={() => focusBranch(null)}>
              All tags
            </button>
            {branch.split("/").map((seg, i, segs) => {
              const path = segs.slice(0, i + 1).join("/");
              return (
                <span key={path} className="tg-crumb-step">
                  <span className="tg-crumb-sep">›</span>
                  {i === segs.length - 1 ? (
                    <span className="tg-crumb tg-crumb-here">{seg}</span>
                  ) : (
                    <button className="tg-crumb" onClick={() => focusBranch(path)}>
                      {seg}
                    </button>
                  )}
                </span>
              );
            })}
          </div>
        )}
        <div className="tg-head">Node types</div>
        <div className="tg-types">
          <TypeRow
            label="Tags"
            color={PALETTE[0]}
            count={kindCounts.tag}
            on={visible.tag}
            onClick={() => setVisible((v) => ({ ...v, tag: !v.tag }))}
          />
          {mode === "community" && (
            <TypeRow
              label="Cameras"
              color={CAMERA_COLOR}
              count={kindCounts.camera}
              on={visible.camera}
              onClick={() => setVisible((v) => ({ ...v, camera: !v.camera }))}
            />
          )}
          {mode === "bipartite" && (
            <TypeRow
              label="Photos"
              color={PHOTO_COLOR}
              count={kindCounts.photo}
              on={visible.photo}
              onClick={() => setVisible((v) => ({ ...v, photo: !v.photo }))}
            />
          )}
        </div>

        {mode === "community" && communities.length > 0 && (
          <>
            <div className="tg-head">{branch ? `Under ${leaf(branch)}` : "Communities"}</div>
            <div className="tg-communities">
              {communities.map((c) => (
                <button
                  key={c.name}
                  className={`tg-comm ${activeCommunity === c.name ? "on" : ""}`}
                  onClick={() =>
                    setActiveCommunity((a) => (a === c.name ? null : c.name))
                  }
                >
                  <span className="tg-sq" style={{ background: communityColor.get(c.name) }} />
                  <span className="tg-comm-name">{c.name}</span>
                  <span className="tg-comm-count">{c.count}</span>
                </button>
              ))}
            </div>
          </>
        )}

        {mode === "community" && (
          <>
            <div className="tg-head">Link strength</div>
            <div className="tg-slider-row">
              <input
                className="tg-slider"
                type="range"
                min={0}
                max={20}
                step={1}
                value={linkThreshold}
                onChange={(e) => setLinkThreshold(Number(e.target.value))}
              />
            </div>
            <div className="tg-slider-labels">
              <span>Loose</span>
              <span>Tight</span>
            </div>
          </>
        )}

        {mode === "bipartite" && (
          <div className="tg-toggle-row">
            <span>Freeze layout</span>
            <button
              className={`tg-switch ${frozen ? "on" : ""}`}
              onClick={() => setFrozen((f) => !f)}
              aria-pressed={frozen}
            >
              <span className="tg-knob" />
            </button>
          </div>
        )}

        {mode === "bipartite" && (
          <div className="tg-toggle-row">
            <span>Thumbnails</span>
            <button
              className={`tg-switch ${thumbs ? "on" : ""}`}
              onClick={() => setThumbs((t) => !t)}
              aria-pressed={thumbs}
            >
              <span className="tg-knob" />
            </button>
          </div>
        )}

        <div className="tg-mode-row">
          <span className="tg-mode-label">Group:</span>
          <div className="tg-seg">
            <button
              className={mode === "community" ? "on" : ""}
              onClick={() => setMode("community")}
            >
              Communities
            </button>
            <button
              className={mode === "bipartite" ? "on" : ""}
              onClick={() => setMode("bipartite")}
            >
              Photo ↔ tag
            </button>
          </div>
        </div>
      </div>

      {/* ── Center canvas ──────────────────────────────────────────── */}
      <div className="tg-center">
        {error && <div className="tg-error">{error}</div>}
        <canvas
          ref={canvasRef}
          className="tg-canvas"
          onPointerDown={onPointerDown}
          onPointerMove={onPointerMove}
          onPointerLeave={onPointerLeave}
        />
        {graph && shownStats.nodes === 0 && (
          <div className="tg-hint">Turn on a node type on the left to draw the graph</div>
        )}

        {/* Top-right mini legend */}
        <div className="tg-legend">
          <span>
            <i style={{ background: PALETTE[0] }} /> Tag
          </span>
          <span>
            <i style={{ background: CAMERA_COLOR }} /> Camera
          </span>
          <span>
            <i style={{ background: PHOTO_COLOR }} /> Photo
          </span>
        </div>

        {/* Floating bottom-center glass pill */}
        <div className="tg-pill">
          <button onClick={() => zoomAt(1 / 1.2)} title="Zoom out">
            −
          </button>
          <button onClick={() => zoomAt(1.2)} title="Zoom in">
            ＋
          </button>
          <span className="tg-pill-sep" />
          <button onClick={fitView} title="Fit the whole graph in view">
            Fit
          </button>
          <button onClick={resetView} title="Re-center the graph">
            Re-center
          </button>
        </div>

        {/* Bottom status strip */}
        <div className="tg-status">
          {graph
            ? `${branch && mode === "community" ? `${branch} · ` : ""}${shownStats.nodes} nodes · ${shownStats.links} links${mode === "community" ? ` · ${shownStats.communities} ${branch ? "branches" : "communities"}` : ""}${photoOverflow > 0 ? ` · top ${BIPARTITE_PHOTO_CAP} most-tagged photos (${photoOverflow.toLocaleString()} more hidden)` : ""}`
            : "Loading…"}
        </div>
      </div>

      {/* ── Right inspector ────────────────────────────────────────── */}
      <div className="tg-right">
        {selNode ? (
          <Inspector
            node={selNode}
            color={nodeColor(selNode)}
            isHub={(degree.get(selNode.id) ?? 0) >= hubThreshold}
            photos={selNode.count}
            children={childrenCount.get(selNode.id) ?? 0}
            links={degree.get(selNode.id) ?? 0}
            connected={connected}
            nodeColorFor={nodeColor}
            topPhotos={topPhotos}
            isolated={isolate === selNode.id}
            onSelect={setSelected}
            onFilter={() => selNode.kind === "tag" && api.filterByTag(selNode.refId)}
            onIsolate={() =>
              setIsolate((cur) => (cur === selNode.id ? null : selNode.id))
            }
            onOpenLoupe={
              api.openLoupe ? () => void api.openLoupe!().catch(() => {}) : undefined
            }
            onFocusBranch={
              mode === "community" &&
              selNode.kind === "tag" &&
              (childrenCount.get(selNode.id) ?? 0) > 0 &&
              branch !== selNode.fullPath
                ? () => focusBranch(selNode.fullPath)
                : undefined
            }
          />
        ) : community ? (
          <CommunityCard
            name={community.name}
            path={community.path}
            color={community.color}
            photos={community.photos}
            tags={community.members.length}
            top={community.members.slice(0, 8)}
            nodeColorFor={nodeColor}
            inBranch={branch != null}
            onSelect={setSelected}
            onFocus={community.canFocus ? () => focusBranch(community.path) : undefined}
            onFilter={
              community.tagId != null ? () => api.filterByTag(community.tagId!) : undefined
            }
            onOpenLoupe={
              api.openLoupe ? () => void api.openLoupe!().catch(() => {}) : undefined
            }
            onClear={() => setActiveCommunity(null)}
          />
        ) : (
          <div className="tg-empty">Select a node, or a community on the left</div>
        )}
      </div>
    </div>
  );
}

/** The right panel's subject when a community is active and no node is selected. */
function CommunityCard({
  name,
  path,
  color,
  photos,
  tags,
  top,
  nodeColorFor,
  inBranch,
  onSelect,
  onFocus,
  onFilter,
  onOpenLoupe,
  onClear,
}: {
  name: string;
  path: string;
  color: string;
  photos: number;
  /** How many tags the family has on the ring. */
  tags: number;
  /** The biggest of them, by count. */
  top: GNode[];
  nodeColorFor: (n: GNode) => string;
  inBranch: boolean;
  onSelect: (id: string) => void;
  /** Redraw the ring for this family. Absent when it is a single tag, or already focused. */
  onFocus?: () => void;
  /** Filter the library to this family's tag. Absent when the vocabulary has no such tag. */
  onFilter?: () => void;
  onOpenLoupe?: () => void;
  onClear: () => void;
}) {
  return (
    <div className="tg-inspector">
      <div className="tg-inspector-scroll">
        <div className="tg-title">
          <span className="tg-dot" style={{ background: color }} />
          <span className="tg-title-text">{name}</span>
        </div>
        <div className="tg-chips">
          <span className="tg-chip">{inBranch ? "Branch" : "Community"}</span>
          {path !== name && <span className="tg-chip tg-chip-comm">{path}</span>}
        </div>

        <div className="tg-stats">
          <div className="tg-stat">
            <div className="tg-stat-n">{photos}</div>
            <div className="tg-stat-l">Photos</div>
          </div>
          <div className="tg-stat">
            <div className="tg-stat-n">{tags}</div>
            <div className="tg-stat-l">Tags</div>
          </div>
        </div>

        {top.length > 0 && (
          <>
            <div className="tg-head">Top tags</div>
            <div className="tg-conn">
              {top.map((n) => (
                <button
                  key={n.id}
                  className="tg-conn-chip"
                  onClick={() => onSelect(n.id)}
                  title={`${n.fullPath} · ${n.count}`}
                >
                  <span className="tg-dot" style={{ background: nodeColorFor(n) }} />
                  <span className="tg-conn-name">{n.label}</span>
                  <span className="tg-conn-w">{n.count}</span>
                </button>
              ))}
            </div>
          </>
        )}
      </div>

      <div className="tg-actions">
        {onFocus && (
          <button
            className="tg-btn tg-btn-primary"
            onClick={onFocus}
            title={`Redraw the graph for just "${name}" and everything under it`}
          >
            Focus on this branch
          </button>
        )}
        <button
          className={`tg-btn ${onFocus ? "tg-btn-ghost" : "tg-btn-primary"}`}
          onClick={onFilter}
          disabled={!onFilter}
          title={onFilter ? "" : "No tag in the vocabulary matches this family"}
        >
          Filter library to "{name}"
        </button>
        {onOpenLoupe && (
          <button
            className="tg-btn tg-btn-ghost"
            onClick={onOpenLoupe}
            title="Show this family — and a wall of its photos — in the pop-out loupe window"
          >
            Open loupe window
          </button>
        )}
        <button className="tg-btn tg-btn-ghost" onClick={onClear}>
          Clear
        </button>
      </div>
    </div>
  );
}

function TypeRow({
  label,
  color,
  count,
  on,
  onClick,
}: {
  label: string;
  color: string;
  count: number;
  on: boolean;
  onClick: () => void;
}) {
  return (
    <button className={`tg-type ${on ? "" : "off"}`} onClick={onClick}>
      <span className="tg-dot" style={{ background: color }} />
      <span className="tg-type-name">{label}</span>
      <span className="tg-type-count">{count}</span>
    </button>
  );
}

function Inspector({
  node,
  color,
  isHub,
  photos,
  children,
  links,
  connected,
  nodeColorFor,
  topPhotos,
  isolated,
  onSelect,
  onFilter,
  onIsolate,
  onOpenLoupe,
  onFocusBranch,
}: {
  node: GNode;
  color: string;
  isHub: boolean;
  photos: number;
  children: number;
  links: number;
  connected: { node: GNode; weight: number }[];
  nodeColorFor: (n: GNode) => string;
  topPhotos: number[];
  isolated: boolean;
  onSelect: (id: string) => void;
  onFilter: () => void;
  onIsolate: () => void;
  /** Open the pop-out loupe window, which mirrors this inspector. Absent on older hosts. */
  onOpenLoupe?: () => void;
  /** Redraw the ring for this tag's branch. Only for a tag with children, not already focused. */
  onFocusBranch?: () => void;
}) {
  const isTag = node.kind === "tag";
  return (
    <div className="tg-inspector">
      <div className="tg-inspector-scroll">
        <div className="tg-title">
          <span className="tg-dot" style={{ background: color }} />
          <span className="tg-title-text">{node.label}</span>
        </div>
        <div className="tg-chips">
          {node.kind === "camera" ? (
            <span className="tg-chip">Camera</span>
          ) : (
            <span className="tg-chip">Tag{isHub ? " · hub" : ""}</span>
          )}
          {isTag && <span className="tg-chip tg-chip-comm">Community: {node.community}</span>}
        </div>

        <div className="tg-stats">
          <div className="tg-stat">
            <div className="tg-stat-n">{photos}</div>
            <div className="tg-stat-l">Photos</div>
          </div>
          {isTag && (
            <div className="tg-stat">
              <div className="tg-stat-n">{children}</div>
              <div className="tg-stat-l">Children</div>
            </div>
          )}
          <div className="tg-stat">
            <div className="tg-stat-n">{links}</div>
            <div className="tg-stat-l">Links</div>
          </div>
        </div>

        {connected.length > 0 && (
          <>
            <div className="tg-head">Connected</div>
            <div className="tg-conn">
              {connected.slice(0, 8).map((c) => (
                <button
                  key={c.node.id}
                  className="tg-conn-chip"
                  onClick={() => onSelect(c.node.id)}
                  title={`${c.node.fullPath} · ${c.weight}`}
                >
                  <span className="tg-dot" style={{ background: nodeColorFor(c.node) }} />
                  <span className="tg-conn-name">{c.node.label}</span>
                  <span className="tg-conn-w">{c.weight}</span>
                </button>
              ))}
            </div>
          </>
        )}

        {isTag && topPhotos.length > 0 && (
          <>
            <div className="tg-head">Top photos</div>
            <div className="tg-thumbs">
              {topPhotos.map((id) => (
                <div className="tg-thumb" key={id}>
                  <img src={convertFileSrc(String(id), "thumb")} alt="" loading="lazy" />
                </div>
              ))}
            </div>
          </>
        )}
      </div>

      <div className="tg-actions">
        {onFocusBranch && (
          <button
            className="tg-btn tg-btn-primary"
            onClick={onFocusBranch}
            title={`Redraw the graph for just "${node.label}" and everything under it`}
          >
            Focus on this branch
          </button>
        )}
        <button
          className={`tg-btn ${onFocusBranch ? "tg-btn-ghost" : "tg-btn-primary"}`}
          onClick={onFilter}
          disabled={!isTag}
          title={isTag ? "" : "No camera filter is available"}
        >
          {isTag ? `Filter library to "${node.label}"` : "Filter library (tags only)"}
        </button>
        <button className="tg-btn tg-btn-ghost" onClick={onIsolate}>
          {isolated ? "Show all" : "Isolate neighbours"}
        </button>
        {onOpenLoupe && (
          <button
            className="tg-btn tg-btn-ghost"
            onClick={onOpenLoupe}
            title="Show this node — and a wall of its photos — in the pop-out loupe window"
          >
            Open loupe window
          </button>
        )}
      </div>
    </div>
  );
}

export const tagGraphModule: ChairPhotoModule = {
  id: "tag-graph",
  name: "Tag Graph",
  version: "0.1.0",
  description:
    "Visualize your library as a graph — tag communities, camera nodes, and a photo↔tag map. Select a node to inspect it.",
  // The two graph projections, plus `list_photos` to resolve a selected node to photos (#48).
  permissions: { commands: ["library_graph", "list_photos", "photo_tag_graph"] },
  onLoad(api) {
    api.registerMainView({
      id: "tag-graph",
      label: "Graph",
      // Network glyph, matching the app's 13px stroke icon language.
      icon: (
        <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
          <circle cx="18" cy="5" r="3" />
          <circle cx="6" cy="12" r="3" />
          <circle cx="18" cy="19" r="3" />
          <line x1="8.59" y1="13.51" x2="15.42" y2="17.49" />
          <line x1="15.41" y1="6.51" x2="8.59" y2="10.49" />
        </svg>
      ),
      render: () => <GraphView api={api} />,
    });
  },
};
