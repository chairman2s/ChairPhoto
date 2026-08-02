import { useEffect, useRef, useState } from "react";
import { createTag } from "../modules/api";
import { parseTagPaste } from "../modules/tagPaste";

const PLACEHOLDER = `Objects
  Cooking Equipment
    Grill
      Gas Grill
      Charcoal Grill

Indent with spaces or tabs to build a hierarchy · a/b creates nested tags`;

const PREVIEW_CAP = 30;

/** Returns depth (0-based) of a path string. */
function pathDepth(path: string): number {
  return path.split("/").length - 1;
}

export function TagCreateModal({
  parentPath,
  onClose,
  onChanged,
}: {
  parentPath: string | null;
  onClose: () => void;
  onChanged: () => void;
}) {
  const [text, setText] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  // Autofocus the textarea on mount.
  useEffect(() => {
    textareaRef.current?.focus();
  }, []);

  // Close on Escape.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  // Parse the textarea content into full paths, prepending parentPath when set.
  const parsed = parseTagPaste(text);
  const paths = parentPath
    ? parsed.map((p) => `${parentPath}/${p}`)
    : parsed;

  const shown = paths.slice(0, PREVIEW_CAP);
  const overflow = paths.length - shown.length;

  // Minimum depth for display indentation: depth of parentPath (if any) + 1 for the parsed items.
  const baseDepth = parentPath ? parentPath.split("/").length : 0;

  const handleConfirm = async () => {
    if (paths.length === 0 || busy) return;
    setBusy(true);
    setError("");
    for (const p of paths) {
      try {
        await createTag(p);
      } catch (e) {
        setError(`Failed to create "${p}": ${e}`);
        setBusy(false);
        return;
      }
    }
    setBusy(false);
    onChanged();
    onClose();
  };

  const title = parentPath ? `New tags under "${parentPath}"` : "New tags";
  const confirmLabel =
    paths.length === 0
      ? "Create tags"
      : `Create ${paths.length} tag${paths.length === 1 ? "" : "s"}`;

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal tag-create-modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <div className="modal-headinfo">
            <div className="modal-title">{title}</div>
            {parentPath && (
              <div className="modal-sub tag-create-parent-chip">{parentPath}</div>
            )}
            {error && <div className="modal-error">{error}</div>}
          </div>
          <button className="chip" onClick={onClose}>
            Cancel
          </button>
        </div>

        <div className="modal-body">
          <textarea
            ref={textareaRef}
            className="tag-input tag-create-textarea"
            rows={10}
            placeholder={PLACEHOLDER}
            value={text}
            onChange={(e) => {
              setText(e.target.value);
              setError("");
            }}
          />

          {/* Live preview */}
          <div className="tag-create-preview">
            {paths.length === 0 ? (
              <span className="tag-create-preview-empty">
                Type tag names above — they'll appear here
              </span>
            ) : (
              <>
                {shown.map((p) => {
                  const depth = pathDepth(p) - baseDepth;
                  return (
                    <div
                      key={p}
                      className="tag-create-preview-row"
                      style={{ paddingLeft: depth * 14 }}
                    >
                      {p.split("/").slice(-1)[0]}
                    </div>
                  );
                })}
                {overflow > 0 && (
                  <div className="tag-create-preview-more">
                    … and {overflow} more
                  </div>
                )}
                <div className="tag-create-preview-count">
                  {paths.length} tag{paths.length === 1 ? "" : "s"} total
                </div>
              </>
            )}
          </div>
        </div>

        <div className="tag-create-footer">
          <button className="chip" onClick={onClose}>
            Cancel
          </button>
          <button
            className="btn-primary"
            disabled={paths.length === 0 || busy}
            onClick={handleConfirm}
          >
            {busy ? "Creating…" : confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
}
