import { useEffect, useState } from "react";
import {
  assembleHashtagBundle,
  ExportPreset,
  ExportResult,
  exportPhotos,
  ImportBatch,
  listTagGroups,
  TagGroup,
} from "../modules/api";

// One-way export-to-files dialog (see docs/storage-and-import.md). Exports the given photos
// to a destination folder using a preset (RAW hand-off, or full JPEG). Publishing to
// services lives in the separate Publish dialog. Reports how many originals were offline so
// the user knows the set wasn't silently truncated.
//
// When `activeBatch` is provided (i.e. the user is browsing a specific import batch),
// an additional "Export as bundle…" entry point is shown so the user can reach the
// bundle export dialog without going back to the Batches sidebar (F1e spec requirement).
export function ExportPanel({
  photoIds,
  versionId,
  versionName,
  activeBatch,
  onExportBatch,
  onClose,
}: {
  photoIds: number[];
  /** The version active in the inspector/editor; Show-off renders it at full res. */
  versionId?: number | null;
  /** That version's name, shown in the Show-off hint. */
  versionName?: string | null;
  /** The import batch currently active as the grid scope, if any. */
  activeBatch?: ImportBatch | null;
  /** Called when the user clicks "Export as bundle…" for the active batch. */
  onExportBatch?: (batch: ImportBatch) => void;
  onClose: () => void;
}) {
  const [dest, setDest] = useState("~/Pictures/Export");
  const [preset, setPreset] = useState<ExportPreset>("handOff");
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<ExportResult | null>(null);
  const [error, setError] = useState("");

  // Reach-hashtag bundle (G3): a tag group emitted as hashtags.txt + a copy button.
  const [groups, setGroups] = useState<TagGroup[]>([]);
  const [hashtagGroupId, setHashtagGroupId] = useState<number | null>(null);
  const [limit, setLimit] = useState("");
  const [bundle, setBundle] = useState<string[]>([]);
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    listTagGroups().then(setGroups).catch(() => {});
  }, []);

  // Preview the bundle whenever the group or limit changes.
  useEffect(() => {
    setCopied(false);
    if (hashtagGroupId == null) {
      setBundle([]);
      return;
    }
    const n = limit.trim() ? parseInt(limit, 10) : null;
    let alive = true;
    assembleHashtagBundle(hashtagGroupId, Number.isNaN(n as number) ? null : n)
      .then((b) => alive && setBundle(b))
      .catch(() => alive && setBundle([]));
    return () => {
      alive = false;
    };
  }, [hashtagGroupId, limit]);

  const limitNum = limit.trim() && !Number.isNaN(parseInt(limit, 10)) ? parseInt(limit, 10) : null;

  const run = async () => {
    setError("");
    setResult(null);
    if (!dest.trim()) {
      setError("Choose a destination folder.");
      return;
    }
    setBusy(true);
    try {
      setResult(
        await exportPhotos(photoIds, preset, dest.trim(), hashtagGroupId, limitNum, versionId),
      );
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const copyBundle = async () => {
    try {
      await navigator.clipboard.writeText(bundle.join(" "));
      setCopied(true);
    } catch {
      setError("Couldn't copy to clipboard.");
    }
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <div className="modal-title">Export {photoIds.length} photo(s)</div>
          <button className="chip" onClick={onClose}>
            Close
          </button>
        </div>
        <div className="modal-body">
          <div className="field">
            <label>Preset</label>
            <div className="row">
              <label className="term-export">
                <input
                  type="radio"
                  name="preset"
                  checked={preset === "handOff"}
                  onChange={() => setPreset("handOff")}
                />
                Hand-off (RAW + XMP)
              </label>
              <label className="term-export">
                <input
                  type="radio"
                  name="preset"
                  checked={preset === "showOff"}
                  onChange={() => setPreset("showOff")}
                />
                Show off (JPEG)
              </label>
            </div>
            {preset === "showOff" && (
              <p className="export-hint">
                {versionName
                  ? `Renders the “${versionName}” version`
                  : "Renders the full-resolution original (no edit selected)"}{" "}
                at full resolution.
              </p>
            )}
          </div>
          <div className="field">
            <label>Destination</label>
            <input
              className="folder-input"
              value={dest}
              onChange={(e) => setDest(e.target.value)}
              placeholder="~/Pictures/Export"
            />
          </div>
          <div className="field">
            <label>Reach hashtags (optional)</label>
            <div className="row">
              <select
                className="folder-input"
                value={hashtagGroupId ?? ""}
                onChange={(e) =>
                  setHashtagGroupId(e.target.value ? Number(e.target.value) : null)
                }
              >
                <option value="">None</option>
                {groups.map((g) => (
                  <option key={g.id} value={g.id}>
                    {g.name}
                  </option>
                ))}
              </select>
              <input
                className="tag-input"
                style={{ maxWidth: 90 }}
                type="number"
                min="1"
                placeholder="limit"
                value={limit}
                onChange={(e) => setLimit(e.target.value)}
                disabled={hashtagGroupId == null}
              />
            </div>
            {bundle.length > 0 && (
              <div className="row">
                <span className="modal-sub" style={{ flex: 1, wordBreak: "break-word" }}>
                  {bundle.join(" ")}
                </span>
                <button className="chip" onClick={copyBundle}>
                  {copied ? "Copied" : "Copy"}
                </button>
              </div>
            )}
            <span className="term-note">Written as hashtags.txt alongside the export.</span>
          </div>
          <div className="row">
            <button className="scan-btn" onClick={run} disabled={busy || photoIds.length === 0}>
              {busy ? "Exporting…" : "Export"}
            </button>
          </div>
          {activeBatch && onExportBatch && (
            <div className="field">
              <label>Bundle export</label>
              <div className="row">
                <button
                  className="chip"
                  onClick={() => {
                    onClose();
                    onExportBatch(activeBatch);
                  }}
                  title={`Export the "${activeBatch.sourceLabel.replace(/\/+$/, "").split("/").pop() || activeBatch.sourceLabel}" batch as a .chairphoto bundle for transfer to another machine`}
                >
                  Export as bundle…
                </button>
              </div>
              <span className="term-note">
                Packages the current batch's originals, XMP sidecars, and metadata into a
                single .chairphoto file that can be imported on another machine.
              </span>
            </div>
          )}
          {result && (
            <div className="modal-sub">
              Exported {result.exported}.
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
