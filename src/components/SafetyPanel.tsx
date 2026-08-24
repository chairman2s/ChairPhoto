import { useCallback, useEffect, useState } from "react";
import {
  librarySafetySummary,
  type SafetySummary,
  type StorageTier,
} from "../modules/api";

// The safety panel (cluster B, B1): would I lose these photos if a disk died?
//
// A second axis, deliberately separate from the grid's storage badge. That one answers
// "can I display this now"; this one answers "would I lose it", and they disagree in the
// case that matters — a photo whose only copy is at home displays perfectly.
//
// Three things this panel must not do, each learned the hard way:
//
//   * **Claim more than it can see.** Home is a RAID array that is itself backed up
//     off-site, and the catalog knows nothing about the off-site copy. Every number here
//     is about volumes ChairPhoto can see, and the panel says so rather than implying it
//     has the whole picture.
//   * **Present `stale` as a total.** Freshness is recorded by the scanner, so it is only
//     ever true as of the last scan. While companions remain unchecked, `stale` is a floor
//     and the panel says that too.
//   * **Show a count you cannot act on.** Every non-zero bucket that has an action offers
//     it, and "show me" filters the grid to exactly the photos the count refers to — the
//     same predicate, so the number and the list cannot disagree.

/** ISO-ish "3 months" from a unix timestamp — how long, not just how many. */
function since(unixSecs: number): string {
  const days = Math.floor((Date.now() / 1000 - unixSecs) / 86_400);
  if (days < 1) return "today";
  if (days === 1) return "1 day";
  if (days < 60) return `${days} days`;
  const months = Math.floor(days / 30);
  return months < 24 ? `${months} months` : `${Math.floor(days / 365)} years`;
}

export function SafetySection({
  onShowTier,
}: {
  /** Filter the grid to a bucket and get out of the way. */
  onShowTier?: (tier: StorageTier) => void;
}) {
  const [summary, setSummary] = useState<SafetySummary | null>(null);
  const [error, setError] = useState<string | null>(null);

  const reload = useCallback(() => {
    librarySafetySummary()
      .then((s) => {
        setSummary(s);
        setError(null);
      })
      .catch((e) => setError(String(e)));
  }, []);

  useEffect(reload, [reload]);

  if (error) {
    return (
      <div className="prefs-section">
        <h3>Safety</h3>
        <div className="modal-error">{error}</div>
      </div>
    );
  }
  if (!summary) {
    return (
      <div className="prefs-section">
        <h3>Safety</h3>
        <div className="panel-empty">Counting…</div>
      </div>
    );
  }

  const total =
    summary.missing + summary.atRisk + summary.unverified + summary.stale + summary.safe;

  return (
    <div className="prefs-section safety-section">
      <h3>Safety</h3>

      {total === 0 ? (
        <div className="panel-empty">No photos in the catalog yet.</div>
      ) : (
        <>
          <Row
            label="At risk"
            count={summary.atRisk}
            tone={summary.atRisk > 0 ? "bad" : "ok"}
            detail={
              summary.atRisk === 0
                ? "Every photo has a copy at home."
                : summary.oldestAtRisk != null
                  ? `No copy at home. The oldest has been waiting ${since(summary.oldestAtRisk)}.`
                  : "No copy at home."
            }
            action={summary.atRisk > 0 && onShowTier ? "Show me" : undefined}
            onAction={() => onShowTier?.("atRisk")}
          />

          <Row
            label="Edits not carried home"
            count={summary.stale}
            tone={summary.stale > 0 ? "bad" : "ok"}
            detail={
              summary.stale === 0
                ? "No local edit is newer than the copy at home."
                : "The photo is safe at home, but an edit made since is only on this machine."
            }
            action={summary.stale > 0 && onShowTier ? "Show me" : undefined}
            onAction={() => onShowTier?.("stale")}
          />

          <Row
            label="Unverified"
            count={summary.unverified}
            tone="mute"
            detail="A copy at home that has never been hash-verified — its bytes have not been checked since it arrived."
          />

          <Row label="Safe" count={summary.safe} tone="ok" detail="Verified at home, with its edits." />

          {summary.missing > 0 && (
            <Row
              label="No copy anywhere"
              count={summary.missing}
              tone="bad"
              detail="No location on record. Usually a catalog row whose file was moved outside ChairPhoto."
            />
          )}

          {(summary.atRisk > 0 || summary.stale > 0) && onShowTier && (
            <div className="safety-next">
              “Show me” filters the grid to that bucket. Select there — <kbd>Ctrl</kbd>+
              <kbd>A</kbd> takes the lot — and use <b>Back up</b> in the toolbar to queue
              them. They copy when the NAS is reachable, and the topbar badge tracks what
              is still waiting.
            </div>
          )}

          <div className="safety-caveats">
            {summary.companionsUnchecked > 0 && (
              <div>
                Freshness is as of the last scan. {summary.companionsUnchecked} carried
                sidecar{summary.companionsUnchecked === 1 ? " has" : "s have"} not been
                looked at since being copied home, so the “edits not carried home” count is
                a floor rather than a total.
              </div>
            )}
            <div>
              These counts cover the volumes ChairPhoto can see. Redundancy inside a
              storage device, and any off-site backup, are invisible to it — a photo listed
              as safe here is safe <em>as far as this catalog knows</em>.
            </div>
          </div>
        </>
      )}
    </div>
  );
}

function Row({
  label,
  count,
  detail,
  tone,
  action,
  onAction,
}: {
  label: string;
  count: number;
  detail: string;
  tone: "ok" | "bad" | "mute";
  action?: string;
  onAction?: () => void;
}) {
  return (
    <div className="safety-row">
      <span className={`safety-count safety-${tone}`}>{count.toLocaleString()}</span>
      <div className="safety-text">
        <div className="safety-label">{label}</div>
        <div className="safety-detail">{detail}</div>
      </div>
      {action && (
        <button className="chip" onClick={onAction}>
          {action}
        </button>
      )}
    </div>
  );
}
