import { useEffect, useRef, useState } from "react";
import {
  createVersion,
  getSetting,
  importLut,
  listLuts,
  listVersions,
  PhotoVersion,
  pickFile,
  renderEdit,
  setSetting,
  setVersionEdit,
} from "../modules/api";
import {
  ASPECTS,
  Bw,
  BW_FILTERS,
  clampStraighten,
  Crop,
  CropOverlay,
  DEFAULT_QUAD,
  fitCrop,
  GOLDEN_SPIRAL_PATH,
  inscribedCrop,
  levelFromLine,
  Look,
  lookFields,
  OVERLAY_LINES,
  OVERLAYS,
  parseEdit,
  Perspective,
  QUAD_CORNERS,
  QuadCorner,
  Split,
  STRAIGHTEN_MAX,
  Tone,
  ZERO_LOOK,
  ZERO_TONE,
} from "../modules/editing";
import { DevelopPreset } from "../modules/presets";
import { PresetBrowser } from "./PresetBrowser";

// The "Develop" view (H5b): a full-window darkroom for one photo version — crop (with
// social aspect presets, drag-to-move, corner-resize) + tone, previewed live by
// re-rendering the cached proxy in Rust. Never touches the original. Edits auto-save to
// the active version. See docs/editing.md.

const PREVIEW_MAX = 1400;
const OVERLAY_KEY = "editor.crop_overlay";

// Tone sliders split into groups matching the mockup.
const TONE_SLIDERS: { key: keyof Omit<Tone, "wb">; label: string; min: number; max: number }[] = [
  { key: "ev", label: "Exposure", min: -3, max: 3 },
  { key: "contrast", label: "Contrast", min: -1, max: 1 },
  { key: "highlights", label: "Highlights", min: -1, max: 1 },
  { key: "shadows", label: "Shadows", min: -1, max: 1 },
  { key: "whites", label: "Whites", min: -1, max: 1 },
  { key: "blacks", label: "Blacks", min: -1, max: 1 },
];

const COLOR_SLIDERS: { key: keyof Omit<Tone, "wb">; label: string; min: number; max: number }[] = [
  { key: "vibrance", label: "Vibrance", min: -1, max: 1 },
  { key: "saturation", label: "Saturation", min: -1, max: 1 },
];


type Corner = "nw" | "ne" | "sw" | "se";

const clamp01 = (v: number) => Math.min(Math.max(v, 0), 1);

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
  const [line, setLine] = useState<{ x1: number; y1: number; x2: number; y2: number } | null>(null);
  const [overlay, setOverlay] = useState<CropOverlay>("thirds");
  const [showSplit, setShowSplit] = useState(false); // split-tone sliders collapsed by default
  const [luts, setLuts] = useState<string[]>([]);
  const [backdrop, setBackdrop] = useState<string>("");
  // Before/After toggle: "before" shows a separately cached unedited render.
  const [showBefore, setShowBefore] = useState(false);
  const [beforeBackdrop, setBeforeBackdrop] = useState<string>("");
  const [imgDims, setImgDims] = useState<{ w: number; h: number } | null>(null);
  const [error, setError] = useState("");
  const [view, setView] = useState({ scale: 1, tx: 0, ty: 0 }); // zoom/pan of the stage
  const [stageSize, setStageSize] = useState({ w: 0, h: 0 });
  const stageRef = useRef<HTMLDivElement>(null);
  const frameRef = useRef<HTMLDivElement>(null); // the image-aspect box the crop is relative to
  const ready = useRef(false); // gate auto-save until the version's edit is loaded

  const current = versions.find((v) => v.id === activeVersionId) ?? null;
  // "Original" is selected (read-only) when no version is active but versions exist.
  const viewingOriginal = activeVersionId == null && versions.length > 0;

  // Measure the stage so we can size the image-frame to the contained image (keeps the
  // crop rectangle's aspect correct and independent of window size).
  useEffect(() => {
    const el = stageRef.current;
    if (!el) return;
    const measure = () => setStageSize({ w: el.clientWidth, h: el.clientHeight });
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, [current]);

  // Contained image size within the stage (before zoom). The crop box is positioned in
  // fractions of this frame, so its aspect is always right.
  const frameSize =
    imgDims && stageSize.w > 0 && stageSize.h > 0
      ? (() => {
          const s = Math.min(stageSize.w / imgDims.w, stageSize.h / imgDims.h);
          return { w: imgDims.w * s, h: imgDims.h * s };
        })()
      : null;

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

  // Load the persisted overlay preference and the available LUTs once.
  useEffect(() => {
    getSetting(OVERLAY_KEY).then((v) => {
      if (v === "none" || v === "thirds" || v === "phi" || v === "golden") setOverlay(v);
    });
    listLuts().then(setLuts).catch(() => {});
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
  // saved editJson doesn't re-init mid-edit).
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
    setView({ scale: 1, tx: 0, ty: 0 });
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
    void setSetting(OVERLAY_KEY, key).catch(() => {});
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

  // Drag one corner of the perspective quad. Positions are fractions of the frame, which
  // is what the engine stores, so no pixel conversion is needed in either direction.
  const onQuadCornerDown = (e: React.MouseEvent, corner: QuadCorner) => {
    if (!perspective || !frameRef.current) return;
    e.preventDefault();
    e.stopPropagation(); // don't start a pan
    const rect = frameRef.current.getBoundingClientRect();
    const move = (ev: MouseEvent) => {
      const x = clamp01((ev.clientX - rect.left) / rect.width);
      const y = clamp01((ev.clientY - rect.top) / rect.height);
      setPerspective((p) => (p ? { ...p, [corner]: [x, y] } : p));
    };
    const up = () => {
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
    };
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
  };

  // Draw a line on the image; on release, level it (to horizontal or vertical) by adding
  // the rotation needed to the current straighten angle.
  const onStraightenDown = (e: React.MouseEvent) => {
    if (!frameRef.current || !frameSize) return;
    e.preventDefault();
    e.stopPropagation();
    const rect = frameRef.current.getBoundingClientRect();
    // Convert client → unzoomed frame coordinates (uniform scale, so angle is preserved).
    const toFrame = (cx: number, cy: number) => ({
      x: (cx - rect.left) * (frameSize.w / rect.width),
      y: (cy - rect.top) * (frameSize.h / rect.height),
    });
    const p0 = toFrame(e.clientX, e.clientY);
    setLine({ x1: p0.x, y1: p0.y, x2: p0.x, y2: p0.y });
    const move = (ev: MouseEvent) => {
      const p = toFrame(ev.clientX, ev.clientY);
      setLine({ x1: p0.x, y1: p0.y, x2: p.x, y2: p.y });
    };
    const up = (ev: MouseEvent) => {
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
      const p = toFrame(ev.clientX, ev.clientY);
      setLine(null);
      setStraightenMode(false);
      if (Math.hypot(p.x - p0.x, p.y - p0.y) > 8) {
        applyStraighten(straighten + levelFromLine(p0.x, p0.y, p.x, p.y));
      }
    };
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
  };

  // Drag the crop body to reposition.
  const onRectDown = (e: React.MouseEvent) => {
    if (!crop || !frameRef.current) return;
    e.preventDefault();
    e.stopPropagation(); // don't start a pan
    const stage = frameRef.current.getBoundingClientRect();
    const start = { mx: e.clientX, my: e.clientY, x: crop.x, y: crop.y };
    const move = (ev: MouseEvent) => {
      const dx = (ev.clientX - start.mx) / stage.width;
      const dy = (ev.clientY - start.my) / stage.height;
      setCrop((c) =>
        c
          ? {
              ...c,
              x: Math.min(Math.max(start.x + dx, 0), 1 - c.w),
              y: Math.min(Math.max(start.y + dy, 0), 1 - c.h),
            }
          : c,
      );
    };
    const up = () => {
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
    };
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
  };

  // Drag a corner to resize (aspect-locked when a ratio preset is active).
  const onCornerDown = (e: React.MouseEvent, corner: Corner) => {
    if (!crop || !frameRef.current) return;
    e.preventDefault();
    e.stopPropagation();
    const stage = frameRef.current.getBoundingClientRect();
    const ratio = ratioFor(aspect);
    const dims = srcDims ?? { w: 1, h: 1 };
    // Fixed (anchor) = the opposite corner.
    const anchor = {
      ax: corner === "nw" || corner === "sw" ? crop.x + crop.w : crop.x,
      ay: corner === "nw" || corner === "ne" ? crop.y + crop.h : crop.y,
    };
    const move = (ev: MouseEvent) => {
      const mx = clamp01((ev.clientX - stage.left) / stage.width);
      const my = clamp01((ev.clientY - stage.top) / stage.height);
      const dx = mx - anchor.ax;
      const dy = my - anchor.ay;
      let w: number;
      let h: number;
      if (ratio) {
        // Drive by whichever axis moved more (in image pixels), keep pixel aspect.
        const wpx = Math.abs(dx) * dims.w;
        const hpx = Math.abs(dy) * dims.h;
        if (wpx / (hpx || 1e-9) > ratio) {
          w = Math.abs(dx);
          h = (w * dims.w) / (ratio * dims.h);
        } else {
          h = Math.abs(dy);
          w = (h * dims.h * ratio) / dims.w;
        }
        // Scale to fit available space toward the drag direction, preserving aspect.
        const availW = dx >= 0 ? 1 - anchor.ax : anchor.ax;
        const availH = dy >= 0 ? 1 - anchor.ay : anchor.ay;
        const s = Math.min(1, availW / (w || 1e-9), availH / (h || 1e-9));
        w *= s;
        h *= s;
      } else {
        w = Math.abs(dx);
        h = Math.abs(dy);
      }
      w = Math.max(w, 0.05);
      h = Math.max(h, 0.05);
      let x = dx >= 0 ? anchor.ax : anchor.ax - w;
      let y = dy >= 0 ? anchor.ay : anchor.ay - h;
      if (!ratio) {
        // Free: clamp each edge independently.
        if (x < 0) {
          w += x;
          x = 0;
        }
        if (y < 0) {
          h += y;
          y = 0;
        }
        if (x + w > 1) w = 1 - x;
        if (y + h > 1) h = 1 - y;
      }
      setCrop((c) => (c ? { ...c, x, y, w, h } : c));
    };
    const up = () => {
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
    };
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
  };

  // Scroll to zoom the stage toward the cursor; drag the background to pan when zoomed.
  const onWheel = (e: React.WheelEvent) => {
    e.preventDefault();
    const stage = stageRef.current?.getBoundingClientRect();
    const cx = stage ? e.clientX - stage.left - stage.width / 2 : 0;
    const cy = stage ? e.clientY - stage.top - stage.height / 2 : 0;
    setView((v) => {
      const next = Math.min(Math.max(v.scale * (e.deltaY < 0 ? 1.15 : 1 / 1.15), 1), 8);
      if (next <= 1.001) return { scale: 1, tx: 0, ty: 0 };
      return {
        scale: next,
        tx: cx - (next / v.scale) * (cx - v.tx),
        ty: cy - (next / v.scale) * (cy - v.ty),
      };
    });
  };
  const onPanDown = (e: React.MouseEvent) => {
    if (view.scale <= 1) return; // nothing to pan at fit
    e.preventDefault();
    const start = { mx: e.clientX, my: e.clientY, tx: view.tx, ty: view.ty };
    const move = (ev: MouseEvent) =>
      setView((v) => ({ ...v, tx: start.tx + (ev.clientX - start.mx), ty: start.ty + (ev.clientY - start.my) }));
    const up = () => {
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
    };
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
  };

  // Frame the view to the current crop (Enter); Esc returns to fit.
  const zoomToCrop = () => {
    const el = stageRef.current;
    if (!crop || !el || !frameSize) return;
    const sw = el.clientWidth;
    const sh = el.clientHeight;
    const cropW = crop.w * frameSize.w;
    const cropH = crop.h * frameSize.h;
    if (cropW <= 0 || cropH <= 0) return;
    const scale = Math.min(sw / cropW, sh / cropH, 8);
    // Translate so the crop centre lands at the stage centre (origin = centre).
    const tx = -scale * frameSize.w * (crop.x + crop.w / 2 - 0.5);
    const ty = -scale * frameSize.h * (crop.y + crop.h / 2 - 0.5);
    setView({ scale, tx, ty });
  };

  // Enter = frame the crop; Esc = back to fit. Intercepted here so Enter doesn't
  // re-trigger a focused aspect button (which would reset the crop to max size).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const t = e.target as HTMLElement;
      if (t.tagName === "INPUT" || t.tagName === "TEXTAREA") return;
      if (e.key === "Enter") {
        e.preventDefault();
        zoomToCrop();
      } else if (e.key === "Escape") {
        e.preventDefault();
        setView({ scale: 1, tx: 0, ty: 0 });
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [crop, frameSize]);

  const setToneKey = (key: keyof Omit<Tone, "wb">, v: number) =>
    setTone((t) => ({ ...t, [key]: v }));
  const setWb = (key: "temp" | "tint", v: number) =>
    setTone((t) => ({ ...t, wb: { ...t.wb, [key]: v } }));
  const setLookPatch = (patch: Partial<Look>) => setLook((l) => ({ ...l, ...patch }));
  // Grain state lives as an optional object; amount 0 clears it.
  const grainAmount = look.grain?.amount ?? 0;
  const grainSize = look.grain?.size ?? 1;
  const setGrain = (amount: number, size: number) =>
    setLookPatch({ grain: amount > 0 ? { amount, size, seed: 0 } : undefined });
  // Split-tone edits start from a sepia-ish hue pair so dragging a sat slider does
  // something visible immediately.
  const setSplit = (patch: Partial<Split>) =>
    setLookPatch({
      split: {
        shadow_hue: 35,
        shadow_sat: 0,
        highlight_hue: 45,
        highlight_sat: 0,
        balance: 0,
        ...look.split,
        ...patch,
      },
    });
  const bwFilterActive = (f: Bw) =>
    !!look.bw &&
    Math.abs(look.bw.r - f.r) < 0.01 &&
    Math.abs(look.bw.g - f.g) < 0.01 &&
    Math.abs(look.bw.b - f.b) < 0.01;

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
          <div
            className="editor-stage"
            ref={stageRef}
            onWheel={onWheel}
            onMouseDown={onPanDown}
            style={{ cursor: view.scale > 1 ? "grab" : "default" }}
          >
            {/* Before/After toggle — glass pill, top-left of stage */}
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

            {displayedBackdrop ? (
              <div
                className="crop-zoom"
                style={{ transform: `translate(${view.tx}px, ${view.ty}px) scale(${view.scale})` }}
              >
                <div
                  className="crop-frame"
                  ref={frameRef}
                  style={frameSize ? { width: frameSize.w, height: frameSize.h } : undefined}
                >
                  <img
                    className="editor-img"
                    src={displayedBackdrop}
                    alt=""
                    draggable={false}
                    onLoad={(e) =>
                      setImgDims({ w: e.currentTarget.naturalWidth, h: e.currentTarget.naturalHeight })
                    }
                  />
                  {/* Don't show the crop overlay while in Before mode */}
                  {crop && !showBefore && (
                    <div
                      className="crop-rect"
                      onMouseDown={onRectDown}
                      style={{
                        left: `${crop.x * 100}%`,
                        top: `${crop.y * 100}%`,
                        width: `${crop.w * 100}%`,
                        height: `${crop.h * 100}%`,
                      }}
                    >
                      <CropGuides overlay={overlay} />
                      {cropPx && <span className="crop-dims">{cropPx.w} × {cropPx.h} px</span>}
                      {(["nw", "ne", "sw", "se"] as Corner[]).map((c) => (
                        <span
                          key={c}
                          className={`crop-handle crop-${c}`}
                          onMouseDown={(e) => onCornerDown(e, c)}
                        />
                      ))}
                    </div>
                  )}
                  {perspectiveMode && perspective && !showBefore && (
                    <div className="quad-layer">
                      <svg
                        className="quad-outline"
                        viewBox="0 0 100 100"
                        preserveAspectRatio="none"
                      >
                        <polygon
                          points={QUAD_CORNERS.map(
                            (c) => `${perspective[c][0] * 100},${perspective[c][1] * 100}`,
                          ).join(" ")}
                          vectorEffect="non-scaling-stroke"
                        />
                      </svg>
                      {QUAD_CORNERS.map((c) => (
                        <span
                          key={c}
                          className="quad-handle"
                          style={{
                            left: `${perspective[c][0] * 100}%`,
                            top: `${perspective[c][1] * 100}%`,
                          }}
                          onMouseDown={(e) => onQuadCornerDown(e, c)}
                        />
                      ))}
                    </div>
                  )}
                  {straightenMode && frameSize && (
                    <div className="straighten-capture" onMouseDown={onStraightenDown}>
                      {line && (
                        <svg
                          className="straighten-line"
                          width={frameSize.w}
                          height={frameSize.h}
                          viewBox={`0 0 ${frameSize.w} ${frameSize.h}`}
                        >
                          <line x1={line.x1} y1={line.y1} x2={line.x2} y2={line.y2} stroke="rgba(0,0,0,0.6)" strokeWidth={3} />
                          <line x1={line.x1} y1={line.y1} x2={line.x2} y2={line.y2} stroke="#fff" strokeWidth={1.5} />
                        </svg>
                      )}
                    </div>
                  )}
                </div>
              </div>
            ) : (
              <div className="editor-loading">Rendering…</div>
            )}
            {view.scale > 1.001 && (
              <button
                className="zoom-fit"
                title="Fit to window"
                onMouseDown={(e) => e.stopPropagation()}
                onClick={(e) => {
                  e.stopPropagation();
                  setView({ scale: 1, tx: 0, ty: 0 });
                }}
              >
                Fit {Math.round(view.scale * 100)}%
              </button>
            )}
          </div>

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

            {/* White Balance group */}
            <div className="develop-section">
              <div className="panel-head develop-group-label">White Balance</div>
              <div className="develop-slider-row">
                <div className="develop-slider-header">
                  <span className="develop-slider-label">Temperature</span>
                  <span className="develop-slider-value">{tone.wb.temp.toFixed(2)}</span>
                </div>
                <input
                  type="range"
                  className="develop-range develop-range-temp"
                  min={-1}
                  max={1}
                  step={0.05}
                  value={tone.wb.temp}
                  onChange={(e) => setWb("temp", parseFloat(e.target.value))}
                  onDoubleClick={() => setWb("temp", 0)}
                />
              </div>
              <div className="develop-slider-row">
                <div className="develop-slider-header">
                  <span className="develop-slider-label">Tint</span>
                  <span className="develop-slider-value">{tone.wb.tint.toFixed(2)}</span>
                </div>
                <input
                  type="range"
                  className="develop-range develop-range-tint"
                  min={-1}
                  max={1}
                  step={0.05}
                  value={tone.wb.tint}
                  onChange={(e) => setWb("tint", parseFloat(e.target.value))}
                  onDoubleClick={() => setWb("tint", 0)}
                />
              </div>
            </div>

            {/* Tone group */}
            <div className="develop-section">
              <div className="panel-head develop-group-label">Tone</div>
              {TONE_SLIDERS.map((s) => (
                <div className="develop-slider-row" key={s.key}>
                  <div className="develop-slider-header">
                    <span className="develop-slider-label">{s.label}</span>
                    <span className="develop-slider-value">{tone[s.key].toFixed(2)}</span>
                  </div>
                  <input
                    type="range"
                    className="develop-range"
                    min={s.min}
                    max={s.max}
                    step={0.05}
                    value={tone[s.key]}
                    onChange={(e) => setToneKey(s.key, parseFloat(e.target.value))}
                    onDoubleClick={() => setToneKey(s.key, 0)}
                  />
                </div>
              ))}
              <div className="editor-hint" style={{ marginTop: 4 }}>Double-click a slider to reset it.</div>
            </div>

            {/* Color group */}
            <div className="develop-section">
              <div className="panel-head develop-group-label">Color</div>
              {COLOR_SLIDERS.map((s) => (
                <div className="develop-slider-row" key={s.key}>
                  <div className="develop-slider-header">
                    <span className="develop-slider-label">{s.label}</span>
                    <span className="develop-slider-value">{tone[s.key].toFixed(2)}</span>
                  </div>
                  <input
                    type="range"
                    className="develop-range"
                    min={s.min}
                    max={s.max}
                    step={0.05}
                    value={tone[s.key]}
                    onChange={(e) => setToneKey(s.key, parseFloat(e.target.value))}
                    onDoubleClick={() => setToneKey(s.key, 0)}
                  />
                </div>
              ))}
            </div>

            {/* Effects group: B&W conversion, film-look controls, LUT */}
            <div className="develop-section">
              <div className="panel-head develop-group-label">Effects</div>
              <div className="editor-aspects">
                <button
                  className={`chip ${!look.bw ? "chip-on" : ""}`}
                  title="Colour (no B&W conversion)"
                  onClick={() => setLookPatch({ bw: undefined })}
                >
                  Color
                </button>
                {BW_FILTERS.map((f) => (
                  <button
                    key={f.label}
                    className={`chip ${bwFilterActive(f.bw) ? "chip-on" : ""}`}
                    title={`B&W with a ${f.label.toLowerCase()} contrast filter`}
                    onClick={() => setLookPatch({ bw: { ...f.bw } })}
                  >
                    B&W {f.label}
                  </button>
                ))}
              </div>
              {(
                [
                  { label: "Fade", value: look.fade ?? 0, min: 0, max: 1, set: (v: number) => setLookPatch({ fade: v }) },
                  { label: "Vignette", value: look.vignette ?? 0, min: -1, max: 1, set: (v: number) => setLookPatch({ vignette: v }) },
                  { label: "Grain", value: grainAmount, min: 0, max: 1, set: (v: number) => setGrain(v, grainSize) },
                  { label: "Grain size", value: grainSize, min: 0.5, max: 3, set: (v: number) => setGrain(grainAmount, v) },
                ] as const
              ).map((s) => (
                <div className="develop-slider-row" key={s.label}>
                  <div className="develop-slider-header">
                    <span className="develop-slider-label">{s.label}</span>
                    <span className="develop-slider-value">{s.value.toFixed(2)}</span>
                  </div>
                  <input
                    type="range"
                    className="develop-range"
                    min={s.min}
                    max={s.max}
                    step={0.05}
                    value={s.value}
                    onChange={(e) => s.set(parseFloat(e.target.value))}
                    onDoubleClick={() => s.set(s.label === "Grain size" ? 1 : 0)}
                  />
                </div>
              ))}

              {/* Split toning, collapsed by default */}
              <button
                className="preset-browser-head"
                style={{ marginTop: 10 }}
                onClick={() => setShowSplit((v) => !v)}
              >
                <span className="develop-slider-label">Split toning</span>
                <span className="preset-browser-caret">{showSplit ? "▾" : "▸"}</span>
              </button>
              {showSplit &&
                (
                  [
                    { label: "Shadow hue", key: "shadow_hue", min: 0, max: 360, step: 5 },
                    { label: "Shadow sat", key: "shadow_sat", min: 0, max: 1, step: 0.02 },
                    { label: "Highlight hue", key: "highlight_hue", min: 0, max: 360, step: 5 },
                    { label: "Highlight sat", key: "highlight_sat", min: 0, max: 1, step: 0.02 },
                    { label: "Balance", key: "balance", min: -1, max: 1, step: 0.05 },
                  ] as const
                ).map((s) => {
                  const value =
                    look.split?.[s.key] ??
                    (s.key === "shadow_hue" ? 35 : s.key === "highlight_hue" ? 45 : 0);
                  return (
                    <div className="develop-slider-row" key={s.key}>
                      <div className="develop-slider-header">
                        <span className="develop-slider-label">{s.label}</span>
                        <span className="develop-slider-value">
                          {s.max === 360 ? `${Math.round(value)}°` : value.toFixed(2)}
                        </span>
                      </div>
                      <input
                        type="range"
                        className={`develop-range ${s.max === 360 ? "develop-range-hue" : ""}`}
                        min={s.min}
                        max={s.max}
                        step={s.step}
                        value={value}
                        onChange={(e) => setSplit({ [s.key]: parseFloat(e.target.value) })}
                        onDoubleClick={() => s.max !== 360 && setSplit({ [s.key]: 0 })}
                      />
                    </div>
                  );
                })}

              {/* LUT picker */}
              <div className="develop-slider-row">
                <div className="develop-slider-header">
                  <span className="develop-slider-label">LUT (.cube)</span>
                </div>
                <div className="editor-lut-row">
                  <select
                    className="editor-lut-select"
                    value={look.lut?.file ?? ""}
                    onChange={(e) =>
                      setLookPatch({
                        lut: e.target.value ? { file: e.target.value, amount: 1 } : undefined,
                      })
                    }
                  >
                    <option value="">None</option>
                    {luts.map((f) => (
                      <option key={f} value={f}>
                        {f.replace(/\.cube$/i, "")}
                      </option>
                    ))}
                    {/* Keep a referenced-but-missing LUT selectable so it isn't silently dropped. */}
                    {look.lut && !luts.includes(look.lut.file) && (
                      <option value={look.lut.file}>{look.lut.file} (missing)</option>
                    )}
                  </select>
                  <button
                    className="chip"
                    title="Copy a .cube file into the LUT folder"
                    onClick={async () => {
                      try {
                        const path = await pickFile();
                        if (!path) return;
                        const file = await importLut(path);
                        setLuts(await listLuts());
                        setLookPatch({ lut: { file, amount: 1 } });
                      } catch (e) {
                        setError(String(e));
                      }
                    }}
                  >
                    Import…
                  </button>
                </div>
                {look.lut && (
                  <div className="develop-slider-row">
                    <div className="develop-slider-header">
                      <span className="develop-slider-label">LUT amount</span>
                      <span className="develop-slider-value">{look.lut.amount.toFixed(2)}</span>
                    </div>
                    <input
                      type="range"
                      className="develop-range"
                      min={0}
                      max={1}
                      step={0.05}
                      value={look.lut.amount}
                      onChange={(e) =>
                        setLookPatch({ lut: { ...look.lut!, amount: parseFloat(e.target.value) } })
                      }
                      onDoubleClick={() => setLookPatch({ lut: { ...look.lut!, amount: 1 } })}
                    />
                  </div>
                )}
              </div>
            </div>

            {/* Crop & Rotate group */}
            <div className="develop-section">
              <div className="panel-head develop-group-label">Crop &amp; Rotate</div>
              <div className="editor-aspects">
                {ASPECTS.map((a) => (
                  <button
                    key={a.label}
                    className={`chip ${aspect === a.label ? "chip-on" : ""}`}
                    title={a.hint}
                    onClick={(e) => {
                      applyAspect(a.label);
                      e.currentTarget.blur(); // so Enter doesn't re-apply (reset) the crop
                    }}
                  >
                    {a.label}
                  </button>
                ))}
              </div>
              <div className="panel-head develop-group-label" style={{ marginTop: 10 }}>
                Overlay
              </div>
              <div className="editor-aspects">
                {OVERLAYS.map((o) => (
                  <button
                    key={o.key}
                    className={`chip ${overlay === o.key ? "chip-on" : ""}`}
                    onClick={() => changeOverlay(o.key)}
                  >
                    {o.label}
                  </button>
                ))}
              </div>
              {cropPx && (
                <div className="editor-hint">
                  Output: {cropPx.w} × {cropPx.h} px
                </div>
              )}
              {crop && <div className="editor-hint">Drag the box to move · drag a corner to resize.</div>}

              {/* Perspective sub-group inside Crop & Rotate */}
              <div className="panel-head develop-group-label" style={{ marginTop: 10 }}>
                Perspective
              </div>
              <div className="editor-aspects">
                <button
                  className={`chip ${perspectiveMode ? "chip-on" : ""}`}
                  onClick={() => (perspectiveMode ? setPerspectiveMode(false) : startPerspective())}
                  title="Drag the four handles onto the corners of the picture, then press Done"
                >
                  {perspectiveMode ? "Done" : perspective ? "Adjust corners" : "Correct perspective"}
                </button>
                {perspective && (
                  <button className="chip" onClick={clearPerspective} title="Reset perspective">
                    Reset
                  </button>
                )}
              </div>
              <div className="editor-hint">
                {perspectiveMode
                  ? "Put each handle on the matching corner of the picture, then press Done."
                  : perspective
                    ? "Corners set — the frame is squared up."
                    : "Squares up a picture or document photographed off-axis."}
              </div>

              {/* Straighten sub-group inside Crop & Rotate */}
              <div className="panel-head develop-group-label" style={{ marginTop: 10 }}>
                Straighten
              </div>
              <div className="editor-aspects">
                <button
                  className={`chip ${straightenMode ? "chip-on" : ""}`}
                  onClick={() => setStraightenMode((m) => !m)}
                  title="Draw a line along something that should be level (a horizon or a vertical edge)"
                >
                  {straightenMode ? "Drawing… drag on image" : "Draw level line"}
                </button>
                {straighten !== 0 && (
                  <button className="chip" onClick={() => applyStraighten(0)} title="Reset straighten">
                    Reset
                  </button>
                )}
              </div>
              <div className="develop-slider-row" style={{ marginTop: 6 }}>
                <div className="develop-slider-header">
                  <span className="develop-slider-label">Angle</span>
                  <span className="develop-slider-value">{straighten.toFixed(1)}°</span>
                </div>
                <input
                  type="range"
                  className="develop-range"
                  min={-STRAIGHTEN_MAX}
                  max={STRAIGHTEN_MAX}
                  step={0.1}
                  value={straighten}
                  onChange={(e) => applyStraighten(parseFloat(e.target.value))}
                  onDoubleClick={() => applyStraighten(0)}
                />
              </div>
              <div className="editor-hint">
                {straightenMode
                  ? "Drag a line along the horizon (or a vertical edge) — the image levels to it."
                  : "Draw a level line or nudge the angle; the crop auto-insets to hide the corners."}
              </div>
            </div>

            {error && <div className="modal-error">{error}</div>}
            <div className="editor-hint" style={{ color: "var(--text-mute)" }}>
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

// Composition guides drawn inside the crop rectangle.
function CropGuides({ overlay }: { overlay: CropOverlay }) {
  if (overlay === "none") return null;
  if (overlay === "golden") {
    return (
      <svg className="crop-overlay" viewBox="0 0 100 100" preserveAspectRatio="none">
        <path d={GOLDEN_SPIRAL_PATH} fill="none" stroke="rgba(255,255,255,0.6)" strokeWidth={0.6} />
      </svg>
    );
  }
  const lines = OVERLAY_LINES[overlay];
  return (
    <svg className="crop-overlay" viewBox="0 0 100 100" preserveAspectRatio="none">
      {lines.map((f, i) => (
        <line key={`v${i}`} x1={f * 100} y1={0} x2={f * 100} y2={100} stroke="rgba(255,255,255,0.5)" strokeWidth={0.5} />
      ))}
      {lines.map((f, i) => (
        <line key={`h${i}`} x1={0} y1={f * 100} x2={100} y2={f * 100} stroke="rgba(255,255,255,0.5)" strokeWidth={0.5} />
      ))}
    </svg>
  );
}
