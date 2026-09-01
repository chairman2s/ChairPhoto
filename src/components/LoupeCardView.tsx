// The pop-out loupe's card view: what a module asked it to show in place of the selected
// photo (ChairPhotoAPI.showInLoupe) — a heading, a few stats and related entries, and a
// wall of the photos in the card's scope, fetched here a page at a time so the card itself
// stays small. Click a tile to view that photo full-size; Back (or Esc) returns to the wall.
import { useEffect, useState } from "react";
import { listPhotos, thumbnailUrl } from "../modules/api";
import type { LoupeCard, Photo } from "../modules/registry";
import { ZoomableImage } from "./ZoomableImage";

const PAGE = 48;

const fileName = (p: Photo) => p.path.split("/").pop() ?? p.path;

export function LoupeCardView({ card }: { card: LoupeCard }) {
  const [photos, setPhotos] = useState<Photo[]>([]);
  const [total, setTotal] = useState(0);
  const [loading, setLoading] = useState(false);
  const [viewing, setViewing] = useState<number | null>(null);
  // The scope as a value, so a new card object with the same scope keeps its wall.
  const scopeKey = JSON.stringify(card.photos ?? null);

  // First page whenever the scope changes; `loadMore` appends the rest on demand.
  useEffect(() => {
    setPhotos([]);
    setTotal(0);
    setViewing(null);
    const scope = card.photos;
    if (!scope) return;
    let alive = true;
    setLoading(true);
    listPhotos({ ...scope, window: { offset: 0, limit: PAGE } })
      .then((page) => {
        if (!alive) return;
        setPhotos(page.photos);
        setTotal(page.total);
      })
      .catch(() => {
        if (!alive) return;
        setPhotos([]);
        setTotal(0);
      })
      .finally(() => {
        if (alive) setLoading(false);
      });
    return () => {
      alive = false;
    };
    // The scope is what matters, and scopeKey is its value.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [scopeKey]);

  const loadMore = () => {
    const scope = card.photos;
    if (!scope || loading) return;
    setLoading(true);
    listPhotos({ ...scope, window: { offset: photos.length, limit: PAGE } })
      .then((page) => {
        setPhotos((prev) => [...prev, ...page.photos]);
        setTotal(page.total);
      })
      .catch(() => {})
      .finally(() => setLoading(false));
  };

  // Esc backs out of a full-size photo.
  useEffect(() => {
    if (viewing == null) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setViewing(null);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [viewing]);

  if (viewing != null) {
    const p = photos.find((x) => x.id === viewing);
    return (
      <div className="loupe-card loupe-card-viewing">
        <div className="loupe-card-bar">
          <button className="loupe-card-back" onClick={() => setViewing(null)}>
            ← {card.title}
          </button>
          <span className="loupe-card-bar-name">{p ? fileName(p) : ""}</span>
        </div>
        <div className="loupe-card-photo">
          <ZoomableImage photoId={viewing} />
        </div>
      </div>
    );
  }

  const wallHead = total
    ? `${total.toLocaleString()} photo${total === 1 ? "" : "s"}`
    : loading
      ? "Loading…"
      : "No photos";

  return (
    <div className="loupe-card">
      <div className="loupe-card-head">
        <div className="loupe-card-title">
          {card.color && <span className="loupe-card-dot" style={{ background: card.color }} />}
          <span>{card.title}</span>
        </div>
        {card.subtitle && <div className="loupe-card-sub">{card.subtitle}</div>}
        {card.chips && card.chips.length > 0 && (
          <div className="loupe-card-chips">
            {card.chips.map((c) => (
              <span key={c} className="loupe-card-chip">
                {c}
              </span>
            ))}
          </div>
        )}
        {card.stats && card.stats.length > 0 && (
          <div className="loupe-card-stats">
            {card.stats.map((s) => (
              <div key={s.label} className="loupe-card-stat">
                <div className="loupe-card-stat-n">
                  {typeof s.value === "number" ? s.value.toLocaleString() : s.value}
                </div>
                <div className="loupe-card-stat-l">{s.label}</div>
              </div>
            ))}
          </div>
        )}
        {card.related && card.related.length > 0 && (
          <>
            <div className="loupe-card-h">Connected</div>
            <div className="loupe-card-related">
              {card.related.map((r, i) => (
                <span key={i} className="loupe-card-chip">
                  {r.color && <i style={{ background: r.color }} />}
                  {r.label}
                  {r.detail && <em>{r.detail}</em>}
                </span>
              ))}
            </div>
          </>
        )}
      </div>

      {card.photos && (
        <>
          <div className="loupe-card-h">{wallHead}</div>
          <div className="loupe-card-wall">
            {photos.map((p) => (
              <button
                key={p.id}
                className="loupe-card-tile"
                onClick={() => setViewing(p.id)}
                title={fileName(p)}
              >
                <img src={thumbnailUrl(p.id)} alt="" loading="lazy" />
              </button>
            ))}
          </div>
          {photos.length < total && (
            <button className="loupe-card-more" onClick={loadMore} disabled={loading}>
              {loading
                ? "Loading…"
                : `Show more (${(total - photos.length).toLocaleString()} left)`}
            </button>
          )}
        </>
      )}
    </div>
  );
}
