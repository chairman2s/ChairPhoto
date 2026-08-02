import { useEffect, useRef, useState } from "react";
import { previewUrl, zoomUrl } from "../modules/previewCache";

// Loupe image with zoom + pan.
//
// At fit (scale 1) it shows the fast 2048 preview. As soon as you zoom in it swaps
// to the full-resolution embedded preview (`zoom://`) so detail is real, not an
// upscaled thumbnail. Controls: scroll wheel zooms toward the cursor, drag pans,
// double-click toggles between fit and 100% (1 image pixel per screen pixel).
export function ZoomableImage({
  photoId,
  srcOverride,
  hiSrcOverride,
  unavailableActions,
  bust,
}: {
  photoId: number;
  /** When set (e.g. an edited version's rendered data URL), shown instead of the
   *  preview/zoom protocol images. Zoom upscales this single source. */
  srcOverride?: string;
  /** Lazily fetch a higher-res render of the override source (e.g. the version
   *  rendered from the native-size preview) — requested once on first zoom-in,
   *  mirroring the preview → zoom:// swap for unedited photos. */
  hiSrcOverride?: () => Promise<string>;
  /** Buttons (relocate / retrieve / remove) shown when the image can't be loaded. */
  unavailableActions?: React.ReactNode;
  /** Cache-bust nonce — bumped after a rotation so the loupe re-fetches. */
  bust?: number;
}) {
  const containerRef = useRef<HTMLDivElement>(null);
  const imgRef = useRef<HTMLImageElement>(null);
  const dragRef = useRef<{ x: number; y: number; tx: number; ty: number } | null>(null);
  const want100Ref = useRef(false);

  const [scale, setScale] = useState(1);
  const [tx, setTx] = useState(0);
  const [ty, setTy] = useState(0);
  const [hi, setHi] = useState(false); // use full-res source once zoomed
  const [failed, setFailed] = useState(false);
  // Hi-res render of srcOverride ("" = not loaded); fetch failure falls back to
  // zooming the fast render rather than blocking zoom entirely.
  const [hiOverride, setHiOverride] = useState("");
  const hiOverrideFailed = useRef(false);

  // Reset view when the photo (or the overridden source, e.g. version) changes.
  useEffect(() => {
    setScale(1);
    setTx(0);
    setTy(0);
    setHi(false);
    setFailed(false);
    setHiOverride("");
    hiOverrideFailed.current = false;
    want100Ref.current = false;
  }, [photoId, srcOverride, bust]);

  // First zoom-in on an override source: fetch its hi-res render once.
  useEffect(() => {
    if (!hi || !srcOverride || !hiSrcOverride || hiOverride || hiOverrideFailed.current) {
      return;
    }
    let cancelled = false;
    hiSrcOverride()
      .then((url) => {
        if (!cancelled && url) setHiOverride(url);
      })
      .catch(() => {
        hiOverrideFailed.current = true;
      });
    return () => {
      cancelled = true;
    };
  }, [hi, srcOverride, hiOverride, hiSrcOverride]);

  // 100% = one image pixel per screen pixel = max(naturalW/containerW, naturalH/containerH).
  // Falls back to 8x until the full-res image (with its true natural size) is loaded.
  // An override source counts as full-res once its hi render is in (or none can come —
  // no fetcher / fetch failed — in which case the fast render is the best we have).
  const hires = srcOverride
    ? !!hiOverride || !hiSrcOverride || hiOverrideFailed.current
    : hi;

  const maxScale = () => {
    const img = imgRef.current;
    const c = containerRef.current;
    if (hires && img && c && img.naturalWidth) {
      return Math.max(img.naturalWidth / c.clientWidth, img.naturalHeight / c.clientHeight);
    }
    return 8;
  };

  const zoomToward = (clientX: number, clientY: number, factor: number) => {
    const c = containerRef.current;
    if (!c) return;
    const rect = c.getBoundingClientRect();
    const cx = clientX - rect.left - rect.width / 2;
    const cy = clientY - rect.top - rect.height / 2;
    // Floor the ceiling at 4x for override sources: a tightly-cropped version can
    // have fewer pixels than the window (its 100% is below fit), which would clamp
    // max to 1 and leave zoom completely dead. Some magnification beats none.
    const max = Math.max(maxScale(), srcOverride ? 4 : 1);
    setScale((prevS) => {
      const next = Math.min(Math.max(prevS * factor, 1), max);
      if (next === prevS) return prevS;
      if (next <= 1.0001) {
        setTx(0);
        setTy(0);
      } else {
        setTx((prevTx) => cx - (next / prevS) * (cx - prevTx));
        setTy((prevTy) => cy - (next / prevS) * (cy - prevTy));
        setHi(true);
      }
      return next;
    });
  };

  const onWheel = (e: React.WheelEvent) => {
    e.preventDefault();
    zoomToward(e.clientX, e.clientY, e.deltaY < 0 ? 1.15 : 1 / 1.15);
  };

  // Snap to 100% centered on a point (or container center).
  const applyHundred = (clientX?: number, clientY?: number) => {
    const img = imgRef.current;
    const c = containerRef.current;
    if (!img || !c || !img.naturalWidth) return;
    // Never snap below fit — a small override source's 100% can be under 1.
    const s = Math.max(
      img.naturalWidth / c.clientWidth,
      img.naturalHeight / c.clientHeight,
      1,
    );
    const rect = c.getBoundingClientRect();
    const cx = (clientX ?? rect.left + rect.width / 2) - rect.left - rect.width / 2;
    const cy = (clientY ?? rect.top + rect.height / 2) - rect.top - rect.height / 2;
    want100Ref.current = false;
    setScale(s);
    setTx(cx * (1 - s));
    setTy(cy * (1 - s));
  };

  const onDoubleClick = (e: React.MouseEvent) => {
    if (scale > 1.0001) {
      setScale(1);
      setTx(0);
      setTy(0);
    } else if (hires && imgRef.current?.naturalWidth) {
      applyHundred(e.clientX, e.clientY);
    } else {
      // Need the full-res image first; apply 100% once it loads.
      want100Ref.current = true;
      setHi(true);
    }
  };

  const onImgLoad = () => {
    if (want100Ref.current) applyHundred();
  };

  const onMouseDown = (e: React.MouseEvent) => {
    if (scale <= 1) return;
    dragRef.current = { x: e.clientX, y: e.clientY, tx, ty };
  };
  const onMouseMove = (e: React.MouseEvent) => {
    const d = dragRef.current;
    if (!d) return;
    setTx(d.tx + (e.clientX - d.x));
    setTy(d.ty + (e.clientY - d.y));
  };
  const endDrag = () => {
    dragRef.current = null;
  };

  if (failed)
    return (
      <div className="loupe-empty">
        <div className="loupe-empty-title">This photo isn’t available right now</div>
        <div className="loupe-empty-hint">
          Its file couldn’t be loaded — it may be on the NAS (connect it), moved, or gone.
        </div>
        {unavailableActions}
      </div>
    );

  return (
    <div
      ref={containerRef}
      className="zoom-container"
      onWheel={onWheel}
      onDoubleClick={onDoubleClick}
      onMouseDown={onMouseDown}
      onMouseMove={onMouseMove}
      onMouseUp={endDrag}
      onMouseLeave={endDrag}
      style={{ cursor: scale > 1 ? "grab" : "default" }}
    >
      <img
        ref={imgRef}
        className="zoom-img"
        src={
          srcOverride
            ? hi && hiOverride
              ? hiOverride
              : srcOverride
            : hi
              ? zoomUrl(photoId, bust)
              : previewUrl(photoId, bust)
        }
        onLoad={onImgLoad}
        onError={() => setFailed(true)}
        draggable={false}
        alt=""
        style={{ transform: `translate(${tx}px, ${ty}px) scale(${scale})` }}
      />
      {scale > 1.001 && (
        <button
          className="zoom-fit"
          title="Fit to window"
          onMouseDown={(e) => e.stopPropagation()}
          onClick={(e) => {
            e.stopPropagation();
            setScale(1);
            setTx(0);
            setTy(0);
            setHi(false);
            want100Ref.current = false;
          }}
        >
          Fit {Math.round(scale * 100)}%
        </button>
      )}
    </div>
  );
}
