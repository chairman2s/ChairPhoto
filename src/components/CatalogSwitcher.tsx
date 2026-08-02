import { useEffect, useState } from "react";
import {
  listRecentCatalogs,
  switchCatalog,
  pickFolder,
  RecentCatalog,
} from "../modules/api";

/**
 * CatalogSwitcher — modal for opening an existing catalog or creating a new one.
 *
 * Two sections:
 *  1. Recent catalogs list (name, path, last opened) — click any row to switch.
 *  2. "New catalog" form — name + folder picker (via pickFolder).
 *
 * On a successful switch the modal closes via `onClose`. The backend emits
 * `catalog:switched`, which App.tsx listens to and resets all React state.
 */
export function CatalogSwitcher({ onClose }: {
  onClose: () => void;
}) {
  const [recents, setRecents] = useState<RecentCatalog[]>([]);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  // "New catalog" form state.
  const [newName, setNewName] = useState("");
  const [newFolder, setNewFolder] = useState("");
  const [showNew, setShowNew] = useState(false);

  useEffect(() => {
    listRecentCatalogs()
      .then(setRecents)
      .catch((e) => setError(String(e)))
      .finally(() => setLoading(false));
  }, []);

  // Format a Unix timestamp as a friendly relative date.
  const formatDate = (ts: number): string => {
    const d = new Date(ts * 1000);
    const now = Date.now();
    const diffMs = now - d.getTime();
    const diffDays = Math.floor(diffMs / 86400000);
    if (diffDays === 0) return "Today";
    if (diffDays === 1) return "Yesterday";
    if (diffDays < 7) return `${diffDays} days ago`;
    return d.toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });
  };

  // Open an existing catalog from the recent list.
  const openRecent = async (rc: RecentCatalog) => {
    setError("");
    setBusy(true);
    try {
      await switchCatalog(rc.catalogPath, rc.root, false, rc.name);
      onClose();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  // Browse for the folder that will contain the new catalog.
  const browseNewFolder = async () => {
    const picked = await pickFolder(newFolder || undefined);
    if (picked) setNewFolder(picked);
  };

  // Create a brand-new catalog.
  const createNew = async () => {
    setError("");
    const name = newName.trim();
    if (!name) {
      setError("Give the catalog a name.");
      return;
    }
    const folder = newFolder.trim();
    if (!folder) {
      setError("Choose a folder for the new catalog.");
      return;
    }
    setBusy(true);
    try {
      // The catalog file is <folder>/<name>.chairphoto; photos root at <folder>.
      const catalogPath = `${folder}/${name}.chairphoto`;
      await switchCatalog(catalogPath, folder, true, name);
      onClose();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="modal-backdrop" onClick={busy ? undefined : onClose}>
      <div
        className="modal catalog-switcher-modal"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="modal-header">
          <div className="modal-title">Open catalog</div>
          <button className="chip" onClick={onClose} disabled={busy}>
            Close
          </button>
        </div>

        <div className="modal-body">
          {/* Recent catalogs */}
          <div className="catalog-section-label">Recent catalogs</div>
          {loading ? (
            <div className="modal-sub catalog-loading">Loading…</div>
          ) : recents.length === 0 ? (
            <div className="modal-sub catalog-loading">No recent catalogs.</div>
          ) : (
            <div className="catalog-recent-list">
              {recents.map((rc) => (
                <button
                  key={rc.catalogPath}
                  className="catalog-recent-row"
                  onClick={() => openRecent(rc)}
                  disabled={busy}
                  title={rc.catalogPath}
                >
                  <div className="catalog-recent-info">
                    <span className="catalog-recent-name">{rc.name}</span>
                    <span className="catalog-recent-path">{rc.catalogPath}</span>
                  </div>
                  <span className="catalog-recent-date">{formatDate(rc.lastOpened)}</span>
                </button>
              ))}
            </div>
          )}

          {/* Divider */}
          <div className="catalog-divider" />

          {/* New catalog section */}
          <div className="catalog-section-label">
            <span>New catalog</span>
            <button
              className="chip"
              onClick={() => setShowNew((v) => !v)}
              disabled={busy}
            >
              {showNew ? "Cancel" : "Create new…"}
            </button>
          </div>

          {showNew && (
            <div className="catalog-new-form">
              <div className="field">
                <label>Catalog name</label>
                <input
                  className="folder-input"
                  placeholder="My Catalog"
                  value={newName}
                  onChange={(e) => setNewName(e.target.value)}
                  autoFocus
                />
              </div>
              <div className="field">
                <label>Folder (photos root and catalog file location)</label>
                <div className="input-with-browse">
                  <input
                    className="folder-input"
                    placeholder="~/Pictures/My Catalog"
                    value={newFolder}
                    onChange={(e) => setNewFolder(e.target.value)}
                  />
                  <button className="chip" onClick={browseNewFolder} disabled={busy}>
                    Browse…
                  </button>
                </div>
              </div>
              <div className="field">
                <span className="term-note">
                  Creates <code>{newName || "CatalogName"}.chairphoto</code> inside the chosen
                  folder. Photos you import will be placed under that folder.
                </span>
              </div>
              <div className="row">
                <button
                  className="scan-btn"
                  onClick={createNew}
                  disabled={busy || !newName.trim() || !newFolder.trim()}
                >
                  {busy ? "Creating…" : "Create catalog"}
                </button>
              </div>
            </div>
          )}

          {error && <div className="modal-error" style={{ marginTop: 8 }}>{error}</div>}
        </div>
      </div>
    </div>
  );
}
