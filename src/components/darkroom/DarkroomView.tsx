// The Darkroom (docs/plans/darkroom): develop-by-choosing. As of slice 2 the tone strip
// is live end-to-end: fills show the rendered working state's real zone masses, and
// dragging a zone sculpts that tonal band on the print (a feathered per-luma gain curve
// in the engine — see src-tauri plugins/edit/zones.rs).
import { useEffect, useRef, useState } from "react";
import { editZoneMasses, renderEdit } from "../../modules/api";
import { parseEdit, type VersionEdit } from "../../modules/editing";
import { ToneStrip } from "./ToneStrip";
import "./darkroom.css";

const PREVIEW_MAX = 1400;

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
  const [masses, setMasses] = useState<number[]>([]);
  const [rendering, setRendering] = useState(false);
  const [error, setError] = useState("");
  const renderSeq = useRef(0);

  // Debounced live render of the working record — the print — plus the strip's zone
  // masses of that same state. A stale response never paints over a newer one.
  useEffect(() => {
    const seq = ++renderSeq.current;
    setRendering(true);
    const t = setTimeout(() => {
      const json = JSON.stringify(working);
      renderEdit(photoId, json, PREVIEW_MAX)
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
      editZoneMasses(photoId, json)
        .then((m) => {
          if (renderSeq.current === seq) setMasses(m);
        })
        .catch(() => {
          // Masses are a cosmetic overlay on the strip — a failure leaves the last fill.
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
          early preview — drag the strip to sculpt; proof sheets and duels are coming
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
          masses={masses}
          zones={working.zones}
          onZones={(zones) => setWorking((w) => ({ ...w, zones }))}
        />
      </div>
    </div>
  );
}
