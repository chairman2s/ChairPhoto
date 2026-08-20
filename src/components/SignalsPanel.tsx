import { useEffect, useState } from "react";
import { explainPhotoSignals, type ClusterFrame, type PhotoSignals } from "../modules/api";

// Inspector "Culling signals" panel (C6): why this photo carries the badges it carries.
//
// The grid's badges are verdicts with the reasoning discarded — `~B` claims a frame is
// soft *in its burst* while showing neither the burst, the median it lost to, nor the
// frame that won. This panel shows the derivation instead: the cluster, every frame's
// score and visual distance from this one, the cutoff the flag was decided against, and
// where this frame ranks.
//
// Two things it must never smooth over, because both are true statements about the
// catalog rather than glitches: a stored flag the recomputation no longer reaches (the
// badge is stale — re-run the analysis), and a burst too long to be seen whole (the
// numbers are lower bounds). See `commands/culling.rs`.
//
// Fetches on mount, and the inspector only mounts a section's body once it is expanded,
// so a collapsed panel costs nothing on the grid's hot path.

const FLAG_LABEL: Record<string, string> = {
  "soft-in-burst": "Soft in burst",
  "sharpest-of-burst": "Sharpest of burst",
};

const flagLabel = (flag: string | null) => (flag ? (FLAG_LABEL[flag] ?? flag) : "no flag");

/** Scores span orders of magnitude, so a fixed decimal count reads badly at both ends. */
const score = (v: number | null | undefined) =>
  v == null ? "—" : v >= 100 ? v.toFixed(0) : v.toFixed(1);

export function SignalsPanel({ photoId }: { photoId: number }) {
  const [signals, setSignals] = useState<PhotoSignals | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    setSignals(null);
    setError(null);
    explainPhotoSignals(photoId)
      .then((s) => {
        if (live) setSignals(s);
      })
      .catch((e) => {
        if (live) setError(String(e));
      });
    return () => {
      // A slower response for the previous photo must not overwrite this one's.
      live = false;
    };
  }, [photoId]);

  if (error) return <div className="panel-empty">Could not read this photo's signals: {error}</div>;
  if (!signals) return <div className="panel-empty">Reading signals…</div>;

  const { sharpness, burst, stack, versionCount } = signals;
  const nothing =
    !sharpness && !burst && stack.childCount === 0 && stack.parentId == null && versionCount === 0;

  return (
    <div className="signals">
      {nothing && (
        <div className="panel-empty">
          No signals yet — this photo has not been scored, hashed or stacked.
        </div>
      )}

      {sharpness && (
        <div className="signals-block">
          <div className="signals-head">
            Sharpness
            <span className={`signals-verdict ${sharpness.belowThreshold ? "is-soft" : ""}`}>
              {sharpness.belowThreshold ? "Soft" : "Above the soft threshold"}
            </span>
          </div>
          <div className="signals-line">
            <b>{score(sharpness.score)}</b> against a threshold of{" "}
            {score(sharpness.softThreshold)}
            {sharpness.method && (
              <>
                {" "}
                · scored by <code>{sharpness.method}</code>
              </>
            )}
          </div>
          {sharpness.method && sharpness.method !== "tile" && (
            <div className="signals-note">
              Scores from different methods are not comparable — this one was measured on{" "}
              {sharpness.method === "face" ? "a detected face" : "the camera's AF point"}.
            </div>
          )}
        </div>
      )}

      {burst && (
        <div className="signals-block">
          <div className="signals-head">
            Burst
            <span
              className={`signals-verdict ${burst.verdict === "soft-in-burst" ? "is-soft" : ""}`}
            >
              {flagLabel(burst.verdict)}
            </span>
          </div>

          <div className="signals-line">
            {burst.clusterSize === 1 ? (
              <>Not part of a burst — no frame within {burst.timeGapSecs}s of it.</>
            ) : (
              <>
                Frame{" "}
                <b>
                  {burst.rank ?? "—"} of {burst.scored}
                </b>{" "}
                by sharpness, in a burst of {burst.clusterSize}
                {burst.truncated && "+"}
                {burst.timeGroupSize > burst.clusterSize && (
                  <> (split from {burst.timeGroupSize} frames shot together)</>
                )}
                .
              </>
            )}
          </div>

          {burst.median != null && burst.cutoff != null && burst.clusterSize > 1 && (
            <div className="signals-line">
              Cluster median <b>{score(burst.median)}</b> · soft below{" "}
              <b>{score(burst.cutoff)}</b> ({Math.round(burst.softFraction * 100)}% of the
              median)
              {burst.best && (
                <>
                  {" "}
                  · sharpest is <b>{burst.best.fileName}</b> at {score(burst.best.sharpness)}
                </>
              )}
            </div>
          )}

          {burst.stale && (
            <div className="signals-warn">
              The badge on this photo says <b>{flagLabel(burst.storedFlag)}</b>, but this
              burst now reads <b>{flagLabel(burst.verdict)}</b>. The stored flag came from an
              earlier run over a different set of photos — re-run burst analysis to refresh
              it.
            </div>
          )}

          {burst.truncated && (
            <div className="signals-warn">
              This run of frames is longer than one lookup can cover, so the count, rank and
              median above are lower bounds on a possibly larger burst.
            </div>
          )}

          {burst.frames.length > 1 && (
            <>
              <table className="signals-frames">
                <thead>
                  <tr>
                    <th>Frame</th>
                    <th>Sharp</th>
                    <th>Δ</th>
                    <th />
                  </tr>
                </thead>
                <tbody>
                  {burst.frames.map((f) => (
                    <FrameRow key={f.photoId} frame={f} threshold={burst.hammingThreshold} />
                  ))}
                </tbody>
              </table>
              {burst.frames.length < burst.clusterSize && (
                <div className="signals-note">
                  Showing {burst.frames.length} of {burst.clusterSize} frames.
                </div>
              )}
              <div className="signals-note">
                Δ is the visual difference from this photo (0 = identical). The engine treats
                frames within {burst.hammingThreshold} as the same scene.
              </div>
            </>
          )}
        </div>
      )}

      {(stack.childCount > 0 || stack.parentId != null || versionCount > 0) && (
        <div className="signals-block">
          <div className="signals-head">Other badges</div>
          {stack.childCount > 0 && (
            <div className="signals-line">
              {stack.childCount} file{stack.childCount === 1 ? "" : "s"} stacked under this one
              (e.g. the camera JPEG) — see the Stack section.
            </div>
          )}
          {stack.parentId != null && (
            <div className="signals-line">
              Stacked under photo #{stack.parentId}, which is what the grid lists instead.
            </div>
          )}
          {versionCount > 0 && (
            <div className="signals-line">
              {versionCount} edit version{versionCount === 1 ? "" : "s"} — see the Versions
              section.
            </div>
          )}
        </div>
      )}
    </div>
  );
}

function FrameRow({ frame, threshold }: { frame: ClusterFrame; threshold: number }) {
  const mark =
    frame.verdict === "sharpest-of-burst" ? "♛" : frame.verdict === "soft-in-burst" ? "~" : "";
  return (
    <tr className={frame.isSubject ? "signals-subject" : ""}>
      <td className="signals-frame-name" title={frame.fileName}>
        {frame.isSubject && <span className="signals-you">▸</span>}
        {frame.fileName}
      </td>
      <td className="signals-num">{score(frame.sharpness)}</td>
      <td
        className="signals-num"
        title={
          frame.hammingDistance == null
            ? "Not hashed yet"
            : frame.hammingDistance <= threshold
              ? "Same scene as this photo"
              : "A different scene"
        }
      >
        {frame.hammingDistance ?? "—"}
      </td>
      <td className="signals-mark" title={flagLabel(frame.verdict)}>
        {mark}
      </td>
    </tr>
  );
}
