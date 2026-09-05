// The Darkroom (docs/plans/darkroom): develop-by-choosing. Slice 1 — the tracer bullet:
// the print renders the working record live, and dragging the tone strip writes real
// `zones` into that record. The engine's zone curve is still the identity, so what this
// slice proves is the wire: strip → record → render → print, on a real photo.
import { useEffect, useRef, useState } from "react";
import { renderEdit } from "../../modules/api";
import { parseEdit, type VersionEdit } from "../../modules/editing";
import { ToneStrip } from "./ToneStrip";
import "./darkroom.css";

const PREVIEW_MAX = 1400;
// Slice 1: a plausible placeholder mountain — real masses arrive with the
// `edit_zone_masses` command in slice 2.
const PLACEHOLDER_MASSES = [0.04, 0.11, 0.2, 0.24, 0.19, 0.12, 0.07, 0.03];

export function DarkroomView({
  photoId,
  initialEditJson,
  onBack,
}: {
  photoId: number;
  /** The active version's record — the working state starts from it (null = as shot). */
  initialEditJson: string | null;
  onBack: () => void;
}) {
  const [working, setWorking] = useState<VersionEdit>(() =>
    parseEdit(initialEditJson ?? undefined),
  );
  const [backdrop, setBackdrop] = useState("");
  const [rendering, setRendering] = useState(false);
  const [error, setError] = useState("");
  const renderSeq = useRef(0);

  // Debounced live render of the working record — the print. A stale response never
  // paints over a newer one (sequence check).
  useEffect(() => {
    const seq = ++renderSeq.current;
    setRendering(true);
    const t = setTimeout(() => {
      renderEdit(photoId, JSON.stringify(working), PREVIEW_MAX)
        .then((url) => {
          if (renderSeq.current !== seq) return;
          setBackdrop(url);
          setError("");
        })
        .catch((e) => {
          if (renderSeq.current === seq) setError(String(e));
        })
        .finally(() => {
          if (renderSeq.current === seq) setRendering(false);
        });
    }, 250);
    return () => clearTimeout(t);
  }, [photoId, working]);

  return (
    <div className="dk-root">
      <header className="dk-bar">
        <button className="dk-back" onClick={onBack}>
          ← Library
        </button>
        <span className="dk-title">Darkroom</span>
        <span className="dk-hint">
          early preview · slice 1 — the strip writes real zone records
          {rendering ? " · rendering…" : ""}
        </span>
      </header>
      {error && <div className="dk-error">{error}</div>}
      <div className="dk-stage">
        {backdrop ? (
          <img className="dk-print" src={backdrop} alt="" />
        ) : (
          <div className="dk-empty">Rendering…</div>
        )}
      </div>
      <div className="dk-strip-row">
        <ToneStrip
          masses={PLACEHOLDER_MASSES}
          zones={working.zones}
          onZones={(zones) => setWorking((w) => ({ ...w, zones }))}
        />
      </div>
    </div>
  );
}
