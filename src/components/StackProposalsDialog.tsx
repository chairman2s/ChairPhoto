import { useEffect, useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import {
  applyStackProposal,
  proposeStacks,
  type StackProposal,
  type StackProposals,
} from "../modules/api";

// Auto-stack proposals (C3): burst clustering, phash similarity and timestamp proximity
// said as one sentence — *these frames are one moment, and this is the keeper* — with the
// frames in front of you so you can disagree.
//
// Accepting collapses a group to a single tile through the stacking that already ships:
// nothing is deleted, the other frames stay reachable under the keeper's Stack section,
// and the inspector's Unstack reverses it one frame at a time. Acceptance is per group and
// never automatic — that reversibility is what makes a proposal safe to accept quickly,
// not a reason to accept them all unread.
//
// Two consequences are shown before they happen rather than reported after: how many
// photos are already stacked under a member (accepting re-homes those onto the keeper),
// and how many frames the rule could not weigh because they have no sharpness score.

/** "1m 12s" / "4s" — a burst's span reads better than a raw second count. */
function span(secs: number): string {
  if (secs < 60) return `${secs}s`;
  const m = Math.floor(secs / 60);
  const s = secs % 60;
  return s ? `${m}m ${s}s` : `${m}m`;
}

const score = (v: number | null) => (v == null ? "—" : v >= 100 ? v.toFixed(0) : v.toFixed(1));

export function StackProposalsDialog({
  photoIds,
  onClose,
  onApplied,
}: {
  /** The photos to examine: the selection, or everything in the current view. */
  photoIds: number[];
  onClose: () => void;
  /** Called after each accepted group, so the grid drops the frames that just collapsed. */
  onApplied: () => void;
}) {
  const [result, setResult] = useState<StackProposals | null>(null);
  const [error, setError] = useState<string | null>(null);
  /** Keeper override per proposal, keyed by the proposal's original keeper id. */
  const [keepers, setKeepers] = useState<Record<number, number>>({});
  /** Groups already accepted or skipped in this session, keyed the same way. */
  const [done, setDone] = useState<Record<number, "stacked" | "skipped">>({});
  const [busy, setBusy] = useState<number | null>(null);

  useEffect(() => {
    let live = true;
    proposeStacks(photoIds)
      .then((r) => {
        if (live) setResult(r);
      })
      .catch((e) => {
        if (live) setError(String(e));
      });
    return () => {
      live = false;
    };
  }, [photoIds]);

  const accept = async (p: StackProposal) => {
    const keeperId = keepers[p.keeperId] ?? p.keeperId;
    setBusy(p.keeperId);
    try {
      await applyStackProposal(
        keeperId,
        p.members.map((m) => m.photoId),
      );
      setDone((d) => ({ ...d, [p.keeperId]: "stacked" }));
      onApplied();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(null);
    }
  };

  const pending = (result?.proposals ?? []).filter((p) => !done[p.keeperId]);
  const stackedCount = Object.values(done).filter((v) => v === "stacked").length;

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal stack-proposals" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <div className="modal-title">Stack bursts</div>
          <button className="chip" onClick={onClose}>
            Close
          </button>
        </div>

        <div className="modal-body">
          {error && <div className="modal-error">{error}</div>}

          {!result && !error && <div className="panel-empty">Looking for groups…</div>}

          {result && (
            <>
              <div className="modal-sub">
                {result.proposals.length === 0 ? (
                  <>
                    No groups in {result.considered} photo
                    {result.considered === 1 ? "" : "s"} — nothing here was shot within{" "}
                    {result.timeGapSecs}s of a similar frame.
                  </>
                ) : (
                  <>
                    {result.proposals.length} group{result.proposals.length === 1 ? "" : "s"} in{" "}
                    {result.considered} photos. Frames within {result.timeGapSecs}s of each
                    other and closer than {result.hammingThreshold} in visual difference.
                  </>
                )}
                {result.skippedStacked > 0 && (
                  <>
                    {" "}
                    {result.skippedStacked} already-stacked photo
                    {result.skippedStacked === 1 ? " was" : "s were"} left out.
                  </>
                )}
                {result.truncated && (
                  <> Only the first {result.proposals.length} are shown — run again after these.</>
                )}
              </div>

              {result.proposals.length > 0 && (
                <div className="stack-proposals-note">
                  Stacking hides the other frames from the grid and keeps them under the
                  keeper, where the inspector's Stack section can unstack any of them again.
                  Nothing is deleted.
                </div>
              )}

              {pending.map((p) => (
                <ProposalRow
                  key={p.keeperId}
                  proposal={p}
                  keeperId={keepers[p.keeperId] ?? p.keeperId}
                  busy={busy === p.keeperId}
                  onPickKeeper={(id) => setKeepers((k) => ({ ...k, [p.keeperId]: id }))}
                  onAccept={() => accept(p)}
                  onSkip={() => setDone((d) => ({ ...d, [p.keeperId]: "skipped" }))}
                />
              ))}

              {stackedCount > 0 && (
                <div className="modal-sub">
                  Stacked {stackedCount} group{stackedCount === 1 ? "" : "s"} this session.
                </div>
              )}
            </>
          )}
        </div>
      </div>
    </div>
  );
}

function ProposalRow({
  proposal,
  keeperId,
  busy,
  onPickKeeper,
  onAccept,
  onSkip,
}: {
  proposal: StackProposal;
  keeperId: number;
  busy: boolean;
  onPickKeeper: (photoId: number) => void;
  onAccept: () => void;
  onSkip: () => void;
}) {
  // The reason names the *engine's* choice. Once a different frame is picked by hand, that
  // sentence no longer explains what would happen, so it is withdrawn rather than reused.
  const overridden = keeperId !== proposal.keeperId;

  return (
    <div className="stack-proposal">
      <div className="stack-proposal-head">
        <b>{proposal.members.length} frames</b> over {span(proposal.spanSecs)}
        {proposal.maxDistance != null && <> · up to {proposal.maxDistance} apart visually</>}
        <span className="stack-proposal-reason">
          {overridden ? "keeper chosen by hand" : `keeper: ${proposal.reason}`}
        </span>
      </div>

      <div className="stack-proposal-frames">
        {proposal.members.map((m) => {
          const isKeeper = m.photoId === keeperId;
          return (
            <button
              key={m.photoId}
              className={`stack-proposal-frame ${isKeeper ? "is-keeper" : ""}`}
              onClick={() => onPickKeeper(m.photoId)}
              title={
                isKeeper
                  ? `${m.fileName} — the keeper: the others stack under this one`
                  : `${m.fileName} — click to keep this frame instead`
              }
            >
              <img src={convertFileSrc(String(m.photoId), "thumb")} alt="" />
              <span className="stack-proposal-frame-meta">
                {isKeeper && <span className="stack-proposal-crown">♛</span>}
                {score(m.sharpness)}
                {m.rating > 0 && <span className="stack-proposal-stars">{"★".repeat(m.rating)}</span>}
              </span>
              {m.childCount > 0 && (
                <span className="stack-proposal-carries" title={`${m.childCount} stacked file(s)`}>
                  ▤ {m.childCount}
                </span>
              )}
            </button>
          );
        })}
      </div>

      {(proposal.unscored > 0 || proposal.absorbedChildren > 0) && (
        <div className="stack-proposal-caveats">
          {proposal.unscored > 0 && (
            <div>
              {proposal.unscored} frame{proposal.unscored === 1 ? " has" : "s have"} no sharpness
              score yet, so {proposal.unscored === 1 ? "it was" : "they were"} not weighed in
              picking the keeper.
            </div>
          )}
          {proposal.absorbedChildren > 0 && (
            <div>
              {proposal.absorbedChildren} photo
              {proposal.absorbedChildren === 1 ? "" : "s"} stacked under these frames will move
              onto the keeper — stacks stay one level deep.
            </div>
          )}
        </div>
      )}

      <div className="row">
        <button className="scan-btn" onClick={onAccept} disabled={busy}>
          {busy ? "Stacking…" : `Stack ${proposal.members.length - 1} under this`}
        </button>
        <button className="chip" onClick={onSkip} disabled={busy}>
          Skip
        </button>
      </div>
    </div>
  );
}
