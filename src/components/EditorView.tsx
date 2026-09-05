import { useEffect, useRef, useState } from "react";
import {
  createVersion,
  getSetting,
  listVersions,
  PhotoVersion,
  renderEdit,
  setVersionEdit,
} from "../modules/api";
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
} from "../modules/editing";
import { DevelopPreset } from "../modules/presets";
import {
  EditStage,
  EffectsRail,
  GeometryRail,
  OVERLAY_KEY,
  persistOverlay,
  ToneRail,
} from "./EditControls";
import { PresetBrowser } from "./PresetBrowser";

// The "Develop" view (H5b): a full-window darkroom for one photo version — crop (with
// social aspect presets, drag-to-move, corner-resize) + tone, previewed live by
// re-rendering the cached proxy in Rust. Never touches the original. Edits auto-save to
// the active version. See docs/editing.md.
//
// The stage and slider-rail widgets are shared with the Darkroom (EditControls.tsx,
// darkroom slice 6): this view owns the edit state and policy, the widgets render it.

const PREVIEW_MAX = 1400;

export function EditorView({
  photoId,
  photoW,
  photoH,
  activeVersionId,
  onPickVersion,
  onSavedActive,
  onChanged,
  onBack,
}: {
  photoId: number;
  /** Original pixel dimensions, for showing the crop size in pixels. */
  photoW: number | null;
  photoH: number | null;
  activeVersionId: number | null;
  onPickVersion: (v: PhotoVersion | null) => void;
  onSavedActive: (editJson: string) => void;
  onChanged: () => void;
  onBack: () => void;
}) {
  const [versions, setVersions] = useState<PhotoVersion[]>([]);
  const [tone, setTone] = useState<Tone>({ ...ZERO_TONE });
  const [look, setLook] = useState<Look>({ ...ZERO_LOOK });
  const [crop, setCrop] = useState<Crop | null>(null);
  const [aspect, setAspect] = useState<string>("Original");
  const [straighten, setStraighten] = useState(0); // degrees
  const [straightenMode, setStraightenMode] = useState(false); // drawing the level line
  const [perspective, setPerspective] = useState<Perspective | null>(null);
  const [perspectiveMode, setPerspectiveMode] = useState(false); // dragging the corners
  const [overlay, setOverlay] = useState<CropOverlay>("thirds");
  const [backdrop, setBackdrop] = useState<string>("");
  // Before/After toggle: "before" shows a separately cached unedited render.
  const [showBefore, setShowBefore] = useState(false);
  const [beforeBackdrop, setBeforeBackdrop] = useState<string>("");
  const [imgDims, setImgDims] = useState<{ w: number; h: number } | null>(null);
  const [error, setError] = useState("");
  const ready = useRef(false); // gate auto-save until the version's edit is loaded

  const current = versions.find((v) => v.id === activeVersionId) ?? null;
  // "Original" is selected (read-only) when no version is active but versions exist.
  const viewingOriginal = activeVersionId == null && versions.length > 0;

  // photoW/photoH are the catalog's SENSOR (unrotated) dimensions; the preview we draw
  // (imgDims) is already oriented. For a portrait-shot photo the two disagree in
  // orientation, which would draw the crop box and px readout on the wrong axes (a 4:5
  // crop looking ~2:5). Swap the full-res dims to match the displayed orientation.
  const orientedDims =
    photoW && photoH
      ? imgDims && imgDims.h > imgDims.w !== photoH > photoW
        ? { w: photoH, h: photoW }
        : { w: photoW, h: photoH }
      : null;

  // Crop size in pixels of the (oriented) original.
  const cropPx = orientedDims
    ? {
        w: Math.round((crop?.w ?? 1) * orientedDims.w),
        h: Math.round((crop?.h ?? 1) * orientedDims.h),
      }
    : null;

  // Aspect math (fitCrop, corner-resize) uses the oriented full-res dimensions so a 4:5
  // crop is 4:5 in the *output* (matching the px readout), not in the slightly-off proxy.
  const srcDims = orientedDims ?? imgDims;

  // Load the persisted overlay preference once.
  useEffect(() => {
    getSetting(OVERLAY_KEY).then((v) => {
      if (v === "none" || v === "thirds" || v === "phi" || v === "golden") setOverlay(v);
    });
  }, []);

  // On entering a photo: make sure there's something to edit. Auto-create the first
  // version when none exists, and otherwise land on a version (not the empty state) when
  // nothing valid is active. Run once per photo (the ref guard is StrictMode-safe — refs
  // persist across the dev double-invoke — so we never create two versions). Picking
  // "Original" afterwards doesn't re-run this, so it stays put.
  const inited = useRef<number | null>(null);
  useEffect(() => {
    if (inited.current === photoId) return;
    inited.current = photoId;
    (async () => {
      let vs = await listVersions(photoId).catch(() => []);
      if (vs.length === 0) {
        const id = await createVersion(photoId, "Version 1");
        vs = await listVersions(photoId).catch(() => []);
        setVersions(vs);
        onChanged();
        onPickVersion(vs.find((v) => v.id === id) ?? null);
        return;
      }
      setVersions(vs);
      if (activeVersionId == null || !vs.some((v) => v.id === activeVersionId)) {
        onPickVersion(vs[0]);
      }
    })();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [photoId]);

  // Load the active version's edit record when it changes (keyed on id so pushing back a
  // saved editJson doesn't re-init mid-edit). The stage remounts on this key too, which
  // resets its zoom/pan to fit — same behaviour the view always had.
  useEffect(() => {
    ready.current = false;
    const init = parseEdit(current?.editJson);
    setTone({ ...ZERO_TONE, ...init.tone, wb: { ...ZERO_TONE.wb, ...init.tone?.wb } });
    setLook({
      ...ZERO_LOOK,
      bw: init.bw,
      split: init.split,
      grain: init.grain,
      fade: init.fade ?? 0,
      vignette: init.vignette ?? 0,
      lut: init.lut,
    });
    setCrop(init.crop ?? null);
    setAspect(init.crop?.aspect ?? "Original");
    setStraighten(init.straighten ?? 0);
    setStraightenMode(false);
    setPerspective(init.perspective ?? null);
    setPerspectiveMode(false);
    // Reset before/after state when switching versions.
    setShowBefore(false);
    setBeforeBackdrop("");
    // Allow auto-save on the next tick once state is set.
    const t = setTimeout(() => {
      ready.current = true;
    }, 0);
    return () => clearTimeout(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeVersionId]);

  // Live preview. With a version active, the backdrop includes tone + straighten (the crop
  // is an overlay). For "Original" (no active version) it renders the unedited proxy.
  useEffect(() => {
    let cancelled = false;
    const editJson =
      activeVersionId == null
        ? "{}"
        : JSON.stringify({
            tone,
            straighten,
            // While the handles are up the backdrop stays un-rectified: the handles are
            // aimed at the *original's* corners, and warping underneath them would move
            // the very thing being aimed at. The warp reappears on leaving the mode.
            perspective: perspectiveMode ? undefined : (perspective ?? undefined),
            ...lookFields(look),
          });
    const t = setTimeout(() => {
      renderEdit(photoId, editJson, PREVIEW_MAX)
        .then((url) => !cancelled && setBackdrop(url))
        .catch((e) => !cancelled && setError(String(e)));
    }, 120);
    return () => {
      cancelled = true;
      clearTimeout(t);
    };
  }, [photoId, tone, look, straighten, perspective, perspectiveMode, activeVersionId]);

  // Fetch the "before" (unedited) render the first time the Before toggle is activated.
  // Cache it in beforeBackdrop so we only fetch once per version session.
  useEffect(() => {
    if (!showBefore || beforeBackdrop || activeVersionId == null) return;
    let cancelled = false;
    renderEdit(photoId, "{}", PREVIEW_MAX)
      .then((url) => { if (!cancelled) setBeforeBackdrop(url); })
      .catch(() => {});
    return () => { cancelled = true; };
  }, [showBefore, beforeBackdrop, photoId, activeVersionId]);

  // Auto-save edits to the active version (debounced).
  useEffect(() => {
    if (!current || !ready.current) return;
    const editJson = JSON.stringify({
      crop: crop ?? undefined,
      tone,
      perspective: perspective ?? undefined,
      straighten: straighten || undefined,
      ...lookFields(look),
    });
    const t = setTimeout(() => {
      setVersionEdit(current.id, editJson)
        .then(() => {
          onSavedActive(editJson);
          onChanged();
        })
        .catch((e) => setError(String(e)));
    }, 250);
    return () => clearTimeout(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tone, look, crop, straighten, perspective]);

  const ratioFor = (label: string): number | null => {
    if (label === "Original" || label === "Free") return null;
    return ASPECTS.find((a) => a.label === label)?.ratio ?? null;
  };

  const applyAspect = (label: string) => {
    setAspect(label);
    if (label === "Original") {
      setCrop(null);
      return;
    }
    if (label === "Free") {
      // Keep the current rect, or start at a centred 80% box.
      setCrop((c) => c ?? { x: 0.1, y: 0.1, w: 0.8, h: 0.8, aspect: "Free" });
      return;
    }
    const ratio = ratioFor(label);
    if (ratio == null || !srcDims) return;
    const fit = fitCrop(srcDims.w, srcDims.h, ratio, 1);
    setCrop({ ...fit, aspect: label });
  };

  const changeOverlay = (key: CropOverlay) => {
    setOverlay(key);
    persistOverlay(key);
  };

  // Set the straighten angle and auto-crop to the largest inscribed rectangle so the
  // rotation's black corners stay out of frame. ~0° clears back to the full frame.
  const applyStraighten = (deg: number) => {
    const d = clampStraighten(deg);
    setStraighten(d);
    setAspect("Original");
    if (Math.abs(d) < 0.05) {
      setCrop(null);
    } else if (srcDims) {
      setCrop(inscribedCrop(srcDims.w, srcDims.h, d));
    }
  };

  // The level line was released: apply the delta (null = too short a line to mean it).
  const handleLevel = (deltaDeg: number | null) => {
    setStraightenMode(false);
    if (deltaDeg != null) applyStraighten(straighten + deltaDeg);
  };

  // Apply a one-shot preset: replace the whole *look* (tone + film-look fields, merged
  // over zero defaults so untouched keys reset). Crop and straighten are untouched — a
  // preset changes the look, never the framing.
  const applyPreset = (preset: DevelopPreset) => {
    const e = preset.edit;
    setTone({ ...ZERO_TONE, ...e.tone, wb: { ...ZERO_TONE.wb, ...e.tone?.wb } });
    setLook({
      ...ZERO_LOOK,
      bw: e.bw,
      split: e.split,
      grain: e.grain,
      fade: e.fade ?? 0,
      vignette: e.vignette ?? 0,
      lut: e.lut,
    });
  };

  // Reset all edits: tone/look back to zero, no crop, Original aspect, no straighten,
  // no perspective.
  const resetAll = () => {
    setTone({ ...ZERO_TONE });
    setLook({ ...ZERO_LOOK });
    setCrop(null);
    setAspect("Original");
    setStraighten(0);
    setStraightenMode(false);
    setPerspective(null);
    setPerspectiveMode(false);
  };

  // Start (or resume) aiming the four corners. Any crop is dropped: crop fractions are
  // relative to the *rectified* frame, and that frame is about to change shape, so
  // keeping the old box would silently reframe the photo.
  const startPerspective = () => {
    setPerspective((p) => p ?? { ...DEFAULT_QUAD });
    setPerspectiveMode(true);
    setStraightenMode(false);
    setCrop(null);
    setAspect("Original");
  };

  const clearPerspective = () => {
    setPerspective(null);
    setPerspectiveMode(false);
    setCrop(null);
    setAspect("Original");
  };

  const addVersion = async () => {
    const id = await createVersion(photoId, `Version ${versions.length + 1}`);
    const next = await listVersions(photoId);
    setVersions(next);
    onChanged();
    const created = next.find((v) => v.id === id) ?? null;
    onPickVersion(created);
  };

  // The image to show in the stage: Before shows the unedited cache; After shows backdrop.
  const displayedBackdrop = showBefore ? beforeBackdrop : backdrop;

  return (
    <div className="develop">
      {/* Top bar: back button + version chips left; Reset + New version right */}
      <div className="develop-bar">
        <button className="btn-ghost" onClick={onBack}>
          ‹ Library
        </button>
        <span className="develop-versions">
          <button
            className={`chip ${activeVersionId == null ? "chip-on" : ""}`}
            onClick={() => onPickVersion(null)}
            title="View the unedited original (read-only)"
          >
            Original
          </button>
          {versions.map((v) => (
            <button
              key={v.id}
              className={`chip ${activeVersionId === v.id ? "chip-on" : ""}`}
              onClick={() => onPickVersion(v)}
            >
              {v.name}
            </button>
          ))}
        </span>
        {/* Right-side actions */}
        <div className="develop-bar-actions">
          {current && (
            <button className="btn-ghost" onClick={resetAll} title="Reset all edits to zero">
              Reset
            </button>
          )}
          <button className="btn-primary" onClick={addVersion}>
            + New version
          </button>
        </div>
      </div>

      {current ? (
        <div className="develop-body">
          <EditStage
            key={activeVersionId ?? "none"}
            backdrop={displayedBackdrop}
            loading={<div className="editor-loading">Rendering…</div>}
            crop={crop}
            setCrop={setCrop}
            ratio={ratioFor(aspect)}
            srcDims={srcDims}
            cropPx={cropPx}
            overlay={overlay}
            perspective={perspective}
            setPerspective={setPerspective}
            perspectiveMode={perspectiveMode}
            straightenMode={straightenMode}
            onLevel={handleLevel}
            showBefore={showBefore}
            onImgDims={setImgDims}
            topLeft={
              <div className="before-after-toggle">
                <button
                  className={`before-after-btn${!showBefore ? " before-after-active" : ""}`}
                  onClick={() => setShowBefore(false)}
                >
                  After
                </button>
                <button
                  className={`before-after-btn${showBefore ? " before-after-active" : ""}`}
                  onClick={() => setShowBefore(true)}
                >
                  Before
                </button>
              </div>
            }
          />

          <div className="editor-controls">
            {/* Histogram */}
            {backdrop && (
              <div className="develop-histogram-card">
                <Histogram src={backdrop} />
              </div>
            )}

            {/* Preset browser (library + user presets, live thumbnails) */}
            <PresetBrowser
              photoId={photoId}
              currentTone={tone}
              currentLook={look}
              onApply={applyPreset}
            />

            <ToneRail tone={tone} onTone={setTone} />

            <EffectsRail look={look} onLook={setLook} onError={setError} />

            <GeometryRail
              aspect={aspect}
              onAspect={applyAspect}
              overlay={overlay}
              onOverlay={changeOverlay}
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

            {error && <div className="modal-error">{error}</div>}
            <div className="editor-hint" style={{ color: "var(--mute)" }}>
              Changes save to "{current.name}" automatically.
            </div>
          </div>
        </div>
      ) : viewingOriginal ? (
        <div className="develop-body">
          <div className="editor-stage editor-original-stage">
            {backdrop ? (
              <img
                className="editor-original-img"
                src={backdrop}
                alt=""
                onLoad={(e) =>
                  setImgDims({ w: e.currentTarget.naturalWidth, h: e.currentTarget.naturalHeight })
                }
              />
            ) : (
              <div className="editor-loading">Rendering…</div>
            )}
          </div>
          <div className="editor-controls">
            <div className="develop-section">
              <div className="panel-head develop-group-label">Original</div>
              <div className="editor-hint">
                Read-only — the original is never changed. Pick a version above to edit, or
                start a new one from here.
              </div>
              <button className="chip" onClick={addVersion} style={{ marginTop: 8 }}>
                + New version from here
              </button>
            </div>
          </div>
        </div>
      ) : (
        <div className="develop-empty">Creating a version…</div>
      )}
    </div>
  );
}

// RGB histogram of the (toned) preview, computed in-browser from the data-URL image.
// Updates whenever the backdrop re-renders (i.e. on tone changes).
function Histogram({ src }: { src: string }) {
  const ref = useRef<HTMLCanvasElement>(null);
  useEffect(() => {
    if (!src) return;
    let cancelled = false;
    const img = new Image();
    img.onload = () => {
      if (cancelled) return;
      const sw = 256;
      const sh = Math.max(1, Math.round((256 * img.height) / (img.width || 1)));
      const off = document.createElement("canvas");
      off.width = sw;
      off.height = sh;
      const octx = off.getContext("2d");
      const cv = ref.current;
      if (!octx || !cv) return;
      octx.drawImage(img, 0, 0, sw, sh);
      const data = octx.getImageData(0, 0, sw, sh).data;
      const r = new Array(256).fill(0);
      const g = new Array(256).fill(0);
      const b = new Array(256).fill(0);
      for (let i = 0; i < data.length; i += 4) {
        r[data[i]]++;
        g[data[i + 1]]++;
        b[data[i + 2]]++;
      }
      const ctx = cv.getContext("2d");
      if (!ctx) return;
      const { width: cw, height: ch } = cv;
      ctx.clearRect(0, 0, cw, ch);
      const max = Math.max(1, ...r, ...g, ...b);
      ctx.globalCompositeOperation = "lighter";
      const draw = (h: number[], color: string) => {
        ctx.fillStyle = color;
        for (let x = 0; x < 256; x++) {
          const bh = (h[x] / max) * ch;
          ctx.fillRect((x / 256) * cw, ch - bh, cw / 256 + 0.5, bh);
        }
      };
      draw(r, "rgba(255,80,80,0.55)");
      draw(g, "rgba(80,220,90,0.55)");
      draw(b, "rgba(90,130,255,0.55)");
      ctx.globalCompositeOperation = "source-over";
    };
    img.src = src;
    return () => {
      cancelled = true;
    };
  }, [src]);
  return <canvas ref={ref} width={256} height={80} className="histogram" />;
}
