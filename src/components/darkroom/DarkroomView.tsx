// The Darkroom (docs/plans/darkroom): develop-by-choosing. As of slice 2 the tone strip
// is live end-to-end: fills show the rendered working state's real zone masses, and
// dragging a zone sculpts that tonal band on the print (a feathered per-luma gain curve
// in the engine — see src-tauri plugins/edit/zones.rs).
import { useEffect, useRef, useState } from "react";
import {
  editZoneMasses,
  getSetting,
  renderEdit,
  setSetting,
  suggestAutoTone,
} from "../../modules/api";
import { parseEdit, type VersionEdit } from "../../modules/editing";
import { broadcastPhoto, onLoupeReady, openLoupeWindow } from "../../modules/loupe";
import { allPresets, BUILTIN_PRESETS, type DevelopPreset } from "../../modules/presets";
import { ProofSheet } from "./ProofSheet";
import { proofSpread, type ProofCandidate } from "./spreads";
import { ToneStrip } from "./ToneStrip";
import "./darkroom.css";

const PREVIEW_MAX = 1400;
/** Persisted "print on the loupe screen" preference (Gate 2's settings key). */
const PRINT_ON_LOUPE_KEY = "basic-editor.printOnLoupe";

/** What the loupe should show for a record: null for an empty record (the loupe then
 *  skips the render round-trip and shows the plain preview), else the record itself. */
const loupeJson = (json: string) => (json === "{}" ? null : json);

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

  // The second-screen print. Refs mirror the states the broadcast paths need, so the
  // debounce timer, the loupe:ready replay, and the unmount hand-back all read the
  // latest values without re-arming their effects.
  const [printOnLoupe, setPrintOnLoupe] = useState(true);
  const printOnLoupeRef = useRef(true);
  const workingRef = useRef(working);
  workingRef.current = working;

  useEffect(() => {
    getSetting(PRINT_ON_LOUPE_KEY)
      .then((v) => {
        const on = v !== "0"; // default on — the loupe simply ignores it when closed
        setPrintOnLoupe(on);
        printOnLoupeRef.current = on;
      })
      .catch(() => {});
  }, []);

  // A loupe window that opens mid-session announces itself; re-send the working print
  // (App replays its own selection first — this listener registered later, so it wins).
  useEffect(() => {
    const unlisten = onLoupeReady(() => {
      if (printOnLoupeRef.current) {
        broadcastPhoto(photoId, loupeJson(JSON.stringify(workingRef.current)));
      }
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, [photoId]);

  // Leaving the Darkroom hands the loupe back to the version the shell knows about.
  useEffect(
    () => () => {
      if (printOnLoupeRef.current) broadcastPhoto(photoId, initialEditJson);
    },
    [photoId, initialEditJson],
  );

  // The proof sheet: the auto-tone fragment is fetched once per photo, the preset
  // library once per mount; the spread itself is pure maths at deal time.
  const [proofs, setProofs] = useState<ProofCandidate[] | null>(null);
  const [autoFragment, setAutoFragment] = useState<VersionEdit | null>(null);
  const [presets, setPresets] = useState<DevelopPreset[]>(BUILTIN_PRESETS);
  useEffect(() => {
    allPresets()
      .then(setPresets)
      .catch(() => setPresets(BUILTIN_PRESETS));
  }, []);
  useEffect(() => {
    let alive = true;
    setAutoFragment(null);
    suggestAutoTone(photoId)
      .then((j) => alive && setAutoFragment(parseEdit(j)))
      .catch(() => alive && setAutoFragment({}));
    return () => {
      alive = false;
    };
  }, [photoId]);

  const dealProofs = () => {
    setProofs(proofSpread(workingRef.current, autoFragment ?? {}, presets));
  };

  const togglePrintOnLoupe = () => {
    const next = !printOnLoupe;
    setPrintOnLoupe(next);
    printOnLoupeRef.current = next;
    setSetting(PRINT_ON_LOUPE_KEY, next ? "1" : "0").catch(() => {});
    if (next) {
      // Opening (or focusing) the window is part of turning the print on.
      void openLoupeWindow().catch(() => {});
      broadcastPhoto(photoId, loupeJson(JSON.stringify(workingRef.current)));
    } else {
      broadcastPhoto(photoId, initialEditJson);
    }
  };

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
      // The loupe print rides the same debounce: one settled state, one broadcast.
      if (printOnLoupeRef.current) broadcastPhoto(photoId, loupeJson(json));
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
        <button
          className={`dk-loupe-toggle ${printOnLoupe ? "on" : ""}`}
          onClick={togglePrintOnLoupe}
          title={
            printOnLoupe
              ? "The loupe window shows this print live — click to stop"
              : "Open the loupe window and print there live"
          }
        >
          🖥 Loupe print
        </button>
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
      <div className="dk-actions">
        <button
          className="dk-act dk-act-primary"
          onClick={dealProofs}
          disabled={autoFragment === null}
          title="Your photo developed a dozen ways — pick the one that's closest"
        >
          ▦ Deal a proof sheet
        </button>
      </div>
      {proofs && (
        <ProofSheet
          photoId={photoId}
          candidates={proofs}
          onAdopt={(record) => {
            setWorking(record);
            setProofs(null);
          }}
          onClose={() => setProofs(null)}
        />
      )}
    </div>
  );
}
