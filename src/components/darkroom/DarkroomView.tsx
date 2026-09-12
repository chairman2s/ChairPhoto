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
  editRenderUrl,
  editZoneMasses,
  getSetting,
  listVersions,
  PhotoVersion,
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
import { markShellLeave, setShellTimingEnabled } from "../../modules/shellTiming";
import { stageJsonFor } from "./stageJson";
import { ToneStrip } from "./ToneStrip";
import {
  formatSample,
  RENDER_TIMING_KEY,
  RENDER_TIMING_SUMMARY_KEY,
  summarize,
  type FrameSample,
} from "./renderTiming";
import "./darkroom.css";

const PREVIEW_MAX = 1400;
/** The live-drag tier: small enough to render ~11 fps through the cached proxy. */
const PREVIEW_FAST = 720;
/** Persisted "print on the loupe screen" preference (Gate 2's settings key). */
const PRINT_ON_LOUPE_KEY = "basic-editor.printOnLoupe";

/** What the loupe should show for a record: null for an empty record (the loupe then
 *  skips the render round-trip and shows the plain preview), else the record itself. */
const loupeJson = (json: string) => (json === "{}" ? null : json);

export function DarkroomView({
  photoId,
  photoW,
  photoH,
  activeVersionId,
  initialEditJson,
  onPickVersion,
  onChanged,
  onBack,
}: {
  photoId: number;
  /** Original (sensor) pixel dimensions, for the crop-size readout and aspect math. */
  photoW: number | null;
  photoH: number | null;
  activeVersionId: number | null;
  /** The active version's record — the working state starts from it (null = as shot). */
  initialEditJson: string | null;
  /** Keep the shell's active version in step with the shelf and saves. */
  onPickVersion: (v: PhotoVersion | null) => void;
  onChanged: () => void;
  onBack: () => void;
}) {
  const [working, setWorking] = useState<VersionEdit>(() =>
    parseEdit(initialEditJson ?? undefined),
  );
  // The Darkroom is a sandbox (user decision, slice 7): it never auto-saves over the
  // version it started from. `savedJson` is the last state banked to (or loaded from) a
  // version — the unsaved marker and the loupe hand-back both compare against it.
  const [versions, setVersions] = useState<PhotoVersion[]>([]);
  const [baseVersionId, setBaseVersionId] = useState<number | null>(activeVersionId);
  const [savedJson, setSavedJson] = useState<string>(() =>
    JSON.stringify(parseEdit(initialEditJson ?? undefined)),
  );
  const savedJsonRef = useRef(savedJson);
  savedJsonRef.current = savedJson;
  // The proof label last adopted — the default name a save gets ("Portra", "Auto"…).
  const adoptedLabelRef = useRef<string | null>(null);

  useEffect(() => {
    listVersions(photoId)
      .then(setVersions)
      .catch(() => setVersions([]));
  }, [photoId]);
  const [backdrop, setBackdrop] = useState("");
  const [masses, setMasses] = useState<number[]>([]);
  const [rendering, setRendering] = useState(false);
  const [error, setError] = useState("");
  const renderSeq = useRef(0);

  // ── Frame timing (dev, `editor.renderTiming`) ──
  // Stamps each tier's request, IPC resolve and on-screen paint; one console line per
  // frame, a summary every 2 s while frames arrive, and the last summary persisted so a
  // run can be read back without the inspector. Off, none of this allocates.
  const timingRef = useRef(false);
  const samplesRef = useRef<FrameSample[]>([]);
  const pendingPaintRef = useRef(new Map<string, FrameSample>());
  const unsummarizedRef = useRef(0);
  useEffect(() => {
    let alive = true;
    getSetting(RENDER_TIMING_KEY)
      .then((v) => {
        if (!alive) return;
        timingRef.current = v === "1";
        setShellTimingEnabled(v === "1");
      })
      .catch(() => {});
    const flush = () => {
      if (unsummarizedRef.current === 0) return;
      unsummarizedRef.current = 0;
      const summary = JSON.stringify(summarize(samplesRef.current));
      console.debug(`[edit-timing] summary ${summary}`);
      setSetting(RENDER_TIMING_SUMMARY_KEY, summary).catch(() => {});
    };
    const timer = setInterval(flush, 2000);
    return () => {
      alive = false;
      clearInterval(timer);
      flush();
    };
  }, []);
  const startSample = (seq: number, tier: FrameSample["tier"]): FrameSample | null => {
    if (!timingRef.current) return null;
    const s: FrameSample = { seq, tier, requested: performance.now() };
    samplesRef.current.push(s);
    if (samplesRef.current.length > 400) samplesRef.current.splice(0, 200);
    return s;
  };
  const logSample = (s: FrameSample) => {
    unsummarizedRef.current++;
    console.debug(formatSample(s));
  };
  // A frame's paint is attributed by URL when the stage <img> reports the load. The
  // element shows only its latest src, so every frame requested before the one that
  // loaded was superseded — the browser dropped it, and so does the log.
  const awaitPaint = (s: FrameSample | null, url: string) => {
    if (!s) return;
    pendingPaintRef.current.set(url, s);
  };
  const settlePaints = (loaded: FrameSample | null) => {
    for (const [url, p] of pendingPaintRef.current) {
      if (loaded && p.requested >= loaded.requested) continue;
      pendingPaintRef.current.delete(url);
      p.superseded = true;
      logSample(p);
    }
  };
  const onPaintOf = (src: string) => {
    const s = pendingPaintRef.current.get(src);
    if (!s) return;
    pendingPaintRef.current.delete(src);
    settlePaints(s);
    requestAnimationFrame((t) => {
      s.painted = t;
      logSample(s);
    });
  };

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

  // Leaving the Darkroom hands the loupe back to the last SAVED state — what the shell
  // shows. (A ref, so this runs only on true unmount, not when a save updates props.)
  useEffect(
    () => () => {
      if (printOnLoupeRef.current) broadcastPhoto(photoId, loupeJson(savedJsonRef.current));
    },
    [photoId],
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
      setVersions(await listVersions(photoId));
      onChanged();
      return name;
    } catch {
      return null;
    }
  };

  // ── Saving: settings only, always to a NEW version ──
  const dirty = JSON.stringify(working) !== savedJson;
  const saveAsVersion = async () => {
    const json = JSON.stringify(workingRef.current);
    try {
      const name = adoptedLabelRef.current ?? `Darkroom ${versions.length + 1}`;
      const id = await createVersion(photoId, name);
      await setVersionEdit(id, json);
      const next = await listVersions(photoId);
      setVersions(next);
      setSavedJson(json);
      setBaseVersionId(id);
      adoptedLabelRef.current = null;
      onChanged();
      const created = next.find((v) => v.id === id);
      if (created) onPickVersion(created);
    } catch (e) {
      setError(String(e));
    }
  };

  // Shelf: load a version's settings into the working state (the shell follows).
  const loadVersion = (v: PhotoVersion) => {
    const record = parseEdit(v.editJson);
    setWorking(record);
    setSavedJson(JSON.stringify(record));
    setBaseVersionId(v.id);
    adoptedLabelRef.current = null;
    onPickVersion(v);
  };
  const loadAsShot = () => {
    setWorking({});
    setSavedJson("{}");
    setBaseVersionId(null);
    adoptedLabelRef.current = null;
    onPickVersion(null);
  };

  // Ctrl+S banks the settings as a new version.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "s") {
        e.preventDefault();
        if (JSON.stringify(workingRef.current) !== savedJsonRef.current) void saveAsVersion();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // saveAsVersion reads refs; versions.length only names the default.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [photoId, versions.length]);

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

  // Live renders, two tiers, both native `edit://` URLs on the stage <img> (never base64
  // over IPC): the backend's bounded LIFO pool renders the newest URL first and coalesces
  // identical ones, and the element shows only its latest src, so a stale frame can never
  // paint over a newer one. While the user drags, a small FAST URL keeps the stage live
  // (leading-edge throttle against the cached proxy decode); once input settles for
  // 250ms, the full-quality URL, the strip's masses, and the loupe print follow. The
  // STAGE renders the record without its crop (an interactive overlay) and un-warped
  // while the perspective handles are up; masses and the loupe use the FULL record —
  // they describe the final print.
  const lastFastRef = useRef(0);
  const settledUrlRef = useRef("");
  useEffect(() => {
    const seq = ++renderSeq.current;
    setRendering(true);
    const fullJson = JSON.stringify(working);
    const stageJson = stageJsonFor(working, perspectiveMode);
    const sinceFast = Date.now() - lastFastRef.current;
    const fastTimer = setTimeout(
      () => {
        lastFastRef.current = Date.now();
        const url = editRenderUrl(photoId, stageJson, { maxEdge: PREVIEW_FAST });
        awaitPaint(startSample(seq, "fast"), url);
        setBackdrop(url);
      },
      sinceFast > 90 ? 0 : 90 - sinceFast,
    );
    const settleTimer = setTimeout(() => {
      const url = editRenderUrl(photoId, stageJson, { maxEdge: PREVIEW_MAX });
      settledUrlRef.current = url;
      awaitPaint(startSample(seq, "settled"), url);
      setBackdrop(url);
      editZoneMasses(photoId, fullJson)
        .then((m) => {
          if (renderSeq.current === seq) setMasses(m);
        })
        .catch(() => {
          // Masses are a cosmetic overlay on the strip — a failure leaves the last fill.
        });
      // The loupe print rides the settle: one settled state, one broadcast.
      if (printOnLoupeRef.current) broadcastPhoto(photoId, loupeJson(fullJson));
    }, 250);
    return () => {
      clearTimeout(fastTimer);
      clearTimeout(settleTimer);
    };
  }, [photoId, working, perspectiveMode]);
  // The settled frame is on screen (or failed): the URL is unique per state, so equality
  // with the one this effect set is the ownership check.
  const onBackdropLoad = (src: string) => {
    onPaintOf(src);
    if (src === settledUrlRef.current) {
      setRendering(false);
      setError("");
    }
  };
  const onBackdropError = (src: string) => {
    if (src === settledUrlRef.current) {
      setRendering(false);
      setError("Render failed — see the app log");
    }
  };

  return (
    <div className="dk-root">
      <header className="dk-bar">
        <button
          className="dk-back"
          onClick={() => {
            markShellLeave("develop");
            onBack();
          }}
        >
          ← Library
        </button>
        <span className="dk-title">Darkroom</span>
        <span className="dk-shelf">
          <button
            className={`chip ${baseVersionId == null && !dirty ? "chip-on" : ""}`}
            onClick={loadAsShot}
            title="Start over from the unedited original"
          >
            As shot
          </button>
          {versions.map((v) => (
            <button
              key={v.id}
              className={`chip ${baseVersionId === v.id ? "chip-on" : ""}`}
              onClick={() => loadVersion(v)}
              title={`Load "${v.name}" into the darkroom`}
            >
              {v.name}
              {baseVersionId === v.id && dirty && <i className="dk-dirty" title="Unsaved changes" />}
            </button>
          ))}
        </span>
        <span className="dk-hint">{rendering ? "rendering…" : ""}</span>
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
        <button
          className="dk-save"
          onClick={() => void saveAsVersion()}
          disabled={!dirty}
          title="Bank these settings as a NEW version (Ctrl+S) — the version you started from is never overwritten, and only settings are stored, never pixels"
        >
          ✓ Save as version
        </button>
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
              onBackdropLoad={onBackdropLoad}
              onBackdropError={onBackdropError}
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
            // Remember what was adopted — it becomes the default save name.
            const picked = proofs.find((c) => c.record === record);
            adoptedLabelRef.current =
              picked && picked.group !== "asShot" ? picked.label : adoptedLabelRef.current;
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
