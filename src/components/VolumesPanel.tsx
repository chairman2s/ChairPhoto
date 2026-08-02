import { useCallback, useEffect, useState } from "react";
import { Volume, VolumeKind, addVolume, listVolumes, removeVolume } from "../modules/api";

// Storage volumes section (Preferences → Storage). Lists registered volumes with
// reachability, lets the user add a volume (name + base path) and remove a non-default
// one. The default "catalog-root" volume is protected by the backend.
export function VolumesSection() {
  const [volumes, setVolumes] = useState<Volume[]>([]);
  const [name, setName] = useState("");
  const [path, setPath] = useState("");
  const [kind, setKind] = useState<VolumeKind>("backup");
  const [error, setError] = useState("");

  const reload = useCallback(() => {
    listVolumes().then(setVolumes).catch((e) => setError(String(e)));
  }, []);
  useEffect(() => {
    reload();
  }, [reload]);

  const onAdd = async () => {
    setError("");
    if (!name.trim() || !path.trim()) {
      setError("Name and path are required.");
      return;
    }
    try {
      await addVolume(name.trim(), path.trim(), kind);
      setName("");
      setPath("");
      reload();
    } catch (e) {
      setError(String(e));
    }
  };

  const onRemove = async (v: Volume) => {
    setError("");
    if (
      !window.confirm(
        `Remove volume "${v.name}"?\n\nFiles on disk are NOT deleted, but the catalog ` +
          `forgets where copies on this volume live. Any photo whose only copy is here ` +
          `will be unreachable until the volume is re-added.`,
      )
    )
      return;
    try {
      await removeVolume(v.id);
      reload();
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <div className="prefs-section">
      <h3>Volumes</h3>
      {volumes.map((v) => (
            <div key={v.id} className="module-row">
              <div className="module-info">
                <div className="module-name">
                  {v.name}{" "}
                  <span className="vol-kind">{v.kind === "backup" ? "NAS / backup" : "local"}</span>{" "}
                  <span className={`vol-badge ${v.reachable ? "vol-ok" : "vol-off"}`}>
                    {v.reachable ? "reachable" : "offline"}
                  </span>
                </div>
                <div className="modal-sub">{v.basePath}</div>
              </div>
              {v.name === "catalog-root" ? (
                <span className="term-note" title="This is your library folder — change it in Library above">
                  library folder
                </span>
              ) : (
                <button
                  className="chip chip-danger"
                  onClick={() => onRemove(v)}
                  title="Remove this volume"
                >
                  Remove
                </button>
              )}
            </div>
          ))}

          <section className="editor-section">
            <div className="modal-sub">Add a volume (e.g. a NAS mount). “~” expands to your home.</div>
            <div className="vol-add">
              <input
                className="folder-input"
                placeholder="Name (e.g. NAS)"
                value={name}
                onChange={(e) => setName(e.target.value)}
              />
              <input
                className="folder-input"
                placeholder="Base path (e.g. ~/ZimaCube/Gallery)"
                value={path}
                onChange={(e) => setPath(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && onAdd()}
              />
              <select
                className="vol-kind-select"
                value={kind}
                onChange={(e) => setKind(e.target.value as VolumeKind)}
                title="Local working disk, or a backup (NAS/remote) target"
              >
                <option value="backup">NAS / backup</option>
                <option value="local">local</option>
              </select>
              <button className="scan-btn" onClick={onAdd}>
                Add
              </button>
            </div>
            {error && <div className="modal-error">{error}</div>}
          </section>
    </div>
  );
}
