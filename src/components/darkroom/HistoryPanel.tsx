// The Darkroom's History panel (docs/editing.md § History): the active version's steps,
// newest first. Click a step to go back (or forward) to it; the steps after the current
// one stay until the next change replaces them. Ctrl+Z / Ctrl+Shift+Z do the same.
import type { VersionHistory } from "../../modules/api";

/** "just now", "5 min ago", "3 h ago", or the date. */
export function whenLabel(createdAtSecs: number, nowMs: number): string {
  const s = Math.max(0, Math.round(nowMs / 1000 - createdAtSecs));
  if (s < 45) return "just now";
  if (s < 3600) return `${Math.round(s / 60)} min ago`;
  if (s < 86400) return `${Math.round(s / 3600)} h ago`;
  return new Date(createdAtSecs * 1000).toLocaleDateString();
}

export function HistoryPanel({
  history,
  onGoto,
}: {
  history: VersionHistory | null;
  onGoto: (seq: number) => void;
}) {
  const steps = history?.steps ?? [];
  const head = history?.head ?? null;
  const now = Date.now();
  return (
    <section className="dk-history" aria-label="History">
      <div className="dk-history-head">
        <span>History</span>
        <span className="dk-history-keys">Ctrl+Z · Ctrl+Shift+Z</span>
      </div>
      {steps.length === 0 ? (
        <div className="dk-history-empty">Changes are saved as you go and listed here.</div>
      ) : (
        <ol className="dk-history-list">
          {[...steps].reverse().map((st) => {
            const state = head == null ? "" : st.seq === head ? "current" : st.seq > head ? "undone" : "";
            return (
              <li key={st.seq}>
                <button
                  className={`dk-history-step ${state}`}
                  aria-current={st.seq === head ? "step" : undefined}
                  onClick={() => onGoto(st.seq)}
                  title={st.seq > (head ?? 0) ? "Undone — click to redo up to here" : "Go back to this step"}
                >
                  <span className="dk-history-label">{st.label}</span>
                  <span className="dk-history-when">{whenLabel(st.createdAt, now)}</span>
                </button>
              </li>
            );
          })}
        </ol>
      )}
    </section>
  );
}
