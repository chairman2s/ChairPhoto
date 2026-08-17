import { useEffect, useState } from "react";
import type { Photo } from "../modules/registry";
import { COLOR_LABELS } from "../modules/labels";
import { FIT_VIEW, ZoomableImage, ZoomView } from "./ZoomableImage";

const LABEL_COLORS: Record<string, string> = Object.fromEntries(
  COLOR_LABELS.map((l) => [l.name, l.color]),
);

// --------------------------------------------------------------------------
// Compare: two to four frames side by side, sharing one pan/zoom, so the choice
// between them can be made on pixels rather than memory.
//
// Burst grouping, phash and sharpness already produce the candidate sets; this is
// the screen on which the decision actually gets made. It differs from the grid in
// one way that matters: culling keys act on the FOCUSED PANE, not the selection.
// In the grid, rating a multi-selection rates all of it, which is exactly wrong
// when the whole point is to separate one frame from its neighbours.
// --------------------------------------------------------------------------

/** Most panes we will show at once. Beyond this each frame is too small to judge. */
export const MAX_PANES = 4;

export function CompareView({
  photos,
  focusedId,
  softThreshold,
  onFocus,
  onKeep,
  onExit,
}: {
  /** The frames to compare, already capped to {@link MAX_PANES} by the caller. */
  photos: Photo[];
  /** Which pane culling keys and the Keep action apply to. */
  focusedId: number | null;
  /** Sharpness below which a frame is flagged soft; `null` = nothing scored yet. */
  softThreshold: number | null;
  onFocus: (photoId: number) => void;
  /** Promote the focused frame: pick it, reject the others. */
  onKeep: (keeperId: number) => void;
  onExit: () => void;
}) {
  // One transform, shared by every pane — the whole point of the view.
  const [view, setView] = useState<ZoomView>(FIT_VIEW);

  // Reset zoom when the compared set changes. Holding a deep zoom across a swap would
  // leave the new frames showing an arbitrary corner with no visible reason.
  const key = photos.map((p) => p.id).join(",");
  useEffect(() => {
    setView(FIT_VIEW);
  }, [key]);

  const zoomed = view.scale > 1.001;

  // Do the panes actually show the same crop at the same zoom? Only if the frames share
  // pixel dimensions. Same-burst frames off one body always do; a mixed set does not, and
  // silently showing different crops side by side would make the comparison a lie.
  const dims = photos.map((p) => `${p.width ?? 0}x${p.height ?? 0}`);
  const mixedSizes = new Set(dims).size > 1;

  if (photos.length === 0) {
    return (
      <div className="compare-empty">
        <div className="loupe-empty-title">Nothing to compare</div>
        <div className="loupe-empty-hint">
          Select two or more photos in the grid, then press C.
        </div>
      </div>
    );
  }

  return (
    <div className="compare-view">
      <div className="loupe-bar compare-bar">
        <button className="chip" onClick={onExit}>
          ‹ Back to grid (Esc)
        </button>
        <span className="compare-count">
          Comparing {photos.length} — ←/→ focus, 0–5 rate, P/X pick or reject, K keep
        </span>
        {zoomed && (
          <button className="chip" onClick={() => setView(FIT_VIEW)} title="Fit all panes">
            Fit {Math.round(view.scale * 100)}%
          </button>
        )}
        {mixedSizes && (
          <span
            className="compare-warn"
            title={`These frames have different pixel dimensions (${[...new Set(dims)].join(", ")}), so at the same zoom the panes do not show the same crop.`}
          >
            ⚠ mixed sizes
          </span>
        )}
      </div>

      <div className={`compare-panes compare-panes-${photos.length}`}>
        {photos.map((photo) => {
          const isFocused = photo.id === focusedId;
          const soft =
            softThreshold != null && photo.sharpness != null && photo.sharpness < softThreshold;
          return (
            <div
              key={photo.id}
              className={`compare-pane ${isFocused ? "compare-pane-focused" : ""} ${
                photo.pickState === "reject" ? "compare-pane-rejected" : ""
              }`}
              // Focus follows the pointer press rather than a click, so starting a pan
              // gesture in a pane also focuses it — otherwise the keys would keep acting
              // on whichever pane was clicked last, which is not the one being examined.
              onMouseDownCapture={() => onFocus(photo.id)}
            >
              <div className="compare-pane-head">
                <span className="compare-name">{photo.path.split("/").pop()}</span>
                {photo.rating > 0 && (
                  <span className="compare-tag">{"★".repeat(photo.rating)}</span>
                )}
                {photo.pickState === "pick" && (
                  <span className="compare-tag compare-pick">pick</span>
                )}
                {photo.pickState === "reject" && (
                  <span className="compare-tag compare-reject">rejected</span>
                )}
                {photo.label && LABEL_COLORS[photo.label] && (
                  <span
                    className="compare-swatch"
                    style={{ background: LABEL_COLORS[photo.label] }}
                  />
                )}
                {photo.sharpness != null && (
                  <span
                    className={`compare-tag ${soft ? "compare-soft" : ""}`}
                    title={`Sharpness ${photo.sharpness.toFixed(1)} (method: ${photo.sharpnessMethod ?? "tile"})`}
                  >
                    ⌖ {photo.sharpness.toFixed(0)}
                  </span>
                )}
                {photo.burstFlag === "sharpest-of-burst" && (
                  <span className="compare-tag" title="Sharpest of burst">
                    ♛
                  </span>
                )}
              </div>

              <div className="compare-pane-img">
                <ZoomableImage photoId={photo.id} view={view} onViewChange={setView} />
              </div>

              <div className="compare-pane-foot">
                <button
                  className={`chip ${isFocused ? "chip-on" : ""}`}
                  onClick={() => onKeep(photo.id)}
                  title="Keep this frame: mark it a pick and reject the others (reversible with U)"
                >
                  Keep this
                </button>
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}
