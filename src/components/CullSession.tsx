import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { Photo } from "../modules/registry";
import {
  getSetting,
  setLabel,
  setPickState,
  setRating,
  setSetting,
} from "../modules/api";
import { PreviewImage } from "./PreviewImage";
import { prefetch } from "../modules/previewCache";
import { COLOR_LABELS } from "../modules/labels";

// Cull session (C4): one photo, full screen, keyboard only, resumable.
//
// The keymap is the app's existing culling keymap, unchanged — 0-5, p/x/u, the colour
// letters, arrows — because the whole value of a session mode is muscle memory, and a
// second dialect of the same shortcuts would destroy it. What the session adds is the
// things a grid cannot have: nothing else on screen, a cursor that survives closing the
// app, and an account of what the session actually did.
//
// Two structural decisions:
//
//   * **The id list is frozen at the start**, like Compare freezes its own. Culling changes
//     the very fields the view is usually filtered by, so a live list would delete photos
//     out from under the cursor the moment you rated one — the position would jump and
//     frames would be skipped unseen.
//   * **Decisions are held locally and the grid is refreshed once, on exit.** The grid
//     handler refreshes after every keystroke; at six-figure library scale that is a full
//     re-query per keypress, and it is what would make a fast session stutter. The HUD
//     reads this session's own record of what it applied, so it stays correct without it.

const COLOR_KEYS: Record<string, string> = {
  r: "Red",
  y: "Yellow",
  g: "Green",
  b: "Blue",
  v: "Purple",
  n: "",
};

const LABEL_COLORS: Record<string, string> = Object.fromEntries(
  COLOR_LABELS.map((l) => [l.name, l.color]),
);

/** Where the cursor lives. Catalog settings, not localStorage: a photo id means nothing
 *  outside the catalog it came from, and the owner has more than one catalog. */
const CURSOR_KEY = "cull.cursor.photo_id";

/** What this session did to one photo. Absent = visited without deciding. */
interface Decision {
  rating?: number;
  pick?: "pick" | "reject" | "none";
  label?: string;
}

export interface CullStats {
  /** Photos the cursor stopped on. */
  visited: number;
  /** Of those, how many got any decision. */
  decided: number;
  picked: number;
  rejected: number;
  rated: number;
  labelled: number;
  /** Photos in the set the cursor never reached. */
  remaining: number;
  elapsedSecs: number;
}

export function CullSession({
  photos,
  onExit,
}: {
  /** The set to cull, in order. Frozen by the caller for the life of the session. */
  photos: Photo[];
  /** Called when the session ends, after the cursor has been persisted. */
  onExit: (stats: CullStats) => void;
}) {
  const [at, setAt] = useState<number | null>(null);
  // The cursor is authoritative in a ref, not in render state. Keys arrive faster than
  // React re-renders — hold `3` then `x` and both keydowns run before either paints — so a
  // handler reading the rendered index would apply both decisions to the same photo and
  // step past the next one unseen. The ref is updated synchronously by every move, so each
  // keypress sees where the previous one left the cursor.
  const atRef = useRef<number | null>(null);
  const [resumeNote, setResumeNote] = useState<string | null>(null);
  const [decisions, setDecisions] = useState<Map<number, Decision>>(new Map());
  const [visited, setVisited] = useState<Set<number>>(new Set());
  const [showHelp, setShowHelp] = useState(false);
  const [summary, setSummary] = useState<CullStats | null>(null);
  /** A decision that did not reach the catalog, shown rather than swallowed. */
  const [failure, setFailure] = useState<string | null>(null);
  const startedAt = useRef(Date.now());

  const setCursor = useCallback((next: number | null) => {
    atRef.current = next;
    setAt(next);
  }, []);

  // ── Resume ────────────────────────────────────────────────────────────────
  // The stored cursor is a photo, not an index: the set differs between sessions, so an
  // index would silently point at a different frame.
  useEffect(() => {
    let live = true;
    getSetting(CURSOR_KEY)
      .then((raw) => {
        if (!live) return;
        const savedId = raw ? Number(raw) : NaN;
        const idx = Number.isFinite(savedId) ? photos.findIndex((p) => p.id === savedId) : -1;
        if (idx >= 0) {
          setCursor(idx);
          setResumeNote(`Resumed where you left off — photo ${idx + 1} of ${photos.length}.`);
        } else {
          setCursor(0);
          if (raw) {
            setResumeNote(
              "The photo you left off at isn't in this set — starting from the beginning.",
            );
          }
        }
      })
      .catch(() => {
        if (live) setCursor(0);
      });
    return () => {
      live = false;
    };
  }, [photos, setCursor]);

  const current = at != null ? photos[at] : undefined;

  // Mark the current photo visited. Visiting is what "reviewed" means here — a photo you
  // looked at and left alone was still culled, and counting only decisions would say
  // otherwise.
  useEffect(() => {
    if (!current) return;
    setVisited((v) => (v.has(current.id) ? v : new Set(v).add(current.id)));
  }, [current]);

  // Preload ahead of the cursor, as the grid does: further forward than back, because
  // culling moves forward (the AGENTS.md preload invariant).
  useEffect(() => {
    if (at == null) return;
    for (let d = 1; d <= 5; d++) prefetch(photos[at + d]?.id);
    prefetch(photos[at - 1]?.id);
  }, [at, photos]);

  // Persist the cursor, trailing-debounced: a session is a burst of keypresses, and one
  // settings write per frame is a write per keystroke for no gain.
  useEffect(() => {
    if (!current) return;
    const t = setTimeout(() => {
      setSetting(CURSOR_KEY, String(current.id)).catch(() => {});
    }, 400);
    return () => clearTimeout(t);
  }, [current]);

  const stats = useCallback((): CullStats => {
    const ds = [...decisions.values()];
    return {
      visited: visited.size,
      decided: ds.length,
      picked: ds.filter((d) => d.pick === "pick").length,
      rejected: ds.filter((d) => d.pick === "reject").length,
      rated: ds.filter((d) => d.rating != null && d.rating > 0).length,
      labelled: ds.filter((d) => d.label != null && d.label !== "").length,
      remaining: photos.length - visited.size,
      elapsedSecs: Math.round((Date.now() - startedAt.current) / 1000),
    };
  }, [decisions, visited, photos.length]);

  const finish = useCallback(() => {
    const s = stats();
    // Write the cursor immediately rather than letting the debounce race the unmount.
    const cursor = current;
    const write = cursor
      ? setSetting(CURSOR_KEY, String(cursor.id)).catch(() => {})
      : Promise.resolve();
    write.then(() => setSummary(s));
  }, [stats, current]);

  const step = useCallback(
    (delta: number) => {
      const i = atRef.current;
      if (i == null) return;
      // Stop at the ends rather than wrapping: a session that silently loops makes
      // "did I see everything?" unanswerable.
      setCursor(Math.min(photos.length - 1, Math.max(0, i + delta)));
    },
    [photos.length, setCursor],
  );

  const record = useCallback((photoId: number, d: Decision) => {
    setDecisions((m) => {
      const next = new Map(m);
      next.set(photoId, { ...next.get(photoId), ...d });
      return next;
    });
  }, []);

  /// Apply one decision: record it, move on, and write it to the catalog in the background.
  ///
  /// Navigation is never gated on the write. A cull session is judged on how fast it feels,
  /// and awaiting SQLite before advancing puts a round-trip between the key and the next
  /// photo. The write is still watched: if it fails, the decision is taken back off the
  /// photo and said out loud, because a HUD showing a rating the catalog never received is
  /// worse than a visible error.
  const apply = useCallback(
    (target: Photo, decision: Decision, write: () => Promise<unknown>) => {
      record(target.id, decision);
      step(1);
      write().catch((err) => {
        setDecisions((m) => {
          const next = new Map(m);
          next.delete(target.id);
          return next;
        });
        setFailure(`${target.path.split("/").pop()}: ${err}`);
      });
    },
    [record, step],
  );

  // ── Keyboard ──────────────────────────────────────────────────────────────
  // The session owns the keyboard while it is open; App's grid handler stands down.
  //
  // The listener does not depend on the current photo — it reads the cursor ref — so it is
  // attached once for the session rather than swapped on every frame.
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      const target = e.target as HTMLElement | null;
      if (target && (target.tagName === "INPUT" || target.tagName === "TEXTAREA")) return;

      const key = e.key.toLowerCase();

      if (summary) {
        // The summary is the last thing between the session and the grid.
        if (e.key === "Escape" || e.key === "Enter") {
          e.preventDefault();
          onExit(summary);
        }
        return;
      }

      if (e.key === "Escape") {
        e.preventDefault();
        // A reflex Escape closes the help, not the session.
        if (showHelp) setShowHelp(false);
        else finish();
        return;
      }
      if (key === "h" || e.key === "?") {
        e.preventDefault();
        setShowHelp((v) => !v);
        return;
      }

      const i = atRef.current;
      const photo = i != null ? photos[i] : undefined;
      if (!photo) return;

      if (e.key === "ArrowRight" || e.key === "ArrowDown" || e.key === " ") {
        step(1);
      } else if (e.key === "ArrowLeft" || e.key === "ArrowUp") {
        step(-1);
      } else if (key >= "0" && key <= "5") {
        const rating = parseInt(key, 10);
        apply(photo, { rating }, () => setRating(photo.id, rating));
      } else if (key === "p" || key === "x" || key === "u") {
        const pick = key === "p" ? "pick" : key === "x" ? "reject" : "none";
        apply(photo, { pick }, () => setPickState(photo.id, pick));
      } else if (key in COLOR_KEYS) {
        const label = COLOR_KEYS[key];
        apply(photo, { label }, () => setLabel(photo.id, label));
      } else {
        return;
      }
      e.preventDefault();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [photos, step, apply, finish, onExit, showHelp, summary]);

  // What the photo looks like now: its row as frozen, overlaid with this session's own
  // decisions. Reading the frozen row alone would show a rating you just replaced.
  const shown = useMemo(() => {
    if (!current) return null;
    const d = decisions.get(current.id) ?? {};
    return {
      rating: d.rating ?? current.rating,
      pick: d.pick ?? current.pickState,
      label: d.label ?? current.label,
    };
  }, [current, decisions]);

  if (summary) {
    return <CullSummary stats={summary} total={photos.length} onDone={() => onExit(summary)} />;
  }

  if (at == null) {
    return (
      <div className="cull-session">
        <div className="cull-empty">Opening session…</div>
      </div>
    );
  }

  const atEnd = at === photos.length - 1;

  return (
    <div className="cull-session">
      <div className="cull-stage">
        <PreviewImage photoId={current?.id ?? null} />
      </div>

      <div className="cull-hud-top">
        <span className="cull-pos">
          {at + 1} / {photos.length}
        </span>
        <span className="cull-name">{current?.path.split("/").pop()}</span>
        {resumeNote && <span className="cull-note">{resumeNote}</span>}
      </div>

      <div className="cull-hud-bottom">
        <div className="cull-state">
          {shown && shown.rating > 0 && <span className="cull-stars">{"★".repeat(shown.rating)}</span>}
          {shown?.pick === "pick" && <span className="cull-pick">PICK</span>}
          {shown?.pick === "reject" && <span className="cull-reject">REJECT</span>}
          {shown?.label && LABEL_COLORS[shown.label] && (
            <span className="cull-label" style={{ background: LABEL_COLORS[shown.label] }} />
          )}
          {atEnd && <span className="cull-note">End of set — Esc for the summary.</span>}
          {failure && <span className="cull-failure">Not saved — {failure}</span>}
        </div>
        <div className="cull-progress">
          <div
            className="cull-progress-fill"
            style={{ width: `${((at + 1) / photos.length) * 100}%` }}
          />
        </div>
        <div className="cull-hint">h — keys · Esc — end session</div>
      </div>

      {showHelp && <CullHelp onClose={() => setShowHelp(false)} />}
    </div>
  );
}

function CullHelp({ onClose }: { onClose: () => void }) {
  return (
    <div className="cull-help" onClick={onClose}>
      <div className="cull-help-card" onClick={(e) => e.stopPropagation()}>
        <div className="cull-help-title">Keys</div>
        <dl>
          <dt>0 – 5</dt>
          <dd>rating</dd>
          <dt>p / x / u</dt>
          <dd>pick · reject · clear</dd>
          <dt>r y g b v</dt>
          <dd>colour label</dd>
          <dt>n</dt>
          <dd>clear colour label</dd>
          <dt>→ ↓ space</dt>
          <dd>next, without deciding</dd>
          <dt>← ↑</dt>
          <dd>back</dd>
          <dt>Esc</dt>
          <dd>end the session</dd>
        </dl>
        <div className="cull-help-note">
          Every decision moves you on, exactly as it does in the grid — press ← to go back
          and change one. Where you stop is remembered, so the next session resumes here.
        </div>
      </div>
    </div>
  );
}

function CullSummary({
  stats,
  total,
  onDone,
}: {
  stats: CullStats;
  total: number;
  onDone: () => void;
}) {
  const mins = Math.floor(stats.elapsedSecs / 60);
  const secs = stats.elapsedSecs % 60;
  const rate = stats.visited > 0 ? stats.elapsedSecs / stats.visited : 0;

  return (
    <div className="cull-session">
      <div className="cull-summary">
        <div className="cull-summary-title">Session over</div>
        <div className="cull-summary-lead">
          {stats.visited} of {total} photo{total === 1 ? "" : "s"} reviewed
          {stats.remaining > 0 && <> · {stats.remaining} still to go</>}
        </div>
        <dl className="cull-summary-stats">
          <dt>Decided</dt>
          <dd>
            {stats.decided}
            {stats.visited > stats.decided && (
              <span className="cull-summary-dim">
                {" "}
                · {stats.visited - stats.decided} left as they were
              </span>
            )}
          </dd>
          <dt>Picked</dt>
          <dd>{stats.picked}</dd>
          <dt>Rejected</dt>
          <dd>{stats.rejected}</dd>
          <dt>Rated</dt>
          <dd>{stats.rated}</dd>
          <dt>Labelled</dt>
          <dd>{stats.labelled}</dd>
          <dt>Time</dt>
          <dd>
            {mins > 0 ? `${mins}m ${secs}s` : `${secs}s`}
            {rate > 0 && (
              <span className="cull-summary-dim"> · {rate.toFixed(1)}s per photo</span>
            )}
          </dd>
        </dl>
        {stats.remaining > 0 && (
          <div className="cull-summary-note">
            Your place is saved — starting a session again picks up here.
          </div>
        )}
        <button className="scan-btn" onClick={onDone} autoFocus>
          Back to the grid
        </button>
      </div>
    </div>
  );
}
