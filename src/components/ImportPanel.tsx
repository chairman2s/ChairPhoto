import { useEffect, useState } from "react";
import {
  CardPhoto,
  cardThumbnail,
  getLibraryRoot,
  getSetting,
  listCardPhotos,
  pickFolder,
  setSetting,
} from "../modules/api";

// Settings key for remembering the last-used card/source folder across sessions.
const LAST_SOURCE = "import.lastSource";

// Import-from-card dialog (E5): pick a card/source folder, preview every photo on it (each
// flagged if already in the library), select which to import (or "Select all new"), then
// hand off to the BACKGROUND import. Destination is always the library root.
export function ImportPanel({
  onClose,
  onImport,
}: {
  onClose: () => void;
  onImport: (source: string, name: string, selected: string[]) => void;
}) {
  const [source, setSource] = useState("");
  const [name, setName] = useState("");
  const [libraryRoot, setLibraryRoot] = useState("");
  const [error, setError] = useState("");
  const [cards, setCards] = useState<CardPhoto[] | null>(null);
  const [scanning, setScanning] = useState(false);
  const [selected, setSelected] = useState<Set<string>>(new Set());

  // Restore the last-used source and show where files land (the library root).
  useEffect(() => {
    let cancelled = false;
    (async () => {
      const [lastSource, root] = await Promise.all([getSetting(LAST_SOURCE), getLibraryRoot()]);
      if (cancelled) return;
      if (lastSource) setSource(lastSource);
      setLibraryRoot(root);
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  // Scan a folder: list its photos and pre-select the new (non-duplicate) ones.
  const scan = async (src?: string) => {
    const s = (src ?? source).trim();
    if (!s) {
      setError("Choose the card / source folder to import from.");
      return;
    }
    setError("");
    setScanning(true);
    setCards(null);
    try {
      const list = await listCardPhotos(s);
      setCards(list);
      setSelected(new Set(list.filter((c) => !c.isDuplicate).map((c) => c.path)));
    } catch (e) {
      setError(String(e));
    } finally {
      setScanning(false);
    }
  };

  const browse = async () => {
    setError("");
    try {
      const picked = await pickFolder(source);
      if (picked) {
        setSource(picked);
        void scan(picked);
      }
    } catch (e) {
      setError(`Couldn't open the folder picker: ${String(e)}`);
    }
  };

  const toggle = (path: string) =>
    setSelected((s) => {
      const n = new Set(s);
      if (n.has(path)) n.delete(path);
      else n.add(path);
      return n;
    });
  const selectAll = () => setSelected(new Set((cards ?? []).map((c) => c.path)));
  const selectNew = () =>
    setSelected(new Set((cards ?? []).filter((c) => !c.isDuplicate).map((c) => c.path)));
  const clear = () => setSelected(new Set());

  const run = () => {
    setError("");
    if (selected.size === 0) {
      setError("Select at least one photo to import.");
      return;
    }
    void setSetting(LAST_SOURCE, source.trim()).catch(() => {});
    onImport(source.trim(), name.trim(), [...selected]);
  };

  const total = cards?.length ?? 0;
  const newCount = cards?.filter((c) => !c.isDuplicate).length ?? 0;

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal import-modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <div className="modal-title">Import from card</div>
          <button className="chip" onClick={onClose}>
            Close
          </button>
        </div>
        <div className="modal-body">
          <div className="field">
            <label>Card / source folder</label>
            <div className="input-with-browse">
              <input
                className="folder-input"
                placeholder="/run/media/you/SDCARD/DCIM"
                value={source}
                onChange={(e) => setSource(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && scan()}
              />
              <button className="chip" onClick={browse}>
                Browse…
              </button>
              <button className="chip" onClick={() => scan()} disabled={scanning}>
                {scanning ? "Scanning…" : "Scan"}
              </button>
            </div>
          </div>

          {cards && (
            <div className="field">
              <div className="import-toolbar">
                <span className="modal-sub">
                  {total} photo{total === 1 ? "" : "s"} · {newCount} new ·{" "}
                  {total - newCount} already imported · <b>{selected.size} selected</b>
                </span>
                <span style={{ flex: 1 }} />
                <button className="chip" onClick={selectNew}>
                  Select all new
                </button>
                <button className="chip" onClick={selectAll}>
                  Select all
                </button>
                <button className="chip" onClick={clear}>
                  Clear
                </button>
              </div>
              {total === 0 ? (
                <div className="panel-empty">No photos found in that folder.</div>
              ) : (
                <div className="card-grid">
                  {cards.map((c) => {
                    const sel = selected.has(c.path);
                    return (
                      <button
                        key={c.path}
                        className={`card-cell ${sel ? "sel" : ""} ${c.isDuplicate ? "dup" : ""}`}
                        onClick={() => toggle(c.path)}
                        title={`${c.name}${c.isDuplicate ? " — already imported" : ""}`}
                      >
                        <CardThumb path={c.path} />
                        {sel && <span className="card-check">✓</span>}
                        {c.isDuplicate && <span className="card-dup">dup</span>}
                        <span className="card-name">{c.name}</span>
                      </button>
                    );
                  })}
                </div>
              )}
            </div>
          )}

          <div className="field">
            <label>Import name (optional)</label>
            <input
              className="folder-input"
              placeholder="e.g. Tønsberg 2026-06 (defaults to the card folder)"
              value={name}
              onChange={(e) => setName(e.target.value)}
            />
          </div>
          <div className="field">
            <span className="term-note">
              Files copy into <code>{libraryRoot || "the library"}</code> under
              Year/Month/Day, then queue for NAS backup. Change the library folder in
              Preferences → Storage.
            </span>
          </div>
          <div className="row">
            <button className="scan-btn" onClick={run} disabled={!cards || selected.size === 0}>
              Import {selected.size > 0 ? selected.size : ""}
            </button>
            <span className="modal-sub">Runs in the background — progress shows up top.</span>
          </div>
          {error && <div className="modal-error">{error}</div>}
        </div>
      </div>
    </div>
  );
}

// One card photo's thumbnail, lazily fetched (capped concurrency in the API layer).
function CardThumb({ path }: { path: string }) {
  const [src, setSrc] = useState("");
  useEffect(() => {
    let alive = true;
    cardThumbnail(path)
      .then((s) => alive && setSrc(s))
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [path]);
  return src ? (
    <img className="card-thumb-img" src={src} alt="" draggable={false} />
  ) : (
    <div className="card-thumb-ph" />
  );
}
