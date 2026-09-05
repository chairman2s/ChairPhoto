// The proof sheet (docs/plans/darkroom/mockups/02-proof-sheet.html): the photo
// developed a dozen ways — real renders from one render_edit_batch call (proxy decoded
// once backend-side). Click a proof to adopt its record; Esc or the backdrop declines.
import { useEffect, useState } from "react";
import { renderEditBatch } from "../../modules/api";
import type { VersionEdit } from "../../modules/editing";
import type { ProofCandidate } from "./spreads";

export function ProofSheet({
  photoId,
  candidates,
  onAdopt,
  onClose,
}: {
  photoId: number;
  candidates: ProofCandidate[];
  onAdopt: (record: VersionEdit) => void;
  onClose: () => void;
}) {
  const [renders, setRenders] = useState<(string | null)[]>([]);

  useEffect(() => {
    let alive = true;
    renderEditBatch(
      photoId,
      candidates.map((c) => JSON.stringify(c.record)),
      320,
    )
      .then((r) => {
        if (alive) setRenders(r);
      })
      .catch(() => {
        if (alive) setRenders(candidates.map(() => null));
      });
    return () => {
      alive = false;
    };
  }, [photoId, candidates]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        onClose();
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onClose]);

  return (
    <div className="dk-proof-backdrop" onClick={onClose}>
      <div className="dk-proof" onClick={(e) => e.stopPropagation()}>
        <header className="dk-proof-head">
          <span className="dk-proof-title">Proof sheet</span>
          <span className="dk-proof-sub">
            your photo, developed {candidates.length} ways — real renders
          </span>
          <button className="dk-proof-close" onClick={onClose} title="Close (Esc)">
            ✕
          </button>
        </header>
        <div className="dk-proof-grid">
          {candidates.map((c, i) => (
            <button
              key={`${c.label}-${i}`}
              className={`dk-proof-cell ${c.group === "asShot" ? "current" : ""}`}
              onClick={() => onAdopt(c.record)}
              title={`Adopt "${c.label}" as the working state`}
            >
              {renders[i] ? (
                <img src={renders[i]!} alt="" />
              ) : (
                <span className="dk-proof-loading">…</span>
              )}
              <span className="dk-proof-tag">
                <b>{c.label}</b>
                {c.group !== "asShot" && c.group !== "auto" && <em>{c.group}</em>}
              </span>
            </button>
          ))}
        </div>
        <footer className="dk-proof-foot">
          Click a proof to adopt it · the current state is always dealt, so declining is a
          click · framing never changes on a proof
        </footer>
      </div>
    </div>
  );
}
