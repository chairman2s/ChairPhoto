// The Darkroom redesign's bench (see agent-notes mockup Main.dc.html `.bench`): a 72px
// strip across the bottom of the centre stage. Left to right: the progress/status readout
// that used to render in the title bar's children slot, the marking controls for the photo
// the inspector is showing, and the selection pile ("N on the table") with the
// selection-flavoured actions relocated out of the More ⋯ menu.
//
// Presentational only, exactly like TitleBar: every value and every command is a prop.
// This file imports nothing from `modules/api`, so it cannot rate, pick or label a photo
// on its own. The marking buttons resolve the *toggle* — clicking the active star / pill /
// dot clears it, mirroring PhotoInspector's controls — and hand App the final value; App
// routes every marking callback through the same `applyToSelection` write path the
// keyboard culling shortcuts use, so the two surfaces cannot drift (the one-code-path
// invariant — pinned by the "one code path" block in __tests__/commandInventory.test.tsx).
import { COLOR_LABELS } from "../../modules/labels";
import type { Photo } from "../../modules/registry";
import { Thumbnail } from "../Thumbnail";
import { MAX_PANES } from "../CompareView";

export interface BenchProps {
  /** The one background job worth showing (App folds import > scan > develop into this).
   *  `total: null` = indeterminate — the bar shows a fixed partial fill. */
  progress: { label: string; done: number; total: number | null } | null;
  /** The status line (was the title bar's `.status` span). Shown when no job is running. */
  status: string;
  /** How many photos match the current query (the old grid status bar's count). */
  total: number;
  selectedCount: number;
  /** The photo whose marks the marking section shows — the same target the inspector
   *  shows (`shellPhoto` via shellTarget): Compare's focused pane, else the selection. */
  active: Photo | null;
  /** Rate the target. The star buttons resolve the toggle: the active star sends 0. */
  onRate: (n: number) => void;
  /** Pick/reject the target. The pills resolve the toggle: the active one sends "none". */
  onPick: (s: "pick" | "reject" | "none") => void;
  /** Colour-label the target. Same vocabulary as the culling COLOR_KEYS: "" clears. */
  onLabel: (name: string) => void;
  /** The first selected rows, for the pile's strip (App passes `selection.photos.slice(0, 3)`). */
  selectionThumbs: Photo[];
  thumbBusts: Map<number, number> | undefined;
  canCompare: boolean;
  compareOn: boolean;
  onCompare: () => void;
  onStack: () => void;
  onCull: () => void;
  canExport: boolean;
  onExport: () => void;
  canPublish: boolean;
  onPublish: () => void;
  onBackUpSelection: () => void;
  canBackUpSelection: boolean;
  onAnalyseBurst: () => void;
  ready: boolean;
  onClearSelection: () => void;
}

export function Bench({
  progress,
  status,
  total,
  selectedCount,
  active,
  onRate,
  onPick,
  onLabel,
  selectionThumbs,
  thumbBusts,
  canCompare,
  compareOn,
  onCompare,
  onStack,
  onCull,
  canExport,
  onExport,
  canPublish,
  onPublish,
  onBackUpSelection,
  canBackUpSelection,
  onAnalyseBurst,
  ready,
  onClearSelection,
}: BenchProps) {
  const pct =
    progress && progress.total ? Math.round((progress.done / progress.total) * 100) : null;
  return (
    <div className="bench">
      <div className="bench-prog">
        {progress ? (
          <div
            title={progress.label}
            role="progressbar"
            aria-valuenow={progress.done}
            aria-valuemax={progress.total ?? undefined}
          >
            <div className="bench-prog-label">{progress.label}</div>
            <div className="progress-track">
              <div
                className="progress-fill"
                style={{ width: pct != null ? `${pct}%` : "40%" }}
              />
            </div>
          </div>
        ) : (
          <>
            <div className="bench-count">
              {total.toLocaleString()} photos
              {selectedCount > 0 && ` · ${selectedCount.toLocaleString()} selected`}
            </div>
            {status && (
              <div className="bench-status" title={status}>
                {status}
              </div>
            )}
          </>
        )}
      </div>

      {active && (
        <>
          <div className="bench-sep" />
          <div className="bench-mark">
            <div className="bench-cap">
              MARKING <b>{active.path.split("/").pop()}</b>
            </div>
            <div className="bench-markrow">
              <div className="bench-stars">
                {[1, 2, 3, 4, 5].map((n) => (
                  <button
                    key={n}
                    className={`bench-star ${n <= active.rating ? "on" : ""}`}
                    aria-label={`Rate ${n}`}
                    // The inspector's toggle rule: the active star clears the rating.
                    onClick={() => onRate(n === active.rating ? 0 : n)}
                  >
                    ★
                  </button>
                ))}
              </div>
              <button
                className={`pkb ${active.pickState === "pick" ? "on" : ""}`}
                onClick={() => onPick(active.pickState === "pick" ? "none" : "pick")}
              >
                Pick
              </button>
              <button
                className={`pkb ${active.pickState === "reject" ? "on" : ""}`}
                onClick={() => onPick(active.pickState === "reject" ? "none" : "reject")}
              >
                Reject
              </button>
              <div className="bench-dots">
                {COLOR_LABELS.map((l) => (
                  <button
                    key={l.name}
                    className={`bench-dot ${active.label === l.name ? "on" : ""}`}
                    style={{ background: l.color, color: l.color }}
                    title={l.name}
                    // The inspector's toggle rule: the active dot clears the label.
                    onClick={() => onLabel(active.label === l.name ? "" : l.name)}
                  />
                ))}
                <button
                  className="bench-dot bench-dot-x"
                  title="Clear label"
                  onClick={() => active.label && onLabel("")}
                />
              </div>
            </div>
          </div>
        </>
      )}

      {selectedCount > 0 && (
        <div className="bench-pile">
          <span className="bench-selcount">
            {selectedCount.toLocaleString()} on the table
          </span>
          <div className="bench-strip">
            {selectionThumbs.map((p) => (
              <div
                key={p.id}
                className={`thumbwrap ${p.id === active?.id ? "hl" : ""}`}
              >
                <Thumbnail photoId={p.id} bust={thumbBusts?.get(p.id)} />
              </div>
            ))}
          </div>
          <button
            className={`txtb ${compareOn ? "on" : ""}`}
            disabled={!compareOn && !canCompare}
            title={
              compareOn || canCompare
                ? `Compare the selection side by side, ${MAX_PANES} at a time (C)`
                : "Select two or more photos to compare them"
            }
            onClick={onCompare}
          >
            Compare
          </button>
          <button
            className="txtb"
            disabled={!ready}
            title={`Propose stacks for ${selectedCount} selected photo(s)`}
            onClick={onStack}
          >
            Stack
          </button>
          <button
            className="txtb"
            disabled={!ready}
            title={`Cull ${selectedCount} selected photo(s) full screen, keyboard only`}
            onClick={onCull}
          >
            Cull
          </button>
          <button
            className="txtb"
            disabled={!ready}
            title={`Rank burst sharpness for ${selectedCount} selected photo(s)`}
            onClick={onAnalyseBurst}
          >
            Analyse
          </button>
          <button
            className="txtb"
            disabled={!canExport}
            title="Export the selected photo(s) to files (RAW + XMP, or JPEG)"
            onClick={onExport}
          >
            Export
          </button>
          <button
            className="txtb"
            disabled={!canPublish}
            title="Publish the selected photo to Instagram, Flickr, SmugMug…"
            onClick={onPublish}
          >
            Publish
          </button>
          <button
            className="txtb"
            disabled={!canBackUpSelection}
            title={`Queue ${selectedCount} selected photo(s) to copy to the NAS`}
            onClick={onBackUpSelection}
          >
            Back up
          </button>
          <button
            className="bench-x"
            title="Clear selection"
            aria-label="Clear selection"
            onClick={onClearSelection}
          >
            <svg
              width="15"
              height="15"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              aria-hidden
            >
              <line x1="18" y1="6" x2="6" y2="18" />
              <line x1="6" y1="6" x2="18" y2="18" />
            </svg>
          </button>
        </div>
      )}
    </div>
  );
}
