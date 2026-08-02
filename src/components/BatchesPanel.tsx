import { useCallback, useEffect, useState } from "react";
import { ImportBatch, listImportBatches } from "../modules/api";

// Left sidebar section: import batches ("negative film roll") — auto-created per
// ingest, immutable, read-only. Clicking one filters the grid to that batch.
// Each batch row has an "Export as bundle…" action (F1e) to kick off a bundle export.
export function BatchesPanel({
  activeBatchId,
  onSelectBatch,
  onExportBatch,
  reloadKey,
}: {
  activeBatchId: number | null;
  /** Called when the user selects (or deselects) a batch. Passes the full batch object
   *  so callers can access metadata (label, uuid, etc.) without a separate lookup. */
  onSelectBatch: (batch: ImportBatch | null) => void;
  /** Called when the user clicks "Export as bundle…" on a batch. */
  onExportBatch?: (batch: ImportBatch) => void;
  /** Bump to reload (e.g. after a scan creates a new batch). */
  reloadKey?: number;
}) {
  const [batches, setBatches] = useState<ImportBatch[]>([]);

  const reload = useCallback(() => {
    listImportBatches().then(setBatches).catch(() => {});
  }, []);
  useEffect(() => {
    reload();
  }, [reload, reloadKey]);

  // The source label is usually a folder path; show its last segment as the title.
  const title = (b: ImportBatch) =>
    b.sourceLabel.replace(/\/+$/, "").split("/").pop() || b.sourceLabel || "(ingest)";

  return (
    <aside className="panel batches-panel">
      <div className="panel-header"><span className="panel-head">Import batches</span></div>
      {batches.map((b) => (
        <div
          key={b.id}
          className={`tag-row ${b.id === activeBatchId ? "tag-active" : ""}`}
        >
          <button
            className="tag-filter"
            title={b.sourceLabel}
            onClick={() => onSelectBatch(b.id === activeBatchId ? null : b)}
          >
            <span className="tag-name">{title(b)}</span>
          </button>
          <span className="tag-count">{b.photoCount}</span>
          {onExportBatch && (
            <button
              className="tag-edit"
              title="Export this batch as a .chairphoto bundle for transfer to another machine"
              onClick={(e) => {
                e.stopPropagation();
                onExportBatch(b);
              }}
            >
              ⬇
            </button>
          )}
        </div>
      ))}
      {batches.length === 0 && <div className="panel-empty">No imports yet</div>}
    </aside>
  );
}
