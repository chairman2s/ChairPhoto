import { useCallback, useEffect, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import {
  IdentityConflictAction,
  IdentityDebtState,
  IdentityRepairSummary,
  PendingIdentity,
  PendingIdentityField,
  PendingIdentitySummary,
  Volume,
  listPendingIdentity,
  listVolumes,
  repairPendingIdentity,
  resolveIdentityConflict,
  summarizePendingIdentity,
} from "../modules/api";

export const STATE_LABEL: Record<IdentityDebtState, string> = {
  unreachable: "Unreachable",
  unwritable: "Unwritable",
  conflict: "Conflict",
  dismissed: "Dismissed",
};

// Unreachable is a normal state (an unmounted/offline volume, or a file this pass
// couldn't find) — never styled as a failure. Unwritable and Conflict both need a
// person's attention, so they read differently, but only Unwritable is a hard failure;
// Conflict is "needs a decision", not an error (see CONTEXT.md § Identity). Dismissed is a
// decision already taken — the copy is on the record and asks nothing of anyone, so it
// reads as neutral, not as an outstanding problem.
export const STATE_CLASS: Record<IdentityDebtState, string> = {
  unreachable: "identity-state-normal",
  unwritable: "identity-state-warn",
  conflict: "identity-state-attention",
  dismissed: "identity-state-normal",
};

function fmtWhen(unixSeconds: number): string {
  if (!unixSeconds) return "never";
  try {
    return new Date(unixSeconds * 1000).toLocaleString();
  } catch {
    return "—";
  }
}

function fieldLabel(f: PendingIdentityField): string {
  return f.field === "identifier" ? "UUID" : "import batch";
}

/** Height of one owed-field line within a copy's row (State/Field/Tries/Last
 *  attempt/Detail are one stacked line per field a copy owes — a row represents a copy
 *  owing 1 or 2 fields, not always exactly 1). */
const FIELD_LINE_HEIGHT = 22;
/** Breathing room above/below a row's field-line stack. */
const ROW_PADDING = 10;
/** A copy's row height: one `FIELD_LINE_HEIGHT` per owed field, never less than one
 *  line's worth even if `fields` were ever empty. Exact, not an estimate — every row's
 *  field count is already known before the virtualizer asks. */
function rowHeight(p: PendingIdentity): number {
  return Math.max(1, p.fields.length) * FIELD_LINE_HEIGHT + ROW_PADDING;
}
/** IPC page size — bounds a single `list_pending_identity` payload regardless of how large
 *  the queue gets (74,488 rows on the 100k harness shape in #20). */
const PAGE_SIZE = 500;

/**
 * Pure — the header line under "Identity debt", e.g. "3 copies owe their identity to a
 * sidecar — 1 conflict". `null` means the summary hasn't loaded yet. Exported so it's
 * testable without a DOM: `vitest.config.ts` runs in plain Node, so rendering this
 * component isn't testable here, but this plural/singular + conflict logic is ordinary
 * string math and doesn't need one.
 */
export function summaryHeadline(summary: PendingIdentitySummary | null): string {
  if (!summary) return "Loading…";
  const copies = summary.total === 1 ? "copy" : "copies";
  const owe = summary.total === 1 ? "owes" : "owe";
  let line = `${summary.total} ${copies} ${owe} their identity to a sidecar`;
  if (summary.conflicts > 0) {
    line += ` — ${summary.conflicts} conflict${summary.conflicts === 1 ? "" : "s"}`;
  }
  // Dismissed copies are deliberately outside `total`: they are a decision on the record,
  // not outstanding debt. Naming them separately is what stops "0 copies owe…" reading as
  // "nothing was ever wrong here".
  if (summary.dismissed > 0) {
    line += `${summary.conflicts > 0 ? "," : " —"} ${summary.dismissed} dismissed`;
  }
  return line;
}

/** The conflicted `identifier` field of a copy, if it has one and it has not been
 *  dismissed — the field the Adopt/Overwrite/Dismiss actions act on. Only `identifier` can
 *  conflict (an import batch write never reports one), so this is deliberately not a
 *  general "first field in some state" helper. Pure, and exported for tests. */
export function conflictField(p: PendingIdentity): PendingIdentityField | null {
  return p.fields.find((f) => f.field === "identifier" && f.state === "conflict") ?? null;
}

/** The dismissed `identifier` field of a copy, if any — the one Restore puts back. */
export function dismissedField(p: PendingIdentity): PendingIdentityField | null {
  return p.fields.find((f) => f.field === "identifier" && f.state === "dismissed") ?? null;
}

/** The line shown after a resolution: what happened, in the vocabulary of CONTEXT.md §
 *  Identity, stated from the OUTCOME the backend returned rather than from the action that
 *  was requested. Pure, and exported for tests. */
export function resolutionMessage(outcome: {
  action: IdentityConflictAction;
  catalogUuid: string;
  previousSidecarUuid: string;
  recheckedCopies: number;
  sidecarBackup: string | null;
}): string {
  switch (outcome.action) {
    case "adopt": {
      const others =
        outcome.recheckedCopies > 0
          ? ` ${outcome.recheckedCopies} other cop${outcome.recheckedCopies === 1 ? "y" : "ies"} of this photo re-checked.`
          : "";
      return `Adopted ${outcome.catalogUuid} from the sidecar. The catalog now uses it.${others}`;
    }
    case "overwrite": {
      const backup = outcome.sidecarBackup
        ? ` The previous sidecar is at ${outcome.sidecarBackup}.`
        : " An earlier backup of this sidecar was already kept and left untouched.";
      return `Overwrote the sidecar with ${outcome.catalogUuid}, replacing ${outcome.previousSidecarUuid}.${backup}`;
    }
    case "dismiss":
      return "Dismissed. The copy stays on the record but is neither retried nor counted as debt.";
    case "restore":
      return "Restored. The copy is queued again.";
  }
}

/**
 * Pure — "Showing N–M of T" for the current page: the panel must not pretend it fetched
 * the whole queue. `total === null` means the summary hasn't resolved yet, so the "of T"
 * half is omitted rather than shown as 0.
 *
 * `total` comes from `summary`, fetched once when the panel mounts, while `shown` comes
 * from the CURRENT page's rows, refetched on every page change (see
 * `IdentityDebtPanel`'s effects below). A concurrent repair or scan can grow or shrink the
 * queue while the panel stays open, leaving `total` stale for the rest of its life — this
 * function has no way to refresh it, so it clamps instead: the displayed total is never
 * rendered smaller than what's already shown, so the label can never read something
 * self-contradictory like "Showing 1–4 of 3".
 */
export function pagingLabel(offset: number, shown: number, total: number | null): string {
  if (shown === 0) return total === 0 ? "No pending copies" : "No rows on this page";
  const from = offset + 1;
  const to = offset + shown;
  if (total === null) return `Showing ${from}–${to}`;
  const displayedTotal = Math.max(total, to);
  return `Showing ${from}–${to} of ${displayedTotal}`;
}

/**
 * Read-only surface for identity debt (issue #50): the pending-identity queue — every
 * photo COPY whose sidecar doesn't yet carry its identity — with a total/conflict count
 * and a per-copy list, plus a way to start a repair pass. Debt is per copy, not per
 * photo: two copies of one photo on different volumes show as two rows.
 *
 * The header total (`summarizePendingIdentity`, one COUNT query) and the per-copy page
 * (`listPendingIdentity`, LIMIT/OFFSET-bound) are two independent IPC calls, not one
 * `Promise.all` — a slow/large page fetch must never delay the cheap header. The page
 * itself is bounded to `PAGE_SIZE` rows regardless of queue size — never the strategy of
 * fetching everything and virtualizing only the DOM.
 *
 * Resolving a conflict (#33) hangs off this same list: a conflicted copy gets Adopt /
 * Overwrite / Dismiss, and a dismissed one gets Restore. Overwrite is destructive (it
 * replaces the identifier in the file), so it is the one action behind a confirm step —
 * Adopt is consequential but reversible-by-decision, and there is no default action for
 * either.
 *
 * Still does NOT expose repair progress/cancellation (that's #34).
 */
export function IdentityDebtPanel({ onClose }: { onClose: () => void }) {
  const [summary, setSummary] = useState<PendingIdentitySummary | null>(null);
  const [rows, setRows] = useState<PendingIdentity[] | null>(null);
  const [volumes, setVolumes] = useState<Volume[]>([]);
  const [page, setPage] = useState(0);
  const [showDismissed, setShowDismissed] = useState(false);
  const [summaryError, setSummaryError] = useState("");
  const [listError, setListError] = useState("");
  const [actionError, setActionError] = useState("");
  const [repairing, setRepairing] = useState(false);
  const [repairResult, setRepairResult] = useState<IdentityRepairSummary | null>(null);
  /** The copy whose Overwrite is awaiting confirmation, keyed like the row itself. Only one
   *  at a time: a pending destructive action must be visible where it will happen. */
  const [confirmOverwrite, setConfirmOverwrite] = useState<string | null>(null);
  /** The copy a resolution is currently in flight for — disables that row's buttons so a
   *  double click can't send two decisions about one copy. */
  const [resolving, setResolving] = useState<string | null>(null);
  const [resolveResult, setResolveResult] = useState("");

  const reloadSummary = useCallback(() => {
    setSummaryError("");
    summarizePendingIdentity()
      .then(setSummary)
      .catch((e) => setSummaryError(String(e)));
  }, []);

  const reloadPage = useCallback((p: number, includeDismissed: boolean) => {
    setListError("");
    listPendingIdentity(PAGE_SIZE, p * PAGE_SIZE, includeDismissed)
      .then(setRows)
      .catch((e) => setListError(String(e)));
  }, []);

  // Summary and volumes load once up front; the page reloads whenever `page` changes.
  useEffect(() => {
    reloadSummary();
    listVolumes().then(setVolumes).catch(() => setVolumes([]));
  }, [reloadSummary]);

  useEffect(() => {
    reloadPage(page, showDismissed);
  }, [reloadPage, page, showDismissed]);

  const volumeName = (id: number) => volumes.find((v) => v.id === id)?.name ?? `volume ${id}`;

  const runRepair = async () => {
    setRepairing(true);
    setActionError("");
    setRepairResult(null);
    try {
      const result = await repairPendingIdentity();
      setRepairResult(result);
      // Repaired copies drop out of the queue, which can shift every later offset —
      // start over rather than show a page that no longer lines up with anything.
      setPage(0);
      reloadSummary();
      reloadPage(0, showDismissed);
    } catch (e) {
      setActionError(String(e));
    } finally {
      setRepairing(false);
    }
  };

  const rowKey = (p: PendingIdentity) => `${p.photoId}-${p.volumeId}-${p.relativePath}`;

  const resolve = async (p: PendingIdentity, action: IdentityConflictAction) => {
    const key = rowKey(p);
    setResolving(key);
    setActionError("");
    setResolveResult("");
    try {
      const outcome = await resolveIdentityConflict(p.photoId, p.volumeId, p.relativePath, action);
      setResolveResult(resolutionMessage(outcome));
      setConfirmOverwrite(null);
      // A resolved copy leaves (or joins) the queue, shifting every later offset — same
      // reason the repair pass returns to the first page.
      setPage(0);
      reloadSummary();
      reloadPage(0, showDismissed);
    } catch (e) {
      // A refusal (most importantly "another photo already holds that identity") is the
      // backend's message, shown verbatim: it names what it refused and why.
      setActionError(String(e));
    } finally {
      setResolving(null);
    }
  };

  /** The three (or one) decisions available for a copy, or nothing if it isn't in a state
   *  that asks for one. Rendered per copy, not per field: only `identifier` conflicts. */
  const rowActions = (p: PendingIdentity) => {
    const key = rowKey(p);
    const busy = resolving === key;
    const conflict = conflictField(p);
    if (conflict) {
      if (confirmOverwrite === key) {
        return (
          <>
            {/* The identifier about to be destroyed is named in `error` (the Detail
                column) — shown here as the backend wrote it rather than picked apart with
                a regex over prose. */}
            <span className="identity-debt-confirm" title={conflict.error}>
              Replace the identifier in the file?
            </span>
            <button
              className="chip chip-danger"
              disabled={busy}
              onClick={() => resolve(p, "overwrite")}
            >
              Overwrite
            </button>
            <button className="chip" disabled={busy} onClick={() => setConfirmOverwrite(null)}>
              Cancel
            </button>
          </>
        );
      }
      return (
        <>
          <button
            className="chip"
            disabled={busy}
            title="The catalog takes the identity already in this copy's sidecar. Changes the catalog, never the file."
            onClick={() => resolve(p, "adopt")}
          >
            Adopt
          </button>
          <button
            className="chip"
            disabled={busy}
            title="This copy's sidecar takes the catalog's identity. Changes the file and destroys the identifier that was there."
            onClick={() => setConfirmOverwrite(key)}
          >
            Overwrite…
          </button>
          <button
            className="chip"
            disabled={busy}
            title="Stop retrying this copy. Changes neither the catalog nor the file."
            onClick={() => resolve(p, "dismiss")}
          >
            Dismiss
          </button>
        </>
      );
    }
    if (dismissedField(p)) {
      return (
        <button
          className="chip"
          disabled={busy}
          title="Put this copy back in the queue."
          onClick={() => resolve(p, "restore")}
        >
          Restore
        </button>
      );
    }
    return null;
  };

  // Virtualized so a large page never renders every row into the DOM at once either.
  const parentRef = useRef<HTMLDivElement>(null);
  const list = rows ?? [];
  const rowVirtualizer = useVirtualizer({
    count: list.length,
    getScrollElement: () => parentRef.current,
    estimateSize: (index) => rowHeight(list[index]),
    overscan: 10,
  });

  const canPrev = page > 0;
  const canNext = rows !== null && rows.length === PAGE_SIZE;

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal identity-debt" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <div className="modal-headinfo">
            <div className="modal-title">Identity debt</div>
            <div className="modal-sub">{summaryHeadline(summary)}</div>
          </div>
          <button className="chip" onClick={onClose}>
            Close
          </button>
        </div>
        <div className="modal-body">
          <div className="modal-sub">
            Each row is one copy of a photo, listing every identity field it still owes
            (a copy can owe both its UUID and its import batch). <strong>Unreachable</strong>{" "}
            means that copy could not be found just now —
            normal, not an error, whether its volume is offline or the file was moved,
            renamed, or deleted outside ChairPhoto; a repair pass picks it up again once
            the file is reachable at its known location. <strong>Unwritable</strong> means
            the file was found but its sidecar could not be written (read-only storage, a
            corrupt sidecar, a full disk) — see Detail for why.{" "}
            <strong>Conflict</strong> means the file's sidecar already carries a different
            identity — the file is left untouched until you decide: <strong>Adopt</strong>{" "}
            the identity that is in the file (changes the catalog, never the file),{" "}
            <strong>Overwrite</strong> the file with the catalog's (destroys the identifier
            that was there; the sidecar is backed up first), or <strong>Dismiss</strong> the
            copy (changes nothing, stops the retries). Adopting an identity another photo
            already holds is refused — no two photos may share one.
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
            <label className="modal-sub" title="Dismissed copies are kept on the record but are not debt — list them to restore one">
              <input
                type="checkbox"
                checked={showDismissed}
                onChange={(e) => {
                  setPage(0);
                  setShowDismissed(e.target.checked);
                }}
              />{" "}
              Show dismissed{summary && summary.dismissed > 0 ? ` (${summary.dismissed})` : ""}
            </label>
            {repairResult && (
              <span className="modal-sub">
                Bound {repairResult.bound} · still unreachable {repairResult.unreachable} ·
                conflict {repairResult.conflicts} · failed {repairResult.failed}
              </span>
            )}
          </div>
          {resolveResult && <div className="modal-sub">{resolveResult}</div>}
          {summaryError && <div className="modal-error">{summaryError}</div>}
          {listError && <div className="modal-error">{listError}</div>}
          {actionError && <div className="modal-error">{actionError}</div>}

          {rows === null ? (
            <div className="panel-empty">Loading…</div>
          ) : rows.length === 0 && page === 0 ? (
            <div className="panel-empty">
              No identity debt — every known copy is bound.
              {/* "Bound" would be a lie about a dismissed copy: it still isn't bound, we
                  just stopped asking. Say so, and offer the way to it. */}
              {!showDismissed && summary && summary.dismissed > 0 && (
                <>
                  {" "}
                  {summary.dismissed} dismissed cop{summary.dismissed === 1 ? "y is" : "ies are"}{" "}
                  hidden.{" "}
                  <button className="chip" onClick={() => setShowDismissed(true)}>
                    Show dismissed
                  </button>
                </>
              )}
            </div>
          ) : (
            <>
              {/* Header cells share the body's column-sizing wrapper classes (rather than
                  duplicating pixel widths) so the two can't drift apart — a copy's field
                  columns stack 1-2 lines, but the header always has exactly one. */}
              <div className="identity-debt-row identity-debt-header">
                <span className="identity-debt-fieldcol identity-debt-statecol">State</span>
                <span className="identity-debt-path">Path</span>
                <span className="identity-debt-volume">Volume</span>
                <span className="identity-debt-relpath">Relative path (on volume)</span>
                <span className="identity-debt-fieldcol">Field</span>
                <span className="identity-debt-fieldcol identity-debt-attemptscol">Tries</span>
                <span className="identity-debt-fieldcol identity-debt-lastattemptcol">
                  Last attempt
                </span>
                <span className="identity-debt-fieldcol identity-debt-errorcol">Detail</span>
                <span className="identity-debt-actions">Resolve</span>
              </div>
              {rows.length === 0 ? (
                <div className="panel-empty">
                  No rows on this page.{" "}
                  <button className="chip" onClick={() => setPage(0)}>
                    Back to first page
                  </button>
                </div>
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
                          key={`${p.photoId}-${p.volumeId}-${p.relativePath}`}
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
                          {/* State/Field/Tries/Last attempt/Detail are per-FIELD — a copy
                              owing both `identifier` and `import_batch` stacks two lines
                              here, one per owed field, because this row represents one
                              copy, not one (copy, field) pair. Path/Volume/Relative path
                              are per-COPY, so they render once and stay vertically
                              centered against however tall the stack is. */}
                          <span className="identity-debt-fieldcol identity-debt-statecol">
                            {p.fields.map((f) => (
                              <span
                                key={f.field}
                                className={`identity-state-badge ${STATE_CLASS[f.state]}`}
                              >
                                {STATE_LABEL[f.state]}
                              </span>
                            ))}
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
                          <span className="identity-debt-fieldcol">
                            {p.fields.map((f) => (
                              <span key={f.field} className="identity-debt-field">
                                {fieldLabel(f)}
                              </span>
                            ))}
                          </span>
                          <span className="identity-debt-fieldcol identity-debt-attemptscol">
                            {p.fields.map((f) => (
                              <span
                                key={f.field}
                                className="identity-debt-attempts"
                                title={`${f.attempts} attempts`}
                              >
                                {f.attempts}×
                              </span>
                            ))}
                          </span>
                          <span className="identity-debt-fieldcol identity-debt-lastattemptcol">
                            {p.fields.map((f) => (
                              <span key={f.field} className="identity-debt-lastattempt">
                                {fmtWhen(f.lastAttemptAt)}
                              </span>
                            ))}
                          </span>
                          <span className="identity-debt-fieldcol identity-debt-errorcol">
                            {p.fields.map((f) => (
                              <span key={f.field} className="identity-debt-error" title={f.error}>
                                {f.error || "—"}
                              </span>
                            ))}
                          </span>
                          {/* Per COPY, not per field: only `identifier` can conflict, and
                              the decision is about this copy's file. Absent for copies in
                              a state that asks nothing of anyone (Unreachable/Unwritable
                              need a repair pass, not a decision). */}
                          <span className="identity-debt-actions">{rowActions(p)}</span>
                        </div>
                      );
                    })}
                  </div>
                </div>
              )}
              <div className="row" style={{ marginTop: 8, alignItems: "center", gap: 8 }}>
                <span className="modal-sub">
                  {/* The page's total is whatever the page is showing: the active queue is
                      `total`, and "show dismissed" widens it to the two disjoint groups
                      (`total` + `dismissed`), so the label never counts a copy twice or
                      claims a smaller total than the rows on screen. */}
                  {pagingLabel(
                    page * PAGE_SIZE,
                    rows.length,
                    summary === null
                      ? null
                      : summary.total + (showDismissed ? summary.dismissed : 0),
                  )}
                </span>
                <button className="chip" disabled={!canPrev} onClick={() => setPage((p) => Math.max(0, p - 1))}>
                  ← Prev
                </button>
                <button className="chip" disabled={!canNext} onClick={() => setPage((p) => p + 1)}>
                  Next →
                </button>
              </div>
            </>
          )}
        </div>
      </div>
    </div>
  );
}
