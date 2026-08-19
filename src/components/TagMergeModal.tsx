import { useMemo, useState } from "react";
import type { TagWithCount } from "../modules/api";
import { mergeTags, type TagMergeReport } from "../modules/api";

/**
 * Merge one tag into another, in two deliberate steps: pick a target, then read what would
 * happen and confirm it.
 *
 * The preview is not a nicety — it is the whole point. This operation replaces hand-written
 * SQL against a live catalog, and a merge you cannot inspect first is exactly as frightening
 * as the SQL was. What is shown comes from a real merge that was rolled back, so the numbers
 * are what a commit will do, not an estimate of it.
 */
export function TagMergeModal({
  source,
  tags,
  onClose,
  onMerged,
}: {
  source: TagWithCount;
  tags: TagWithCount[];
  onClose: () => void;
  /** Called after a committed merge, with the report, so the caller can refresh and report. */
  onMerged: (report: TagMergeReport) => void;
}) {
  const [filter, setFilter] = useState("");
  const [target, setTarget] = useState<TagWithCount | null>(null);
  const [preview, setPreview] = useState<TagMergeReport | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  // A tag cannot merge into itself or into its own subtree — the backend refuses both, and
  // offering them would be offering a dead end.
  const candidates = useMemo(() => {
    const prefix = `${source.fullPath}/`;
    const needle = filter.trim().toLowerCase();
    return tags
      .filter((t) => t.id !== source.id && !t.fullPath.startsWith(prefix))
      .filter((t) => !needle || t.fullPath.toLowerCase().includes(needle))
      .slice(0, 200);
  }, [tags, source, filter]);

  const runPreview = async (candidate: TagWithCount) => {
    setTarget(candidate);
    setPreview(null);
    setError("");
    setBusy(true);
    try {
      setPreview(await mergeTags([source.id], candidate.id, true));
    } catch (e) {
      // A refusal (path collision, auto-tag, subtree) arrives here, already naming its cause.
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const commit = async () => {
    if (!target) return;
    setBusy(true);
    setError("");
    try {
      onMerged(await mergeTags([source.id], target.id, false));
    } catch (e) {
      setError(String(e));
      setBusy(false);
    }
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal tag-merge-modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <div className="modal-headinfo">
            <div className="modal-title">Merge “{source.name}”</div>
            <div className="modal-sub">
              {source.fullPath} — {source.photoCount.toLocaleString()} photo
              {source.photoCount === 1 ? "" : "s"}. The tag is removed; its photos, children,
              synonyms and album rules move to the tag you pick.
            </div>
          </div>
          <button className="chip" onClick={onClose}>
            Cancel
          </button>
        </div>

        <div className="modal-body">
          {!target && (
            <>
              <input
                className="tag-input"
                autoFocus
                placeholder="Merge into…"
                value={filter}
                onChange={(e) => setFilter(e.target.value)}
              />
              <div className="tag-move-list">
                {candidates.map((t) => (
                  <button
                    key={t.id}
                    className="tag-menu-item"
                    onClick={() => void runPreview(t)}
                  >
                    {t.fullPath}
                    <span className="tag-count">{t.photoCount.toLocaleString()}</span>
                  </button>
                ))}
                {candidates.length === 0 && (
                  <div className="modal-sub">No tag matches “{filter}”.</div>
                )}
              </div>
            </>
          )}

          {target && (
            <>
              <div className="modal-sub">
                <strong>{source.fullPath}</strong> → <strong>{target.fullPath}</strong>
              </div>

              {busy && !preview && <div className="modal-sub">Working out what would change…</div>}

              {error && (
                <div className="develop-note" role="alert">
                  {error}
                </div>
              )}

              {preview && <MergePreview report={preview} />}

              <div className="row" style={{ gap: 6, marginTop: 12 }}>
                <button
                  className="chip"
                  disabled={busy || !preview}
                  onClick={() => void commit()}
                >
                  {busy ? "Merging…" : "Merge"}
                </button>
                <button
                  className="chip"
                  disabled={busy}
                  onClick={() => {
                    setTarget(null);
                    setPreview(null);
                    setError("");
                  }}
                >
                  Pick a different tag
                </button>
              </div>
            </>
          )}
        </div>
      </div>
    </div>
  );
}

/**
 * The dry run, in words. Everything the merge touched gets a line — including the things it
 * could *not* move, which are the ones worth reading before committing.
 */
export function MergePreview({ report }: { report: TagMergeReport }) {
  const lines: string[] = [];

  if (report.photosRetagged > 0) {
    lines.push(`${report.photosRetagged.toLocaleString()} photos gain ${report.targetPath}`);
  }
  if (report.assignmentsCollapsed > 0) {
    lines.push(
      `${report.assignmentsCollapsed.toLocaleString()} already had it — two assignments become one`,
    );
  }
  if (report.childrenReparented > 0) {
    lines.push(
      `${report.childrenReparented} child tag${report.childrenReparented === 1 ? "" : "s"} move across` +
        (report.descendantsRepathed > 0
          ? `, and ${report.descendantsRepathed} tag path${
              report.descendantsRepathed === 1 ? "" : "s"
            } are rewritten`
          : ""),
    );
  }
  if (report.termsMoved > 0) lines.push(`${report.termsMoved} term(s) move`);
  if (report.synonymsMoved > 0) lines.push(`${report.synonymsMoved} synonym(s) move`);
  if (report.groupsRepointed > 0) {
    lines.push(`${report.groupsRepointed} quick-tag group membership(s) repoint`);
  }
  if (report.smartAlbumsRewritten.length > 0) {
    lines.push(`Smart album rules rewritten: ${report.smartAlbumsRewritten.join(", ")}`);
  }
  // null = that plugin is compiled out and nothing was checked; 0 = checked, nothing there.
  if (report.facesRepointed) lines.push(`${report.facesRepointed} face(s) stay named`);
  if (report.faceRejectionsRepointed) {
    lines.push(`${report.faceRejectionsRepointed} face rejection(s) follow`);
  }
  if (report.classifiersDropped) {
    lines.push(
      `${report.classifiersDropped} Smart Tagging classifier(s) dropped — retrained from the merged set`,
    );
  }
  if (report.suggestionsRepointed) {
    lines.push(`${report.suggestionsRepointed} pending suggestion(s) follow the merge`);
  }
  if (report.aliasesRecorded > 0) {
    lines.push(
      `${report.aliasesRecorded} tombstone(s) recorded, so importing an old bundle cannot bring the tag back`,
    );
  }

  return (
    <div className="tag-merge-preview">
      {lines.length === 0 ? (
        <div className="modal-sub">
          Nothing to move — the tag is empty. Merging removes it.
        </div>
      ) : (
        <ul className="tag-merge-lines">
          {lines.map((line) => (
            <li key={line}>{line}</li>
          ))}
        </ul>
      )}

      {report.termsSkipped.length > 0 && (
        <div className="modal-sub">
          Kept as they are — {report.targetPath} already has them: {report.termsSkipped.join(", ")}
        </div>
      )}

      {report.warnings.map((warning) => (
        <div key={warning} className="develop-note">
          {warning}
        </div>
      ))}
    </div>
  );
}
