// The proof sheet (docs/plans/darkroom/mockups/02-proof-sheet.html): the photo
// developed a dozen ways — real renders, each a native `edit://` URL from the same source
// as the stage (the RAW working image when it is resident; the framed-base cache makes
// the dozen share one geometry pass). Click a proof to adopt its record; Esc or the
// backdrop declines.
import { useEffect } from "react";
import type { VersionEdit } from "../../modules/editing";
import { RenderedImage } from "./RenderedImage";
import type { ProofCandidate } from "./spreads";

export function ProofSheet({
  candidates,
  renderUrl,
  onAdopt,
  onClose,
}: {
  candidates: ProofCandidate[];
  /** The render URL for a record at a given long edge — the Darkroom's engine and source. */
  renderUrl: (record: VersionEdit, maxEdge: number) => string;
  onAdopt: (record: VersionEdit) => void;
  onClose: () => void;
}) {
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
              <RenderedImage src={renderUrl(c.record, 320)} loadingClass="dk-proof-loading" loadingText="…" />
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
