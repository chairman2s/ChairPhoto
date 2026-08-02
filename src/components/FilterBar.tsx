import { useEffect, useState } from "react";
import {
  CullingFilter,
  distinctPhotoValues,
  Facet,
  listAlbums,
  listFacets,
  listImportBatches,
  PhotoSort,
  StorageTier,
} from "../modules/api";
import { COLOR_LABELS } from "../modules/labels";

// The active-filters bar. The catalog ANDs together the culling filter, the selected
// tag, and the selected album (see catalog::list_photos); this bar is the single place
// that shows what's currently applied and lets each scope be cleared. Facets (B2) will
// slot in here as more removable chips.
export function FilterBar({
  filters,
  filter,
  onFilter,
  activeTagLabel,
  onClearTag,
  activeAlbumId,
  onClearAlbum,
  activeBatchId,
  onClearBatch,
  activeFacets,
  onToggleFacet,
  storageTier,
  onStorageTier,
  photoSort,
  onPhotoSort,
  activeCamera,
  onCamera,
  activeLens,
  onLens,
  activeLabels,
  onToggleLabel,
  reloadKey,
}: {
  filters: CullingFilter[];
  filter: CullingFilter;
  onFilter: (f: CullingFilter) => void;
  activeTagLabel: string | null;
  onClearTag: () => void;
  activeAlbumId: number | null;
  onClearAlbum: () => void;
  activeBatchId: number | null;
  onClearBatch: () => void;
  activeFacets: string[];
  onToggleFacet: (key: string) => void;
  /** Storage-tier filter: all / on-disk / NAS-only (offloaded). */
  storageTier: StorageTier;
  onStorageTier: (t: StorageTier) => void;
  /** Sort order for the photo grid (date / sharpness). */
  photoSort: PhotoSort;
  onPhotoSort: (s: PhotoSort) => void;
  /** Camera / lens exact-match filters (null = any). */
  activeCamera: string | null;
  onCamera: (c: string | null) => void;
  activeLens: string | null;
  onLens: (l: string | null) => void;
  /** Colour-label filter, OR-combined; "" is the No-label member. Empty = off. */
  activeLabels: string[];
  onToggleLabel: (label: string) => void;
  /** Bumped by the app on data changes so dynamic facets (e.g. published platforms) refresh. */
  reloadKey?: number;
}) {
  // Resolve the active album's name for display (the album list lives in the sidebar).
  const [albumName, setAlbumName] = useState<string | null>(null);
  useEffect(() => {
    if (activeAlbumId == null) {
      setAlbumName(null);
      return;
    }
    let alive = true;
    listAlbums()
      .then((albums) => {
        if (alive) setAlbumName(albums.find((a) => a.id === activeAlbumId)?.name ?? null);
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [activeAlbumId]);

  // Available facets to offer. Reloaded when reloadKey changes, since some facets are
  // dynamic (a "Published: <platform>" chip appears once a photo is published there).
  const [facets, setFacets] = useState<Facet[]>([]);
  useEffect(() => {
    listFacets().then(setFacets).catch(() => {});
  }, [reloadKey]);
  const labelFor = (key: string) => facets.find((f) => f.key === key)?.label ?? key;

  // Camera / lens options for the dropdowns (distinct values present in the catalog).
  // Reloaded on reloadKey so a scan that adds new bodies/lenses shows up.
  const [cameras, setCameras] = useState<string[]>([]);
  const [lenses, setLenses] = useState<string[]>([]);
  useEffect(() => {
    distinctPhotoValues("camera").then(setCameras).catch(() => {});
    distinctPhotoValues("lens").then(setLenses).catch(() => {});
  }, [reloadKey]);

  // Resolve the active batch's label (its source folder's last segment).
  const [batchName, setBatchName] = useState<string | null>(null);
  useEffect(() => {
    if (activeBatchId == null) {
      setBatchName(null);
      return;
    }
    let alive = true;
    listImportBatches()
      .then((batches) => {
        if (!alive) return;
        const b = batches.find((x) => x.id === activeBatchId);
        setBatchName(b ? b.sourceLabel.replace(/\/+$/, "").split("/").pop() || "ingest" : null);
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [activeBatchId]);

  const scoped =
    activeTagLabel != null ||
    activeAlbumId != null ||
    activeBatchId != null ||
    activeFacets.length > 0 ||
    activeCamera != null ||
    activeLens != null;

  return (
    <div className="filter-bar">
      <div className="seg-pill" title="Culling state filter">
        {filters.map((f) => (
          <button
            key={f}
            className={`seg-item ${filter === f ? "on" : ""}`}
            onClick={() => onFilter(f)}
          >
            {f}
          </button>
        ))}
      </div>
      <span className="filter-bar-sep" aria-hidden />
      {/* Colour-label filter: one dot per label + a "none" dot, multi-select (OR).
          The dots themselves show the active state, so no removable chip is needed. */}
      <div className="label-dots" title="Filter by colour label (click to toggle; multiple = either)">
        {COLOR_LABELS.map((l) => (
          <button
            key={l.name}
            className={`label-dot ${activeLabels.includes(l.name) ? "on" : ""}`}
            style={{ background: l.color }}
            onClick={() => onToggleLabel(l.name)}
            title={`${l.name} label`}
            aria-pressed={activeLabels.includes(l.name)}
          />
        ))}
        <button
          className={`label-dot label-dot-none ${activeLabels.includes("") ? "on" : ""}`}
          onClick={() => onToggleLabel("")}
          title="No label"
          aria-pressed={activeLabels.includes("")}
        />
      </div>
      <span className="filter-bar-sep" aria-hidden />
      <div className="seg-pill" title="Where the photo is stored">
        {(
          [
            ["all", "All"],
            ["local", "On disk"],
            ["nas", "On NAS"],
          ] as [StorageTier, string][]
        ).map(([t, label]) => (
          <button
            key={t}
            className={`seg-item ${storageTier === t ? "on" : ""}`}
            onClick={() => onStorageTier(t)}
          >
            {label}
          </button>
        ))}
      </div>
      <span className="filter-bar-sep" aria-hidden />
      <div className="filters" title="Sort photos">
        <select
          className="filter-select"
          value={photoSort}
          onChange={(e) => onPhotoSort(e.target.value as PhotoSort)}
          title="Sort order"
        >
          <option value="date">Sort: Date</option>
          <option value="sharpness_asc">Sort: Least sharp first</option>
          <option value="sharpness_desc">Sort: Sharpest first</option>
        </select>
      </div>
      <span className="filter-bar-sep" aria-hidden />
      <div className="filters" title="Filter by camera / lens (from EXIF)">
        <select
          className="filter-select"
          value={activeCamera ?? ""}
          onChange={(e) => onCamera(e.target.value || null)}
          title="Camera"
        >
          <option value="">Any camera</option>
          {cameras.map((c) => (
            <option key={c} value={c}>
              {c}
            </option>
          ))}
        </select>
        <select
          className="filter-select"
          value={activeLens ?? ""}
          onChange={(e) => onLens(e.target.value || null)}
          title="Lens"
        >
          <option value="">Any lens</option>
          {lenses.map((l) => (
            <option key={l} value={l}>
              {l}
            </option>
          ))}
        </select>
      </div>
      {scoped && <span className="filter-bar-sep" aria-hidden />}
      {activeCamera != null && (
        <button className="filter-chip" onClick={() => onCamera(null)} title="Clear camera filter">
          Camera: {activeCamera} <span className="filter-chip-x">✕</span>
        </button>
      )}
      {activeLens != null && (
        <button className="filter-chip" onClick={() => onLens(null)} title="Clear lens filter">
          Lens: {activeLens} <span className="filter-chip-x">✕</span>
        </button>
      )}
      {activeTagLabel != null && (
        <button className="filter-chip" onClick={onClearTag} title="Clear tag filter">
          Tag: {activeTagLabel} <span className="filter-chip-x">✕</span>
        </button>
      )}
      {activeAlbumId != null && (
        <button className="filter-chip" onClick={onClearAlbum} title="Clear album filter">
          Album: {albumName ?? "…"} <span className="filter-chip-x">✕</span>
        </button>
      )}
      {activeBatchId != null && (
        <button className="filter-chip" onClick={onClearBatch} title="Clear batch filter">
          Batch: {batchName ?? "…"} <span className="filter-chip-x">✕</span>
        </button>
      )}
      {activeFacets.map((key) => (
        <button
          key={key}
          className="filter-chip"
          onClick={() => onToggleFacet(key)}
          title="Remove facet filter"
        >
          {labelFor(key)} <span className="filter-chip-x">✕</span>
        </button>
      ))}
      {/* Facet picker: chips for facets not already active. */}
      {facets
        .filter((f) => !activeFacets.includes(f.key))
        .map((f) => (
          <button
            key={f.key}
            className="facet-add"
            onClick={() => onToggleFacet(f.key)}
            title={`Filter: ${f.label}`}
          >
            + {f.label}
          </button>
        ))}
    </div>
  );
}
