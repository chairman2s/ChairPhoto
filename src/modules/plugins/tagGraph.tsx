// Tag Graph module: a main-view force-directed graph of the tag vocabulary — nodes are tags
// sized by photo count, edges are co-occurrence. Frontend-only; data comes from the
// `library_graph` command. See docs/tag-graph.md.
import { useEffect, useMemo, useReducer, useRef, useState } from "react";
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
import type { ChairPhotoAPI, ChairPhotoModule } from "../registry";
import "./tagGraph.css";

// The Graph module: a full-surface view that visualizes the library as a graph.
//  • Communities — nodes = tags (coloured by their top-level community) + cameras, edges =
//    tag co-occurrence, tag hierarchy, and camera↔tag usage (from the `library_graph`
//    command). Selecting a node opens an inspector; it does NOT navigate directly.
//  • Photo ↔ tag — a bipartite graph of photos and the tags assigned to them (legacy view).
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

// Community colour palette (families). Assigned stably by sorted community name.
const PALETTE = [
  "#3B82F6",
  "#8B5CF6",
  "#10B981",
  "#F59E0B",
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

function GraphView({ api }: { api: ChairPhotoAPI }) {
  const [mode, setMode] = useState<Mode>("community");
  const [graph, setGraph] = useState<{ nodes: GNode[]; links: GLink[] } | null>(null);
  const [error, setError] = useState("");
  const [hover, setHover] = useState<string | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [thumbs, setThumbs] = useState(false); // show photo thumbnails (bipartite)
  const [view, setView] = useState({ k: 1, x: 0, y: 0 });

  // Left-panel controls.
  const [visible, setVisible] = useState<Record<Kind, boolean>>({
    tag: true,
    camera: true,
    photo: true,
  });
  const [activeCommunity, setActiveCommunity] = useState<string | null>(null);
  const [linkThreshold, setLinkThreshold] = useState(0);
  const [frozen, setFrozen] = useState(false);
  const [isolate, setIsolate] = useState<string | null>(null); // selected node whose nbhd is isolated

  const svgRef = useRef<SVGSVGElement>(null);
  const simRef = useRef<Simulation<GNode, GLink> | null>(null);
  const [, tick] = useReducer((n: number) => n + 1, 0);

  // Community → colour map, computed from the loaded tags (stable by sorted name).
  const communityColor = useMemo(() => {
    const m = new Map<string, string>();
    if (!graph) return m;
    const names = Array.from(
      new Set(
        graph.nodes
          .filter((n) => n.kind === "tag")
          .map((n) => n.community),
      ),
    ).sort();
    names.forEach((name, i) => m.set(name, PALETTE[i % PALETTE.length]));
    return m;
  }, [graph]);

  const nodeColor = (n: GNode): string => {
    if (n.kind === "camera") return CAMERA_COLOR;
    if (n.kind === "photo") return PHOTO_COLOR;
    return communityColor.get(n.community) ?? PALETTE[0];
  };

  // Communities list for the left panel (tag communities + total count).
  const communities = useMemo(() => {
    if (!graph) return [] as { name: string; count: number; color: string }[];
    const totals = new Map<string, number>();
    for (const n of graph.nodes) {
      if (n.kind !== "tag") continue;
      totals.set(n.community, (totals.get(n.community) ?? 0) + n.count);
    }
    return Array.from(totals.entries())
      .map(([name, count]) => ({ name, count, color: communityColor.get(name) ?? PALETTE[0] }))
      .sort((a, b) => b.count - a.count);
  }, [graph, communityColor]);

  // Load + shape the data whenever the mode changes.
  useEffect(() => {
    let alive = true;
    setError("");
    setGraph(null);
    setSelected(null);
    setIsolate(null);
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
          if (alive) setGraph({ nodes, links });
        } else {
          const g = await api.invoke<{
            photos: RawNode[];
            tags: RawNode[];
            edges: [number, number][];
          }>("photo_tag_graph");
          const nodes: GNode[] = [
            ...g.tags.map((n) => ({
              id: `t${n.id}`,
              kind: "tag" as const,
              refId: n.id,
              label: leaf(n.label),
              fullPath: n.label,
              count: n.count,
              community: topLevel(n.label),
              r: tagRadius(n.count),
            })),
            ...g.photos.map((n) => ({
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
          const links: GLink[] = g.edges.map(([p, t]) => ({
            source: `p${p}`,
            target: `t${t}`,
            weight: 1,
            kind: "bipartite" as const,
          }));
          if (alive) setGraph({ nodes, links });
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

  // Run / re-run the force simulation when the graph or the active link set changes.
  useEffect(() => {
    simRef.current?.stop();
    if (!graph) return;
    // d3 throws (e.g. "node not found" on a dangling link) — surface it in the view's
    // error banner instead of letting the exception unmount the whole app.
    try {
      const sim = forceSimulation<GNode, GLink>(graph.nodes)
        .force(
          "link",
          forceLink<GNode, GLink>(simLinks)
            .id((d) => d.id)
            .distance((l) => (mode === "community" ? 40 + 30 / Math.sqrt(l.weight) : 28))
            .strength(0.4),
        )
        .force("charge", forceManyBody<GNode>().strength(mode === "community" ? -120 : -60))
        .force("collide", forceCollide<GNode>().radius((d) => d.r + 2))
        .force("center", forceCenter(0, 0))
        .force("x", forceX(0).strength(0.04))
        .force("y", forceY(0).strength(0.04));
      sim.on("tick", () => tick());
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
      .slice(0, 8);
  }, [selNode, graph, nodeById]);

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

  // Pan / zoom.
  const onWheel = (e: React.WheelEvent) => {
    e.preventDefault();
    const rect = svgRef.current!.getBoundingClientRect();
    const mx = e.clientX - rect.left;
    const my = e.clientY - rect.top;
    setView((v) => {
      const k = Math.min(Math.max(v.k * (e.deltaY < 0 ? 1.15 : 1 / 1.15), 0.15), 6);
      return { k, x: mx - (k / v.k) * (mx - v.x), y: my - (k / v.k) * (my - v.y) };
    });
  };
  const zoomBy = (factor: number) => {
    const rect = svgRef.current?.getBoundingClientRect();
    const mx = (rect?.width ?? 0) / 2;
    const my = (rect?.height ?? 0) / 2;
    setView((v) => {
      const k = Math.min(Math.max(v.k * factor, 0.15), 6);
      return { k, x: mx - (k / v.k) * (mx - v.x), y: my - (k / v.k) * (my - v.y) };
    });
  };
  const resetView = () => setView({ k: 1, x: 0, y: 0 });

  const onBgPointerDown = (e: React.PointerEvent) => {
    // Clicking empty canvas deselects.
    if (e.target === svgRef.current) setSelected(null);
    const start = { mx: e.clientX, my: e.clientY, x: view.x, y: view.y };
    const move = (ev: PointerEvent) =>
      setView((v) => ({ ...v, x: start.x + (ev.clientX - start.mx), y: start.y + (ev.clientY - start.my) }));
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  };

  // Drag a node (in graph coordinates). A tiny drag = a click → SELECT the node.
  const onNodePointerDown = (e: React.PointerEvent, n: GNode) => {
    e.stopPropagation();
    const rect = svgRef.current!.getBoundingClientRect();
    const toGraph = (cx: number, cy: number) => ({
      x: (cx - rect.left - view.x) / view.k,
      y: (cy - rect.top - view.y) / view.k,
    });
    const sim = simRef.current!;
    if (!frozen) sim.alphaTarget(0.3).restart();
    const move = (ev: PointerEvent) => {
      const p = toGraph(ev.clientX, ev.clientY);
      n.fx = p.x;
      n.fy = p.y;
      if (frozen) tick();
    };
    const up = (ev: PointerEvent) => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      sim.alphaTarget(0);
      n.fx = null;
      n.fy = null;
      if (Math.hypot(ev.clientX - e.clientX, ev.clientY - e.clientY) < 4) {
        setSelected(n.id);
      }
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  };

  // Escape deselects.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setSelected(null);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const linkEnd = (e: GLink, which: "source" | "target") => {
    const v = e[which];
    return typeof v === "object" ? (v as GNode) : null;
  };

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
    const shownLinks = simLinks.filter((l) => {
      const s = typeof l.source === "object" ? (l.source as GNode).id : (l.source as string);
      const t = typeof l.target === "object" ? (l.target as GNode).id : (l.target as string);
      return shownIds.has(s) && shownIds.has(t);
    });
    return { nodes: shownNodes.length, links: shownLinks.length, communities: communities.length };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [graph, simLinks, visible, isolateSet, communities.length]);

  const communityDimmed = (n: GNode) =>
    activeCommunity != null && n.kind === "tag" && n.community !== activeCommunity;

  return (
    <div className="tg-root">
      {/* ── Left panel ─────────────────────────────────────────────── */}
      <div className="tg-left">
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
            <div className="tg-head">Communities</div>
            <div className="tg-communities">
              {communities.map((c) => (
                <button
                  key={c.name}
                  className={`tg-comm ${activeCommunity === c.name ? "on" : ""}`}
                  onClick={() =>
                    setActiveCommunity((a) => (a === c.name ? null : c.name))
                  }
                >
                  <span className="tg-sq" style={{ background: c.color }} />
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
        <svg
          ref={svgRef}
          className="tg-svg"
          onWheel={onWheel}
          onPointerDown={onBgPointerDown}
        >
          <g
            transform={`translate(${view.x},${view.y}) scale(${view.k}) translate(${(svgRef.current?.clientWidth ?? 0) / 2},${(svgRef.current?.clientHeight ?? 0) / 2})`}
          >
            {graph?.links.map((l, i) => {
              if (l.kind === "cooc" && l.weight < linkThreshold) return null;
              const s = linkEnd(l, "source");
              const t = linkEnd(l, "target");
              if (!s || !t) return null;
              if (!nodeShown(s) || !nodeShown(t)) return null;
              const active = hover != null && (s.id === hover || t.id === hover);
              const sel = selected != null && (s.id === selected || t.id === selected);
              const stroke =
                l.kind === "camera"
                  ? CAMERA_COLOR
                  : nodeColor(s.kind === "camera" ? t : s);
              const cls = `tg-link tg-link-${l.kind}${active || sel ? " active" : ""}`;
              return (
                <line
                  key={i}
                  x1={s.x}
                  y1={s.y}
                  x2={t.x}
                  y2={t.y}
                  className={cls}
                  stroke={stroke}
                  strokeWidth={Math.min(1 + Math.log2(l.weight + 1) * 0.6, 4)}
                />
              );
            })}
            {graph?.nodes.map((n) => {
              if (!nodeShown(n)) return null;
              const dim =
                (hover != null && hover !== n.id && !neighbors.get(hover)?.has(n.id)) ||
                communityDimmed(n);
              const isSel = selected === n.id;
              const showLabel =
                n.kind !== "photo" && (n.r > 7 || hover === n.id || isSel);
              const fill = nodeColor(n);
              return (
                <g
                  key={n.id}
                  transform={`translate(${n.x ?? 0},${n.y ?? 0})`}
                  className={`tg-node ${n.kind} ${dim ? "dim" : ""} ${isSel ? "sel" : ""}`}
                  onPointerDown={(e) => onNodePointerDown(e, n)}
                  onPointerEnter={() => setHover(n.id)}
                  onPointerLeave={() => setHover((h) => (h === n.id ? null : h))}
                >
                  {isSel && <circle r={n.r + 3} className="tg-sel-ring" />}
                  {n.kind === "photo" && thumbs ? (
                    <>
                      <clipPath id={`clip-${n.id}`}>
                        <circle r={n.r} />
                      </clipPath>
                      <image
                        href={convertFileSrc(String(n.refId), "thumb")}
                        x={-n.r}
                        y={-n.r}
                        width={n.r * 2}
                        height={n.r * 2}
                        clipPath={`url(#clip-${n.id})`}
                        preserveAspectRatio="xMidYMid slice"
                      />
                      <circle r={n.r} className="tg-thumb-ring" fill="none">
                        <title>
                          {n.label} · {n.count} tag(s)
                        </title>
                      </circle>
                    </>
                  ) : (
                    <circle r={n.kind === "photo" ? 5 : n.r} fill={fill}>
                      <title>
                        {n.fullPath} · {n.count}{" "}
                        {n.kind === "tag" ? "photo(s)" : n.kind === "camera" ? "photo(s)" : "tag(s)"}
                      </title>
                    </circle>
                  )}
                  {showLabel && (
                    <text x={n.r + 3} y={3} className="tg-label">
                      {n.label}
                    </text>
                  )}
                </g>
              );
            })}
          </g>
        </svg>

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
          <button onClick={() => zoomBy(1 / 1.2)} title="Zoom out">
            −
          </button>
          <button onClick={() => zoomBy(1.2)} title="Zoom in">
            ＋
          </button>
          <span className="tg-pill-sep" />
          <button onClick={resetView} title="Fit / reset view">
            Fit
          </button>
          <button onClick={resetView} title="Re-center the graph">
            Re-center
          </button>
        </div>

        {/* Bottom status strip */}
        <div className="tg-status">
          {graph
            ? `${shownStats.nodes} nodes · ${shownStats.links} links${mode === "community" ? ` · ${shownStats.communities} communities` : ""}`
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
          />
        ) : (
          <div className="tg-empty">Select a node</div>
        )}
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
              {connected.map((c) => (
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
        <button
          className="tg-btn tg-btn-primary"
          onClick={onFilter}
          disabled={!isTag}
          title={isTag ? "" : "No camera filter is available"}
        >
          {isTag ? `Filter library to "${node.label}"` : "Filter library (tags only)"}
        </button>
        <button className="tg-btn tg-btn-ghost" onClick={onIsolate}>
          {isolated ? "Show all" : "Isolate neighbours"}
        </button>
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
