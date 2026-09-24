// Shared, controlled edit controls — extracted verbatim from EditorView (darkroom
// slice 6, docs/plans/darkroom) so the classic Develop view and the Darkroom render the
// SAME stage and rail widgets. All edit state lives in the parent; the only state kept
// here is presentation-only (zoom/pan, drag lines, collapsed groups, the LUT list).
//
// - EditStage: the zoomable image stage with the crop box, perspective quad, and
//   straighten line. Scroll zooms toward the cursor, drag pans when zoomed, Enter
//   frames the crop, Esc returns to fit.
// - ToneRail / EffectsRail / GeometryRail: the classic slider rail, one group each.
import { ReactNode, useEffect, useRef, useState } from "react";
import { importLut, listLuts, pickFile, setSetting } from "../modules/api";
import {
  ASPECTS,
  Bw,
  BW_FILTERS,
  Crop,
  CropOverlay,
  GOLDEN_SPIRAL_PATH,
  levelFromLine,
  Look,
  OVERLAY_LINES,
  OVERLAYS,
  Perspective,
  QUAD_CORNERS,
  QuadCorner,
  Split,
  STRAIGHTEN_MAX,
  Tone,
} from "../modules/editing";
import {
  KELVIN_TINT_RANGE,
  kelvinToSlider,
  kelvinWb,
  SLIDER_STEPS,
  sliderToKelvin,
  wbShown,
  type KelvinContext,
} from "./darkroom/kelvin";

/** The persisted crop-overlay preference, shared by every Develop surface. */
export const OVERLAY_KEY = "editor.crop_overlay";

export const TONE_SLIDERS: { key: keyof Omit<Tone, "wb">; label: string; min: number; max: number }[] = [
  { key: "ev", label: "Exposure", min: -3, max: 3 },
  { key: "contrast", label: "Contrast", min: -1, max: 1 },
  { key: "highlights", label: "Highlights", min: -1, max: 1 },
  { key: "shadows", label: "Shadows", min: -1, max: 1 },
  { key: "whites", label: "Whites", min: -1, max: 1 },
  { key: "blacks", label: "Blacks", min: -1, max: 1 },
];

export const COLOR_SLIDERS: { key: keyof Omit<Tone, "wb">; label: string; min: number; max: number }[] = [
  { key: "vibrance", label: "Vibrance", min: -1, max: 1 },
  { key: "saturation", label: "Saturation", min: -1, max: 1 },
];

type Corner = "nw" | "ne" | "sw" | "se";

const clamp01 = (v: number) => Math.min(Math.max(v, 0), 1);

// ── EditStage ───────────────────────────────────────────────────────────────

export function EditStage({
  backdrop,
  loading,
  crop,
  setCrop,
  ratio,
  srcDims,
  cropPx,
  overlay,
  perspective,
  setPerspective,
  perspectiveMode,
  straightenMode,
  onLevel,
  showBefore = false,
  topLeft,
  onImgDims,
  onBackdropLoad,
  onBackdropError,
  stageLayer,
}: {
  /** The rendered preview (data URL); empty shows `loading`. */
  backdrop: string;
  loading?: ReactNode;
  crop: Crop | null;
  /** Functional setter, mirroring React's — the parent owns the crop. */
  setCrop: (f: (c: Crop | null) => Crop | null) => void;
  /** Locked pixel aspect (width/height) for corner drags, or null for free. */
  ratio: number | null;
  /** Oriented full-res dimensions — the aspect math's frame of reference. */
  srcDims: { w: number; h: number } | null;
  /** Output size readout shown on the crop box, or null to hide. */
  cropPx: { w: number; h: number } | null;
  overlay: CropOverlay;
  perspective: Perspective | null;
  setPerspective: (f: (p: Perspective | null) => Perspective | null) => void;
  perspectiveMode: boolean;
  straightenMode: boolean;
  /** Fired when the level line is released: the delta to add, or null = too short. The
   *  parent exits straighten mode either way. */
  onLevel: (deltaDeg: number | null) => void;
  /** Hide the crop/quad overlays (the Before toggle shows the untouched frame). */
  showBefore?: boolean;
  /** Slot drawn over the stage's top-left (the Before/After pill). */
  topLeft?: ReactNode;
  /** The displayed preview's natural dimensions, for the parent's orientation math. */
  onImgDims?: (d: { w: number; h: number }) => void;
  /** The backdrop <img> finished loading — `src` is the `backdrop` value that loaded, so
   *  the caller can match it against what it set. On screen next paint. */
  onBackdropLoad?: (src: string) => void;
  /** The backdrop <img> failed to load that `backdrop` value. */
  onBackdropError?: (src: string) => void;
  /** Drawn over the picture inside the frame (so it pans, zooms and sizes with it) and
   *  under the crop and quad overlays — the Darkroom's sensor-clipping layer. */
  stageLayer?: ReactNode;
}) {
  const stageRef = useRef<HTMLDivElement>(null);
  const frameRef = useRef<HTMLDivElement>(null);
  const [stageSize, setStageSize] = useState({ w: 0, h: 0 });
  const [imgDims, setImgDims] = useState<{ w: number; h: number } | null>(null);
  const [view, setView] = useState({ scale: 1, tx: 0, ty: 0 });
  const [line, setLine] = useState<{ x1: number; y1: number; x2: number; y2: number } | null>(null);

  useEffect(() => {
    const el = stageRef.current;
    if (!el) return;
    const measure = () => setStageSize({ w: el.clientWidth, h: el.clientHeight });
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const frameSize =
    imgDims && stageSize.w > 0 && stageSize.h > 0
      ? (() => {
          const s = Math.min(stageSize.w / imgDims.w, stageSize.h / imgDims.h);
          return { w: imgDims.w * s, h: imgDims.h * s };
        })()
      : null;

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
    const dims = srcDims ?? { w: 1, h: 1 };
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

  // Drag one corner of the perspective quad (fractions of the frame — engine units).
  const onQuadCornerDown = (e: React.MouseEvent, corner: QuadCorner) => {
    if (!perspective || !frameRef.current) return;
    e.preventDefault();
    e.stopPropagation();
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

  // Draw a line on the image; on release, hand the levelling delta to the parent.
  const onStraightenDown = (e: React.MouseEvent) => {
    if (!frameRef.current || !frameSize) return;
    e.preventDefault();
    e.stopPropagation();
    const rect = frameRef.current.getBoundingClientRect();
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
      onLevel(
        Math.hypot(p.x - p0.x, p.y - p0.y) > 8
          ? levelFromLine(p0.x, p0.y, p.x, p.y)
          : null,
      );
    };
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
  };

  // Scroll to zoom toward the cursor; drag the background to pan when zoomed.
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
    if (view.scale <= 1) return;
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
    const tx = -scale * frameSize.w * (crop.x + crop.w / 2 - 0.5);
    const ty = -scale * frameSize.h * (crop.y + crop.h / 2 - 0.5);
    setView({ scale, tx, ty });
  };

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

  return (
    <div
      className="editor-stage"
      ref={stageRef}
      onWheel={onWheel}
      onMouseDown={onPanDown}
      style={{ cursor: view.scale > 1 ? "grab" : "default" }}
    >
      {topLeft}
      {backdrop ? (
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
              src={backdrop}
              alt=""
              draggable={false}
              onLoad={(e) => {
                const d = { w: e.currentTarget.naturalWidth, h: e.currentTarget.naturalHeight };
                setImgDims(d);
                onImgDims?.(d);
                onBackdropLoad?.(backdrop);
              }}
              onError={() => onBackdropError?.(backdrop)}
            />
            {stageLayer}
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
                <svg className="quad-outline" viewBox="0 0 100 100" preserveAspectRatio="none">
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
        (loading ?? <div className="editor-loading">Rendering…</div>)
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
  );
}

// ── ToneRail ────────────────────────────────────────────────────────────────

export function ToneRail({
  tone,
  onTone,
  kelvin,
}: {
  tone: Tone;
  onTone: (t: Tone) => void;
  /** Engine 2 with an as-shot light: white balance can be stated in Kelvin
   *  (docs/plans/raw-foundation, slice 9). Absent: the relative sliders only. */
  kelvin?: KelvinContext | null;
}) {
  const setToneKey = (key: keyof Omit<Tone, "wb">, v: number) => onTone({ ...tone, [key]: v });
  const setWb = (key: "temp" | "tint", v: number) =>
    onTone({
      ...tone,
      // A relative move keeps an explicit "relative" choice, so the rail does not flip back
      // to Kelvin when the slider returns to zero.
      wb: { temp: tone.wb.temp, tint: tone.wb.tint, ...(tone.wb.mode === "relative" ? { mode: "relative" as const } : {}), [key]: v },
    });
  const shown = wbShown(tone.wb, kelvin);
  // Back to as-shot: a blank white balance renders exactly as the camera's light.
  const asShot = () => onTone({ ...tone, wb: { temp: 0, tint: 0 } });
  return (
    <>
      <div className="develop-section">
        <div className="panel-head develop-group-label">
          White Balance
          {kelvin && (
            <button
              className="develop-wb-mode"
              title={
                shown.mode === "kelvin"
                  ? "Adjust white balance as warmer/cooler than as-shot instead"
                  : "State the scene's light in Kelvin"
              }
              onClick={() =>
                onTone({
                  ...tone,
                  wb:
                    shown.mode === "kelvin"
                      ? { temp: 0, tint: 0, mode: "relative" }
                      : kelvinWb(kelvin.asShot.kelvin, kelvin.asShot.tint),
                })
              }
            >
              {shown.mode === "kelvin" ? "K" : "±"}
            </button>
          )}
        </div>
        {shown.mode === "kelvin" ? (
          <>
            <div className="develop-slider-row">
              <div className="develop-slider-header">
                <span className="develop-slider-label">Temperature</span>
                <span className="develop-slider-value">{Math.round(shown.kelvin)} K</span>
              </div>
              <input
                type="range"
                className="develop-range develop-range-temp"
                min={0}
                max={SLIDER_STEPS}
                step={1}
                value={kelvinToSlider(shown.kelvin)}
                onChange={(e) =>
                  onTone({ ...tone, wb: kelvinWb(sliderToKelvin(parseFloat(e.target.value)), shown.tint) })
                }
                onDoubleClick={asShot}
                title={kelvin ? `As shot: ${Math.round(kelvin.asShot.kelvin)} K` : undefined}
              />
            </div>
            <div className="develop-slider-row">
              <div className="develop-slider-header">
                <span className="develop-slider-label">Tint</span>
                <span className="develop-slider-value">
                  {shown.tint >= 0 ? "+" : "−"}
                  {Math.abs(shown.tint).toFixed(0)}
                </span>
              </div>
              <input
                type="range"
                className="develop-range develop-range-tint"
                min={-KELVIN_TINT_RANGE}
                max={KELVIN_TINT_RANGE}
                step={1}
                value={shown.tint}
                onChange={(e) => onTone({ ...tone, wb: kelvinWb(shown.kelvin, parseFloat(e.target.value)) })}
                onDoubleClick={asShot}
              />
            </div>
          </>
        ) : (
          <>
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
          </>
        )}
      </div>

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
    </>
  );
}

// ── EffectsRail ─────────────────────────────────────────────────────────────

export function EffectsRail({
  look,
  onLook,
  onError,
}: {
  look: Look;
  onLook: (l: Look) => void;
  onError?: (msg: string) => void;
}) {
  const [showSplit, setShowSplit] = useState(false);
  const [luts, setLuts] = useState<string[]>([]);
  useEffect(() => {
    listLuts().then(setLuts).catch(() => {});
  }, []);

  const setLookPatch = (patch: Partial<Look>) => onLook({ ...look, ...patch });
  const grainAmount = look.grain?.amount ?? 0;
  const grainSize = look.grain?.size ?? 1;
  const setGrain = (amount: number, size: number) =>
    setLookPatch({ grain: amount > 0 ? { amount, size, seed: 0 } : undefined });
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

  return (
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
                onError?.(String(e));
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
  );
}

// ── GeometryRail ────────────────────────────────────────────────────────────

export function GeometryRail({
  aspect,
  onAspect,
  overlay,
  onOverlay,
  cropPx,
  cropActive,
  perspective,
  perspectiveMode,
  onPerspectiveToggle,
  onPerspectiveClear,
  straighten,
  straightenMode,
  onStraightenModeToggle,
  onStraighten,
}: {
  aspect: string;
  onAspect: (label: string) => void;
  overlay: CropOverlay;
  onOverlay: (key: CropOverlay) => void;
  cropPx: { w: number; h: number } | null;
  cropActive: boolean;
  perspective: Perspective | null;
  perspectiveMode: boolean;
  onPerspectiveToggle: () => void;
  onPerspectiveClear: () => void;
  straighten: number;
  straightenMode: boolean;
  onStraightenModeToggle: () => void;
  onStraighten: (deg: number) => void;
}) {
  return (
    <div className="develop-section">
      <div className="panel-head develop-group-label">Crop &amp; Rotate</div>
      <div className="editor-aspects">
        {ASPECTS.map((a) => (
          <button
            key={a.label}
            className={`chip ${aspect === a.label ? "chip-on" : ""}`}
            title={a.hint}
            onClick={(e) => {
              onAspect(a.label);
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
            onClick={() => onOverlay(o.key)}
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
      {cropActive && <div className="editor-hint">Drag the box to move · drag a corner to resize.</div>}

      <div className="panel-head develop-group-label" style={{ marginTop: 10 }}>
        Perspective
      </div>
      <div className="editor-aspects">
        <button
          className={`chip ${perspectiveMode ? "chip-on" : ""}`}
          onClick={onPerspectiveToggle}
          title="Drag the four handles onto the corners of the picture, then press Done"
        >
          {perspectiveMode ? "Done" : perspective ? "Adjust corners" : "Correct perspective"}
        </button>
        {perspective && (
          <button className="chip" onClick={onPerspectiveClear} title="Reset perspective">
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

      <div className="panel-head develop-group-label" style={{ marginTop: 10 }}>
        Straighten
      </div>
      <div className="editor-aspects">
        <button
          className={`chip ${straightenMode ? "chip-on" : ""}`}
          onClick={onStraightenModeToggle}
          title="Draw a line along something that should be level (a horizon or a vertical edge)"
        >
          {straightenMode ? "Drawing… drag on image" : "Draw level line"}
        </button>
        {straighten !== 0 && (
          <button className="chip" onClick={() => onStraighten(0)} title="Reset straighten">
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
          onChange={(e) => onStraighten(parseFloat(e.target.value))}
          onDoubleClick={() => onStraighten(0)}
        />
      </div>
      <div className="editor-hint">
        {straightenMode
          ? "Drag a line along the horizon (or a vertical edge) — the image levels to it."
          : "Draw a level line or nudge the angle; the crop auto-insets to hide the corners."}
      </div>
    </div>
  );
}

/** Change the overlay and persist the preference (shared by both Develop surfaces). */
export function persistOverlay(key: CropOverlay): void {
  void setSetting(OVERLAY_KEY, key).catch(() => {});
}

// Composition guides drawn inside the crop rectangle.
export function CropGuides({ overlay }: { overlay: CropOverlay }) {
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
