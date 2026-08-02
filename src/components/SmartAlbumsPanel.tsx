import { useCallback, useEffect, useState } from "react";
import {
  SmartAlbum,
  deleteSmartAlbum,
  listSmartAlbums,
  renameSmartAlbum,
} from "../modules/api";
import { SmartAlbumEditor } from "./SmartAlbumEditor";

// Left sidebar section: smart albums (saved rules evaluated live — see docs/smart-albums.md).
// Clicking a smart album filters the grid to the photos matching its rule (composed with the
// culling chips). Mirrors AlbumsPanel; the difference is membership is a query, not a list.
export function SmartAlbumsPanel({
  activeSmartAlbumId,
  onSelectSmartAlbum,
  onEditRule,
  reloadKey,
}: {
  activeSmartAlbumId: number | null;
  onSelectSmartAlbum: (smartAlbumId: number | null) => void;
  /** Optional hook fired (in addition to opening the builder) when a rule is edited —
   *  lets the host refresh dependent state. The panel owns the builder modal itself. */
  onEditRule?: (album: SmartAlbum) => void;
  /** Bump to force a reload (e.g. after a rule changes the count). */
  reloadKey?: number;
}) {
  const [albums, setAlbums] = useState<SmartAlbum[]>([]);
  // The rule builder modal: `undefined` = closed, `null` = open for a new album,
  // a SmartAlbum = open for editing that album's rule (round-tripped into the editor).
  const [editing, setEditing] = useState<SmartAlbum | null | undefined>(undefined);

  const reload = useCallback(() => {
    listSmartAlbums().then(setAlbums).catch(() => {});
  }, []);

  useEffect(() => {
    reload();
  }, [reload, reloadKey]);

  const onNew = () => setEditing(null);

  const openEditor = (album: SmartAlbum) => {
    onEditRule?.(album);
    setEditing(album);
  };

  // Saved (create or rule update): refresh the list, select the album, close the modal.
  const onSaved = (id: number) => {
    reload();
    onSelectSmartAlbum(id);
    setEditing(undefined);
  };

  const onRename = async (album: SmartAlbum) => {
    const name = window.prompt("Rename smart album", album.name)?.trim();
    if (!name || name === album.name) return;
    await renameSmartAlbum(album.id, name);
    reload();
  };

  const onDelete = async (album: SmartAlbum) => {
    if (!window.confirm(`Delete smart album "${album.name}"? Photos are not deleted.`)) return;
    await deleteSmartAlbum(album.id);
    if (activeSmartAlbumId === album.id) onSelectSmartAlbum(null);
    reload();
  };

  return (
    <aside className="panel smart-albums-panel">
      <div className="panel-header">
        <span className="panel-head">Smart albums</span>
        <button className="panel-add" title="New smart album" onClick={onNew}>
          ＋
        </button>
      </div>
      {albums.map((album) => (
        <div
          key={album.id}
          className={`tag-row ${album.id === activeSmartAlbumId ? "tag-active" : ""}`}
        >
          <button
            className="tag-filter"
            onClick={() =>
              onSelectSmartAlbum(album.id === activeSmartAlbumId ? null : album.id)
            }
          >
            <span className="tag-name">{album.name}</span>
          </button>
          <span className="tag-count">{album.photoCount}</span>
          <button
            className="tag-edit"
            title="Edit rule"
            onClick={() => openEditor(album)}
          >
            ⚙
          </button>
          <button className="tag-edit" title="Rename" onClick={() => onRename(album)}>
            ✎
          </button>
          <button
            className="tag-edit"
            title="Delete smart album"
            onClick={() => onDelete(album)}
          >
            ✕
          </button>
        </div>
      ))}
      {albums.length === 0 && <div className="panel-empty">No smart albums yet</div>}
      {editing !== undefined && (
        <SmartAlbumEditor
          album={editing}
          onClose={() => setEditing(undefined)}
          onSaved={onSaved}
        />
      )}
    </aside>
  );
}
