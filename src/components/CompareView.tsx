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

/** How a larger-than-screen pool is worked through: grid batches, or a duel — the champion
 *  holds the left pane while challengers arrive on the right, one verdict per frame. */
export type CompareMode = "grid" | "duel";

export function CompareView({
  photos,
  focusedId,
  softThreshold,
  poolTotal = photos.length,
  poolOffset = 0,
  onPage = () => {},
  mode = "grid",
  onMode,
  duel,
  onFocus,
  onKeep,
  onExit,
}: {
  /** The frames on screen this round, already capped to {@link MAX_PANES} by the caller. */
  photos: Photo[];
  /** Which pane culling keys and the Keep action apply to. */
  focusedId: number | null;
  /** Sharpness below which a frame is flagged soft; `null` = nothing scored yet. */
  softThreshold: number | null;
  /** Size of the whole compared selection. When it exceeds the visible panes, the bar
   *  shows the batch range and ‹ › paging — a 27-frame selection is seven rounds of
   *  four, never a silent "first four only". Defaults to the visible set (no paging). */
  poolTotal?: number;
  /** 0-based index of the first visible frame within the pool. */
  poolOffset?: number;
  /** Step to the previous (-1) or next (+1) batch; the caller clamps at the ends. */
  onPage?: (dir: -1 | 1) => void;
  /** Grid shows a batch of panes at once; duel is champion-vs-challenger, two at a time. */
  mode?: CompareMode;
  /** Offered in the bar whenever the pool could use either presentation. */
  onMode?: (m: CompareMode) => void;
  /** Duel bookkeeping for the bar and the champion pane marking. */
  duel?: { championId: number | null; round: number; totalRounds: number; done: boolean };
  onFocus: (photoId: number) => void;
  /** Promote the focused frame: pick it, reject its on-screen rivals. */
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
        {onMode && poolTotal > 2 && (
          <span className="compare-modes" role="group" aria-label="Compare mode">
            <button
              className={`chip ${mode === "duel" ? "chip-on" : ""}`}
              onClick={() => onMode("duel")}
              title="Champion vs challenger, two at a time"
            >
              Duel
            </button>
            <button
              className={`chip ${mode === "grid" ? "chip-on" : ""}`}
              onClick={() => onMode("grid")}
              title={`Batches of up to ${MAX_PANES} side by side`}
            >
              Grid
            </button>
          </span>
        )}
        {mode === "duel" && duel ? (
          duel.done ? (
            <span className="compare-count">
              Champion — {photos[0]?.path.split("/").pop() ?? ""} · picked, rivals rejected ·
              Esc to finish
            </span>
          ) : (
            <span className="compare-batch">
              <span className="compare-count">
                Duel {duel.round} of {duel.totalRounds}
              </span>
              <span className="compare-hint">
                ← left wins · → right wins · loser is rejected · 0–5 rate the focused pane
              </span>
            </span>
          )
        ) : poolTotal > photos.length ? (
          <span className="compare-batch">
            <button
              className="chip"
              onClick={() => onPage(-1)}
              disabled={poolOffset === 0}
              title="Previous four (PgUp)"
              aria-label="Previous batch"
            >
              ‹
            </button>
            <span className="compare-count">
              Comparing {poolOffset + 1}–{poolOffset + photos.length} of {poolTotal}
            </span>
            <button
              className="chip"
              onClick={() => onPage(1)}
              disabled={poolOffset + photos.length >= poolTotal}
              title="Next four (PgDn)"
              aria-label="Next batch"
            >
              ›
            </button>
            <span className="compare-hint">
              ←/→ focus · 0–5 rate · P/X pick or reject · K keep &amp; next batch
            </span>
          </span>
        ) : (
          <span className="compare-count">
            Comparing {photos.length} — ←/→ focus, 0–5 rate, P/X pick or reject, K keep
          </span>
        )}
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
          const isChampion = mode === "duel" && duel != null && photo.id === duel.championId;
          const soft =
            softThreshold != null && photo.sharpness != null && photo.sharpness < softThreshold;
          return (
            <div
              key={photo.id}
              className={`compare-pane ${isFocused ? "compare-pane-focused" : ""} ${
                photo.pickState === "reject" ? "compare-pane-rejected" : ""
              } ${isChampion ? "compare-pane-champion" : ""}`}
              // Focus follows the pointer press rather than a click, so starting a pan
              // gesture in a pane also focuses it — otherwise the keys would keep acting
              // on whichever pane was clicked last, which is not the one being examined.
              onMouseDownCapture={() => onFocus(photo.id)}
            >
              <div className="compare-pane-head">
                <span className="compare-name">{photo.path.split("/").pop()}</span>
                {isChampion && (
                  <span
                    className="compare-tag compare-champ"
                    title={duel!.done ? "Won the duel" : "Reigning champion — ← keeps it"}
                  >
                    {duel!.done ? "♛ winner" : "champion"}
                  </span>
                )}
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
                {!(mode === "duel" && duel?.done) && (
                <button
                  className={`chip ${isFocused ? "chip-on" : ""}`}
                  onClick={() => onKeep(photo.id)}
                  title={
                    mode === "duel"
                      ? "This frame wins the round: its rival is rejected and the next challenger steps up (reversible with U)"
                      : "Keep this frame: mark it a pick and reject the others (reversible with U)"
                  }
                >
                  {mode === "duel" ? "This one wins" : "Keep this"}
                </button>
                )}
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}
