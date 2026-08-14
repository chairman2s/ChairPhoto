import { useCallback, useEffect, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import {
  IdentityConflictAction,
  IdentityDebtState,
  IdentityRepairDone,
  IdentityRepairProgress,
  IdentityRepairStatus,
  IdentityRepairSummary,
  PendingIdentity,
  PendingIdentityField,
  PendingIdentitySummary,
  Volume,
  cancelIdentityRepair,
  identityRepairStatus,
  listPendingIdentity,
  listVolumes,
  onIdentityRepairDone,
  onIdentityRepairProgress,
  repairPendingIdentity,
  resolveIdentityConflict,
  summarizePendingIdentity,
} from "../modules/api";
// The shared owned-listener utility (issue #13): an owner token per attempt, a registration
// that resolves after teardown stopped rather than stored, job-id filtering, and a buffer
// for a terminal event that beats the job id it belongs to.
import { forJob, terminalBuffer, useOwnedListeners } from "../modules/ownedEvents";

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

/** Listener slots for the repair pass's two event streams. See `useOwnedListeners`. */
const REPAIR_PROGRESS = "repair-progress";
const REPAIR_DONE = "repair-done";

/**
 * Pure — what a finished (or stopped) repair pass did, in CONTEXT.md § Identity's
 * vocabulary. `null` means no pass has reported yet.
 *
 * A stopped pass is labelled as stopped. Its counters are partial by construction — the
 * queue reached 74,488 rows on the 100k harness shape in #20 and a cancel can land on row 3
 * — so presenting "Bound 2 · still unreachable 0" as if the queue were now clean would be a
 * lie the user acts on. `superseded` is only named when it happened: it is the pass telling
 * the truth about rows somebody else decided under it (#34), and it is not a failure, so it
 * has no business appearing as a permanent `0` next to the counts that are.
 */
export function repairSummaryLine(summary: IdentityRepairSummary | null): string {
  if (!summary) return "";
  const parts = [
    `bound ${summary.bound}`,
    `still unreachable ${summary.unreachable}`,
    `conflict ${summary.conflicts}`,
    `failed ${summary.failed}`,
  ];
  if (summary.superseded > 0) {
    parts.push(`${summary.superseded} decided elsewhere while the pass ran`);
  }
  const lead = summary.aborted
    ? `Stopped after ${summary.bound + summary.unreachable + summary.conflicts + summary.failed + summary.superseded} of ${summary.total}`
    : "Finished";
  return `${lead} — ${parts.join(" · ")}`;
}

/** Pure — the live progress line, e.g. "Repairing… 120 of 74488". `total` of 0 means the
 *  pass has claimed its job but has not counted the queue yet, so no denominator is shown
 *  rather than a false "of 0". */
export function repairProgressLine(done: number, total: number): string {
  return total > 0 ? `Repairing… ${done} of ${total}` : "Repairing…";
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
 * The repair pass is a background job (#34), not an awaited call: it reports through
 * `identity:repair_progress` and finishes with `identity:repair_done`, it can be cancelled,
 * and it survives this panel being closed and reopened — the panel re-attaches on mount via
 * `identityRepairStatus()`. A pass over a 74k-row queue on a NAS is minutes of network round
 * trips, so "the modal is open" is not a lifetime it can be tied to.
 *
 * Resolving a conflict deliberately does NOT stop a running pass: the backend gives each
 * queue row an owner, so the decision wins and only that row's result is dropped
 * (`catalog/identity.rs` § Who owns a queue row). Both actions stay available at once.
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
  const [repairProgress, setRepairProgress] = useState<{ done: number; total: number } | null>(
    null,
  );
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

  // ── The repair pass (#34) ──────────────────────────────────────────────────
  //
  // A job, not an awaited call: the command returns a job id and the result arrives as
  // `identity:repair_done`. The listener lifecycle is `useOwnedListeners`; what stays here
  // is the policy — the terminal signal is REQUIRED, so a host that cannot deliver it must
  // fail closed rather than leave the panel in "Repairing…" forever.

  /** The pass this panel is following. A ref, not state: `forJob` and `terminalBuffer` read
   *  it per event, and state has not updated by the time the first event lands. */
  const repairJobRef = useRef<number | null>(null);
  /** The in-flight attempt (a start or a re-attach). Excludes a second same-tick click, and
   *  stops a stale attempt releasing a replacement's listeners. */
  const repairAttemptRef = useRef<symbol | null>(null);
  const listeners = useOwnedListeners();
  /** `showDismissed` as of now, for the reload a pass triggers when it ends. A pass outlives
   *  the render that started it, so the value captured then can be stale by the time it
   *  finishes — and reloading the wrong view is a page the user did not ask for. */
  const showDismissedRef = useRef(showDismissed);
  showDismissedRef.current = showDismissed;

  useEffect(
    () => () => {
      // The listeners release themselves on unmount; the claim and job are this panel's own.
      repairAttemptRef.current = null;
      repairJobRef.current = null;
    },
    [],
  );

  /** Finish following a pass: drop its listeners and its claim, and — if the panel is still
   *  here — refresh the queue, since repaired copies have left it and every later offset has
   *  shifted. Scoped to `attempt`, so a superseded one cannot tear down its replacement. */
  const endRepair = useCallback(
    (attempt: symbol | null) => {
      if (!attempt || repairAttemptRef.current !== attempt) return;
      listeners.release(REPAIR_PROGRESS, attempt);
      listeners.release(REPAIR_DONE, attempt);
      repairAttemptRef.current = null;
      repairJobRef.current = null;
      if (!listeners.isMounted()) return;
      setRepairing(false);
      setRepairProgress(null);
      setPage(0);
      reloadSummary();
      reloadPage(0, showDismissedRef.current);
    },
    [listeners, reloadSummary, reloadPage],
  );

  /**
   * Follow a repair pass: install both listeners, then obtain the job id from `start` and
   * adopt it. Shared by "the user pressed Start" and "the panel remounted while a pass was
   * already running", which differ only in how the id is obtained.
   *
   * The listeners go up BEFORE `start` runs, because a pass over an empty (or already
   * clean) queue finishes before the command's promise resolves; `terminalBuffer` holds
   * that event until the id is adopted, so the panel cannot sit in "Repairing…" for a pass
   * that has already ended. `start` returning `null` means there was nothing to follow.
   */
  const followRepair = useCallback(
    async (start: () => Promise<number | null>) => {
      if (repairAttemptRef.current) return;
      const attempt = Symbol("identity-repair");
      repairAttemptRef.current = attempt;
      const mine = () => listeners.isMounted() && repairAttemptRef.current === attempt;
      setRepairing(true);
      setActionError("");
      setRepairResult(null);
      setRepairProgress(null);

      const currentJob = () => repairJobRef.current;
      const terminal = terminalBuffer<IdentityRepairDone>(currentJob);
      const handleDone = (d: IdentityRepairDone) => {
        if (mine()) {
          if (d.error) setActionError(d.error);
          else setRepairResult(d.summary);
        }
        endRepair(attempt);
      };

      try {
        // Terminal first: it is the one signal the pass's outcome depends on.
        const doneOutcome = await listeners.attach(
          REPAIR_DONE,
          attempt,
          () => onIdentityRepairDone(terminal.handler(handleDone)),
          mine,
        );
        if (doneOutcome === "superseded") return;
        if (doneOutcome !== "installed") {
          // AGENTS.md: "a required terminal signal must fail closed with an explicit
          // state". Without it the pass would still run, but this panel could never learn
          // that it had — so refuse to start one rather than show a spinner with no end.
          if (mine()) {
            setActionError(
              "Cannot follow a repair pass: this build cannot receive backend events, so " +
                "the pass could not report when it finished.",
            );
          }
          endRepair(attempt);
          return;
        }
        // Progress is cosmetic — it drives the counter, not the outcome — so an unsupported
        // or failed registration is not a reason to refuse the pass.
        await listeners.attach(
          REPAIR_PROGRESS,
          attempt,
          () =>
            onIdentityRepairProgress(
              forJob<IdentityRepairProgress>(currentJob, (p) => {
                if (mine()) setRepairProgress({ done: p.done, total: p.total });
              }),
            ),
          mine,
        );

        const job = await start();
        if (!mine()) return;
        if (job === null) {
          // Nothing running (a re-attach that found an idle backend).
          endRepair(attempt);
          return;
        }
        repairJobRef.current = job;
        // A terminal event that arrived before the id was known is replayed now, so a pass
        // that finished inside this window is not waited on forever.
        const early = terminal.buffered(job);
        if (early) handleDone(early);
      } catch (e) {
        if (mine()) setActionError(String(e));
        endRepair(attempt);
      }
    },
    [listeners, endRepair],
  );

  const runRepair = () => followRepair(() => repairPendingIdentity());

  const stopRepair = async () => {
    try {
      await cancelIdentityRepair();
    } catch (e) {
      setActionError(String(e));
    }
    // Deliberately no local state change: the pass still has to stop at its next copy and
    // emit its terminal event, and that event's summary — partial, and flagged `aborted` —
    // is the honest report. Clearing "Repairing…" here would claim it had already stopped.
  };

  // Re-attach on mount to a pass that is already running. A pass over a 74k-row queue on a
  // NAS long outlives one open/close of this modal, and a panel that reopened as if nothing
  // were happening would invite the user to start a second pass over the same queue.
  //
  // The status query comes FIRST, before any listener: an idle backend must cost this panel
  // nothing — no registration, and in particular no "cannot follow a pass" failure for a pass
  // that does not exist. Only once something is running does the ordinary follow path run,
  // and it re-queries after its listeners are live, because a pass that ended in between has
  // already emitted the terminal event this panel would otherwise wait forever for.
  useEffect(() => {
    void (async () => {
      let running: IdentityRepairStatus | null = null;
      try {
        running = await identityRepairStatus();
      } catch {
        return; // can't tell; Start is still available and will report its own errors
      }
      if (!running || !listeners.isMounted()) return;
      await followRepair(async () => {
        const now = await identityRepairStatus();
        if (now) setRepairProgress({ done: now.done, total: now.total });
        return now ? now.job : null;
      });
    })();
    // Mount only: a re-attach is not something to redo when `showDismissed` changes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

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
            {/* Only while a pass is actually running — a Cancel with nothing to cancel is
                a button that lies about what is happening. */}
            {repairing && (
              <>
                <button
                  className="chip"
                  onClick={stopRepair}
                  title="Stop the pass at its next copy. Copies already bound stay bound."
                >
                  Cancel
                </button>
                <span className="modal-sub">
                  {repairProgress
                    ? repairProgressLine(repairProgress.done, repairProgress.total)
                    : repairProgressLine(0, 0)}
                </span>
              </>
            )}
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
            {repairResult && <span className="modal-sub">{repairSummaryLine(repairResult)}</span>}
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
