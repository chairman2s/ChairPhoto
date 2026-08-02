import { useCallback, useEffect, useState } from "react";
import {
  createVersion,
  deleteVersion,
  duplicateVersion,
  listVersions,
  PhotoVersion,
  renameVersion,
} from "../modules/api";

// Inspector "Versions" panel (H5a/H5b): manage and select the named non-destructive
// variants of a photo. Clicking a row selects it as the active version (what the loupe
// shows); "Original" is the unedited base. With the Basic Editor module enabled, each
// version gets an Edit button (crop/exposure). See docs/editing.md.
export function VersionsPanel({
  photoId,
  onChanged,
  activeVersionId,
  onSelectVersion,
  canEdit,
  onEditVersion,
}: {
  photoId: number;
  onChanged: () => void;
  activeVersionId: number | null;
  onSelectVersion: (v: PhotoVersion | null) => void;
  canEdit: boolean;
  onEditVersion: (v: PhotoVersion) => void;
}) {
  const [versions, setVersions] = useState<PhotoVersion[]>([]);
  const [newName, setNewName] = useState("");
  const [editingId, setEditingId] = useState<number | null>(null);
  const [editingName, setEditingName] = useState("");

  const reload = useCallback(() => {
    listVersions(photoId).then(setVersions).catch(() => setVersions([]));
  }, [photoId]);

  useEffect(() => {
    reload();
  }, [reload]);

  const afterChange = () => {
    reload();
    onChanged();
  };

  const add = async () => {
    const name = newName.trim() || `Version ${versions.length + 1}`;
    await createVersion(photoId, name);
    setNewName("");
    afterChange();
  };

  const commitRename = async () => {
    if (editingId != null && editingName.trim()) {
      await renameVersion(editingId, editingName.trim());
    }
    setEditingId(null);
    setEditingName("");
    afterChange();
  };

  return (
    <div className="versions">
      <ul className="versions-list">
        <li className={`versions-row ${activeVersionId == null ? "versions-active" : ""}`}>
          <button className="versions-name" onClick={() => onSelectVersion(null)}>
            Original
          </button>
        </li>
        {versions.map((v) => (
          <li
            key={v.id}
            className={`versions-row ${activeVersionId === v.id ? "versions-active" : ""}`}
          >
            {editingId === v.id ? (
              <input
                className="folder-input versions-rename"
                value={editingName}
                autoFocus
                onChange={(e) => setEditingName(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") commitRename();
                  if (e.key === "Escape") setEditingId(null);
                }}
                onBlur={commitRename}
              />
            ) : (
              <button
                className="versions-name"
                title="Click to view · double-click to rename"
                onClick={() => onSelectVersion(v)}
                onDoubleClick={() => {
                  setEditingId(v.id);
                  setEditingName(v.name);
                }}
              >
                {v.name}
              </button>
            )}
            <div className="versions-actions">
              {canEdit && (
                <button className="chip" title="Edit crop / exposure" onClick={() => onEditVersion(v)}>
                  ✎
                </button>
              )}
              <button
                className="chip"
                title="Duplicate this version"
                onClick={async () => {
                  await duplicateVersion(v.id);
                  afterChange();
                }}
              >
                ⧉
              </button>
              <button
                className="chip"
                title="Delete this version (the original is never affected)"
                onClick={async () => {
                  await deleteVersion(v.id);
                  if (activeVersionId === v.id) onSelectVersion(null);
                  afterChange();
                }}
              >
                ✕
              </button>
            </div>
          </li>
        ))}
      </ul>
      <div className="versions-add">
        <input
          className="folder-input"
          placeholder="New version (e.g. Instagram square)"
          value={newName}
          onChange={(e) => setNewName(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") add();
          }}
        />
        <button className="chip" onClick={add}>
          + Add
        </button>
      </div>
      {!canEdit && versions.length > 0 && (
        <div className="editor-hint">Enable “Basic Editor” in Preferences → Modules to edit crop/exposure.</div>
      )}
    </div>
  );
}
