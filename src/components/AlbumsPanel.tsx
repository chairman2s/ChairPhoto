import { useCallback, useEffect, useState } from "react";
import {
  Album,
  createAlbum,
  deleteAlbum,
  listAlbums,
  renameAlbum,
} from "../modules/api";

// Left sidebar section: manual albums (organising axis 3 — see storage-and-import.md).
// Clicking an album filters the grid to its photos (composed with the culling chips).
// With photos selected, each album shows an "+N" button to add the selection.
export function AlbumsPanel({
  activeAlbumId,
  onSelectAlbum,
  selectionCount,
  onAddSelection,
  reloadKey,
}: {
  activeAlbumId: number | null;
  onSelectAlbum: (albumId: number | null) => void;
  selectionCount: number;
  /** Add the current selection to an album; resolves so we can refresh counts. */
  onAddSelection: (albumId: number) => Promise<void>;
  /** Bump to force a reload (e.g. after photos are added elsewhere). */
  reloadKey?: number;
}) {
  const [albums, setAlbums] = useState<Album[]>([]);

  const reload = useCallback(() => {
    listAlbums().then(setAlbums).catch(() => {});
  }, []);

  useEffect(() => {
    reload();
  }, [reload, reloadKey]);

  const onNew = async () => {
    const name = window.prompt("New album name")?.trim();
    if (!name) return;
    await createAlbum(name);
    reload();
  };

  const onRename = async (album: Album) => {
    const name = window.prompt("Rename album", album.name)?.trim();
    if (!name || name === album.name) return;
    await renameAlbum(album.id, name);
    reload();
  };

  const onDelete = async (album: Album) => {
    if (!window.confirm(`Delete album "${album.name}"? Photos are not deleted.`)) return;
    await deleteAlbum(album.id);
    if (activeAlbumId === album.id) onSelectAlbum(null);
    reload();
  };

  const onAdd = async (album: Album) => {
    await onAddSelection(album.id);
    reload();
  };

  return (
    <aside className="panel albums-panel">
      <div className="panel-header">
        <span className="panel-head">Albums</span>
        <button className="panel-add" title="New album" onClick={onNew}>
          ＋
        </button>
      </div>
      {albums.map((album) => (
        <div
          key={album.id}
          className={`tag-row ${album.id === activeAlbumId ? "tag-active" : ""}`}
        >
          <button
            className="tag-filter"
            onClick={() =>
              onSelectAlbum(album.id === activeAlbumId ? null : album.id)
            }
          >
            <span className="tag-name">{album.name}</span>
          </button>
          {selectionCount > 0 && (
            <button
              className="album-add"
              title={`Add ${selectionCount} selected photo(s) to ${album.name}`}
              onClick={() => onAdd(album)}
            >
              +{selectionCount}
            </button>
          )}
          <span className="tag-count">{album.photoCount}</span>
          <button className="tag-edit" title="Rename" onClick={() => onRename(album)}>
            ⚙
          </button>
          <button className="tag-edit" title="Delete album" onClick={() => onDelete(album)}>
            ✕
          </button>
        </div>
      ))}
      {albums.length === 0 && <div className="panel-empty">No albums yet</div>}
    </aside>
  );
}
