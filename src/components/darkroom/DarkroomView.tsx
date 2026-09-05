// The Darkroom (docs/plans/darkroom): develop-by-choosing. The proof sheet and duels
// are the fast path, the tone strip is the adjustable histogram, and — since slice 6 —
// the classic slider rail and interactive crop/straighten/perspective stage (shared
// with EditorView via EditControls.tsx) are the precision path. The pop-out loupe is
// the full-bleed print.
//
// State model: one `working: VersionEdit` record. The rail and stage widgets speak the
// decomposed Tone/Look/geometry dialect, so this view bridges: derived values go down,
// updates merge back into the record. The stage renders the record WITHOUT its crop
// (the crop is an overlay, exactly like EditorView); the loupe print and the strip's
// masses always use the full record — they describe the finished print.
import { useEffect, useRef, useState } from "react";
import {
  createVersion,
  editZoneMasses,
  getSetting,
  renderEdit,
  setSetting,
  setVersionEdit,
  suggestAutoTone,
} from "../../modules/api";
import {
  ASPECTS,
  clampStraighten,
  Crop,
  CropOverlay,
  DEFAULT_QUAD,
  fitCrop,
  inscribedCrop,
  Look,
  lookFields,
  parseEdit,
  Perspective,
  Tone,
  ZERO_LOOK,
  ZERO_TONE,
  type VersionEdit,
} from "../../modules/editing";
import { broadcastPhoto, onLoupeReady, openLoupeWindow } from "../../modules/loupe";
import { allPresets, BUILTIN_PRESETS, type DevelopPreset } from "../../modules/presets";
import {
  EditStage,
  EffectsRail,
  GeometryRail,
  OVERLAY_KEY,
  persistOverlay,
  ToneRail,
} from "../EditControls";
import { DuelView } from "./DuelView";
import { ProofSheet } from "./ProofSheet";
import { DUEL_LABELS, proofSpread, type DuelDim, type ProofCandidate } from "./spreads";
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
  photoW,
  photoH,
  initialEditJson,
  onBack,
}: {
  photoId: number;
  /** Original (sensor) pixel dimensions, for the crop-size readout and aspect math. */
  photoW: number | null;
  photoH: number | null;
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

  // ── Bridge: the record, spoken in the rail/stage's decomposed dialect ──
  const tone: Tone = {
    ...ZERO_TONE,
    ...working.tone,
    wb: { ...ZERO_TONE.wb, ...working.tone?.wb },
  };
  const look: Look = {
    ...ZERO_LOOK,
    bw: working.bw,
    split: working.split,
    grain: working.grain,
    fade: working.fade ?? 0,
    vignette: working.vignette ?? 0,
    lut: working.lut,
  };
  const crop = working.crop ?? null;
  const aspect = crop?.aspect ?? "Original";
  const straighten = working.straighten ?? 0;
  const perspective = working.perspective ?? null;

  const onTone = (t: Tone) => setWorking((w) => ({ ...w, tone: t }));
  const onLook = (l: Look) =>
    setWorking((w) => ({
      ...w,
      bw: undefined,
      split: undefined,
      grain: undefined,
      fade: undefined,
      vignette: undefined,
      lut: undefined,
      ...lookFields(l),
    }));
  const setCropF = (f: (c: Crop | null) => Crop | null) =>
    setWorking((w) => ({ ...w, crop: f(w.crop ?? null) ?? undefined }));
  const setPerspectiveF = (f: (p: Perspective | null) => Perspective | null) =>
    setWorking((w) => ({ ...w, perspective: f(w.perspective ?? null) ?? undefined }));

  // ── Geometry UI state (presentation-only) ──
  const [overlay, setOverlay] = useState<CropOverlay>("thirds");
  const [straightenMode, setStraightenMode] = useState(false);
  const [perspectiveMode, setPerspectiveMode] = useState(false);
  const [imgDims, setImgDims] = useState<{ w: number; h: number } | null>(null);
  useEffect(() => {
    getSetting(OVERLAY_KEY).then((v) => {
      if (v === "none" || v === "thirds" || v === "phi" || v === "golden") setOverlay(v);
    });
  }, []);

  // Sensor dims are unrotated; the displayed preview is oriented — swap to match
  // (same reasoning as EditorView's orientedDims).
  const orientedDims =
    photoW && photoH
      ? imgDims && imgDims.h > imgDims.w !== photoH > photoW
        ? { w: photoH, h: photoW }
        : { w: photoW, h: photoH }
      : null;
  const cropPx = orientedDims
    ? {
        w: Math.round((crop?.w ?? 1) * orientedDims.w),
        h: Math.round((crop?.h ?? 1) * orientedDims.h),
      }
    : null;
  const srcDims = orientedDims ?? imgDims;

  const ratioFor = (label: string): number | null => {
    if (label === "Original" || label === "Free") return null;
    return ASPECTS.find((a) => a.label === label)?.ratio ?? null;
  };
  const applyAspect = (label: string) => {
    if (label === "Original") return setWorking((w) => ({ ...w, crop: undefined }));
    if (label === "Free") {
      return setWorking((w) => ({
        ...w,
        crop: w.crop ? { ...w.crop, aspect: "Free" } : { x: 0.1, y: 0.1, w: 0.8, h: 0.8, aspect: "Free" },
      }));
    }
    const ratio = ratioFor(label);
    if (ratio == null || !srcDims) return;
    const fit = fitCrop(srcDims.w, srcDims.h, ratio, 1);
    setWorking((w) => ({ ...w, crop: { ...fit, aspect: label } }));
  };
  // Straighten auto-insets the crop so the rotation's corners stay hidden (EditorView's
  // policy, restated over the record).
  const applyStraighten = (deg: number) => {
    const d = clampStraighten(deg);
    setWorking((w) => ({
      ...w,
      straighten: Math.abs(d) < 0.05 ? undefined : d,
      crop:
        Math.abs(d) < 0.05
          ? undefined
          : srcDims
            ? (inscribedCrop(srcDims.w, srcDims.h, d) as Crop)
            : w.crop,
    }));
  };
  const handleLevel = (deltaDeg: number | null) => {
    setStraightenMode(false);
    if (deltaDeg != null) applyStraighten(straighten + deltaDeg);
  };
  const startPerspective = () => {
    setWorking((w) => ({ ...w, perspective: w.perspective ?? { ...DEFAULT_QUAD }, crop: undefined }));
    setPerspectiveMode(true);
    setStraightenMode(false);
  };
  const clearPerspective = () => {
    setWorking((w) => ({ ...w, perspective: undefined, crop: undefined }));
    setPerspectiveMode(false);
  };

  // ── The second-screen print ──
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

  // The duel: picks stream into `working` (stage, strip, and loupe follow live), and
  // either pane can be banked as a version mid-round.
  const [duelOpen, setDuelOpen] = useState(false);
  const forkVersion = async (record: VersionEdit, dim: DuelDim): Promise<string | null> => {
    try {
      const name = `What-if — ${DUEL_LABELS[dim].toLowerCase()}`;
      const id = await createVersion(photoId, name);
      await setVersionEdit(id, JSON.stringify(record));
      return name;
    } catch {
      return null;
    }
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

  // Debounced live renders. The STAGE renders the record without its crop (the crop is
  // an interactive overlay) and un-warped while the perspective handles are up; the
  // loupe print and zone masses use the FULL record — they describe the final print.
  useEffect(() => {
    const seq = ++renderSeq.current;
    setRendering(true);
    const t = setTimeout(() => {
      const fullJson = JSON.stringify(working);
      const stageJson = JSON.stringify({
        ...working,
        crop: undefined,
        perspective: perspectiveMode ? undefined : working.perspective,
      });
      renderEdit(photoId, stageJson, PREVIEW_MAX)
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
      editZoneMasses(photoId, fullJson)
        .then((m) => {
          if (renderSeq.current === seq) setMasses(m);
        })
        .catch(() => {
          // Masses are a cosmetic overlay on the strip — a failure leaves the last fill.
        });
      // The loupe print rides the same debounce: one settled state, one broadcast.
      if (printOnLoupeRef.current) broadcastPhoto(photoId, loupeJson(fullJson));
    }, 250);
    return () => clearTimeout(t);
  }, [photoId, working, perspectiveMode]);

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
          early preview — choose with proofs and duels, steer with the strip and rail
          {rendering ? " · rendering…" : ""}
        </span>
      </header>
      {error && <div className="dk-error">{error}</div>}
      <div className="dk-body">
        <div className="dk-center">
          <div className="dk-stage">
            <EditStage
              backdrop={backdrop}
              loading={<div className="dk-empty">Rendering…</div>}
              crop={crop}
              setCrop={setCropF}
              ratio={ratioFor(aspect)}
              srcDims={srcDims}
              cropPx={cropPx}
              overlay={overlay}
              perspective={perspective}
              setPerspective={setPerspectiveF}
              perspectiveMode={perspectiveMode}
              straightenMode={straightenMode}
              onLevel={handleLevel}
              onImgDims={setImgDims}
            />
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
            <button
              className="dk-act"
              onClick={() => setDuelOpen(true)}
              title="Refine by choosing: two prints per round, pick the better one"
            >
              ⚖ Refine by duel
            </button>
            <button
              className="dk-act"
              onClick={() => setWorking({})}
              title="Back to as shot — clears every adjustment, framing included"
            >
              Reset
            </button>
          </div>
        </div>
        <aside className="dk-rail">
          <ToneRail tone={tone} onTone={onTone} />
          <EffectsRail look={look} onLook={onLook} onError={setError} />
          <GeometryRail
            aspect={aspect}
            onAspect={applyAspect}
            overlay={overlay}
            onOverlay={(key) => {
              setOverlay(key);
              persistOverlay(key);
            }}
            cropPx={cropPx}
            cropActive={!!crop}
            perspective={perspective}
            perspectiveMode={perspectiveMode}
            onPerspectiveToggle={() =>
              perspectiveMode ? setPerspectiveMode(false) : startPerspective()
            }
            onPerspectiveClear={clearPerspective}
            straighten={straighten}
            straightenMode={straightenMode}
            onStraightenModeToggle={() => setStraightenMode((m) => !m)}
            onStraighten={applyStraighten}
          />
        </aside>
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
      {duelOpen && (
        <DuelView
          photoId={photoId}
          working={working}
          onApply={setWorking}
          onFork={forkVersion}
          onClose={() => setDuelOpen(false)}
        />
      )}
    </div>
  );
}
