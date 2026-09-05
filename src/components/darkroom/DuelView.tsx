// Duel refinement (docs/plans/darkroom/mockups/03-duel.html): two prints, pick the
// better one with a click or ←/→. Each round explores one dimension; the winner
// immediately becomes the working state (the stage, strip, and loupe all follow), ↓
// says "same" and skips the dimension, Esc keeps the standing winner and leaves.
// Either pane can be banked as a version (⑂) without ending the round.
import { useEffect, useMemo, useState } from "react";
import { renderEditBatch } from "../../modules/api";
import type { VersionEdit } from "../../modules/editing";
import { DUEL_DIMS, DUEL_LABELS, duelPair, type DuelDim } from "./spreads";

export function DuelView({
  photoId,
  working,
  onApply,
  onFork,
  onClose,
}: {
  photoId: number;
  working: VersionEdit;
  /** The picked variant becomes the working state. */
  onApply: (record: VersionEdit) => void;
  /** Bank a variant as a version without ending the round. Resolves to its name. */
  onFork: (record: VersionEdit, dim: DuelDim) => Promise<string | null>;
  onClose: () => void;
}) {
  const [dimIdx, setDimIdx] = useState(0);
  const [renders, setRenders] = useState<(string | null)[]>([]);
  const [note, setNote] = useState("");
  const dim = DUEL_DIMS[dimIdx];

  const pair = useMemo(() => duelPair(working, dim, 0), [working, dim]);

  useEffect(() => {
    let alive = true;
    setRenders([]);
    renderEditBatch(
      photoId,
      pair.map((r) => JSON.stringify(r)),
      1024,
    )
      .then((r) => {
        if (alive) setRenders(r);
      })
      .catch(() => {
        if (alive) setRenders([null, null]);
      });
    return () => {
      alive = false;
    };
  }, [photoId, pair]);

  const advance = () => {
    setNote("");
    if (dimIdx + 1 >= DUEL_DIMS.length) onClose();
    else setDimIdx(dimIdx + 1);
  };
  const pick = (i: 0 | 1) => {
    onApply(pair[i]);
    advance();
  };
  const fork = (i: 0 | 1) => {
    onFork(pair[i], dim).then((name) => {
      if (name) setNote(`Kept as “${name}”`);
    });
  };

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "ArrowLeft") pick(0);
      else if (e.key === "ArrowRight") pick(1);
      else if (e.key === "ArrowDown") advance();
      else if (e.key === "Escape") onClose();
      else return;
      e.preventDefault();
      e.stopPropagation();
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
    // pick/advance close over the current pair/dim — re-arm per round.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [pair, dimIdx]);

  return (
    <div className="dk-duel">
      <header className="dk-duel-head">
        <span className="dk-duel-title">⚖ Duel — round {dimIdx + 1}</span>
        <div className="dk-duel-rounds">
          {DUEL_DIMS.map((d, i) => (
            <span
              key={d}
              className={`dk-duel-round ${i < dimIdx ? "done" : ""} ${i === dimIdx ? "on" : ""}`}
            >
              {DUEL_LABELS[d]}
            </span>
          ))}
        </div>
        {note && <span className="dk-duel-note">{note}</span>}
        <span className="dk-duel-esc">↓ same · Esc done</span>
      </header>
      <main className="dk-duel-panes">
        {([0, 1] as const).map((i) => (
          <div key={i} className="dk-duel-pane" onClick={() => pick(i)}>
            {renders[i] ? (
              <img src={renders[i]!} alt="" />
            ) : (
              <div className="dk-duel-loading">Rendering…</div>
            )}
            <div className="dk-duel-pane-bar">
              <button
                className="dk-duel-pick"
                onClick={(e) => {
                  e.stopPropagation();
                  pick(i);
                }}
              >
                {i === 0 ? "← This one" : "This one →"}
              </button>
              <button
                className="dk-duel-fork"
                title="Keep this variant as a version and continue"
                onClick={(e) => {
                  e.stopPropagation();
                  fork(i);
                }}
              >
                ⑂
              </button>
            </div>
          </div>
        ))}
      </main>
    </div>
  );
}
