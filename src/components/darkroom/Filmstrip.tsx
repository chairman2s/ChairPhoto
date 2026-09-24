// The Darkroom filmstrip: the Library's photos in their current order, the one being
// developed in the middle. Click a frame, or ← / →, to move on — the Darkroom saves as it
// goes, so nothing is lost by stepping. Thumbnails come through the native `thumb://`
// protocol; only a window around the current photo is rendered.
import { useEffect, useRef } from "react";
import { thumbnailUrl } from "../../modules/api";
import { arrowsBelongToTarget, stepTarget, windowAround } from "./filmstrip";

export function Filmstrip({
  ids,
  names,
  currentId,
  onSelect,
  keysDisabled,
}: {
  ids: number[];
  /** File names for tooltips, by photo id. */
  names: Map<number, string>;
  currentId: number;
  onSelect: (id: number) => void;
  /** A modal (proof sheet, duel) owns the arrow keys right now. */
  keysDisabled: boolean;
}) {
  const current = useRef<HTMLButtonElement | null>(null);
  useEffect(() => {
    current.current?.scrollIntoView({ inline: "center", block: "nearest" });
  }, [currentId]);

  const disabledRef = useRef(keysDisabled);
  disabledRef.current = keysDisabled;
  const stateRef = useRef({ ids, currentId, onSelect });
  stateRef.current = { ids, currentId, onSelect };
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
      if (disabledRef.current || e.ctrlKey || e.metaKey || e.altKey) return;
      if (arrowsBelongToTarget(e.target)) return; // a slider or field is using them
      const { ids, currentId, onSelect } = stateRef.current;
      const next = stepTarget(ids, currentId, e.key === "ArrowRight" ? 1 : -1);
      if (next == null) return;
      e.preventDefault();
      onSelect(next);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const { start, ids: shown } = windowAround(ids, currentId);
  return (
    <nav className="dk-strip" aria-label="Filmstrip">
      {shown.map((id, k) => (
        <button
          key={id}
          ref={id === currentId ? current : undefined}
          className={`dk-strip-frame ${id === currentId ? "current" : ""}`}
          onClick={() => id !== currentId && onSelect(id)}
          title={`${names.get(id) ?? ""} (${start + k + 1} of ${ids.length})`}
          aria-current={id === currentId ? "true" : undefined}
        >
          <img src={thumbnailUrl(id)} alt="" loading="lazy" draggable={false} />
        </button>
      ))}
    </nav>
  );
}
