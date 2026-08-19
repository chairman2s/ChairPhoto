import { useState } from "react";
import type { TagWithCount } from "../modules/api";
import { splitTag, type TagSplitReport } from "../modules/api";

/**
 * Split a tag by moving the selected photos onto a different one — the "this tag is doing
 * two jobs" operation, for a `Concert` that turned out to mean both the event and the venue.
 *
 * It acts only on the photos currently selected, because nothing else can know which half a
 * photo belongs to. Photos in the selection that never carried the tag are reported back
 * rather than tagged: the request was to split a tag, not to apply one.
 */
export function TagSplitModal({
  source,
  photoIds,
  onClose,
  onSplit,
}: {
  source: TagWithCount;
  photoIds: number[];
  onClose: () => void;
  onSplit: (message: string) => void;
}) {
  const [path, setPath] = useState("");
  const [keepSource, setKeepSource] = useState(false);
  const [preview, setPreview] = useState<TagSplitReport | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  const run = async (dryRun: boolean) => {
    const target = path.trim();
    if (!target) return;
    setBusy(true);
    setError("");
    try {
      const report = await splitTag(source.id, photoIds, target, keepSource, dryRun);
      if (dryRun) {
        setPreview(report);
      } else {
        onSplit(splitSummary(report));
      }
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal tag-merge-modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <div className="modal-headinfo">
            <div className="modal-title">Split “{source.name}”</div>
            <div className="modal-sub">
              {photoIds.length.toLocaleString()} selected photo
              {photoIds.length === 1 ? "" : "s"} move to a new tag.
              {keepSource
                ? " Both tags will apply to them."
                : ` They lose ${source.fullPath}.`}
            </div>
          </div>
          <button className="chip" onClick={onClose}>
            Cancel
          </button>
        </div>

        <div className="modal-body">
          <input
            className="tag-input"
            autoFocus
            placeholder="New tag path, e.g. Venue/Grieghallen"
            value={path}
            onChange={(e) => {
              setPath(e.target.value);
              setPreview(null);
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter") void run(true);
            }}
          />
          <label className="row" style={{ gap: 6, marginTop: 8 }}>
            <input
              type="checkbox"
              checked={keepSource}
              onChange={(e) => {
                setKeepSource(e.target.checked);
                setPreview(null);
              }}
            />
            Keep {source.name} on these photos too
          </label>

          {error && (
            <div className="develop-note" role="alert">
              {error}
            </div>
          )}

          {preview && (
            <ul className="tag-merge-lines">
              <li>
                {preview.photosMoved.toLocaleString()} photo
                {preview.photosMoved === 1 ? "" : "s"} gain {preview.newTagPath}
                {preview.createdNewTag ? " (a new tag)" : ""}
              </li>
              {preview.photosUntagged > 0 && (
                <li>
                  {preview.photosUntagged.toLocaleString()} lose {preview.sourcePath}
                </li>
              )}
              {preview.photosAlreadyTagged > 0 && (
                <li>{preview.photosAlreadyTagged.toLocaleString()} already had the new tag</li>
              )}
              {preview.photosWithoutSource > 0 && (
                <li>
                  {preview.photosWithoutSource.toLocaleString()} of the selection never carried{" "}
                  {preview.sourcePath} — left alone
                </li>
              )}
            </ul>
          )}

          <div className="row" style={{ gap: 6, marginTop: 12 }}>
            <button className="chip" disabled={busy || !path.trim()} onClick={() => void run(true)}>
              Preview
            </button>
            <button
              className="chip"
              disabled={busy || !preview}
              onClick={() => void run(false)}
            >
              {busy ? "Splitting…" : "Split"}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}

/** One line for the status bar, counting only what actually happened. */
export function splitSummary(report: TagSplitReport): string {
  const parts = [
    `${report.photosMoved.toLocaleString()} photo${report.photosMoved === 1 ? "" : "s"} moved to ${report.newTagPath}`,
  ];
  if (report.photosAlreadyTagged > 0) {
    parts.push(`${report.photosAlreadyTagged} already had it`);
  }
  if (report.photosWithoutSource > 0) {
    parts.push(`${report.photosWithoutSource} never carried ${report.sourcePath}`);
  }
  return `${parts.join(" — ")}.`;
}
