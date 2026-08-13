import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { MouseEvent } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import type { Photo } from "../modules/registry";
import type { StorageStatus } from "../modules/api";
import { isVideoPath } from "../modules/previewCache";
import { COLOR_LABELS } from "../modules/labels";
import { Thumbnail } from "./Thumbnail";

const LABEL_COLORS: Record<string, string> = Object.fromEntries(
  COLOR_LABELS.map((l) => [l.name, l.color]),
);

// Grid layout constants (must match the .grid-scroll padding and .grid-row gap in CSS).
const GAP = 10;
const MIN_TILE = 160;

// Whether a status implies a local and/or remote (NAS) copy, for the tile icons.
function storageIcons(status: StorageStatus | undefined): {
  local: boolean;
  remote: "online" | "offline" | false;
} {
  switch (status) {
    case "localOnly":
      return { local: true, remote: false };
    case "backedUp":
      return { local: true, remote: "online" };
    case "archived":
      return { local: false, remote: "online" };
    case "offline":
      return { local: false, remote: "offline" };
    default:
      return { local: false, remote: false };
  }
}

// --------------------------------------------------------------------------
// Tile — extracted top-level component so React.memo can skip re-renders when
// its own props haven't changed. StorageStatus is a string primitive so the
// default Object.is comparison in React.memo handles it correctly; no custom
// comparator is needed.
// --------------------------------------------------------------------------
interface TileProps {
  photo: Photo;
  isSelected: boolean;
  isActive: boolean;
  status: StorageStatus | undefined;
  bust: number | undefined;
  /** If non-null and photo.sharpness < softThreshold, show a soft badge. */
  softThreshold: number | null;
  onSelect: (photo: Photo, mods: { ctrl: boolean; shift: boolean }) => void;
  onOpen: (photo: Photo) => void;
  onContextMenu: ((photo: Photo, e: MouseEvent) => void) | undefined;
}

const Tile = React.memo(function Tile({
  photo,
  isSelected,
  isActive,
  status,
  bust,
  softThreshold,
  onSelect,
  onOpen,
  onContextMenu,
}: TileProps) {
  const displayName = photo.path.split("/").pop();
  const storage = storageIcons(status);

  return (
    <button
      key={photo.id}
      className={`tile ${isSelected ? "tile-selected" : ""} ${
        isActive ? "tile-active" : ""
      } ${photo.pickState === "reject" ? "tile-rejected" : ""}`}
      onClick={(e) => onSelect(photo, { ctrl: e.ctrlKey || e.metaKey, shift: e.shiftKey })}
      onDoubleClick={() => onOpen(photo)}
      onContextMenu={(e) => {
        e.preventDefault();
        onContextMenu?.(photo, e);
      }}
    >
      <Thumbnail
        photoId={photo.id}
        bust={bust}
        status={status}
        metadataReady={photo.metadataReady !== 0}
      />
      {isVideoPath(photo.path) && (
        <span className="video-badge" title="Video — double-click to play">
          ▶
        </span>
      )}
      <div className="tile-overlay">
        {photo.rating > 0 && <span className="badge">{"★".repeat(photo.rating)}</span>}
        {photo.pickState === "pick" && <span className="badge badge-pick">P</span>}
        {photo.pickState === "reject" && <span className="badge badge-reject">X</span>}
        {photo.label && LABEL_COLORS[photo.label] && (
          <span
            className="badge badge-label"
            style={{ background: LABEL_COLORS[photo.label] }}
          />
        )}
        {(photo.versionCount ?? 0) > 0 && (
          <span className="badge badge-versions" title={`${photo.versionCount} version(s)`}>
            ⧉ {photo.versionCount}
          </span>
        )}
        {(photo.stackCount ?? 0) > 0 && (
          <span
            className="badge badge-stack"
            title={`${photo.stackCount} stacked file(s) — e.g. the camera JPEG. Open the inspector's Stack section.`}
          >
            ▤ {photo.stackCount}
          </span>
        )}
        {softThreshold != null &&
          photo.sharpness != null &&
          photo.sharpness < softThreshold && (
            <span
              className="badge badge-soft"
              title={`Soft — sharpness score ${photo.sharpness.toFixed(1)} (method: ${photo.sharpnessMethod ?? "tile"})`}
            >
              ~
            </span>
          )}
        {photo.burstFlag === "soft-in-burst" && (
          <span
            className="badge badge-soft-in-burst"
            title="Soft in burst — below 60% of cluster median sharpness"
          >
            ~B
          </span>
        )}
        {photo.burstFlag === "sharpest-of-burst" && (
          <span
            className="badge badge-sharpest-of-burst"
            title="Sharpest of burst — best frame in this cluster"
          >
            ♛
          </span>
        )}
      </div>
      <div className="tile-storage">
        {storage.local && (
          <span className="store-icon store-local" title="On local disk">
            ▣
          </span>
        )}
        {storage.remote && (
          <span
            className={`store-icon store-remote ${storage.remote === "offline" ? "store-offline" : ""}`}
            title={storage.remote === "offline" ? "On NAS (offline)" : "On NAS / backup"}
          >
            ☁
          </span>
        )}
      </div>
      <div className="tile-name">{displayName}</div>
    </button>
  );
});

// --------------------------------------------------------------------------
// The main library view: a *virtualized* grid of photo tiles — only the rows
// in (or near) the viewport are mounted, so it scrolls smoothly with tens of
// thousands of photos. Selection and culling badges are driven entirely by
// props so the parent owns all state.
// --------------------------------------------------------------------------
export function CatalogGrid({
  photos,
  selectedId,
  selectedIds,
  statuses,
  thumbBusts,
  softThreshold = null,
  emptyMessage,
  onSelect,
  onOpen,
  onContextMenu,
}: {
  photos: Photo[];
  selectedId: number | null;
  selectedIds: number[];
  statuses?: Map<number, StorageStatus>;
  /** Per-photo cache-bust nonce, bumped after a photo's file is recovered. */
  thumbBusts?: Map<number, number>;
  /**
   * Sharpness score below which a tile shows a soft badge. `null` = no badge shown
   * (e.g. no photos have been scored yet). Mirrors `sharpness.soft_threshold` setting.
   */
  softThreshold?: number | null;
  /** Empty-state text (filter-aware); defaults to the first-run hint. */
  emptyMessage?: string;
  onSelect: (photo: Photo, mods: { ctrl: boolean; shift: boolean }) => void;
  onOpen: (photo: Photo) => void;
  /** Right-click on a tile (for the context menu). */
  onContextMenu?: (photo: Photo, e: MouseEvent) => void;
}) {
  const parentRef = useRef<HTMLDivElement | null>(null);
  const roRef = useRef<ResizeObserver | null>(null);
  // Columns and an estimated row height, derived from the container width (responsive,
  // mirroring the old `repeat(auto-fill, minmax(160px, 1fr))`).
  const [cols, setCols] = useState(1);
  const [estRow, setEstRow] = useState(180);
  const [measured, setMeasured] = useState(false);
  const didInitialBottom = useRef(false);

  // --------------------------------------------------------------------------
  // Stable callbacks via ref-dispatch: the handlers passed in from App.tsx are
  // inline arrows (recreated every App render) so we can't pass them directly
  // to the memoized Tile — it would re-render on every App render. Instead we
  // keep the current handler values in a ref and expose stable dispatch
  // functions (empty-dep useCallback) that read from the ref at call time.
  // --------------------------------------------------------------------------
  const handlersRef = useRef({ onSelect, onOpen, onContextMenu });
  handlersRef.current = { onSelect, onOpen, onContextMenu };

  const stableOnSelect = useCallback(
    (photo: Photo, mods: { ctrl: boolean; shift: boolean }) =>
      handlersRef.current.onSelect(photo, mods),
    [],
  );
  const stableOnOpen = useCallback(
    (photo: Photo) => handlersRef.current.onOpen(photo),
    [],
  );
  const stableOnContextMenu = useCallback(
    (photo: Photo, e: MouseEvent) => handlersRef.current.onContextMenu?.(photo, e),
    [],
  );

  // Callback ref: measure (and observe) the scroll container whenever it mounts. This must
  // be a callback ref, not a mount effect — the grid is absent on first render (photos load
  // async), so a one-shot effect would measure nothing and the column count would stay 1.
  const setScrollEl = useCallback((el: HTMLDivElement | null) => {
    parentRef.current = el;
    roRef.current?.disconnect();
    if (!el) return;
    const recompute = () => {
      // .grid-scroll has no padding (the 12px gutter is .grid-wrap's), so its clientWidth
      // is exactly the width available to tiles.
      const w = el.clientWidth;
      if (w <= 0) return;
      const c = Math.max(1, Math.floor((w + GAP) / (MIN_TILE + GAP)));
      const colW = (w - GAP * (c - 1)) / c;
      const thumbH = (colW * 2) / 3; // .thumb aspect-ratio is 3/2
      setCols(c);
      setEstRow(Math.round(thumbH + 30 + GAP)); // + name/border (~30) + row gap
      setMeasured(true);
    };
    recompute();
    const ro = new ResizeObserver(recompute);
    ro.observe(el);
    roRef.current = ro;
  }, []);

  const idIndex = useMemo(() => {
    const m = new Map<number, number>();
    photos.forEach((p, i) => m.set(p.id, i));
    return m;
  }, [photos]);

  // selectedSet: only recomputed when selectedIds array reference changes.
  const selectedSet = useMemo(() => new Set(selectedIds), [selectedIds]);

  const rowCount = Math.ceil(photos.length / cols);
  const rowVirtualizer = useVirtualizer({
    count: rowCount,
    getScrollElement: () => parentRef.current,
    estimateSize: () => estRow,
    overscan: 3,
  });

  // Column count or estimate changed → drop cached row measurements so they re-measure.
  useEffect(() => {
    rowVirtualizer.measure();
  }, [cols, estRow, rowVirtualizer]);

  // Start at the bottom (newest photos — the list is oldest-first) on first load. Skipped
  // when a photo is already selected (e.g. returning from the loupe), so selection drives
  // the position instead. Runs once per mount, only after the width has been measured.
  useEffect(() => {
    if (!measured || didInitialBottom.current || photos.length === 0) return;
    didInitialBottom.current = true;
    if (selectedId != null) return; // selection-into-view handles positioning
    rowVirtualizer.scrollToIndex(Math.ceil(photos.length / cols) - 1, { align: "end" });
  }, [measured, photos.length, cols, selectedId, rowVirtualizer]);

  // Keep the selected photo on screen during keyboard navigation (it may be far off).
  useEffect(() => {
    if (selectedId == null) return;
    const idx = idIndex.get(selectedId);
    if (idx == null) return;
    rowVirtualizer.scrollToIndex(Math.floor(idx / cols), { align: "auto" });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectedId, cols]);

  if (photos.length === 0) {
    return (
      <div className="grid-empty">
        {emptyMessage ?? "No photos. Scan a folder to begin."}
      </div>
    );
  }

  return (
    <div ref={setScrollEl} className="grid-scroll">
      <div style={{ height: rowVirtualizer.getTotalSize(), width: "100%", position: "relative" }}>
        {rowVirtualizer.getVirtualItems().map((vrow) => {
          const start = vrow.index * cols;
          const rowPhotos = photos.slice(start, Math.min(start + cols, photos.length));
          return (
            <div
              key={vrow.key}
              data-index={vrow.index}
              ref={rowVirtualizer.measureElement}
              className="grid-row"
              style={{
                position: "absolute",
                top: 0,
                left: 0,
                width: "100%",
                transform: `translateY(${vrow.start}px)`,
                gridTemplateColumns: `repeat(${cols}, minmax(0, 1fr))`,
              }}
            >
              {rowPhotos.map((photo) => (
                <Tile
                  key={photo.id}
                  photo={photo}
                  isSelected={selectedSet.has(photo.id)}
                  isActive={photo.id === selectedId}
                  status={statuses?.get(photo.id)}
                  bust={thumbBusts?.get(photo.id)}
                  softThreshold={softThreshold}
                  onSelect={stableOnSelect}
                  onOpen={stableOnOpen}
                  onContextMenu={stableOnContextMenu}
                />
              ))}
            </div>
          );
        })}
      </div>
    </div>
  );
}
