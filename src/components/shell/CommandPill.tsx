// The Darkroom redesign's floating command pill (see agent-notes mockup Main.dc.html
// `.float-tb`): a small centered pill over the stage that replaces the old
// `<div className="filter-bar">` row — one row of segmented controls, dots, dropdowns and
// removable chips that used to sit between the title bar and `.body`. It absorbs every one
// of FilterBar's internals verbatim (the culling seg, the colour-label dots, the scope
// chips, the facet/camera/lens/storage/sort pickers) plus two things FilterBar never had:
// a smart-album scope chip and the thumbnail-size slider that now lives here instead of a
// Preferences setting.
//
// Presentational only, same as TitleBar/Bench: every piece of state and every command is a
// prop. App.tsx owns the scope (`library`/`scope`) and decides when this renders at all —
// it is hidden in Develop, module views and Compare, where there is no grid/loupe to filter.
import { useEffect, useState } from "react";
import {
  CullingFilter,
  distinctPhotoValues,
  Facet,
  listAlbums,
  listFacets,
  listImportBatches,
  listSmartAlbums,
  PhotoSort,
  StorageTier,
} from "../../modules/api";
import { COLOR_LABELS } from "../../modules/labels";
import { MenuButton, MenuCheckItem, MenuItem, MenuLabel, MenuSeparator, MenuSub } from "./Menu";

// Mockup labels for the culling seg (`All / Unrated / Picks / Rejects / Edited`) — plural
// for Picks/Rejects, unlike the raw CullingFilter values they map from.
const FILTER_LABELS: Record<CullingFilter, string> = {
  all: "All",
  unrated: "Unrated",
  pick: "Picks",
  reject: "Rejects",
  edited: "Edited",
};

// Chip text for a non-default storage tier. "all" never renders a chip (see `scoped` use
// below), so it has no entry here.
const STORAGE_CHIP_LABELS: Record<Exclude<StorageTier, "all">, string> = {
  local: "On disk",
  nas: "NAS only",
  // The safety tiers are set from the Safety panel, not the +Filter menu — but a filter you
  // cannot see is a filter you cannot turn off, so the active one still shows as a chip.
  atRisk: "At risk",
  stale: "Edits not carried home",
};

// The +Filter menu's storage picker offers only the three tiers a user can choose directly;
// atRisk/stale are Safety-panel-driven (see STORAGE_CHIP_LABELS above).
const STORAGE_MENU: [StorageTier, string][] = [
  ["all", "All"],
  ["local", "On disk"],
  ["nas", "NAS only"],
];

const SORTS: { value: PhotoSort; label: string }[] = [
  { value: "date", label: "Date" },
  { value: "sharpness_asc", label: "Least sharp first" },
  { value: "sharpness_desc", label: "Sharpest first" },
];

export interface CommandPillProps {
  filters: CullingFilter[];
  filter: CullingFilter;
  onFilter: (f: CullingFilter) => void;
  activeTagLabel: string | null;
  onClearTag: () => void;
  activeAlbumId: number | null;
  onClearAlbum: () => void;
  /** Active smart album (a saved rule, evaluated live — see docs/smart-albums.md). */
  activeSmartAlbumId: number | null;
  onClearSmartAlbum: () => void;
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
  /** Grid tile minimum width (px) — bound to CatalogGrid's `tileMin` prop. */
  thumbSize: number;
  onThumbSize: (n: number) => void;
}

export function CommandPill({
  filters,
  filter,
  onFilter,
  activeTagLabel,
  onClearTag,
  activeAlbumId,
  onClearAlbum,
  activeSmartAlbumId,
  onClearSmartAlbum,
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
  thumbSize,
  onThumbSize,
}: CommandPillProps) {
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

  // Resolve the active smart album's name, same pattern as the plain album above.
  const [smartAlbumName, setSmartAlbumName] = useState<string | null>(null);
  useEffect(() => {
    if (activeSmartAlbumId == null) {
      setSmartAlbumName(null);
      return;
    }
    let alive = true;
    listSmartAlbums()
      .then((albums) => {
        if (alive)
          setSmartAlbumName(albums.find((a) => a.id === activeSmartAlbumId)?.name ?? null);
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [activeSmartAlbumId]);

  // Available facets to offer. Reloaded when reloadKey changes, since some facets are
  // dynamic (a "Published: <platform>" chip appears once a photo is published there).
  const [facets, setFacets] = useState<Facet[]>([]);
  useEffect(() => {
    listFacets().then(setFacets).catch(() => {});
  }, [reloadKey]);
  const labelFor = (key: string) => facets.find((f) => f.key === key)?.label ?? key;

  // Camera / lens options for the +Filter menu (distinct values present in the catalog).
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

  return (
    <div className="float-tb">
      {filters.map((f) => (
        <button key={f} className={`ft ${filter === f ? "on" : ""}`} onClick={() => onFilter(f)}>
          {FILTER_LABELS[f]}
        </button>
      ))}
      <span className="ftsep" aria-hidden />
      {/* Colour-label filter: one dot per label + a "none" dot, multi-select (OR). The dots
          themselves show the active state, so no removable chip is needed. */}
      <div className="ftdots" title="Filter by colour label (click to toggle; multiple = either)">
        {COLOR_LABELS.map((l) => (
          <button
            key={l.name}
            className={`fd ${activeLabels.includes(l.name) ? "on" : ""}`}
            style={{ background: l.color, color: l.color }}
            onClick={() => onToggleLabel(l.name)}
            title={`${l.name} label`}
            aria-pressed={activeLabels.includes(l.name)}
          />
        ))}
        <button
          className={`fd fd-x ${activeLabels.includes("") ? "on" : ""}`}
          onClick={() => onToggleLabel("")}
          title="No label"
          aria-pressed={activeLabels.includes("")}
        />
      </div>
      <span className="ftsep" aria-hidden />

      {activeTagLabel != null && (
        <button className="ftchip" onClick={onClearTag} title="Clear tag filter">
          Tag: {activeTagLabel} <span className="ftchip-x">✕</span>
        </button>
      )}
      {activeAlbumId != null && (
        <button className="ftchip" onClick={onClearAlbum} title="Clear album filter">
          Album: {albumName ?? "…"} <span className="ftchip-x">✕</span>
        </button>
      )}
      {activeSmartAlbumId != null && (
        <button className="ftchip" onClick={onClearSmartAlbum} title="Clear smart album filter">
          Smart album: {smartAlbumName ?? "…"} <span className="ftchip-x">✕</span>
        </button>
      )}
      {activeBatchId != null && (
        <button className="ftchip" onClick={onClearBatch} title="Clear batch filter">
          Batch: {batchName ?? "…"} <span className="ftchip-x">✕</span>
        </button>
      )}
      {activeFacets.map((key) => (
        <button
          key={key}
          className="ftchip"
          onClick={() => onToggleFacet(key)}
          title="Remove facet filter"
        >
          {labelFor(key)} <span className="ftchip-x">✕</span>
        </button>
      ))}
      {activeCamera != null && (
        <button className="ftchip" onClick={() => onCamera(null)} title="Clear camera filter">
          Camera: {activeCamera} <span className="ftchip-x">✕</span>
        </button>
      )}
      {activeLens != null && (
        <button className="ftchip" onClick={() => onLens(null)} title="Clear lens filter">
          Lens: {activeLens} <span className="ftchip-x">✕</span>
        </button>
      )}
      {storageTier !== "all" && (
        <button className="ftchip" onClick={() => onStorageTier("all")} title="Clear storage filter">
          {STORAGE_CHIP_LABELS[storageTier]} <span className="ftchip-x">✕</span>
        </button>
      )}

      <MenuButton className="ft" label="＋ Filter" title="Add a filter">
        <MenuLabel>Facets</MenuLabel>
        {facets.map((f) => (
          <MenuCheckItem
            key={f.key}
            checked={activeFacets.includes(f.key)}
            onChange={() => onToggleFacet(f.key)}
          >
            {f.label}
          </MenuCheckItem>
        ))}
        <MenuSeparator />
        <MenuSub label="Camera">
          <MenuItem onSelect={() => onCamera(null)}>Any camera</MenuItem>
          {cameras.map((c) => (
            <MenuItem key={c} onSelect={() => onCamera(c)}>
              {c}
            </MenuItem>
          ))}
        </MenuSub>
        <MenuSub label="Lens">
          <MenuItem onSelect={() => onLens(null)}>Any lens</MenuItem>
          {lenses.map((l) => (
            <MenuItem key={l} onSelect={() => onLens(l)}>
              {l}
            </MenuItem>
          ))}
        </MenuSub>
        <MenuSeparator />
        <MenuLabel>Storage</MenuLabel>
        {STORAGE_MENU.map(([t, label]) => (
          <MenuCheckItem key={t} checked={storageTier === t} onChange={() => onStorageTier(t)}>
            {label}
          </MenuCheckItem>
        ))}
        <MenuSeparator />
        <MenuLabel>Sort</MenuLabel>
        {SORTS.map((s) => (
          <MenuCheckItem
            key={s.value}
            checked={photoSort === s.value}
            onChange={() => onPhotoSort(s.value)}
          >
            {s.label}
          </MenuCheckItem>
        ))}
      </MenuButton>

      <span className="ftsep" aria-hidden />
      <div className="ftsize" title="Thumbnail size">
        <svg width="12" height="12" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
          <rect x="4" y="4" width="7" height="7" rx="1" />
          <rect x="13" y="4" width="7" height="7" rx="1" />
          <rect x="4" y="13" width="7" height="7" rx="1" />
          <rect x="13" y="13" width="7" height="7" rx="1" />
        </svg>
        <input
          type="range"
          min={120}
          max={320}
          step={8}
          value={thumbSize}
          onChange={(e) => onThumbSize(Number(e.target.value))}
          aria-label="Thumbnail size"
        />
      </div>
    </div>
  );
}
