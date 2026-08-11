import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import {
  IdentityDebtState,
  IdentityRepairSummary,
  PendingIdentity,
  PendingIdentitySummary,
  Volume,
  listPendingIdentity,
  listVolumes,
  repairPendingIdentity,
  summarizePendingIdentity,
} from "../modules/api";

const STATE_LABEL: Record<IdentityDebtState, string> = {
  unreachable: "Unreachable",
  unwritable: "Unwritable",
  conflict: "Conflict",
};

// Unreachable is a normal state (an unmounted/offline volume) — never styled as a
// failure. Unwritable and Conflict both need a person's attention, so they read
// differently, but only Unwritable is a hard failure; Conflict is "needs a decision".
const STATE_CLASS: Record<IdentityDebtState, string> = {
  unreachable: "identity-state-normal",
  unwritable: "identity-state-warn",
  conflict: "identity-state-attention",
};

function fmtWhen(unixSeconds: number): string {
  if (!unixSeconds) return "never";
  try {
    return new Date(unixSeconds * 1000).toLocaleString();
  } catch {
    return "—";
  }
}

const ROW_HEIGHT = 40;

/**
 * Read-only surface for identity debt (issue #50): the pending-identity queue — every
 * photo COPY whose sidecar doesn't yet carry its identity — with a total/conflict count
 * and a per-copy list, plus a way to start a repair pass. Debt is per copy, not per
 * photo: two copies of one photo on different volumes show as two rows.
 *
 * Deliberately does NOT resolve conflicts (that's #33 — Adopt/Overwrite/Dismiss) and
 * does NOT expose repair progress/cancellation (that's #34). Both hang their actions off
 * this same list once it exists; this issue only has to make the list and the "start a
 * repair pass" button reachable.
 */
export function IdentityDebtPanel({ onClose }: { onClose: () => void }) {
  const [summary, setSummary] = useState<PendingIdentitySummary | null>(null);
  const [rows, setRows] = useState<PendingIdentity[] | null>(null);
  const [volumes, setVolumes] = useState<Volume[]>([]);
  const [error, setError] = useState("");
  const [repairing, setRepairing] = useState(false);
  const [repairResult, setRepairResult] = useState<IdentityRepairSummary | null>(null);

  // Count and list are separate async IPC calls (both off the UI thread on the Rust
  // side — see commands::storage::{summarize_pending_identity, list_pending_identity})
  // so the header total never has to wait on (or block on) fetching every row.
  const reload = useCallback(() => {
    setError("");
    Promise.all([summarizePendingIdentity(), listPendingIdentity(), listVolumes()])
      .then(([s, list, vols]) => {
        setSummary(s);
        setRows(list);
        setVolumes(vols);
      })
      .catch((e) => setError(String(e)));
  }, []);

  useEffect(() => {
    reload();
  }, [reload]);

  const volumeName = useMemo(() => {
    const byId = new Map(volumes.map((v) => [v.id, v.name]));
    return (id: number) => byId.get(id) ?? `volume ${id}`;
  }, [volumes]);

  const runRepair = async () => {
    setRepairing(true);
    setError("");
    setRepairResult(null);
    try {
      const result = await repairPendingIdentity();
      setRepairResult(result);
      reload(); // repaired copies drop out; unreachable/failed stay queued
    } catch (e) {
      setError(String(e));
    } finally {
      setRepairing(false);
    }
  };

  // Virtualized so a large queue (74,488 rows on the 100k harness shape in #20) never
  // renders every row into the DOM — only what's on/near screen.
  const parentRef = useRef<HTMLDivElement>(null);
  const list = rows ?? [];
  const rowVirtualizer = useVirtualizer({
    count: list.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: 10,
  });

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal identity-debt" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <div className="modal-headinfo">
            <div className="modal-title">Identity debt</div>
            <div className="modal-sub">
              {summary
                ? `${summary.total} cop${summary.total === 1 ? "y" : "ies"} owe${
                    summary.total === 1 ? "s" : ""
                  } their identity to a sidecar` +
                  (summary.conflicts > 0
                    ? ` — ${summary.conflicts} conflict${summary.conflicts === 1 ? "" : "s"}`
                    : "")
                : "Loading…"}
            </div>
          </div>
          <button className="chip" onClick={onClose}>
            Close
          </button>
        </div>
        <div className="modal-body">
          <div className="modal-sub">
            Each row is one copy of a photo whose file doesn't carry its identity yet.{" "}
            <strong>Unreachable</strong> means that copy's volume is offline right now —
            normal, not an error; it'll clear on the next repair pass once the volume
            returns. <strong>Conflict</strong> means the file's sidecar already carries a
            different identity — left untouched until a person resolves it.
          </div>

          <div className="row" style={{ marginTop: 10, marginBottom: 10, alignItems: "center" }}>
            <button
              className="scan-btn"
              disabled={repairing || !summary || summary.total === 0}
              onClick={runRepair}
              title="Retry every queued copy now"
            >
              {repairing ? "Repairing…" : "Start repair pass"}
            </button>
            {repairResult && (
              <span className="modal-sub">
                Repaired {repairResult.repaired} · still unreachable {repairResult.unreachable} ·
                failed {repairResult.failed}
              </span>
            )}
          </div>
          {error && <div className="modal-error">{error}</div>}

          {rows === null ? (
            <div className="panel-empty">Loading…</div>
          ) : rows.length === 0 ? (
            <div className="panel-empty">No identity debt — every known copy is bound.</div>
          ) : (
            <div ref={parentRef} className="identity-debt-list">
              <div
                style={{
                  height: rowVirtualizer.getTotalSize(),
                  width: "100%",
                  position: "relative",
                }}
              >
                {rowVirtualizer.getVirtualItems().map((vi) => {
                  const p = list[vi.index];
                  return (
                    <div
                      key={`${p.photoId}-${p.field}-${p.volumeId}-${p.relativePath}`}
                      className="identity-debt-row"
                      style={{
                        position: "absolute",
                        top: 0,
                        left: 0,
                        width: "100%",
                        height: vi.size,
                        transform: `translateY(${vi.start}px)`,
                      }}
                    >
                      <span className={`identity-state-badge ${STATE_CLASS[p.state]}`}>
                        {STATE_LABEL[p.state]}
                      </span>
                      <span className="identity-debt-path" title={p.path}>
                        {p.path}
                      </span>
                      <span className="identity-debt-volume" title={volumeName(p.volumeId)}>
                        {volumeName(p.volumeId)}
                      </span>
                      <span className="identity-debt-relpath" title={p.relativePath}>
                        {p.relativePath}
                      </span>
                      <span className="identity-debt-field">
                        {p.field === "identifier" ? "UUID" : "import batch"}
                      </span>
                      <span
                        className="identity-debt-attempts"
                        title={`Last attempt: ${fmtWhen(p.lastAttemptAt)}`}
                      >
                        {p.attempts}×
                      </span>
                    </div>
                  );
                })}
              </div>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
