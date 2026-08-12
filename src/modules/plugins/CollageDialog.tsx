import { useEffect, useMemo, useRef, useState } from "react";
import type { ChairPhotoAPI, Photo } from "../registry";
import { getThumbnail, pickFolder, revealInFolder } from "../api";
import { COLLAGE_TEMPLATES } from "./collageTemplates";

// ── Backend commands (owned by this module) ───────────────────────────────────
// Per the module contract, a module's own commands go through `ChairPhotoAPI.invoke`
// rather than core's `api.ts`, so the command names travel with the module.

/** Output file format for the collage. PNG keeps alpha (rounded corners / transparent mat);
 *  anything else flattens onto the background and writes JPEG. */
type CollageFormat = "png" | "jpeg";

/**
 * Collage render options (camelCase, matching the backend serde DTO). `aspect` is the output
 * aspect ratio: null or "free" grows the canvas with the rows; a ratio like "1:1"/"4:5"/"9:16"
 * fixes the canvas to `width × width/r` (the backend binary-searches the row height to fit).
 * `background` accepts `#rgb`/`#rrggbb`/`#rrggbbaa`/`rgba(r,g,b,a)`. `fit` is "cover" or
 * "contain" (anything not "cover" → contain). See docs/collage.md.
 */
interface CollageOptions {
  width: number;
  aspect: string | null;
  rowHeight: number;
  gap: number;
  background: string;
  fit: "contain" | "cover";
  borderWidth: number;
  cornerRadius: number;
}

/** A freeform tile placement: normalized x/y (top-left) and w/h, both 0–1 of the canvas,
 *  plus stacking order z (higher = on top, overlap allowed). */
interface Placement {
  photoId: number;
  x: number;
  y: number;
  w: number;
  h: number;
  z: number;
  /** Focal offset (0–1; 0.5 = centered) for the cover-crop — pan the photo within its frame. */
  ox: number;
  oy: number;
  /** Zoom factor (≥1; 1 = fill the cell) for the cover-crop. */
  zoom: number;
}

/** Canvas/styling options for a freeform render. `width`×`height` is the output pixel size. */
interface FreeformOptions {
  width: number;
  height: number;
  background: string;
  borderWidth: number;
  cornerRadius: number;
}

/** Lay the photos out as the justified mosaic and return normalized placements (seeds the
 *  freeform canvas). `opts` reuses the mosaic options (width/aspect/rowHeight/gap). */
const collageAutoArrange = (api: ChairPhotoAPI, photoIds: number[], opts: CollageOptions) =>
  api.invoke<Placement[]>("collage_auto_arrange", { photoIds, opts });

/** Composite a freeform collage from explicit placements; returns the saved file path. */
const makeCollageFreeform = (
  api: ChairPhotoAPI,
  placements: Placement[],
  opts: FreeformOptions,
  format: CollageFormat,
  destDir: string,
) => api.invoke<string>("make_collage_freeform", { placements, opts, format, destDir });

/** Composite a freeform collage and save it into the library; returns the new photo id.
 *  `kind` is the layout used (e.g. "Grid"/"Freeform"); the collage is tagged `Collage/<kind>`. */
const saveCollageToCatalog = (
  api: ChairPhotoAPI,
  placements: Placement[],
  opts: FreeformOptions,
  format: CollageFormat,
  kind: string,
) => api.invoke<number>("save_collage_to_catalog", { placements, opts, format, kind });

// The Collage dialog (H12) — a freeform layout canvas. Photos start auto-arranged as a
// justified mosaic; you then drag to move, corner-drag to resize (photo aspect locked), and
// stack them so they overlap (z-order). Export composites the full-resolution image from the
// placements. See docs/collage.md ("Freeform layout").

// Canvas aspect (also the export aspect). No "Free" — the canvas needs a bounded shape.
const ASPECTS = [
  { value: "1:1", w: 1, h: 1 },
  { value: "4:5", w: 4, h: 5 },
  { value: "5:4", w: 5, h: 4 },
  { value: "3:2", w: 3, h: 2 },
  { value: "2:3", w: 2, h: 3 },
  { value: "16:9", w: 16, h: 9 },
  { value: "9:16", w: 9, h: 16 },
] as const;
const WIDTH_PRESETS = [1080, 2048, 4096];
// Tag suffix per template id → the collage is tagged "Collage/<kind>".
const TEMPLATE_KIND: Record<string, string> = {
  grid: "Grid",
  columns: "Columns",
  rows: "Rows",
  "feature-left": "Feature-left",
  "feature-right": "Feature-right",
  "feature-top": "Feature-top",
  "feature-bottom": "Feature-bottom",
};

const CANVAS_MAX_W = 520;
const CANVAS_MAX_H = 460;
const MIN_TILE = 0.06; // smallest tile, as a fraction of the canvas

const clamp = (v: number, lo: number, hi: number) => Math.min(Math.max(v, lo), Math.max(lo, hi));

/** A canvas tile's image, cover-filled at `zoom` and panned by `ox`/`oy` to exactly mirror
 *  the backend `resize_cover_offset` (so the preview matches the export). The parent clips it
 *  (overflow hidden). Reports the photo's displayed aspect once loaded. */
function CanvasTile({
  photo,
  boxW,
  boxH,
  zoom,
  ox,
  oy,
  onAspect,
}: {
  photo: Photo;
  boxW: number;
  boxH: number;
  zoom: number;
  ox: number;
  oy: number;
  onAspect: (aspect: number) => void;
}) {
  const [src, setSrc] = useState<string>("");
  const [nat, setNat] = useState<{ w: number; h: number } | null>(null);
  useEffect(() => {
    let alive = true;
    getThumbnail(photo.id)
      .then((s) => alive && setSrc(s))
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [photo.id]);

  if (!src) return <div className="collage-tile-loading" />;

  let style: React.CSSProperties = { position: "absolute", inset: 0, width: "100%", height: "100%", objectFit: "cover", display: "block" };
  if (nat) {
    const pa = nat.w / nat.h;
    const ba = boxW / Math.max(1, boxH);
    let w: number;
    let h: number;
    if (pa > ba) {
      h = boxH * zoom;
      w = h * pa;
    } else {
      w = boxW * zoom;
      h = w / pa;
    }
    style = {
      position: "absolute",
      width: w,
      height: h,
      maxWidth: "none",
      left: -(w - boxW) * ox,
      top: -(h - boxH) * oy,
      display: "block",
    };
  }
  return (
    <img
      src={src}
      alt=""
      draggable={false}
      onLoad={(e) => {
        const el = e.currentTarget;
        if (el.naturalWidth && el.naturalHeight) {
          setNat({ w: el.naturalWidth, h: el.naturalHeight });
          onAspect(el.naturalWidth / el.naturalHeight);
        }
      }}
      style={style}
    />
  );
}

export function CollageDialog({ api, onClose }: { api: ChairPhotoAPI; onClose: () => void }) {
  // Snapshot the selection once on open, deliberately unsubscribed: the arranged layout
  // below is keyed by these photo ids, so re-reading the selection mid-dialog would
  // invalidate an arrangement the user has already made.
  const order = useMemo<Photo[]>(() => api.getSelectedPhotos(), [api]);
  const photoById = useMemo(() => new Map(order.map((p) => [p.id, p])), [order]);

  const [aspect, setAspect] = useState<string>("1:1");
  const [width, setWidth] = useState(2048);
  const [background, setBackground] = useState("#ffffff");
  const [borderWidth, setBorderWidth] = useState(0);
  const [cornerRadius, setCornerRadius] = useState(0);
  const [format, setFormat] = useState<CollageFormat>("jpeg");
  const [saveTo, setSaveTo] = useState<"folder" | "library">("library");
  const [dest, setDest] = useState("~/Pictures/Export");
  const [savedToLibrary, setSavedToLibrary] = useState(false);
  // The layout used, for the auto-tag (Collage/<kind>): Mosaic / Grid / … / Freeform.
  const [layoutKind, setLayoutKind] = useState("Freeform");

  const [placements, setPlacements] = useState<Placement[]>([]);
  const [selected, setSelected] = useState<number | null>(null);
  // Locked (template) mode: positions are fixed and dragging a photo swaps it into the slot
  // it's dropped on (e.g. drop onto the feature cell to make it the feature).
  const [locked, setLocked] = useState(false);
  const [swapTarget, setSwapTarget] = useState<number | null>(null);
  const swapTargetRef = useRef<number | null>(null);
  const [busy, setBusy] = useState(false);
  const [arranging, setArranging] = useState(false);
  const [outputPath, setOutputPath] = useState<string | null>(null);
  const [error, setError] = useState("");

  // Displayed aspect per photo (from the loaded thumbnail), for aspect-locked resize.
  const aspectMap = useRef<Map<number, number>>(new Map());
  const canvasRef = useRef<HTMLDivElement>(null);
  // Active drag/resize gesture.
  const gesture = useRef<{
    id: number;
    mode: "move" | "resize" | "pan" | "swap";
    px: number;
    py: number;
    start: Placement;
    photoAspect: number;
  } | null>(null);

  const ar = ASPECTS.find((a) => a.value === aspect) ?? ASPECTS[0];
  const ratio = ar.w / ar.h;
  let dispW = CANVAS_MAX_W;
  let dispH = CANVAS_MAX_W / ratio;
  if (dispH > CANVAS_MAX_H) {
    dispH = CANVAS_MAX_H;
    dispW = CANVAS_MAX_H * ratio;
  }

  const needsPng = cornerRadius > 0;

  const autoArrange = async () => {
    if (order.length === 0) return;
    setArranging(true);
    setError("");
    try {
      const pls = await collageAutoArrange(
        api,
        order.map((p) => p.id),
        { width: 2000, aspect, rowHeight: 460, gap: 10, background, fit: "contain", borderWidth, cornerRadius },
      );
      setPlacements(pls);
      setLocked(false); // the mosaic is a free starting point you can tweak
      setLayoutKind("Mosaic");
    } catch (e) {
      setError(String(e));
    } finally {
      setArranging(false);
    }
  };

  const applyTemplate = (id: string) => {
    const t = COLLAGE_TEMPLATES.find((x) => x.id === id);
    if (!t) return;
    const cells = t.gen(order.length);
    if (!cells) {
      setError(`Template "${t.label}" doesn't fit ${order.length} photos.`);
      return;
    }
    setError("");
    setPlacements(
      cells.map((c, i) => ({
        photoId: order[i].id,
        x: c.x,
        y: c.y,
        w: c.w,
        h: c.h,
        z: i,
        ox: 0.5,
        oy: 0.5,
        zoom: 1,
      })),
    );
    setSelected(null);
    setLocked(true); // template = fixed slots; drag swaps photos between them
    setLayoutKind(TEMPLATE_KIND[id] ?? "Freeform");
  };

  // Seed the canvas with an auto-arranged layout on open.
  useEffect(() => {
    if (order.length >= 2) void autoArrange();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const update = (id: number, patch: Partial<Placement>) =>
    setPlacements((prev) => prev.map((p) => (p.photoId === id ? { ...p, ...patch } : p)));

  const onMove = (e: PointerEvent) => {
    const g = gesture.current;
    const rect = canvasRef.current?.getBoundingClientRect();
    if (!g || !rect) return;
    const dx = (e.clientX - g.px) / rect.width;
    const dy = (e.clientY - g.py) / rect.height;
    if (g.mode === "swap") {
      // Highlight the slot under the cursor (the swap target); the tile itself stays put.
      const nx = (e.clientX - rect.left) / rect.width;
      const ny = (e.clientY - rect.top) / rect.height;
      let target: number | null = null;
      let bestZ = -Infinity;
      for (const p of placements) {
        if (p.photoId === g.id) continue;
        if (nx >= p.x && nx <= p.x + p.w && ny >= p.y && ny <= p.y + p.h && p.z > bestZ) {
          bestZ = p.z;
          target = p.photoId;
        }
      }
      swapTargetRef.current = target;
      setSwapTarget(target);
    } else if (g.mode === "move") {
      const x = clamp(g.start.x + dx, 0, 1 - g.start.w);
      const y = clamp(g.start.y + dy, 0, 1 - g.start.h);
      update(g.id, { x, y });
    } else if (g.mode === "pan") {
      // Reposition the photo inside its frame. Compute the scaled image size exactly as the
      // tile renders it (cover × zoom); BOTH axes can overflow once zoomed in, so pan each
      // axis that has slack — not just the aspect-determined one.
      const boxW = g.start.w * rect.width;
      const boxH = g.start.h * rect.height;
      const pa = g.photoAspect;
      const ba = boxW / Math.max(1, boxH);
      let imgW: number;
      let imgH: number;
      if (pa > ba) {
        imgH = boxH * g.start.zoom;
        imgW = imgH * pa;
      } else {
        imgW = boxW * g.start.zoom;
        imgH = imgW / pa;
      }
      const overflowX = imgW - boxW;
      const overflowY = imgH - boxH;
      const dxpx = e.clientX - g.px;
      const dypx = e.clientY - g.py;
      let { ox, oy } = g.start;
      if (overflowX > 0.5) ox = clamp(g.start.ox - dxpx / overflowX, 0, 1);
      if (overflowY > 0.5) oy = clamp(g.start.oy - dypx / overflowY, 0, 1);
      update(g.id, { ox, oy });
    } else {
      // Free resize from the corner — any cell shape (the photo cover-fills it). The cell may
      // overlap or bleed off the canvas (clipped).
      const w = clamp(g.start.w + dx, MIN_TILE, 1);
      const h = clamp(g.start.h + dy, MIN_TILE, 1);
      update(g.id, { w, h });
    }
  };
  const onUp = () => {
    const g = gesture.current;
    if (g && (g.mode === "move" || g.mode === "resize")) {
      setLayoutKind("Freeform"); // a manual edit is no longer a pure template layout
    }
    if (g && g.mode === "swap") {
      const target = swapTargetRef.current;
      if (target != null && target !== g.id) {
        const source = g.id;
        // Swap which photo occupies each slot (slots stay fixed); reset framing for the new cell.
        setPlacements((prev) =>
          prev.map((p) =>
            p.photoId === source
              ? { ...p, photoId: target, ox: 0.5, oy: 0.5, zoom: 1 }
              : p.photoId === target
                ? { ...p, photoId: source, ox: 0.5, oy: 0.5, zoom: 1 }
                : p,
          ),
        );
      }
    }
    swapTargetRef.current = null;
    setSwapTarget(null);
    gesture.current = null;
    window.removeEventListener("pointermove", onMove);
    window.removeEventListener("pointerup", onUp);
  };
  const startGesture = (
    e: React.PointerEvent,
    p: Placement,
    mode: "move" | "resize" | "pan" | "swap",
  ) => {
    e.preventDefault();
    e.stopPropagation();
    setSelected(p.photoId);
    gesture.current = {
      id: p.photoId,
      mode,
      px: e.clientX,
      py: e.clientY,
      start: { ...p },
      photoAspect: aspectMap.current.get(p.photoId) ?? 1,
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
  };
  useEffect(() => () => onUp(), []); // cleanup listeners on unmount

  const bringToFront = () => {
    if (selected == null) return;
    const maxZ = placements.reduce((m, p) => Math.max(m, p.z), 0);
    update(selected, { z: maxZ + 1 });
  };
  const sendToBack = () => {
    if (selected == null) return;
    const minZ = placements.reduce((m, p) => Math.min(m, p.z), 0);
    update(selected, { z: minZ - 1 });
  };

  const run = async () => {
    setError("");
    setOutputPath(null);
    setSavedToLibrary(false);
    if (placements.length === 0) {
      setError("Auto-arrange or place at least one photo first.");
      return;
    }
    if (saveTo === "folder" && !dest.trim()) {
      setError("Choose an output folder.");
      return;
    }
    const height = Math.round(width / ratio);
    const opts: FreeformOptions = { width, height, background, borderWidth, cornerRadius };
    setBusy(true);
    try {
      if (saveTo === "library") {
        await saveCollageToCatalog(api, placements, opts, format, layoutKind);
        api.notifyChange(); // the new collage appears in the library
        api.showToast("Collage saved to your library.");
        setSavedToLibrary(true);
      } else {
        const path = await makeCollageFreeform(api, placements, opts, format, dest.trim());
        setOutputPath(path);
      }
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const chooseFolder = async () => {
    const picked = await pickFolder(dest.trim() || undefined);
    if (picked) setDest(picked);
  };

  const sorted = [...placements].sort((a, b) => a.z - b.z);

  return (
    // No backdrop click-to-close — an errant click (e.g. ending a drag outside the canvas)
    // shouldn't discard the layout. Use the Close button.
    <div className="modal-backdrop">
      <div className="modal collage-modal">
        <div className="modal-header">
          <div className="modal-title">Make collage</div>
          <button className="chip" onClick={onClose}>
            Close
          </button>
        </div>

        {order.length < 2 ? (
          <div className="modal-body">
            <div className="modal-sub">
              Select at least 2 photos in the library, then open Make collage again.
            </div>
          </div>
        ) : (
          <div className="modal-body">
            <div className="field">
              <div className="row" style={{ justifyContent: "space-between" }}>
                <label>Canvas</label>
                <div className="row">
                  <select
                    className="folder-input"
                    value=""
                    onChange={(e) => {
                      if (e.target.value) applyTemplate(e.target.value);
                      e.target.value = "";
                    }}
                    title="Apply a layout template"
                  >
                    <option value="">Template…</option>
                    {COLLAGE_TEMPLATES.map((t) => (
                      <option key={t.id} value={t.id}>
                        {t.label}
                      </option>
                    ))}
                  </select>
                  <button className="chip" onClick={autoArrange} disabled={arranging}>
                    {arranging ? "Arranging…" : "Auto-arrange"}
                  </button>
                  <button className="chip" onClick={bringToFront} disabled={selected == null}>
                    Front
                  </button>
                  <button className="chip" onClick={sendToBack} disabled={selected == null}>
                    Back
                  </button>
                  <label className="term-export" title="Fixed slots; drag a photo onto another to swap">
                    <input type="checkbox" checked={locked} onChange={(e) => setLocked(e.target.checked)} />
                    Lock layout
                  </label>
                </div>
              </div>
              <span className="term-note">
                {locked ? (
                  <>
                    Locked: <strong>drag a photo onto another to swap</strong> (drop onto the
                    big cell to set the feature) · <strong>Shift-drag</strong> to reposition ·{" "}
                    <strong>scroll</strong> to zoom. Uncheck Lock layout to move/resize freely.
                  </>
                ) : (
                  <>
                    Drag to move · corner to resize · <strong>Shift-drag</strong> to reposition the
                    photo in its frame · <strong>scroll</strong> to zoom · tiles can overlap.
                  </>
                )}
              </span>
              <div className="collage-canvas-wrap">
                <div
                  ref={canvasRef}
                  className="collage-canvas"
                  style={{ width: dispW, height: dispH, background }}
                  onPointerDown={() => setSelected(null)}
                >
                  {sorted.map((p) => {
                    const photo = photoById.get(p.photoId);
                    if (!photo) return null;
                    return (
                      <div
                        key={p.photoId}
                        className={`collage-place ${selected === p.photoId ? "collage-place-sel" : ""} ${
                          swapTarget === p.photoId ? "collage-place-swap" : ""
                        }`}
                        style={{
                          left: p.x * dispW,
                          top: p.y * dispH,
                          width: p.w * dispW,
                          height: p.h * dispH,
                        }}
                        onPointerDown={(e) =>
                          startGesture(e, p, e.shiftKey ? "pan" : locked ? "swap" : "move")
                        }
                        onWheel={(e) => {
                          e.preventDefault();
                          e.stopPropagation();
                          setSelected(p.photoId);
                          const factor = Math.exp(-e.deltaY * 0.0015);
                          update(p.photoId, { zoom: clamp(p.zoom * factor, 1, 6) });
                        }}
                      >
                        <CanvasTile
                          photo={photo}
                          boxW={p.w * dispW}
                          boxH={p.h * dispH}
                          zoom={p.zoom}
                          ox={p.ox}
                          oy={p.oy}
                          onAspect={(a) => aspectMap.current.set(p.photoId, a)}
                        />
                        {!locked && selected === p.photoId && (
                          <span
                            className="collage-resize"
                            onPointerDown={(e) => startGesture(e, p, "resize")}
                          />
                        )}
                      </div>
                    );
                  })}
                </div>
              </div>
            </div>

            <div className="field">
              <label>Aspect</label>
              <div className="row">
                <select className="folder-input" value={aspect} onChange={(e) => setAspect(e.target.value)}>
                  {ASPECTS.map((a) => (
                    <option key={a.value} value={a.value}>
                      {a.value}
                    </option>
                  ))}
                </select>
                <select className="folder-input" value={width} onChange={(e) => setWidth(Number(e.target.value))}>
                  {WIDTH_PRESETS.map((w) => (
                    <option key={w} value={w}>
                      {w}px wide
                    </option>
                  ))}
                </select>
              </div>
              <span className="term-note">Output {width}×{Math.round(width / ratio)}.</span>
            </div>

            <div className="field">
              <label>Background</label>
              <input
                type="color"
                value={/^#[0-9a-fA-F]{6}$/.test(background) ? background : "#ffffff"}
                onChange={(e) => setBackground(e.target.value)}
              />
            </div>

            <div className="row">
              <div className="field" style={{ flex: 1 }}>
                <label>Border (px)</label>
                <input
                  className="folder-input"
                  type="number"
                  min="0"
                  max="64"
                  value={borderWidth}
                  onChange={(e) => setBorderWidth(Number(e.target.value))}
                />
              </div>
              <div className="field" style={{ flex: 1 }}>
                <label>Corner radius (px)</label>
                <input
                  className="folder-input"
                  type="number"
                  min="0"
                  max="128"
                  value={cornerRadius}
                  onChange={(e) => setCornerRadius(Number(e.target.value))}
                />
              </div>
            </div>

            <div className="field">
              <label>Format</label>
              <div className="row">
                <label className="term-export">
                  <input
                    type="radio"
                    name="collage-format"
                    checked={format === "jpeg"}
                    onChange={() => setFormat("jpeg")}
                  />
                  JPEG
                </label>
                <label className="term-export">
                  <input
                    type="radio"
                    name="collage-format"
                    checked={format === "png"}
                    onChange={() => setFormat("png")}
                  />
                  PNG (alpha)
                </label>
              </div>
              {needsPng && format !== "png" && (
                <span className="term-note">
                  Rounded corners need PNG — JPEG flattens them onto the background color.
                </span>
              )}
            </div>

            <div className="field">
              <label>Save to</label>
              <div className="row">
                <label className="term-export">
                  <input
                    type="radio"
                    name="collage-saveto"
                    checked={saveTo === "library"}
                    onChange={() => setSaveTo("library")}
                  />
                  Library (catalog)
                </label>
                <label className="term-export">
                  <input
                    type="radio"
                    name="collage-saveto"
                    checked={saveTo === "folder"}
                    onChange={() => setSaveTo("folder")}
                  />
                  Folder
                </label>
              </div>
              {saveTo === "folder" && (
                <div className="row">
                  <input
                    className="folder-input"
                    value={dest}
                    onChange={(e) => setDest(e.target.value)}
                    placeholder="~/Pictures/Export"
                  />
                  <button className="chip" onClick={chooseFolder}>
                    Browse…
                  </button>
                </div>
              )}
            </div>

            <div className="row">
              <button className="scan-btn" onClick={run} disabled={busy || placements.length === 0}>
                {busy
                  ? saveTo === "library"
                    ? "Saving…"
                    : "Rendering…"
                  : saveTo === "library"
                    ? "Save to library"
                    : "Render"}
              </button>
            </div>

            {savedToLibrary && (
              <div className="modal-sub">Saved to your library. Close to see it in the grid.</div>
            )}
            {outputPath && (
              <div className="modal-sub">
                Saved to <code>{outputPath}</code>{" "}
                <button className="chip" onClick={() => revealInFolder(outputPath)}>
                  Reveal
                </button>
              </div>
            )}
            {error && <div className="modal-error">{error}</div>}
          </div>
        )}
      </div>
    </div>
  );
}
