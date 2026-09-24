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
  commitVersionEdit,
  createVersion,
  setCoverVersion,
  developOpen,
  editRenderUrl,
  editZoneMasses,
  getSetting,
  listVersions,
  onDevelopSource,
  PhotoVersion,
  type DevelopSource,
  setSetting,
  setVersionEdit,
  gotoVersionStep,
  versionHistory,
  type VersionHistory,
  suggestAutoTone,
} from "../../modules/api";
import {
  asLinearRecord,
  ASPECTS,
  forLinearEngine,
  isEngine1Version,
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
import { addUserPreset, allPresets, BUILTIN_PRESETS, type DevelopPreset } from "../../modules/presets";
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
import { useOwnedSubscription } from "../../modules/ownedEvents";
import { markShellLeave, setShellTimingEnabled } from "../../modules/shellTiming";
import { badgeFor, INITIAL_SOURCE, reduceSource, type SourceState } from "./developSource";
import { stageJsonFor } from "./stageJson";
import { describeChange, shouldAmend } from "./history";
import { HistoryPanel } from "./HistoryPanel";
import { Filmstrip } from "./Filmstrip";
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
  onSavedActive,
  neighbours = [],
  strip,
  coverVersionId = null,
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
  /** The active version's settings were saved (autosave, a history step): the shell's copy
   *  follows, so the loupe and the grid show what is saved. */
  onSavedActive?: (editJson: string) => void;
  /** The photos either side, next first: preloaded after this one is ready. */
  neighbours?: number[];
  /** The filmstrip (absent: none shown). Stepping saves first — the view unmounts. */
  strip?: {
    ids: number[];
    names: Map<number, string>;
    covers?: Map<number, string | null>;
    onSelect: (id: number) => void;
  };
  /** The version this photo's Library thumbnail shows (its cover), if any. */
  coverVersionId?: number | null;
}) {
  const neighboursRef = useRef(neighbours);
  neighboursRef.current = neighbours;
  const [working, setWorking] = useState<VersionEdit>(() =>
    parseEdit(initialEditJson ?? undefined),
  );
  // Autosave (user decision 2026-09-24, replacing slice 7's sandbox): every change is saved
  // to the active version as a history step — settings only, never pixels. A photo with
  // no version gets one on its first change. `committed*` is what the version holds (last
  // saved or loaded); the view is keyed per photo, so none of this crosses photos.
  const [versions, setVersions] = useState<PhotoVersion[]>([]);
  // The version on the stage was made on engine 1 (the camera preview): it keeps rendering
  // there, whatever the source, until "Develop with the new engine" forks it (slice 7).
  const [engine1Version, setEngine1Version] = useState<boolean>(
    () => activeVersionId != null && isEngine1Version(parseEdit(initialEditJson ?? undefined)),
  );
  const versionsRef = useRef(versions);
  versionsRef.current = versions;
  const [versionId, setVersionId] = useState<number | null>(activeVersionId);
  const versionIdRef = useRef(versionId);
  versionIdRef.current = versionId;
  const [history, setHistory] = useState<VersionHistory | null>(null);
  const historyRef = useRef(history);
  historyRef.current = history;
  const committedJsonRef = useRef<string>(JSON.stringify(working));
  const committedRecordRef = useRef<VersionEdit>(working);
  /** The last committed step's control and time — the amend window's memory. */
  const lastStepRef = useRef<{ key: string; at: number } | null>(null);
  /** A caller-named change ("Proof: Portra", "Reset") for the next commit. */
  const nextLabelRef = useRef<string | null>(null);
  /** Autosave stays off until the photo's version is resolved. */
  const loadedRef = useRef(false);
  const aliveRef = useRef(true);
  const [saving, setSaving] = useState(false);
  // The proof label last adopted — the default name "New version" gives ("Portra", …).
  const adoptedLabelRef = useRef<string | null>(null);
  const onSavedActiveRef = useRef(onSavedActive);
  onSavedActiveRef.current = onSavedActive;
  const onPickVersionRef = useRef(onPickVersion);
  onPickVersionRef.current = onPickVersion;
  const onChangedRef = useRef(onChanged);
  onChangedRef.current = onChanged;

  /** Take `record` as what the version holds right now (a load, a step): no commit. */
  const adoptCommitted = (record: VersionEdit) => {
    committedJsonRef.current = JSON.stringify(record);
    committedRecordRef.current = record;
    lastStepRef.current = null;
  };

  // Resolve the version on open: the shell's active version when it is this photo's, else
  // the first one (what the classic Develop does), else none until the first change.
  useEffect(() => {
    aliveRef.current = true;
    listVersions(photoId)
      .then((vs) => {
        if (!aliveRef.current) return;
        setVersions(vs);
        const v = vs.find((x) => x.id === activeVersionId) ?? vs[0] ?? null;
        if (v && v.id !== activeVersionId) {
          const record = parseEdit(v.editJson);
          adoptCommitted(record);
          setWorking(record);
          onPickVersionRef.current(v);
        }
        setEngine1Version(v != null && isEngine1Version(parseEdit(v.editJson)));
        setVersionId(v?.id ?? null);
        if (v) {
          versionHistory(v.id)
            .then((h) => aliveRef.current && setHistory(h))
            .catch(() => {});
        }
      })
      .catch(() => setVersions([]))
      .finally(() => {
        loadedRef.current = true;
      });
    return () => {
      aliveRef.current = false;
    };
    // Once per photo: the view is keyed by photoId.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [photoId]);
  const [backdrop, setBackdrop] = useState("");
  // The source (docs/plans/raw-foundation): opening a photo claims the develop session
  // and starts preparing its RAW working image; `develop:source` events move the stage
  // from the camera preview to the RAW, and leaving releases it. The reduced state names
  // the token every render URL carries and the engine a saved record is stamped with.
  const [sourceState, setSourceState] = useState<SourceState>(INITIAL_SOURCE);
  const source: DevelopSource | null = sourceState.source;
  useEffect(() => {
    let alive = true;
    setSourceState(INITIAL_SOURCE);
    // Neighbours are read at open time: they preload in the background, and a change in
    // them alone (the list re-sorted around this photo) is not a reason to reopen it.
    developOpen(photoId, neighboursRef.current)
      .then((s) => {
        if (!alive) return;
        // Dev evidence for the badge (docs/plans/raw-foundation): the exact payload.
        console.debug(`[develop] open photo=${photoId} ${JSON.stringify(s)}`);
        setSourceState((prev) => reduceSource(prev, s, photoId));
      })
      .catch((e) => {
        if (alive) console.debug(`[develop] open failed photo=${photoId}: ${String(e)}`);
      });
    return () => {
      alive = false;
    };
  }, [photoId]);
  useOwnedSubscription(
    () =>
      onDevelopSource((e) => {
        console.debug(`[develop] event ${JSON.stringify(e)}`);
        setSourceState((prev) => reduceSource(prev, e, photoId));
      }),
    [photoId],
  );
  // Leaving Develop releases the working images — DevelopSurface owns that, so stepping
  // to the next photo (a remount of this view) keeps the preloaded neighbours.
  // An engine-1 version renders from the preview even with the RAW resident: no token in
  // its URLs, no engine-2 stamp on its saves.
  const sourceToken = engine1Version ? undefined : sourceState.token;
  const sourceTokenRef = useRef(sourceToken);
  sourceTokenRef.current = sourceToken;
  const engineRef = useRef<1 | 2>(engine1Version ? 1 : sourceState.engine);
  engineRef.current = engine1Version ? 1 : sourceState.engine;
  const cameraEvRef = useRef(sourceState.cameraEv);
  cameraEvRef.current = sourceState.cameraEv;
  /** The record as saved: stamped with the engine that rendered it (engine 1 = absent).
   *  A record becoming engine 2 here also gets engine 2's default display transform and
   *  this frame's camera match. */
  const stamped = (record: VersionEdit): VersionEdit =>
    engineRef.current === 2 ? asLinearRecord(record, cameraEvRef.current) : record;
  /** Put the working state on the loupe as the print: the stamped record and, on the RAW
   *  engine, the session token, so the loupe renders the stage's own pixels. */
  const broadcastPrint = () =>
    broadcastPhoto(photoId, loupeJson(JSON.stringify(stamped(workingRef.current))), sourceTokenRef.current ?? null);
  /** The sensor-clipping layer: the stage's geometry at the settled size (tone does not
   *  move it, so slider drags do not refetch it). */
  const [showClipping, setShowClipping] = useState(false);
  const clipUrl = (geometry: VersionEdit, perspectiveOff: boolean) =>
    editRenderUrl(photoId, stageJsonFor(stamped(geometry), perspectiveOff), {
      maxEdge: PREVIEW_MAX,
      source: sourceToken,
      clip: true,
    });
  /** A variant's render (proof sheet, duel) from the stage's own source and engine. */
  const variantUrl = (record: VersionEdit, maxEdge: number): string =>
    editRenderUrl(photoId, JSON.stringify(stamped(record)), { maxEdge, source: sourceToken });
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
        broadcastPrint();
      }
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, [photoId]);

  // Leaving the photo hands the loupe back to what the version holds — what the shell
  // shows. (Pending changes are flushed on the same unmount, below.)
  useEffect(
    () => () => {
      // No token: leaving re-keys (a step) or releases (Develop closed) this session's
      // image, so the loupe renders the record from an offline load instead of a 404.
      if (printOnLoupeRef.current) broadcastPhoto(photoId, loupeJson(JSON.stringify(stamped(workingRef.current))), null);
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
    // On the RAW engine the fragment is measured on the working image as-shot, so it waits
    // for the token; until then (and on engine 1) it reads the camera preview.
    const base = sourceToken ? JSON.stringify(stamped({})) : undefined;
    suggestAutoTone(photoId, sourceToken, base)
      .then((j) => alive && setAutoFragment(parseEdit(j)))
      .catch(() => alive && setAutoFragment({}));
    return () => {
      alive = false;
    };
    // `stamped` reads refs; the token names the source and changes with it.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [photoId, sourceToken]);

  // "Save as preset": the current look (never the framing) under a name, into the user
  // presets the proof sheet and the preset browser deal from.
  const [presetName, setPresetName] = useState<string | null>(null);
  const [notice, setNotice] = useState("");
  const savePreset = async () => {
    const name = (presetName ?? "").trim();
    if (!name) return;
    try {
      await addUserPreset(name, workingRef.current);
      setPresets(await allPresets());
      setPresetName(null);
      setNotice(`Saved preset “${name}”`);
      setTimeout(() => setNotice(""), 3000);
    } catch (e) {
      setError(String(e));
    }
  };

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
      await setVersionEdit(id, JSON.stringify(stamped(record)));
      setVersions(await listVersions(photoId));
      onChanged();
      return name;
    } catch {
      return null;
    }
  };

  // ── Autosave: every settled change is a history step on the active version ──
  /** Commit the working state if it differs from what the version holds. Serialized by
   *  `flush` — one commit at a time, in order. */
  const commitNow = async (): Promise<void> => {
    const record = workingRef.current;
    const json = JSON.stringify(record);
    if (!loadedRef.current || json === committedJsonRef.current) return;
    const change = describeChange(committedRecordRef.current, record, nextLabelRef.current ?? undefined);
    nextLabelRef.current = null;
    const before = { json: committedJsonRef.current, record: committedRecordRef.current };
    committedJsonRef.current = json;
    committedRecordRef.current = record;
    if (aliveRef.current) setSaving(true);
    try {
      let vid = versionIdRef.current;
      let created: number | null = null;
      if (vid == null) {
        vid = await createVersion(photoId, `Version ${versionsRef.current.length + 1}`);
        created = vid;
        versionIdRef.current = vid;
        if (aliveRef.current) setVersionId(vid);
      }
      const h = created == null ? historyRef.current : null;
      const tip = h?.steps[h.steps.length - 1]?.seq;
      const atTip = h?.head != null && h.head === tip;
      const now = Date.now();
      const saved = JSON.stringify(stamped(record));
      const next = await commitVersionEdit(vid, saved, change.label, shouldAmend(lastStepRef.current, change, now, atTip));
      lastStepRef.current = { key: change.key, at: now };
      historyRef.current = next;
      if (aliveRef.current) setHistory(next);
      onSavedActiveRef.current?.(saved);
      if (created != null) {
        const vs = await listVersions(photoId);
        if (aliveRef.current) setVersions(vs);
        const v = vs.find((x) => x.id === created);
        if (v) onPickVersionRef.current({ ...v, editJson: saved });
        onChangedRef.current();
      }
    } catch (e) {
      committedJsonRef.current = before.json;
      committedRecordRef.current = before.record;
      if (aliveRef.current) setError(`Autosave failed: ${String(e)}`);
    } finally {
      if (aliveRef.current) setSaving(false);
    }
  };
  const commitRef = useRef(commitNow);
  commitRef.current = commitNow;
  const chainRef = useRef<Promise<void>>(Promise.resolve());
  /** Commit now (after any commit already running). Returns when it is saved. */
  const flush = () => {
    chainRef.current = chainRef.current.then(() => commitRef.current()).catch(() => {});
    return chainRef.current;
  };
  // A change settles after a short quiet: one step per adjustment, not per drag frame.
  useEffect(() => {
    if (!loadedRef.current || JSON.stringify(working) === committedJsonRef.current) return;
    const t = setTimeout(() => void flush(), 600);
    return () => clearTimeout(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [working]);
  // Leaving the photo (the filmstrip, Library, a catalog switch) saves what is pending.
  useEffect(
    () => () => {
      void flush();
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [],
  );

  /** Go to history step `seq`: pending changes are saved first, so nothing is lost. */
  const gotoStep = async (seq: number) => {
    await flush();
    const vid = versionIdRef.current;
    if (vid == null) return;
    try {
      const [json, h] = await gotoVersionStep(vid, seq);
      const record = parseEdit(json);
      adoptCommitted(record);
      historyRef.current = h;
      setHistory(h);
      setWorking(record);
      onSavedActiveRef.current?.(json);
    } catch (e) {
      setError(String(e));
    }
  };
  const stepBy = (delta: -1 | 1) => {
    const h = historyRef.current;
    if (!h || h.head == null) return;
    const i = h.steps.findIndex((st) => st.seq === h.head);
    const target = h.steps[i + delta];
    if (target) void gotoStep(target.seq);
  };

  // The cover: which version the Library shows for this photo. Toggled for the version
  // being edited; the grid follows on its next refresh (now, and on leaving Develop).
  const [cover, setCover] = useState<number | null>(coverVersionId);
  const toggleCover = async () => {
    const vid = versionIdRef.current;
    if (vid == null) return;
    await flush();
    try {
      const next = cover === vid ? null : vid;
      await setCoverVersion(photoId, next);
      setCover(next);
      onChanged();
    } catch (e) {
      setError(String(e));
    }
  };

  /** Fork the current settings into a new version and continue there. */
  const newVersion = async () => {
    await flush();
    try {
      const name = adoptedLabelRef.current ?? `Version ${versionsRef.current.length + 1}`;
      const json = JSON.stringify(stamped(workingRef.current));
      const id = await createVersion(photoId, name);
      await setVersionEdit(id, json);
      const vs = await listVersions(photoId);
      setVersions(vs);
      setVersionId(id);
      versionIdRef.current = id;
      historyRef.current = null;
      setHistory(null);
      adoptCommitted(workingRef.current);
      adoptedLabelRef.current = null;
      onChanged();
      const created = vs.find((v) => v.id === id);
      if (created) onPickVersion({ ...created, editJson: json });
    } catch (e) {
      setError(String(e));
    }
  };

  /** "Develop with the new engine": an engine-1 version's framing as a fresh engine-2
   *  version (tone and look reset — an old EV is not a new EV), which the stage switches
   *  to. The engine-1 version stays as it was. */
  const developOnNewEngine = async () => {
    await flush();
    try {
      const from = versionsRef.current.find((v) => v.id === versionIdRef.current);
      const record = forLinearEngine(workingRef.current, cameraEvRef.current);
      const json = JSON.stringify(record);
      const id = await createVersion(photoId, `${from?.name ?? "Version"} (RAW)`);
      await setVersionEdit(id, json);
      const vs = await listVersions(photoId);
      setVersions(vs);
      setEngine1Version(false);
      setVersionId(id);
      versionIdRef.current = id;
      historyRef.current = null;
      setHistory(null);
      adoptCommitted(record);
      setWorking(record);
      adoptedLabelRef.current = null;
      onChanged();
      const created = vs.find((v) => v.id === id);
      if (created) onPickVersion({ ...created, editJson: json });
    } catch (e) {
      setError(String(e));
    }
  };

  /** Shelf: switch to a version (or the original, `null`) — pending changes saved first. */
  const switchVersion = async (target: PhotoVersion | null) => {
    await flush();
    const vs = await listVersions(photoId).catch(() => versionsRef.current);
    setVersions(vs);
    const v = target ? vs.find((x) => x.id === target.id) ?? target : null;
    const record = parseEdit(v?.editJson ?? undefined);
    adoptCommitted(record);
    setWorking(record);
    setEngine1Version(v != null && isEngine1Version(record));
    setVersionId(v?.id ?? null);
    versionIdRef.current = v?.id ?? null;
    historyRef.current = null;
    setHistory(null);
    if (v) {
      versionHistory(v.id)
        .then((h) => {
          if (versionIdRef.current === v.id && aliveRef.current) {
            historyRef.current = h;
            setHistory(h);
          }
        })
        .catch(() => {});
    }
    adoptedLabelRef.current = null;
    onPickVersion(v);
  };

  // Ctrl+Z / Ctrl+Shift+Z (and Ctrl+Y) walk the history; Ctrl+S saves what is pending now.
  // Text fields keep their own undo.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!(e.ctrlKey || e.metaKey)) return;
      const t = e.target as HTMLElement | null;
      const typing =
        t?.tagName === "TEXTAREA" ||
        (t?.tagName === "INPUT" && !["range", "checkbox", "radio", "button"].includes((t as HTMLInputElement).type));
      const k = e.key.toLowerCase();
      if (k === "s") {
        e.preventDefault();
        void flush();
      } else if (!typing && k === "z") {
        e.preventDefault();
        stepBy(e.shiftKey ? 1 : -1);
      } else if (!typing && k === "y") {
        e.preventDefault();
        stepBy(1);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // Reads refs only.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const togglePrintOnLoupe = () => {
    const next = !printOnLoupe;
    setPrintOnLoupe(next);
    printOnLoupeRef.current = next;
    setSetting(PRINT_ON_LOUPE_KEY, next ? "1" : "0").catch(() => {});
    if (next) {
      // Opening (or focusing) the window is part of turning the print on.
      void openLoupeWindow().catch(() => {});
      broadcastPrint();
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
    const engineWorking = stamped(working);
    const fullJson = JSON.stringify(engineWorking);
    const stageJson = stageJsonFor(engineWorking, perspectiveMode);
    const sinceFast = Date.now() - lastFastRef.current;
    const fastTimer = setTimeout(
      () => {
        lastFastRef.current = Date.now();
        const url = editRenderUrl(photoId, stageJson, { maxEdge: PREVIEW_FAST, source: sourceToken });
        awaitPaint(startSample(seq, "fast"), url);
        setBackdrop(url);
      },
      sinceFast > 90 ? 0 : 90 - sinceFast,
    );
    const settleTimer = setTimeout(() => {
      const url = editRenderUrl(photoId, stageJson, { maxEdge: PREVIEW_MAX, source: sourceToken });
      settledUrlRef.current = url;
      awaitPaint(startSample(seq, "settled"), url);
      setBackdrop(url);
      editZoneMasses(photoId, fullJson, sourceToken)
        .then((m) => {
          if (renderSeq.current === seq) setMasses(m);
        })
        .catch(() => {
          // Masses are a cosmetic overlay on the strip — a failure leaves the last fill.
        });
      // The loupe print rides the settle: one settled state, one broadcast.
      if (printOnLoupeRef.current) broadcastPhoto(photoId, loupeJson(fullJson), sourceToken ?? null);
    }, 250);
    return () => {
      clearTimeout(fastTimer);
      clearTimeout(settleTimer);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [photoId, working, perspectiveMode, sourceToken]);
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
            className={`chip ${versionId == null ? "chip-on" : ""}`}
            onClick={() => void switchVersion(null)}
            title="The unedited original. Changing anything here starts a new version."
          >
            Original
          </button>
          {versions.map((v) => (
            <button
              key={v.id}
              className={`chip ${versionId === v.id ? "chip-on" : ""}`}
              onClick={() => void switchVersion(v)}
              title={`Edit "${v.name}" — every change is saved to it, with history`}
            >
              {v.name}
            </button>
          ))}
        </span>
        <span className="dk-hint">{saving ? "saving…" : rendering ? "rendering…" : ""}</span>
        {source && engine1Version && sourceState.token ? (
          <>
            <span
              className="dk-source dk-source-warn"
              title="This version was developed on the camera preview. It keeps rendering exactly as it was saved; the RAW is ready for a new version."
            >
              camera preview · this version's engine
            </span>
            <button
              className="dk-loupe-toggle"
              onClick={() => void developOnNewEngine()}
              title="Start a new version on the RAW with this version's framing (tone and look start fresh — the engines read sliders differently)"
            >
              Develop with the new engine
            </button>
          </>
        ) : (
          source && (
            <span className={`dk-source dk-source-${badgeFor(source).tone}`} title={badgeFor(source).title}>
              {badgeFor(source).label}
            </span>
          )
        )}
        {sourceToken && (
          <button
            className={`dk-loupe-toggle ${showClipping ? "on" : ""}`}
            onClick={() => setShowClipping((v) => !v)}
            title="Mark where the sensor itself clipped — the only white no slider can bring back"
          >
            ◩ Clipping
          </button>
        )}
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
        {versionId != null && (
          <button
            className={`dk-loupe-toggle ${cover === versionId ? "on" : ""}`}
            onClick={() => void toggleCover()}
            title={
              cover === versionId
                ? "The Library shows this version for the photo — click to show the original again"
                : "Show this version's look as the photo's thumbnail in the Library"
            }
          >
            {cover === versionId ? "★ Cover" : "☆ Use as cover"}
          </button>
        )}
        <button
          className="dk-save"
          onClick={() => void newVersion()}
          title="Copy these settings into a new version and keep editing there. Changes are saved automatically; only settings are stored, never pixels."
        >
          + New version
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
              stageLayer={
                showClipping && sourceToken ? (
                  <img
                    className="dk-clip-layer"
                    src={clipUrl(
                      { crop: working.crop, straighten: working.straighten, perspective: working.perspective },
                      perspectiveMode,
                    )}
                    alt=""
                    draggable={false}
                  />
                ) : null
              }
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
            {presetName == null ? (
              <button
                className="dk-act"
                onClick={() => setPresetName("")}
                title="Save this look — tone and effects, not the crop or straighten — as a preset"
              >
                ☆ Save as preset
              </button>
            ) : (
              <span className="dk-preset-name">
                <input
                  autoFocus
                  value={presetName}
                  placeholder="Preset name"
                  aria-label="Preset name"
                  onChange={(e) => setPresetName(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") void savePreset();
                    if (e.key === "Escape") setPresetName(null);
                  }}
                />
                <button className="dk-act dk-act-primary" disabled={!presetName.trim()} onClick={() => void savePreset()}>
                  Save
                </button>
                <button className="dk-act" onClick={() => setPresetName(null)}>
                  Cancel
                </button>
              </span>
            )}
            {notice && <span className="dk-notice">{notice}</span>}
            <button
              className="dk-act"
              onClick={() => {
                nextLabelRef.current = "Reset";
                setWorking({});
              }}
              title="Back to as shot — clears every adjustment, framing included"
            >
              Reset
            </button>
          </div>
          {strip && strip.ids.length > 1 && (
            <Filmstrip
              ids={strip.ids}
              names={strip.names}
              covers={strip.covers}
              currentId={photoId}
              onSelect={strip.onSelect}
              keysDisabled={duelOpen || proofs != null}
            />
          )}
        </div>
        <aside className="dk-rail">
          <HistoryPanel history={history} onGoto={(seq) => void gotoStep(seq)} />
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
          candidates={proofs}
          renderUrl={variantUrl}
          onAdopt={(record) => {
            // Remember what was adopted — the history step's name, and the default name a
            // "New version" gets.
            const picked = proofs.find((c) => c.record === record);
            if (picked && picked.group !== "asShot") {
              adoptedLabelRef.current = picked.label;
              nextLabelRef.current = `Proof: ${picked.label}`;
            }
            setWorking(record);
            setProofs(null);
          }}
          onClose={() => setProofs(null)}
        />
      )}
      {duelOpen && (
        <DuelView
          working={working}
          renderUrl={variantUrl}
          onApply={(record) => {
            nextLabelRef.current = "Duel pick";
            setWorking(record);
          }}
          onFork={forkVersion}
          onClose={() => setDuelOpen(false)}
        />
      )}
    </div>
  );
}
