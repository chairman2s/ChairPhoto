import { useEffect, useMemo, useRef, useState } from "react";
import type { ChairPhotoAPI, Photo } from "../registry";
import { getThumbnail, pickFolder, revealInFolder } from "../api";

// ── Backend commands (owned by this module) ───────────────────────────────────
// Per the module contract, a module's own commands go through `ChairPhotoAPI.invoke`
// rather than core's `api.ts`, so the command names travel with the module.

/**
 * Slideshow render options (camelCase, matching the backend serde DTO). `width`/`height` come
 * from the chosen aspect/resolution preset (16:9 1080p = 1920×1080, 16:9 4K = 3840×2160,
 * 1:1 = 1080×1080, 9:16 = 1080×1920). `transitionDuration` is only used when `transition` is on.
 * See docs/slideshow.md.
 */
interface SlideshowOptions {
  /** Seconds each photo is shown. */
  durationPerPhoto: number;
  /** Crossfade between photos (ffmpeg xfade). */
  transition: boolean;
  /** Crossfade length in seconds (ignored when transition is off). */
  transitionDuration: number;
  /** Slow pan/zoom per photo (ffmpeg zoompan). */
  kenBurns: boolean;
  /** Output frame rate (default 30). */
  fps: number;
  /** Output pixel dimensions (from the preset). */
  width: number;
  height: number;
}

interface SlideshowProgress {
  done: number;
  total: number;
}

/**
 * Render the given photos (in `photoIds` order = play order) into an `.mp4` slideshow written
 * into `destDir`. Returns the absolute output path (collision-safe; `~` expanded). Runs the
 * frame export + ffmpeg encode off the UI thread, streaming `slideshow:progress` events.
 * Requires the `slideshow` backend feature (and ffmpeg on PATH).
 */
const makeSlideshow = (
  api: ChairPhotoAPI,
  photoIds: number[],
  opts: SlideshowOptions,
  destDir: string,
) => api.invoke<string>("make_slideshow", { photoIds, opts, destDir });

// The Slideshow settings dialog (H11, see docs/slideshow.md). Reads the host's current
// selection, lets the user reorder the photos by drag, pick the per-photo duration, a
// resolution/aspect preset, optional crossfade + Ken Burns, and the frame rate, then renders
// to an .mp4 off the UI thread (ffmpeg) with a progress bar driven by `slideshow:progress`. On
// success it shows the output path with a reveal affordance. A slideshow needs ≥2 photos.

// Orientation (for mobile use, pick Portrait) × resolution → the backend's width/height.
const ORIENTATIONS = [
  { value: "landscape", label: "Landscape (16:9)" },
  { value: "portrait", label: "Portrait (9:16)" },
  { value: "square", label: "Square (1:1)" },
] as const;
const RESOLUTIONS = [
  { value: "hd", label: "1080p (HD)", base: 1080 },
  { value: "4k", label: "4K", base: 2160 },
] as const;
type Orientation = (typeof ORIENTATIONS)[number]["value"];
type Resolution = (typeof RESOLUTIONS)[number]["value"];

// base = the short side (1080 / 2160); the long side is 16:9 of it (integer, since both
// bases divide by 9). Portrait swaps width/height; square is base × base.
function videoDims(orientation: Orientation, resolution: Resolution): { width: number; height: number } {
  const base = RESOLUTIONS.find((r) => r.value === resolution)?.base ?? 1080;
  if (orientation === "square") return { width: base, height: base };
  const long = Math.round((base * 16) / 9);
  return orientation === "portrait" ? { width: base, height: long } : { width: long, height: base };
}

/** A single reorderable thumbnail tile (loads its preview lazily via the cache). */
function TileThumb({ photoId }: { photoId: number }) {
  const [src, setSrc] = useState("");
  useEffect(() => {
    let alive = true;
    getThumbnail(photoId)
      .then((s) => alive && setSrc(s))
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [photoId]);
  return src ? (
    <img className="collage-tile-img" src={src} alt="" draggable={false} />
  ) : (
    <div className="collage-tile-img collage-tile-empty" />
  );
}

export function SlideshowDialog({ api, onClose }: { api: ChairPhotoAPI; onClose: () => void }) {
  // Snapshot the selection once on open so the play order is stable while the dialog lives.
  const initial = useMemo<Photo[]>(() => api.getSelectedPhotos(), [api]);
  const [order, setOrder] = useState<Photo[]>(initial);
  const dragIndex = useRef<number | null>(null);

  const [durationPerPhoto, setDurationPerPhoto] = useState(4);
  const [transition, setTransition] = useState(true);
  const [transitionDuration, setTransitionDuration] = useState(1);
  const [kenBurns, setKenBurns] = useState(false);
  const [fps, setFps] = useState(30);
  const [orientation, setOrientation] = useState<Orientation>("landscape");
  const [resolution, setResolution] = useState<Resolution>("hd");
  const [dest, setDest] = useState("~/Videos");

  const [busy, setBusy] = useState(false);
  const [progress, setProgress] = useState<{ done: number; total: number } | null>(null);
  /** True once a `slideshow:progress` listener is actually live, so the determinate bar
   *  is shown only when updates can really arrive — not merely when `onEvent` exists. */
  const [progressLive, setProgressLive] = useState(false);
  const [outputPath, setOutputPath] = useState<string | null>(null);
  const [error, setError] = useState("");

  const reorder = (from: number, to: number) => {
    if (from === to) return;
    setOrder((prev) => {
      const next = [...prev];
      const [moved] = next.splice(from, 1);
      next.splice(to, 0, moved);
      return next;
    });
  };

  const run = async () => {
    setError("");
    setOutputPath(null);
    setProgress(null);
    if (!dest.trim()) {
      setError("Choose an output folder.");
      return;
    }
    const { width, height } = videoDims(orientation, resolution);
    const opts: SlideshowOptions = {
      durationPerPhoto,
      transition,
      transitionDuration,
      kenBurns,
      fps,
      width,
      height,
    };
    setBusy(true);
    setProgressLive(false);
    // Declared before the try so `finally` always releases it.
    let unlisten: (() => void) | null = null;
    try {
      // Progress is nonessential: subscribe inside its own try so neither a host
      // predating `onEvent` nor a failed registration can abort the render. Either way
      // the encode proceeds and the UI falls back to an indeterminate "Rendering…".
      try {
        unlisten =
          (await api.onEvent?.<SlideshowProgress>("slideshow:progress", (p) =>
            setProgress(p),
          )) ?? null;
        if (unlisten) setProgressLive(true);
      } catch {
        unlisten = null;
      }

      const path = await makeSlideshow(
        api,
        order.map((p) => p.id),
        opts,
        dest.trim(),
      );
      setOutputPath(path);
    } catch (e) {
      setError(String(e));
    } finally {
      unlisten?.();
      setProgressLive(false);
      setBusy(false);
    }
  };

  const chooseFolder = async () => {
    const picked = await pickFolder(dest.trim() || undefined);
    if (picked) setDest(picked);
  };

  const pct =
    progress && progress.total > 0
      ? Math.min(100, Math.round((progress.done / progress.total) * 100))
      : 0;

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal slideshow-modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <div className="modal-title">Make slideshow</div>
          <button className="chip" onClick={onClose}>
            Close
          </button>
        </div>

        {order.length < 2 ? (
          <div className="modal-body">
            <div className="modal-sub">
              Select at least 2 photos in the library, then open Make slideshow again.
            </div>
          </div>
        ) : (
          <div className="modal-body">
            <div className="field">
              <label>Photos ({order.length}) — drag to reorder</label>
              <div className="collage-tiles">
                {order.map((p, i) => (
                  <div
                    key={p.id}
                    className="collage-tile"
                    draggable
                    onDragStart={() => (dragIndex.current = i)}
                    onDragOver={(e) => e.preventDefault()}
                    onDrop={(e) => {
                      e.preventDefault();
                      if (dragIndex.current != null) reorder(dragIndex.current, i);
                      dragIndex.current = null;
                    }}
                    title={p.path.split("/").pop()}
                  >
                    <TileThumb photoId={p.id} />
                    <span className="collage-tile-idx">{i + 1}</span>
                  </div>
                ))}
              </div>
            </div>

            <div className="field">
              <label>Duration per photo {durationPerPhoto}s</label>
              <input
                type="range"
                min="1"
                max="15"
                step="0.5"
                value={durationPerPhoto}
                onChange={(e) => setDurationPerPhoto(Number(e.target.value))}
              />
            </div>

            <div className="field">
              <label>Orientation &amp; resolution</label>
              <div className="row">
                <select
                  className="folder-input"
                  value={orientation}
                  onChange={(e) => setOrientation(e.target.value as Orientation)}
                >
                  {ORIENTATIONS.map((o) => (
                    <option key={o.value} value={o.value}>
                      {o.label}
                    </option>
                  ))}
                </select>
                <select
                  className="folder-input"
                  value={resolution}
                  onChange={(e) => setResolution(e.target.value as Resolution)}
                >
                  {RESOLUTIONS.map((r) => (
                    <option key={r.value} value={r.value}>
                      {r.label}
                    </option>
                  ))}
                </select>
              </div>
              <span className="term-note">
                {(() => {
                  const d = videoDims(orientation, resolution);
                  return `Output ${d.width}×${d.height}. Portrait is for mobile (Snapchat/Stories). Photos are rotated upright and letterboxed to fit (never cropped).`;
                })()}
              </span>
            </div>

            <div className="field">
              <label>
                <input
                  type="checkbox"
                  checked={transition}
                  onChange={(e) => setTransition(e.target.checked)}
                />{" "}
                Crossfade transitions
              </label>
              {transition && (
                <div className="field" style={{ marginTop: 6 }}>
                  <label>Transition duration {transitionDuration}s</label>
                  <input
                    type="range"
                    min="0.2"
                    max="3"
                    step="0.1"
                    value={transitionDuration}
                    onChange={(e) => setTransitionDuration(Number(e.target.value))}
                  />
                </div>
              )}
            </div>

            <div className="field">
              <label>
                <input
                  type="checkbox"
                  checked={kenBurns}
                  onChange={(e) => setKenBurns(e.target.checked)}
                />{" "}
                Ken Burns (slow pan/zoom)
              </label>
            </div>

            <div className="field">
              <label>Frame rate</label>
              <div className="row">
                {[24, 30, 60].map((f) => (
                  <label className="term-export" key={f}>
                    <input
                      type="radio"
                      name="slideshow-fps"
                      checked={fps === f}
                      onChange={() => setFps(f)}
                    />
                    {f} fps
                  </label>
                ))}
              </div>
            </div>

            <div className="field">
              <label>Output folder</label>
              <div className="row">
                <input
                  className="folder-input"
                  value={dest}
                  onChange={(e) => setDest(e.target.value)}
                  placeholder="~/Videos"
                />
                <button className="chip" onClick={chooseFolder}>
                  Browse…
                </button>
              </div>
            </div>

            <div className="row">
              <button className="scan-btn" onClick={run} disabled={busy || order.length < 2}>
                {busy ? "Rendering…" : "Render"}
              </button>
            </div>

            {busy && (
              <div className="field">
                {progressLive && (
                  <div className="slideshow-progress">
                    <div className="slideshow-progress-bar" style={{ width: `${pct}%` }} />
                  </div>
                )}
                <span className="term-note">
                  {!progressLive
                    ? "Rendering…"
                    : progress
                      ? `Encoding… ${pct}%`
                      : "Preparing frames…"}
                </span>
              </div>
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
