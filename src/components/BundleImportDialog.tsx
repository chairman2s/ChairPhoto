import { useState } from "react";
import {
  BundleImportResult,
  BundlePreview,
  importBundle,
  pickBundleFile,
  previewBundle,
} from "../modules/api";

// Dialog for importing a .chairphoto bundle (F1e).
// Step 1: the user picks a .chairphoto file using the native file picker.
// Step 2: a lightweight preview command peeks at the manifest and shows
//         "N new / M already present" so the user can confirm before committing.
// Step 3: the actual import runs in the background; progress shows in the topbar.
export function BundleImportDialog({
  onClose,
  onImport,
  onClear,
}: {
  onClose: () => void;
  /**
   * Called when the import finishes successfully.
   * The caller should refresh the grid and bump the batches-reload key.
   */
  onImport?: (result: BundleImportResult) => void;
  /** Called when the import completes (success or failure) — clears the topbar progress bar. */
  onClear?: () => void;
}) {
  const [bundlePath, setBundlePath] = useState("");
  const [preview, setPreview] = useState<BundlePreview | null>(null);
  const [previewing, setPreviewing] = useState(false);
  const [importing, setImporting] = useState(false);
  const [result, setResult] = useState<BundleImportResult | null>(null);
  const [error, setError] = useState("");

  // Open the native file picker filtered to .chairphoto files, then preview.
  const browse = async () => {
    setError("");
    setPreview(null);
    setResult(null);
    try {
      const p = await pickBundleFile(bundlePath || undefined);
      if (!p) return;
      setBundlePath(p);
      await loadPreview(p);
    } catch (e) {
      setError(`Couldn't open the file picker: ${String(e)}`);
    }
  };

  // Preview a bundle at the given path (peek at its manifest, count new vs existing).
  const loadPreview = async (path: string) => {
    setPreview(null);
    setResult(null);
    setError("");
    if (!path.trim()) return;
    setPreviewing(true);
    try {
      const p = await previewBundle(path.trim());
      setPreview(p);
    } catch (e) {
      setError(`Could not read bundle: ${String(e)}`);
    } finally {
      setPreviewing(false);
    }
  };

  const run = async () => {
    setError("");
    setResult(null);
    if (!bundlePath.trim()) {
      setError("Choose a .chairphoto bundle file to import.");
      return;
    }
    setImporting(true);
    try {
      const r = await importBundle(bundlePath.trim());
      setResult(r);
      onImport?.(r);
    } catch (e) {
      setError(String(e));
    } finally {
      setImporting(false);
      onClear?.();
    }
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal import-modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <div className="modal-title">Import bundle</div>
          <button className="chip" onClick={onClose}>
            Close
          </button>
        </div>
        <div className="modal-body">
          <div className="field">
            <label>Bundle file (.chairphoto)</label>
            <div className="input-with-browse">
              <input
                className="folder-input"
                placeholder="~/Documents/my-trip.chairphoto"
                value={bundlePath}
                onChange={(e) => {
                  setBundlePath(e.target.value);
                  setPreview(null);
                  setResult(null);
                }}
                onKeyDown={(e) => e.key === "Enter" && loadPreview(bundlePath)}
              />
              <button className="chip" onClick={browse}>
                Browse…
              </button>
              <button
                className="chip"
                onClick={() => loadPreview(bundlePath)}
                disabled={previewing || !bundlePath.trim()}
              >
                {previewing ? "Checking…" : "Check"}
              </button>
            </div>
          </div>

          {preview && !result && (
            <div className="field">
              <div className="import-toolbar">
                <span className="modal-sub">
                  Batch: <b>{preview.batchLabel || "(unnamed)"}</b>
                </span>
              </div>
              <div className="import-toolbar" style={{ marginTop: 4 }}>
                <span className="modal-sub">
                  {preview.total} photo{preview.total === 1 ? "" : "s"} in bundle
                  {" · "}
                  <b>{preview.newCount} new</b>
                  {preview.existing > 0 && ` · ${preview.existing} already in catalog`}
                </span>
              </div>
              {preview.newCount === 0 && preview.existing > 0 && (
                <div className="modal-sub" style={{ marginTop: 4 }}>
                  All photos are already present — re-importing will be a no-op (safe to run).
                </div>
              )}
            </div>
          )}

          <div className="field">
            <span className="term-note">
              Originals copy into your library under Year/Month/Day. Existing photos (matched by
              UUID) are never overwritten — the import is always additive. Progress shows in the
              topbar; the dialog can be closed once import starts.
            </span>
          </div>

          <div className="row">
            <button
              className="scan-btn"
              onClick={run}
              disabled={importing || !bundlePath.trim() || !preview}
            >
              {importing
                ? "Importing…"
                : preview
                  ? `Import${preview.newCount > 0 ? ` ${preview.newCount} new` : " (no-op)"}`
                  : "Import bundle"}
            </button>
            {importing && (
              <span className="modal-sub">Runs in the background — progress shows up top.</span>
            )}
          </div>

          {result && (
            <div className="modal-sub">
              Import complete.{" "}
              {result.merge.photosAdded > 0
                ? `${result.merge.photosAdded} photo${result.merge.photosAdded === 1 ? "" : "s"} added`
                : "No new photos"}.
              {result.copied > 0 && ` ${result.copied} original${result.copied === 1 ? "" : "s"} copied.`}
              {result.skippedDuplicate > 0 &&
                ` ${result.skippedDuplicate} already present (skipped).`}
              {result.merge.tagsCreated > 0 &&
                ` ${result.merge.tagsCreated} tag${result.merge.tagsCreated === 1 ? "" : "s"} created.`}
              {result.errors > 0 && ` ${result.errors} error(s).`}
            </div>
          )}
          {error && <div className="modal-error">{error}</div>}
        </div>
      </div>
    </div>
  );
}
