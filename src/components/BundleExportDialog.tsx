import { useState } from "react";
import { BundleWriteResult, exportBundle, pickFolder } from "../modules/api";

// Dialog for exporting one import batch as a .chairphoto bundle zip (F1e).
// The user picks a destination folder, names the file, and clicks Export.
// Progress is shown in the topbar via import:progress events (same as ingest).
export function BundleExportDialog({
  batchId,
  batchLabel,
  onClose,
  onExport,
  onClear,
}: {
  /** The import batch to export. */
  batchId: number;
  /** Display label (source folder name or uuid). */
  batchLabel: string;
  onClose: () => void;
  /** Called when the export finishes successfully. */
  onExport?: (result: BundleWriteResult) => void;
  /** Called when the export completes (success or failure) — clears the topbar progress bar. */
  onClear?: () => void;
}) {
  const [dest, setDest] = useState("");
  const [filename, setFilename] = useState(
    // Derive a sensible default filename from the label; strip slashes/spaces.
    `${batchLabel.replace(/^.*\//, "").replace(/[^a-zA-Z0-9._-]/g, "_") || "export"}.chairphoto`,
  );
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<BundleWriteResult | null>(null);
  const [error, setError] = useState("");

  const browse = async () => {
    setError("");
    try {
      const picked = await pickFolder(dest || undefined);
      if (picked) setDest(picked);
    } catch (e) {
      setError(`Couldn't open the folder picker: ${String(e)}`);
    }
  };

  const run = async () => {
    setError("");
    setResult(null);
    const folder = dest.trim();
    const file = filename.trim();
    if (!folder) {
      setError("Choose a destination folder.");
      return;
    }
    if (!file) {
      setError("Enter a filename.");
      return;
    }
    // Ensure the file has the right extension.
    const name = file.endsWith(".chairphoto") ? file : `${file}.chairphoto`;
    const destPath = `${folder}/${name}`;
    setBusy(true);
    try {
      const r = await exportBundle(batchId, destPath);
      setResult(r);
      onExport?.(r);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
      onClear?.();
    }
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <div className="modal-title">Export batch as bundle</div>
          <button className="chip" onClick={onClose}>
            Close
          </button>
        </div>
        <div className="modal-body">
          <div className="field">
            <label>Batch</label>
            <span className="modal-sub">{batchLabel || "(unnamed)"}</span>
          </div>
          <div className="field">
            <label>Destination folder</label>
            <div className="input-with-browse">
              <input
                className="folder-input"
                placeholder="~/Documents"
                value={dest}
                onChange={(e) => setDest(e.target.value)}
              />
              <button className="chip" onClick={browse}>
                Browse…
              </button>
            </div>
          </div>
          <div className="field">
            <label>Bundle filename</label>
            <input
              className="folder-input"
              placeholder="my-trip.chairphoto"
              value={filename}
              onChange={(e) => setFilename(e.target.value)}
            />
          </div>
          <div className="field">
            <span className="term-note">
              Exports the batch's RAW originals, XMP sidecars, and catalog metadata (ratings,
              tags, versions) into a single <code>.chairphoto</code> zip. Originals that are
              offline/missing are skipped but their metadata travels.
            </span>
          </div>
          <div className="row">
            <button className="scan-btn" onClick={run} disabled={busy}>
              {busy ? "Exporting…" : "Export bundle"}
            </button>
            {busy && (
              <span className="modal-sub">Progress shows in the topbar.</span>
            )}
          </div>
          {result && (
            <div className="modal-sub">
              Bundle written. {result.exported} original{result.exported === 1 ? "" : "s"} exported.
              {result.skippedOffline > 0 &&
                ` ${result.skippedOffline} skipped — original offline/missing (connect the NAS to include them).`}
              {result.errors > 0 && ` ${result.errors} failed.`}
            </div>
          )}
          {error && <div className="modal-error">{error}</div>}
        </div>
      </div>
    </div>
  );
}
