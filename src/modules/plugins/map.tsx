// Map module — full-surface Leaflet/OSM map view of photos by GPS location.
//
// Architecture:
//   • Fetches GPS points via `map_photo_points` (backend feature "map").
//   • Renders clustered markers via Leaflet + leaflet.markercluster.
//   • Clicking a marker or cluster opens a bottom-docked filmstrip showing every
//     photo at that node. Clicking a filmstrip thumb calls `api.selectPhotoSilent`
//     so the pop-out loupe follows WITHOUT leaving the map view.
//   • "Show in Library" on the filmstrip navigates to the Library and selects the
//     photo (replaces the old H2c marker-click → Library behavior).
//   • Esc / filmstrip × button dismisses the strip.
//   • Tile source URL is a module setting (defaults to OSM; user-configurable so
//     heavy users can point at their own tile server).
//   • A settings panel lets the user change the tile URL.
//   • Fence drawing: click "Draw fence" → click the map to add vertices, double-click
//     to close the polygon. Existing fences are shown as polygons with vertex drag handles.
//     Each fence has name + tag path; saving goes via the H2b CRUD commands.
//   • "Apply to existing photos" per fence + "Apply all" with result-count toast.
//   • Module only activates when the "map" backend feature is compiled in.

import { useCallback, useEffect, useRef, useState } from "react";
import L from "leaflet";
import "leaflet/dist/leaflet.css";
import "leaflet.markercluster/dist/MarkerCluster.css";
import "leaflet.markercluster/dist/MarkerCluster.Default.css";
// leaflet.markercluster extends L.markerClusterGroup at runtime; import for side-effects.
import "leaflet.markercluster";
import type { ChairPhotoAPI, ChairPhotoModule } from "../registry";
import { thumbnailUrl } from "../api";
import { useHostSelection } from "../host";
import "./map.css";

// ── Backend commands (owned by this module) ───────────────────────────────────
// Per the module contract, a module's own commands go through `ChairPhotoAPI.invoke`
// rather than core's `api.ts`. The fence/point commands below already call
// `api.invoke` inline; these are the geocode ones that used to live in core.

/** Progress payload for `geocode_all_to_iptc` (`geocode:progress`). */
interface GeocodeProgress {
  done: number;
  total: number;
  filled: number;
}

/** Summary returned by `geocode_all_to_iptc`. */
interface GeocodeAllSummary {
  total: number;
  filled: number;
  skipped: number;
}

/**
 * Reverse-geocode all photos with GPS that have at least one empty IPTC location
 * field. Emits `geocode:progress` events. Returns a summary.
 *
 * Requires the "map" backend feature.
 */
const geocodeAllToIptc = (api: ChairPhotoAPI) =>
  api.invoke<GeocodeAllSummary>("geocode_all_to_iptc");

// ── Types ─────────────────────────────────────────────────────────────────────

interface PhotoPoint {
  id: number;
  lat: number;
  lng: number;
}

/** Mirror of the backend `Fence` struct. Polygon is an array of [lat, lng] pairs. */
interface Fence {
  id: number;
  name: string;
  tagPath: string;
  polygon: [number, number][];
  createdAt: number;
}

/** The set of photo IDs shown in the bottom filmstrip after a marker/cluster click. */
interface FilmstripState {
  /** Ordered photo ids shown in the strip. */
  ids: number[];
  /** The id currently active in the strip (highlighted + loupe follows it). */
  activeId: number;
}

// ── Constants ─────────────────────────────────────────────────────────────────

const SETTING_TILE_URL = "tileUrl";
const DEFAULT_TILE_URL =
  "https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png";
const OSM_ATTRIBUTION =
  '&copy; <a href="https://www.openstreetmap.org/copyright" target="_blank">OpenStreetMap</a> contributors';

// Polygon colours for fences (cycle through palette).
const FENCE_COLORS = [
  "#f59e0b",
  "#10b981",
  "#8b5cf6",
  "#ec4899",
  "#14b8a6",
  "#f97316",
  "#ef4444",
];

function fenceColor(index: number) {
  return FENCE_COLORS[index % FENCE_COLORS.length];
}

// ── Fix Leaflet's default icon paths (broken in bundlers) ─────────────────────
// Leaflet expects its marker PNGs to live next to the JS file; in a Vite bundle the
// paths break.  Provide inline SVG data URIs instead.

const iconSvg = (color: string) =>
  `<svg xmlns="http://www.w3.org/2000/svg" width="24" height="36" viewBox="0 0 24 36">
    <path fill="${color}" stroke="#fff" stroke-width="1.5"
      d="M12 0C5.37 0 0 5.37 0 12c0 9 12 24 12 24S24 21 24 12C24 5.37 18.63 0 12 0z"/>
    <circle cx="12" cy="12" r="4" fill="#fff"/>
  </svg>`;

function makeIcon(color = "#3b82f6") {
  return L.divIcon({
    html: iconSvg(color),
    className: "",
    iconSize: [24, 36],
    iconAnchor: [12, 36],
    popupAnchor: [0, -36],
  });
}

const MARKER_ICON = makeIcon();

// Vertex drag handle icon (small circle).
function makeVertexIcon(color: string) {
  return L.divIcon({
    html: `<div style="width:10px;height:10px;border-radius:50%;background:${color};border:2px solid #fff;box-shadow:0 0 3px rgba(0,0,0,.5);cursor:grab;"></div>`,
    className: "",
    iconSize: [10, 10],
    iconAnchor: [5, 5],
  });
}

// ── Fence editor dialog ────────────────────────────────────────────────────────
// Modal for entering / editing a fence's name and tag path before saving.

interface FenceEditorProps {
  initialName?: string;
  initialTagPath?: string;
  onSave(name: string, tagPath: string): void;
  onCancel(): void;
}

function FenceEditorDialog({
  initialName = "",
  initialTagPath = "",
  onSave,
  onCancel,
}: FenceEditorProps) {
  const [name, setName] = useState(initialName);
  const [tagPath, setTagPath] = useState(initialTagPath);
  const [error, setError] = useState("");

  const handleSave = () => {
    const trimmedName = name.trim();
    const trimmedPath = tagPath.trim();
    if (!trimmedName) { setError("Name is required."); return; }
    if (!trimmedPath) { setError("Tag path is required (e.g. Places/Oslo/Aker Brygge)."); return; }
    onSave(trimmedName, trimmedPath);
  };

  return (
    <div className="mp-dialog-backdrop" onClick={onCancel}>
      <div className="mp-dialog" onClick={(e) => e.stopPropagation()}>
        <div className="mp-dialog-title">
          {initialName ? "Edit fence" : "New fence"}
        </div>

        <div className="mp-dialog-row">
          <label className="mp-dialog-label">Name</label>
          <input
            className="mp-dialog-input"
            value={name}
            onChange={(e) => { setName(e.target.value); setError(""); }}
            placeholder="e.g. Aker Brygge"
            autoFocus
            onKeyDown={(e) => { if (e.key === "Enter") handleSave(); if (e.key === "Escape") onCancel(); }}
          />
        </div>

        <div className="mp-dialog-row">
          <label className="mp-dialog-label">Tag path</label>
          <input
            className="mp-dialog-input"
            value={tagPath}
            onChange={(e) => { setTagPath(e.target.value); setError(""); }}
            placeholder="e.g. Places/Oslo/Aker Brygge"
            onKeyDown={(e) => { if (e.key === "Enter") handleSave(); if (e.key === "Escape") onCancel(); }}
          />
          <div className="mp-dialog-hint">
            Use forward slashes for hierarchy, e.g. <code>Places/Norway/Oslo/Sentrum</code>.
            The tag and its ancestors are created automatically.
          </div>
        </div>

        {error && <div className="mp-dialog-error">{error}</div>}

        <div className="mp-dialog-actions">
          <button className="mp-btn mp-btn-primary" onClick={handleSave}>
            Save
          </button>
          <button className="mp-btn" onClick={onCancel}>
            Cancel
          </button>
        </div>
      </div>
    </div>
  );
}

// ── Fence list panel (overlay on the left side of the map) ────────────────────

interface FenceListProps {
  fences: Fence[];
  selectedId: number | null;
  applying: number | null; // fence id currently being applied (null = none)
  applyingAll: boolean;
  onSelect(id: number): void;
  onEdit(fence: Fence): void;
  onDelete(fence: Fence): void;
  onApply(fence: Fence): void;
  onApplyAll(): void;
  onDraw(): void;
  drawing: boolean;
}

function FenceListPanel({
  fences,
  selectedId,
  applying,
  applyingAll,
  onSelect,
  onEdit,
  onDelete,
  onApply,
  onApplyAll,
  onDraw,
  drawing,
}: FenceListProps) {
  return (
    <div className="mp-fence-panel">
      <div className="mp-fence-header">
        <span className="mp-fence-title">Fences</span>
        <button
          className={`mp-btn mp-btn-sm${drawing ? " mp-btn-active" : ""}`}
          onClick={onDraw}
          title={drawing ? "Cancel drawing" : "Draw new fence"}
        >
          {drawing ? "Cancel" : "+ Draw"}
        </button>
      </div>

      {fences.length === 0 && !drawing && (
        <div className="mp-fence-empty">
          No fences yet. Click <strong>+ Draw</strong> to draw a polygon on the map.
        </div>
      )}

      {drawing && (
        <div className="mp-fence-drawing-hint">
          Click the map to add vertices.
          Double-click the last vertex (or the first) to close the polygon.
        </div>
      )}

      <div className="mp-fence-list">
        {fences.map((fence, idx) => (
          <div
            key={fence.id}
            className={`mp-fence-item${selectedId === fence.id ? " mp-fence-item-selected" : ""}`}
            onClick={() => onSelect(fence.id)}
          >
            <div className="mp-fence-item-color" style={{ background: fenceColor(idx) }} />
            <div className="mp-fence-item-body">
              <div className="mp-fence-item-name">{fence.name}</div>
              <div className="mp-fence-item-path">{fence.tagPath}</div>
              <div className="mp-fence-item-actions">
                <button
                  className="mp-btn mp-btn-xs"
                  onClick={(e) => { e.stopPropagation(); onApply(fence); }}
                  disabled={applying === fence.id || applyingAll}
                  title="Apply this fence to all existing photos"
                >
                  {applying === fence.id ? "Applying…" : "Apply"}
                </button>
                <button
                  className="mp-btn mp-btn-xs"
                  onClick={(e) => { e.stopPropagation(); onEdit(fence); }}
                  disabled={applying !== null || applyingAll}
                >
                  Edit
                </button>
                <button
                  className="mp-btn mp-btn-xs mp-btn-danger"
                  onClick={(e) => { e.stopPropagation(); onDelete(fence); }}
                  disabled={applying !== null || applyingAll}
                >
                  Delete
                </button>
              </div>
            </div>
          </div>
        ))}
      </div>

      {fences.length > 0 && (
        <div className="mp-fence-footer">
          <button
            className="mp-btn mp-btn-sm mp-btn-primary"
            onClick={onApplyAll}
            disabled={applying !== null || applyingAll}
          >
            {applyingAll ? "Applying all…" : "Apply all fences"}
          </button>
        </div>
      )}
    </div>
  );
}

// ── MapFilmstrip component ────────────────────────────────────────────────────
//
// A bottom-docked horizontal thumb row opened when the user clicks a marker or
// cluster on the map. Clicking a thumb:
//   1. Sets it as the active thumb (highlighted).
//   2. Calls `api.selectPhotoSilent()` so the pop-out loupe updates WITHOUT
//      leaving the map view.
// "Show in Library" navigates to the Library for the currently-active thumb.
// Esc / × closes the strip.

interface MapFilmstripProps {
  strip: FilmstripState;
  /** Called when the user clicks a thumb; the caller should update activeId and call
   *  api.selectPhotoSilent(id) so the loupe follows. */
  onSelect(id: number): void;
  /** Called when "Show in Library" is clicked; navigates to Library for the active photo. */
  onShowInLibrary(): void;
  onClose(): void;
}

function MapFilmstrip({ strip, onSelect, onShowInLibrary, onClose }: MapFilmstripProps) {
  const { ids, activeId } = strip;

  // Dismiss on Esc.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  return (
    <div className="mp-filmstrip" role="region" aria-label="Photos at this location">
      {/* Header row with count + actions */}
      <div className="mp-filmstrip-header">
        <span className="mp-filmstrip-count">
          {ids.length} photo{ids.length !== 1 ? "s" : ""} at this location
        </span>
        <div className="mp-filmstrip-actions">
          <button
            className="mp-btn mp-btn-sm mp-btn-primary"
            onClick={onShowInLibrary}
            title="Open in Library and select this photo"
          >
            Show in Library
          </button>
          <button
            className="mp-btn mp-btn-sm mp-filmstrip-close"
            onClick={onClose}
            title="Close filmstrip (Esc)"
            aria-label="Close filmstrip"
          >
            ×
          </button>
        </div>
      </div>

      {/* Scrollable thumb row */}
      <div className="mp-filmstrip-row">
        {ids.map((id) => {
          const thumbUrl = thumbnailUrl(id);
          const isActive = id === activeId;
          return (
            <button
              key={id}
              className={`mp-filmstrip-thumb${isActive ? " mp-filmstrip-thumb-active" : ""}`}
              onClick={() => onSelect(id)}
              title={`Select photo ${id}`}
              aria-pressed={isActive}
            >
              <img
                src={thumbUrl}
                alt=""
                className="mp-filmstrip-thumb-img"
                loading="lazy"
                draggable={false}
                onError={(e) => {
                  (e.currentTarget as HTMLImageElement).style.visibility = "hidden";
                }}
              />
            </button>
          );
        })}
      </div>
    </div>
  );
}

// ── MapView component ─────────────────────────────────────────────────────────

function MapView({ api }: { api: ChairPhotoAPI }) {
  const containerRef = useRef<HTMLDivElement>(null);
  // Keep a stable ref for the Leaflet map instance so effects don't recreate it.
  const mapRef = useRef<L.Map | null>(null);
  const clusterRef = useRef<L.MarkerClusterGroup | null>(null);
  const tileRef = useRef<L.TileLayer | null>(null);

  // Refs for fence layers: maps fence id → { polygon layer, vertex markers }
  const fenceLayersRef = useRef<Map<number, {
    poly: L.Polygon;
    vertices: L.Marker[];
  }>>(new Map());

  // Drawing state refs (kept out of React state to avoid re-renders on every click).
  const drawingRef = useRef(false);
  const drawVerticesRef = useRef<[number, number][]>([]);
  const drawPolyRef = useRef<L.Polygon | null>(null);
  const drawMarkersRef = useRef<L.Marker[]>([]);

  const [points, setPoints] = useState<PhotoPoint[] | null>(null);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(true);
  const [tileUrl, setTileUrl] = useState<string>(DEFAULT_TILE_URL);

  // Filmstrip: null = closed; set when a marker or cluster is clicked.
  const [filmstrip, setFilmstrip] = useState<FilmstripState | null>(null);

  // Fence state.
  const [fences, setFences] = useState<Fence[]>([]);
  const [selectedFenceId, setSelectedFenceId] = useState<number | null>(null);
  const [drawing, setDrawing] = useState(false);
  const [applying, setApplying] = useState<number | null>(null);
  const [applyingAll, setApplyingAll] = useState(false);

  // Editor dialog state.
  const [editorOpen, setEditorOpen] = useState(false);
  const [editorFence, setEditorFence] = useState<Fence | null>(null); // null = new fence
  const [pendingPolygon, setPendingPolygon] = useState<[number, number][] | null>(null);

  // Load the tile URL setting once on mount.
  useEffect(() => {
    api
      .getSetting(SETTING_TILE_URL)
      .then((v) => {
        if (v) setTileUrl(v);
      })
      .catch(() => {
        // If the setting isn't stored yet, use the default — already set.
      });
  }, [api]);

  // Fetch GPS points from the backend.
  useEffect(() => {
    let alive = true;
    setLoading(true);
    setError("");
    api
      .invoke<PhotoPoint[]>("map_photo_points")
      .then((pts) => {
        if (alive) {
          setPoints(pts);
          setLoading(false);
        }
      })
      .catch((e: unknown) => {
        if (alive) {
          setError(String(e));
          setLoading(false);
        }
      });
    return () => {
      alive = false;
    };
  }, [api]);

  // Fetch fences from the backend.
  const loadFences = useCallback(() => {
    api
      .invoke<Fence[]>("list_fences")
      .then(setFences)
      .catch((e: unknown) => {
        setError(String(e));
      });
  }, [api]);

  useEffect(() => {
    loadFences();
  }, [loadFences]);

  // Create the Leaflet map once (the container div is stable across re-renders).
  useEffect(() => {
    const el = containerRef.current;
    if (!el || mapRef.current) return; // already initialised

    const map = L.map(el, {
      center: [20, 0],
      zoom: 2,
      zoomControl: true,
    });
    mapRef.current = map;

    const tile = L.tileLayer(tileUrl, {
      attribution: OSM_ATTRIBUTION,
      maxZoom: 19,
    }).addTo(map);
    tileRef.current = tile;

    // Marker cluster group — will hold all photo markers.
    // `zoomToBoundsOnClick: false` prevents the library from internally binding
    // `_zoomOrSpiderfy` to `clusterclick`, which would otherwise zoom/spiderfy
    // the map at the same time as our filmstrip-open handler runs.
    const cluster = L.markerClusterGroup({
      maxClusterRadius: 60,
      spiderfyOnMaxZoom: true,
      showCoverageOnHover: false,
      animate: false, // keep it snappy in WebKit
      zoomToBoundsOnClick: false,
    });
    clusterRef.current = cluster;
    map.addLayer(cluster);

    return () => {
      // Leaflet attaches event listeners to the DOM; clean them up on unmount.
      map.remove();
      mapRef.current = null;
      clusterRef.current = null;
      tileRef.current = null;
    };
    // tileUrl intentionally excluded: tile changes are handled by the effect below.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []); // run once

  // Swap the tile layer when the URL changes.
  useEffect(() => {
    const map = mapRef.current;
    const old = tileRef.current;
    if (!map) return;
    if (old) map.removeLayer(old);
    const tile = L.tileLayer(tileUrl, {
      attribution: OSM_ATTRIBUTION,
      maxZoom: 19,
    }).addTo(map);
    tileRef.current = tile;
  }, [tileUrl]);

  // Populate/replace markers whenever GPS points change.
  useEffect(() => {
    const cluster = clusterRef.current;
    const map = mapRef.current;
    if (!cluster || !map || points === null) return;

    cluster.clearLayers();
    if (points.length === 0) return;

    const markers: L.Marker[] = [];
    for (const pt of points) {
      const m = L.marker([pt.lat, pt.lng], { icon: MARKER_ICON });
      // Clicking a single marker opens the filmstrip (one photo) and selects it
      // silently so the loupe follows WITHOUT leaving the map view.
      m.on("click", () => {
        api.selectPhotoSilent(pt.id);
        setFilmstrip({ ids: [pt.id], activeId: pt.id });
      });
      // Attach the photo id to the marker so cluster-click can collect them.
      (m as L.Marker & { _photoId?: number })._photoId = pt.id;
      markers.push(m);
    }
    cluster.addLayers(markers);

    // Cluster click: collect all leaf marker ids and open the filmstrip.
    // Remove any previously-bound listener first so we don't accumulate handlers
    // across re-runs of this effect (points can change after a catalog refresh).
    cluster.off("clusterclick");
    cluster.on("clusterclick", (e: L.LeafletEvent) => {
      const clusterMarker = (e as { layer: L.MarkerCluster }).layer;
      const leaves = clusterMarker.getAllChildMarkers() as Array<L.Marker & { _photoId?: number }>;
      const ids = leaves
        .map((l) => l._photoId)
        .filter((id): id is number => id !== undefined);
      if (ids.length === 0) return;
      // Select the first photo so the loupe shows something immediately.
      api.selectPhotoSilent(ids[0]);
      setFilmstrip({ ids, activeId: ids[0] });
    });

    // On first load, fit the map to the data extent.
    if (markers.length > 0) {
      const group = L.featureGroup(markers);
      try {
        map.fitBounds(group.getBounds().pad(0.05), { maxZoom: 12 });
      } catch {
        // getBounds can throw if all markers are at the exact same point.
      }
    }
  }, [points, api]);

  // ── Helper: clear the draw-in-progress layers ──────────────────────────────
  const clearDrawLayers = useCallback(() => {
    const map = mapRef.current;
    if (!map) return;
    if (drawPolyRef.current) {
      map.removeLayer(drawPolyRef.current);
      drawPolyRef.current = null;
    }
    for (const m of drawMarkersRef.current) {
      map.removeLayer(m);
    }
    drawMarkersRef.current = [];
    drawVerticesRef.current = [];
  }, []);

  // ── Helper: rebuild the in-progress polygon preview ──────────────────────────
  const updateDrawPoly = useCallback(() => {
    const map = mapRef.current;
    if (!map) return;
    const verts = drawVerticesRef.current;
    if (drawPolyRef.current) map.removeLayer(drawPolyRef.current);
    if (verts.length < 2) { drawPolyRef.current = null; return; }
    const poly = L.polygon(verts as L.LatLngExpression[], {
      color: "#3b82f6",
      weight: 2,
      fillColor: "#3b82f6",
      fillOpacity: 0.15,
      dashArray: "6 4",
    }).addTo(map);
    drawPolyRef.current = poly;
  }, []);

  // ── Helper: close + save the drawn polygon ─────────────────────────────────
  const finishDrawing = useCallback(() => {
    const map = mapRef.current;
    // Re-enable double-click zoom now that drawing is done.
    if (map) map.doubleClickZoom.enable();

    let verts = [...drawVerticesRef.current];
    if (verts.length < 3) {
      // Not enough vertices — cancel.
      clearDrawLayers();
      drawingRef.current = false;
      setDrawing(false);
      return;
    }

    // A double-click to close fires two click events before dblclick, so the
    // last vertex is a duplicate of the second-to-last.  Drop it.
    if (verts.length >= 2) {
      const last = verts[verts.length - 1];
      const prev = verts[verts.length - 2];
      if (last[0] === prev[0] && last[1] === prev[1]) {
        verts = verts.slice(0, -1);
      }
    }

    // Save the polygon, then open the editor to set name + tag path.
    clearDrawLayers();
    drawingRef.current = false;
    setDrawing(false);
    setPendingPolygon(verts);
    setEditorFence(null);
    setEditorOpen(true);
  }, [clearDrawLayers]);

  // ── Map click handler for drawing mode ────────────────────────────────────
  useEffect(() => {
    const map = mapRef.current;
    if (!map) return;

    const onMapClick = (e: L.LeafletMouseEvent) => {
      if (!drawingRef.current) return;
      const { lat, lng } = e.latlng;
      const verts = drawVerticesRef.current;

      // If clicking near the first vertex (within ~15px on screen) and we have ≥3
      // vertices, close the polygon.
      if (verts.length >= 3) {
        const firstPt = map.latLngToContainerPoint(
          L.latLng(verts[0][0], verts[0][1])
        );
        const clickPt = map.latLngToContainerPoint(e.latlng);
        const dx = firstPt.x - clickPt.x;
        const dy = firstPt.y - clickPt.y;
        if (Math.sqrt(dx * dx + dy * dy) < 16) {
          finishDrawing();
          return;
        }
      }

      // Add vertex.
      const newVerts: [number, number][] = [...verts, [lat, lng]];
      drawVerticesRef.current = newVerts;

      // Vertex marker.
      const icon = makeVertexIcon("#3b82f6");
      const m = L.marker([lat, lng], { icon, draggable: false }).addTo(map);
      drawMarkersRef.current.push(m);
      updateDrawPoly();
    };

    const onMapDblClick = (e: L.LeafletMouseEvent) => {
      if (!drawingRef.current) return;
      L.DomEvent.stopPropagation(e);
      // Double-click also adds the point first (from the preceding click event which
      // Leaflet fires before dblclick), so we just close.
      finishDrawing();
    };

    map.on("click", onMapClick);
    map.on("dblclick", onMapDblClick);

    return () => {
      map.off("click", onMapClick);
      map.off("dblclick", onMapDblClick);
    };
  }, [finishDrawing, updateDrawPoly]);

  // ── Sync fence polygons onto the map ──────────────────────────────────────
  useEffect(() => {
    const map = mapRef.current;
    if (!map) return;

    const existing = fenceLayersRef.current;

    // Remove layers for fences that are no longer in the list.
    for (const [id, layers] of existing.entries()) {
      if (!fences.find((f) => f.id === id)) {
        map.removeLayer(layers.poly);
        for (const v of layers.vertices) map.removeLayer(v);
        existing.delete(id);
      }
    }

    // Add or update layers for current fences.
    fences.forEach((fence, idx) => {
      const color = fenceColor(idx);
      const latLngs = fence.polygon as L.LatLngExpression[];

      const existingLayers = existing.get(fence.id);
      if (existingLayers) {
        // Update polygon shape and color.
        existingLayers.poly.setLatLngs(latLngs);
        existingLayers.poly.setStyle({ color, fillColor: color });
        // Rebuild vertex markers.
        for (const v of existingLayers.vertices) map.removeLayer(v);
        existingLayers.vertices = buildVertexMarkers(map, fence, color, existingLayers.poly, (newPoly) => {
          // On vertex drag end — update the fence on the backend.
          handlePolygonEdit(fence, newPoly);
        });
      } else {
        // Create polygon.
        const poly = L.polygon(latLngs, {
          color,
          weight: 2,
          fillColor: color,
          fillOpacity: 0.15,
        }).addTo(map);

        // Click on the polygon selects it in the list.
        poly.on("click", () => setSelectedFenceId(fence.id));

        // Vertex drag markers.
        const vertices = buildVertexMarkers(map, fence, color, poly, (newPoly) => {
          handlePolygonEdit(fence, newPoly);
        });

        existing.set(fence.id, { poly, vertices });
      }
    });
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [fences]);

  // ── Handle polygon vertex edit (drag) → update_fence ─────────────────────
  const handlePolygonEdit = useCallback(
    (fence: Fence, newPolygon: [number, number][]) => {
      api
        .invoke<number>("update_fence", {
          fenceId: fence.id,
          name: fence.name,
          tagPath: fence.tagPath,
          polygon: newPolygon,
        })
        .then(() => {
          // Update local state without full reload.
          setFences((prev) =>
            prev.map((f) =>
              f.id === fence.id ? { ...f, polygon: newPolygon } : f
            )
          );
        })
        .catch((e: unknown) => {
          api.showToast(`Failed to save polygon: ${String(e)}`);
        });
    },
    [api]
  );

  // ── Start drawing mode ────────────────────────────────────────────────────
  const handleStartDraw = useCallback(() => {
    const map = mapRef.current;
    if (drawing) {
      // Cancel.
      clearDrawLayers();
      drawingRef.current = false;
      setDrawing(false);
      if (map) map.doubleClickZoom.enable();
    } else {
      drawingRef.current = true;
      setDrawing(true);
      drawVerticesRef.current = [];
      if (map) map.doubleClickZoom.disable();
    }
  }, [drawing, clearDrawLayers]);

  // ── Save new fence (from editor dialog) ──────────────────────────────────
  const handleEditorSave = useCallback(
    (name: string, tagPath: string) => {
      setEditorOpen(false);
      if (editorFence) {
        // Editing existing fence metadata (name / tag path).
        api
          .invoke<number>("update_fence", {
            fenceId: editorFence.id,
            name,
            tagPath,
            polygon: editorFence.polygon,
          })
          .then(() => loadFences())
          .catch((e: unknown) => api.showToast(`Failed to update fence: ${String(e)}`));
      } else if (pendingPolygon) {
        // Creating a new fence.
        api
          .invoke<Fence>("create_fence", {
            name,
            tagPath,
            polygon: pendingPolygon,
          })
          .then(() => {
            setPendingPolygon(null);
            loadFences();
          })
          .catch((e: unknown) => api.showToast(`Failed to save fence: ${String(e)}`));
      }
    },
    [api, editorFence, pendingPolygon, loadFences]
  );

  // ── Edit an existing fence (open the dialog) ──────────────────────────────
  const handleEdit = useCallback((fence: Fence) => {
    setEditorFence(fence);
    setPendingPolygon(null);
    setEditorOpen(true);
  }, []);

  // ── Delete a fence ────────────────────────────────────────────────────────
  const handleDelete = useCallback(
    (fence: Fence) => {
      if (!window.confirm(`Delete fence "${fence.name}"? Existing photo tags are kept.`)) return;
      api
        .invoke<number>("delete_fence", { fenceId: fence.id })
        .then(() => {
          if (selectedFenceId === fence.id) setSelectedFenceId(null);
          loadFences();
        })
        .catch((e: unknown) => api.showToast(`Failed to delete fence: ${String(e)}`));
    },
    [api, selectedFenceId, loadFences]
  );

  // ── Apply a single fence ──────────────────────────────────────────────────
  const handleApply = useCallback(
    (fence: Fence) => {
      setApplying(fence.id);
      api
        .invoke<number>("apply_fence", { fenceId: fence.id })
        .then((count) => {
          setApplying(null);
          api.showToast(
            count === 1
              ? `Applied "${fence.name}": 1 photo newly tagged.`
              : `Applied "${fence.name}": ${count} photos newly tagged.`
          );
          api.notifyChange();
        })
        .catch((e: unknown) => {
          setApplying(null);
          api.showToast(`Failed to apply fence: ${String(e)}`);
        });
    },
    [api]
  );

  // ── Apply all fences ──────────────────────────────────────────────────────
  const handleApplyAll = useCallback(() => {
    setApplyingAll(true);
    api
      .invoke<number>("apply_all_fences")
      .then((count) => {
        setApplyingAll(false);
        api.showToast(
          count === 1
            ? "Applied all fences: 1 photo newly tagged."
            : `Applied all fences: ${count} photos newly tagged.`
        );
        api.notifyChange();
      })
      .catch((e: unknown) => {
        setApplyingAll(false);
        api.showToast(`Failed to apply fences: ${String(e)}`);
      });
  }, [api]);

  const gpsCount = points?.length ?? 0;

  // ── Filmstrip callbacks ────────────────────────────────────────────────────
  const handleFilmstripSelect = useCallback(
    (id: number) => {
      setFilmstrip((prev) => (prev ? { ...prev, activeId: id } : prev));
      api.selectPhotoSilent(id);
    },
    [api]
  );

  const handleFilmstripShowInLibrary = useCallback(() => {
    if (filmstrip) {
      api.selectPhoto(filmstrip.activeId); // navigates to Library + selects
    }
    setFilmstrip(null);
  }, [api, filmstrip]);

  const handleFilmstripClose = useCallback(() => {
    setFilmstrip(null);
  }, []);

  return (
    <div className="mp-root">
      {/* ── Leaflet map ─────────────────────────────────────── */}
      <div ref={containerRef} className={`mp-map${drawing ? " mp-map-drawing" : ""}${filmstrip ? " mp-map-with-strip" : ""}`} />

      {/* ── Fence list overlay ────────────────────────────── */}
      <FenceListPanel
        fences={fences}
        selectedId={selectedFenceId}
        applying={applying}
        applyingAll={applyingAll}
        onSelect={setSelectedFenceId}
        onEdit={handleEdit}
        onDelete={handleDelete}
        onApply={handleApply}
        onApplyAll={handleApplyAll}
        onDraw={handleStartDraw}
        drawing={drawing}
      />

      {/* Overlays */}
      {loading && <div className="mp-overlay">Loading GPS points…</div>}
      {error && <div className="mp-error">{error}</div>}
      {!loading && !error && gpsCount === 0 && (
        <div className="mp-empty">
          <div className="mp-empty-icon">
            <svg width="48" height="48" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
              <circle cx="12" cy="12" r="10" />
              <line x1="2" y1="12" x2="22" y2="12" />
              <path d="M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z" />
            </svg>
          </div>
          <span>No photos with GPS data in this catalog.</span>
          <span style={{ fontSize: 11 }}>
            GPS is read from EXIF during scan.
          </span>
        </div>
      )}

      {/* ── Bottom-docked filmstrip ──────────────────────── */}
      {filmstrip && (
        <MapFilmstrip
          strip={filmstrip}
          onSelect={handleFilmstripSelect}
          onShowInLibrary={handleFilmstripShowInLibrary}
          onClose={handleFilmstripClose}
        />
      )}

      {/* ── Status strip ──────────────────────────────────── */}
      <div className="mp-status">
        <span>
          {loading
            ? "Loading…"
            : `${gpsCount} photo${gpsCount !== 1 ? "s" : ""} with GPS`}
        </span>
        {drawing && (
          <span className="mp-status-drawing">
            Drawing fence — click to add vertices, double-click to close
          </span>
        )}
        <span style={{ marginLeft: "auto", opacity: 0.6 }}>
          Tiles © OpenStreetMap contributors
        </span>
      </div>

      {/* ── Fence editor dialog ──────────────────────────── */}
      {editorOpen && (
        <FenceEditorDialog
          initialName={editorFence?.name ?? ""}
          initialTagPath={editorFence?.tagPath ?? ""}
          onSave={handleEditorSave}
          onCancel={() => {
            setEditorOpen(false);
            setPendingPolygon(null);
            setEditorFence(null);
          }}
        />
      )}
    </div>
  );
}

// ── Vertex drag handle helpers ────────────────────────────────────────────────

/**
 * Create draggable vertex markers for an existing fence polygon. When a vertex
 * is dragged and released, `onEdit` is called with the new polygon coordinates.
 * `polyLayer` is the Leaflet Polygon already rendered on the map — its geometry
 * is updated live during drag so the polygon follows the vertex in real time.
 */
function buildVertexMarkers(
  map: L.Map,
  fence: Fence,
  color: string,
  polyLayer: L.Polygon,
  onEdit: (newPoly: [number, number][]) => void
): L.Marker[] {
  const markers: L.Marker[] = [];
  const poly = fence.polygon;

  poly.forEach(([lat, lng], idx) => {
    const icon = makeVertexIcon(color);
    const m = L.marker([lat, lng], {
      icon,
      draggable: true,
      zIndexOffset: 1000,
    }).addTo(map);

    m.on("drag", () => {
      // Live visual feedback — rebuild coords from all current marker positions
      // and update the polygon layer that's already on the map.
      const liveLatLngs: L.LatLngExpression[] = markers.map((mk, i) =>
        i === idx ? m.getLatLng() : mk.getLatLng()
      );
      polyLayer.setLatLngs(liveLatLngs);
      polyLayer.redraw();
    });

    m.on("dragend", () => {
      // Gather updated positions from all sibling markers.
      const newPoly: [number, number][] = markers.map((mk, i) => {
        if (i === idx) {
          const pos = m.getLatLng();
          return [pos.lat, pos.lng];
        }
        const pos = mk.getLatLng();
        return [pos.lat, pos.lng];
      });
      onEdit(newPoly);
    });

    markers.push(m);
  });

  return markers;
}

// ── Settings panel ────────────────────────────────────────────────────────────

function MapSettings({ api }: { api: ChairPhotoAPI }) {
  const [tileUrl, setTileUrl] = useState("");
  const [saved, setSaved] = useState(false);

  // Geocode-all state.
  const [geocodeBusy, setGeocodeBusy] = useState(false);
  const [geocodeProgress, setGeocodeProgress] = useState<GeocodeProgress | null>(null);
  const [geocodeStatus, setGeocodeStatus] = useState("");

  // Load stored setting on mount.
  useEffect(() => {
    api
      .getSetting(SETTING_TILE_URL)
      .then((v) => setTileUrl(v ?? DEFAULT_TILE_URL))
      .catch(() => setTileUrl(DEFAULT_TILE_URL));
  }, [api]);

  const handleSave = () => {
    const url = tileUrl.trim() || DEFAULT_TILE_URL;
    api.setSetting(SETTING_TILE_URL, url).then(() => {
      setSaved(true);
      setTimeout(() => setSaved(false), 2000);
    });
  };

  const handleReset = () => {
    setTileUrl(DEFAULT_TILE_URL);
    api.setSetting(SETTING_TILE_URL, DEFAULT_TILE_URL).then(() => {
      setSaved(true);
      setTimeout(() => setSaved(false), 2000);
    });
  };

  const handleGeocodeAll = async () => {
    if (geocodeBusy) return;
    setGeocodeBusy(true);
    setGeocodeProgress(null);
    setGeocodeStatus("Starting…");

    // Subscribe to progress events for the duration of this run. `onEvent` is an
    // optional host-API member: without it the geocode run still completes and only
    // the per-photo progress line is missing.
    let unlisten: (() => void) | null = null;
    try {
      unlisten =
        (await api.onEvent?.<GeocodeProgress>("geocode:progress", (p) => {
          setGeocodeProgress(p);
          setGeocodeStatus(`${p.done} / ${p.total} processed, ${p.filled} filled…`);
        })) ?? null;

      const summary = await geocodeAllToIptc(api);
      const msg =
        summary.total === 0
          ? "No photos with GPS and empty location fields."
          : `Done: ${summary.filled} of ${summary.total} photos had location fields filled.` +
            (summary.skipped > 0 ? ` ${summary.skipped} already set or no result.` : "");
      setGeocodeStatus(msg);
      setGeocodeProgress(null);
      api.showToast(msg);
      if (summary.filled > 0) api.notifyChange();
    } catch (e: unknown) {
      const msg = `Geocode failed: ${String(e)}`;
      setGeocodeStatus(msg);
      api.showToast(msg);
    } finally {
      unlisten?.();
      setGeocodeBusy(false);
    }
  };

  const pct =
    geocodeProgress && geocodeProgress.total > 0
      ? Math.round((geocodeProgress.done / geocodeProgress.total) * 100)
      : null;

  return (
    <div className="mp-settings">
      <div className="mp-settings-title">Map</div>

      <div className="mp-settings-row">
        <div className="mp-settings-label">Tile source URL</div>
        <input
          className="mp-settings-input"
          type="text"
          value={tileUrl}
          onChange={(e) => {
            setTileUrl(e.target.value);
            setSaved(false);
          }}
          placeholder={DEFAULT_TILE_URL}
          spellCheck={false}
        />
        <div className="mp-settings-hint">
          A Leaflet-compatible tile URL with <code>{"{s}"}</code>,{" "}
          <code>{"{z}"}</code>, <code>{"{x}"}</code>, <code>{"{y}"}</code>{" "}
          placeholders. Default: OpenStreetMap. You can point this at a
          self-hosted tile server or a commercial provider for heavy use
          (OpenStreetMap&apos;s tile policy limits high-volume access).
        </div>
      </div>

      <div style={{ display: "flex", gap: 8 }}>
        <button
          className="mp-settings-btn mp-settings-btn-primary"
          onClick={handleSave}
        >
          {saved ? "Saved" : "Save"}
        </button>
        <button className="mp-settings-btn" onClick={handleReset}>
          Reset to default
        </button>
      </div>

      <div className="mp-settings-hint">
        <strong>Attribution:</strong> OpenStreetMap tiles are provided under the
        ODbL licence. The map footer always shows the attribution required by
        that licence.
      </div>

      {/* ── Reverse geocoding section ────────────────────────────────────── */}
      <div className="mp-settings-title" style={{ marginTop: 20 }}>Reverse geocoding</div>

      <div className="mp-settings-hint">
        Fill empty IPTC location fields (City, State/Province, Country, Country code) for
        every photo that has GPS coordinates but missing location metadata. Uses OSM
        Nominatim (max 1 request/s). Existing values are never overwritten.
      </div>

      <div style={{ display: "flex", gap: 8, marginTop: 8, alignItems: "center" }}>
        <button
          className="mp-settings-btn mp-settings-btn-primary"
          onClick={handleGeocodeAll}
          disabled={geocodeBusy}
        >
          {geocodeBusy ? "Geocoding…" : "Geocode all with GPS"}
        </button>
        {pct != null && (
          <span style={{ fontSize: 12, opacity: 0.75 }}>{pct}%</span>
        )}
      </div>

      {geocodeStatus && (
        <div className="mp-settings-hint" style={{ marginTop: 6 }}>
          {geocodeStatus}
        </div>
      )}

      {geocodeProgress && geocodeProgress.total > 0 && (
        <div style={{ marginTop: 6, height: 4, background: "rgba(255,255,255,0.12)", borderRadius: 2 }}>
          <div
            style={{
              height: "100%",
              background: "#3b82f6",
              borderRadius: 2,
              width: `${pct ?? 0}%`,
              transition: "width 0.2s",
            }}
          />
        </div>
      )}
    </div>
  );
}

// ── Geocode inspector panel (per-photo "Geocode location" action) ─────────────
//
// Rendered in the inspector's "Geocode" block (registered via registerPanel
// slot="inspector"). Shows the active photo's GPS coordinates (if any) and a
// "Geocode location" button that fills empty IPTC location fields via the backend
// `geocode_to_iptc` command.

function GeocodePanelContent({ api }: { api: ChairPhotoAPI }) {
  // Rendered inside PhotoInspector, which (issue #16) now subscribes only to
  // contributions — it no longer re-renders on selection changes, so this panel needs
  // its own subscription to pick up the active photo.
  useHostSelection();
  const [status, setStatus] = useState<string>("");
  const [busy, setBusy] = useState(false);

  const photoId = api.getActivePhotoId();

  const handleGeocode = async () => {
    if (photoId == null) return;
    setBusy(true);
    setStatus("Geocoding…");
    try {
      const filled = await api.invoke<boolean>("geocode_to_iptc", { photoId });
      if (filled) {
        setStatus("Location fields filled.");
        api.notifyChange();
      } else {
        setStatus("No GPS data, fields already set, or no result from geocoder.");
      }
    } catch (e: unknown) {
      setStatus(`Error: ${String(e)}`);
    } finally {
      setBusy(false);
    }
  };

  if (photoId == null) {
    return <span className="panel-empty">No photo selected</span>;
  }

  return (
    <div>
      <div className="develop-row">
        <button
          className="chip"
          onClick={handleGeocode}
          disabled={busy}
          title="Fill empty City / State / Country / Country code fields from GPS via OpenStreetMap Nominatim. Existing values are never overwritten."
        >
          {busy ? "Geocoding…" : "Geocode location"}
        </button>
      </div>
      {status && <div className="develop-note">{status}</div>}
    </div>
  );
}

// ── Module export ─────────────────────────────────────────────────────────────

export const mapModule: ChairPhotoModule = {
  id: "map",
  name: "Map",
  version: "0.1.0",
  description:
    "Plot your photos on an OpenStreetMap map by GPS location. Click a marker or cluster to browse photos in a filmstrip; clicking a thumb selects it in the pop-out loupe without leaving the map. Draw geofences to auto-tag photos by location. Requires the map backend feature.",
  backendFeature: "map",
  // Map points, geofence CRUD, and the reverse-geocode writers (#48). The `geocode_*`
  // commands write IPTC location fields into the catalog and sidecars.
  permissions: {
    commands: [
      "apply_all_fences",
      "apply_fence",
      "create_fence",
      "delete_fence",
      "geocode_all_to_iptc",
      "geocode_to_iptc",
      "list_fences",
      "map_photo_points",
      "update_fence",
    ],
  },
  onLoad(api) {
    api.registerMainView({
      id: "map",
      label: "Map",
      icon: (
        <svg
          width="13"
          height="13"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="2"
          strokeLinecap="round"
          strokeLinejoin="round"
          aria-hidden
        >
          <polygon points="1 6 1 22 8 18 16 22 23 18 23 2 16 6 8 2 1 6" />
          <line x1="8" y1="2" x2="8" y2="18" />
          <line x1="16" y1="6" x2="16" y2="22" />
        </svg>
      ),
      render: () => <MapView api={api} />,
    });
    api.registerSettingsPanel(() => <MapSettings api={api} />);

    // Inspector panel: "Geocode" — fills empty IPTC location fields from GPS for the
    // selected photo. Only meaningful when the map backend feature is compiled in.
    api.registerPanel({
      id: "map-geocode",
      label: "Geocode",
      slot: "inspector",
      render: () => <GeocodePanelContent api={api} />,
    });
  },
};
