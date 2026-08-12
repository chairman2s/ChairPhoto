import { useEffect, useState } from "react";
import { confirm } from "@tauri-apps/plugin-dialog";
import {
  applyOffloadPolicy,
  AvailableEditor,
  availableEditors,
  findEmptyPhotos,
  findUnavailablePhotos,
  getLibraryRoot,
  purgeEmptyPhotos,
  getSetting,
  listVolumes,
  OFFLOAD_AGE_SETTING,
  pickFolder,
  purgeUnavailablePhotos,
  rapidrawAvailable,
  scanNasFolder,
  setLibraryRoot,
  setSetting,
  tidyRedundantTags,
  vacuumCatalog,
} from "../modules/api";
import {
  listModules,
  settingsPanelsForModule,
  useHostLifecycle,
  useHostSettingsPanels,
} from "../modules/host";
import { VolumesSection } from "./VolumesPanel";
import { ModulesSection } from "./ModulesPanel";

// One preferences dialog. Fixed tabs: Storage (library root + volumes) and Modules
// (enable/disable). Then one tab per enabled module that contributes settings (AI, Flickr,
// SmugMug, …) — that's where a module's API keys / options live.
export function Preferences({
  onClose,
  onLibraryRootChanged,
}: {
  onClose: () => void;
  /** Called after the library root is re-rooted, so the app can prompt a rescan. */
  onLibraryRootChanged: () => void;
}) {
  // Re-render when modules enable/disable (moduleTabs filters by m.enabled) or contribute
  // settings panels (settingsPanelsForModule) — not on selection or other contribution types.
  useHostLifecycle();
  useHostSettingsPanels();
  const moduleTabs = listModules().filter(
    (m) => m.enabled && settingsPanelsForModule(m.id).length > 0,
  );
  const [tab, setTab] = useState<string>("storage");
  const validIds = ["storage", "editors", "modules", ...moduleTabs.map((m) => m.id)];
  const active = validIds.includes(tab) ? tab : "storage";

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal prefs" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <div className="modal-title">Preferences</div>
          <button className="chip" onClick={onClose}>
            Close
          </button>
        </div>
        <div className="prefs-body">
          <nav className="prefs-tabs">
            <button
              className={`prefs-tab ${active === "storage" ? "prefs-tab-on" : ""}`}
              onClick={() => setTab("storage")}
            >
              Storage
            </button>
            <button
              className={`prefs-tab ${active === "editors" ? "prefs-tab-on" : ""}`}
              onClick={() => setTab("editors")}
            >
              Editors
            </button>
            <button
              className={`prefs-tab ${active === "modules" ? "prefs-tab-on" : ""}`}
              onClick={() => setTab("modules")}
            >
              Modules
            </button>
            {moduleTabs.map((m) => (
              <button
                key={m.id}
                className={`prefs-tab ${active === m.id ? "prefs-tab-on" : ""}`}
                onClick={() => setTab(m.id)}
              >
                {m.name}
              </button>
            ))}
          </nav>
          <div className="prefs-content">
            {active === "storage" && (
              <>
                <LibrarySection onChanged={onLibraryRootChanged} />
                <VolumesSection />
                <TieringSection onChanged={onLibraryRootChanged} />
                <MaintenanceSection onChanged={onLibraryRootChanged} />
              </>
            )}
            {active === "editors" && <EditorsSection />}
            {active === "modules" && <ModulesSection />}
            {moduleTabs.some((m) => m.id === active) && <ModuleSettingsTab moduleId={active} />}
          </div>
        </div>
      </div>
    </div>
  );
}

// Library root (= catalog root = local volume base).
function LibrarySection({ onChanged }: { onChanged: () => void }) {
  const [root, setRoot] = useState("");
  const [status, setStatus] = useState("");

  useEffect(() => {
    getLibraryRoot().then(setRoot).catch(() => {});
  }, []);

  const apply = async () => {
    setStatus("");
    if (!root.trim()) return;
    try {
      await setLibraryRoot(root.trim());
      setStatus("Library folder set — re-scan to index it.");
      onChanged();
    } catch (e) {
      setStatus(String(e));
    }
  };

  return (
    <div className="prefs-section">
      <h3>Library</h3>
      <div className="modal-sub">
        Your photo library folder. Photos are stored relative to it (it's also the local
        volume). Changing it re-roots the catalog — re-scan afterward.
      </div>
      <div className="row">
        <input
          className="folder-input"
          style={{ flex: 1 }}
          value={root}
          onChange={(e) => setRoot(e.target.value)}
          placeholder="~/Pictures/Raw"
        />
        <button className="scan-btn" onClick={apply}>
          Set
        </button>
      </div>
      {status && <div className="modal-sub">{status}</div>}

      <h3 style={{ marginTop: 18 }}>Tidy tags</h3>
      <div className="modal-sub">
        Remove redundant ancestor tags across your library — when a photo has a more
        specific tag (e.g. <code>Harbor/Marina</code>), the parent it implies
        (<code>Harbor</code>) is dropped. New tagging keeps tags to leaves automatically.
      </div>
      <div className="row">
        <button
          className="chip"
          onClick={async () => {
            setStatus("Tidying…");
            try {
              const n = await tidyRedundantTags();
              setStatus(n > 0 ? `Removed ${n} redundant tag(s).` : "Already tidy — nothing to remove.");
              onChanged();
            } catch (e) {
              setStatus(String(e));
            }
          }}
        >
          Tidy redundant tags
        </button>
      </div>
    </div>
  );
}

// Storage tiering: keep recent photos on local disk; older photos (with a verified NAS
// backup) are offloaded to free local space but stay visible in the library (the grid
// keeps a local thumbnail and serves the original from the NAS when it's mounted).
function TieringSection({ onChanged }: { onChanged: () => void }) {
  const [days, setDays] = useState("");
  const [nasFolder, setNasFolder] = useState("");
  const [status, setStatus] = useState("");
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    getSetting(OFFLOAD_AGE_SETTING)
      .then((v) => setDays(v && v !== "0" ? v : ""))
      .catch(() => {});
  }, []);

  const save = async () => {
    const n = days.trim() === "" ? 0 : Math.max(0, parseInt(days.trim(), 10) || 0);
    try {
      await setSetting(OFFLOAD_AGE_SETTING, String(n));
      setStatus(
        n > 0
          ? `Saved — photos older than ${n} day(s) will be offloaded to the NAS.`
          : "Saved — automatic offload is off (photos stay on local disk).",
      );
    } catch (e) {
      setStatus(String(e));
    }
  };

  const applyNow = async () => {
    setBusy(true);
    setStatus("Offloading older photos to the NAS…");
    try {
      const n = await applyOffloadPolicy();
      setStatus(
        n > 0
          ? `Offloaded ${n} photo(s) to the NAS (still visible in the library).`
          : "Nothing to offload (none old enough, or the NAS is unreachable).",
      );
      onChanged();
    } catch (e) {
      setStatus(String(e));
    } finally {
      setBusy(false);
    }
  };

  const browseNas = async () => {
    setStatus("");
    try {
      // Default the picker to the NAS (backup) volume's base path, if one is set up.
      const nasBase = await listVolumes()
        .then((vols) => vols.find((v) => v.kind === "backup")?.basePath)
        .catch(() => undefined);
      const picked = await pickFolder(nasBase || nasFolder);
      if (picked) {
        setNasFolder(picked);
      }
    } catch (e) {
      setStatus(`Couldn't open the folder picker: ${String(e)}`);
    }
  };

  const scanNas = async (folder?: string) => {
    const path = (folder ?? nasFolder).trim();
    if (!path) {
      setStatus("Choose the NAS folder to index.");
      return;
    }
    setBusy(true);
    setStatus("Indexing NAS photos… (this can take a while for a large archive)");
    try {
      const r = await scanNasFolder(path);
      setStatus(
        `Indexed ${r.created} new photo(s) from the NAS` +
          (r.errors ? `, ${r.errors} errors` : "") +
          '. Find them under the "On NAS" filter.',
      );
      onChanged();
    } catch (e) {
      setStatus(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="prefs-section">
      <h3>Local / NAS tiering</h3>
      <div className="modal-sub">
        Keep recent photos on local disk and offload older ones (that already have a
        verified NAS backup) to free space. Offloaded photos stay in the library — the grid
        shows a kept thumbnail, and the full photo loads from the NAS when it's connected.
        Leave blank to keep everything on local disk.
      </div>
      <div className="row">
        <span className="modal-sub">Keep photos newer than</span>
        <input
          className="folder-input"
          style={{ width: 80 }}
          value={days}
          onChange={(e) => setDays(e.target.value.replace(/[^0-9]/g, ""))}
          placeholder="90"
          inputMode="numeric"
        />
        <span className="modal-sub">days on local disk</span>
        <button className="scan-btn" onClick={save}>
          Save
        </button>
        <button className="chip" disabled={busy} onClick={applyNow}>
          Offload older now
        </button>
      </div>

      <h3 style={{ marginTop: 18 }}>Index existing NAS photos</h3>
      <div className="modal-sub">
        One-time: bring an archive that already lives on the NAS into the catalog
        <em> in place</em> — nothing is copied to local disk. Those photos appear under the
        "On NAS" tier and are viewable while the NAS is connected.
      </div>
      <div className="row">
        <input
          className="folder-input"
          style={{ flex: 1 }}
          placeholder="/mnt/nas/Photos or similar"
          value={nasFolder}
          onChange={(e) => setNasFolder(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && scanNas()}
        />
        <button className="chip" disabled={busy} onClick={browseNas}>
          Browse...
        </button>
        <button className="scan-btn" disabled={busy} onClick={() => scanNas()}>
          Index
        </button>
      </div>
      {status && <div className="modal-sub">{status}</div>}
    </div>
  );
}

// Catalog maintenance: remove entries whose original is gone from BOTH the library and
// the (reachable) backup. Deletes catalog rows only — never any file. Photos that might
// still live on an offline volume (unmounted NAS) are never touched.
// External develop editors (darktable / RawTherapee / ART). Paths are optional — blank uses
// the auto-detected command on PATH. Availability (GUI/CLI found) is shown per editor.
function EditorsSection() {
  const [editors, setEditors] = useState<AvailableEditor[]>([]);
  const [paths, setPaths] = useState<Record<string, { gui: string; cli: string }>>({});
  const [status, setStatus] = useState("");
  // RapidRAW is a request/response editor (its own protocol), not a sidecar+CLI one — so it
  // gets its own row: a single binary-path override plus an output-format choice.
  const [rrAvailable, setRrAvailable] = useState(false);
  const [rrBin, setRrBin] = useState("");
  const [rrFormat, setRrFormat] = useState("tiff");

  const load = () => availableEditors().then(setEditors).catch(() => {});
  const loadRapidraw = () =>
    rapidrawAvailable()
      .then((s) => {
        setRrAvailable(s.available);
        setRrFormat(s.format);
      })
      .catch(() => setRrAvailable(false));
  useEffect(() => {
    load();
    loadRapidraw();
    getSetting("editor.rapidraw.bin").then((v) => setRrBin(v ?? "")).catch(() => {});
  }, []);
  useEffect(() => {
    editors.forEach((e) => {
      Promise.all([getSetting(`editor.${e.key}.gui`), getSetting(`editor.${e.key}.cli`)])
        .then(([g, c]) =>
          setPaths((p) => ({ ...p, [e.key]: { gui: g ?? "", cli: c ?? "" } })),
        )
        .catch(() => {});
    });
  }, [editors.length]);

  const save = async (key: string, which: "gui" | "cli", value: string) => {
    try {
      await setSetting(`editor.${key}.${which}`, value.trim());
      setStatus("Saved.");
      load();
    } catch (e) {
      setStatus(String(e));
    }
  };
  const saveRapidraw = async (key: "bin" | "format", value: string) => {
    try {
      await setSetting(`editor.rapidraw.${key}`, value.trim());
      setStatus("Saved.");
      loadRapidraw();
    } catch (e) {
      setStatus(String(e));
    }
  };

  return (
    <div className="prefs-section">
      <h3>External editors</h3>
      <div className="modal-sub">
        Send a photo to darktable, RawTherapee, or ART to develop it; when the editor closes,
        the result is rendered via its command-line tool and stacked under the original. Leave a
        field blank to use the auto-detected command on your PATH.
      </div>
      {editors.map((e) => (
        <div className="editor-row" key={e.key}>
          <div className="modal-sub">
            <strong>{e.label}</strong> — GUI {e.gui ? "✓" : "✗ not found"} · CLI{" "}
            {e.cli ? "✓ (auto-render)" : "✗ (launch only)"}
          </div>
          <div className="row">
            <input
              className="folder-input"
              style={{ flex: 1 }}
              placeholder={`${e.key} (GUI command / path)`}
              value={paths[e.key]?.gui ?? ""}
              onChange={(ev) =>
                setPaths((p) => ({ ...p, [e.key]: { ...p[e.key], gui: ev.target.value } }))
              }
              onBlur={(ev) => save(e.key, "gui", ev.target.value)}
            />
            <input
              className="folder-input"
              style={{ flex: 1 }}
              placeholder={`${e.key}-cli (CLI command / path)`}
              value={paths[e.key]?.cli ?? ""}
              onChange={(ev) =>
                setPaths((p) => ({ ...p, [e.key]: { ...p[e.key], cli: ev.target.value } }))
              }
              onBlur={(ev) => save(e.key, "cli", ev.target.value)}
            />
          </div>
        </div>
      ))}
      <div className="editor-row">
        <div className="modal-sub">
          <strong>RapidRAW</strong> — {rrAvailable ? "✓ found" : "✗ not found"} · round-trip
          (opens the photo, stacks the exported result on Done)
        </div>
        <div className="row">
          <input
            className="folder-input"
            style={{ flex: 1 }}
            placeholder="RapidRAW (binary command / path)"
            value={rrBin}
            onChange={(ev) => setRrBin(ev.target.value)}
            onBlur={(ev) => saveRapidraw("bin", ev.target.value)}
          />
          <select
            value={rrFormat}
            onChange={(ev) => {
              setRrFormat(ev.target.value);
              saveRapidraw("format", ev.target.value);
            }}
            title="Export format for RapidRAW's result (TIFF/PNG are 16-bit)"
          >
            <option value="tiff">TIFF (16-bit)</option>
            <option value="png">PNG (16-bit)</option>
            <option value="jpg">JPEG</option>
          </select>
        </div>
      </div>
      {status && <div className="modal-sub">{status}</div>}
    </div>
  );
}

function MaintenanceSection({ onChanged }: { onChanged: () => void }) {
  const [status, setStatus] = useState("");
  const [busy, setBusy] = useState(false);

  const removeUnavailable = async () => {
    setBusy(true);
    setStatus("Checking for unavailable photos…");
    try {
      const gone = await findUnavailablePhotos();
      if (gone.length === 0) {
        setStatus("All photos are available — nothing to remove.");
        return;
      }
      const preview = gone
        .slice(0, 12)
        .map((p) => `• ${p.path}`)
        .join("\n");
      const more = gone.length > 12 ? `\n…and ${gone.length - 12} more` : "";
      const ok = await confirm(
        `Remove ${gone.length} photo(s) from the catalog whose file is gone from both ` +
          `your library and the NAS backup?\n\n${preview}${more}\n\n` +
          `This deletes only the catalog entries (tags, ratings, versions). No files on ` +
          `disk or on the NAS are touched. Photos on an offline volume are never removed.`,
        { title: "Remove unavailable photos", kind: "warning" },
      );
      if (!ok) {
        setStatus("Cancelled.");
        return;
      }
      const removed = await purgeUnavailablePhotos();
      setStatus(`Removed ${removed.length} unavailable entr${removed.length === 1 ? "y" : "ies"} (no files deleted).`);
      onChanged();
    } catch (e) {
      setStatus(String(e));
    } finally {
      setBusy(false);
    }
  };

  const removeEmpty = async () => {
    setBusy(true);
    setStatus("Checking for empty (0-byte) photos…");
    try {
      const gone = await findEmptyPhotos();
      if (gone.length === 0) {
        setStatus("No empty (0-byte) photos found.");
        return;
      }
      const preview = gone
        .slice(0, 12)
        .map((p) => `• ${p.path}`)
        .join("\n");
      const more = gone.length > 12 ? `\n…and ${gone.length - 12} more` : "";
      const ok = await confirm(
        `Remove ${gone.length} photo(s) whose only file is empty (0 bytes — the image data ` +
          `is gone)?\n\n${preview}${more}\n\n` +
          `This deletes only the catalog entries; the empty files on disk/NAS are left as-is.`,
        { title: "Remove empty photos", kind: "warning" },
      );
      if (!ok) {
        setStatus("Cancelled.");
        return;
      }
      const removed = await purgeEmptyPhotos();
      setStatus(`Removed ${removed.length} empty entr${removed.length === 1 ? "y" : "ies"} (no files deleted).`);
      onChanged();
    } catch (e) {
      setStatus(String(e));
    } finally {
      setBusy(false);
    }
  };

  const compact = async () => {
    setBusy(true);
    setStatus("Compacting the catalog… this can take a moment.");
    try {
      const r = await vacuumCatalog();
      const mb = (n: number) => `${(n / 1024 / 1024).toFixed(0)} MB`;
      const saved = r.beforeBytes - r.afterBytes;
      setStatus(
        saved > 0
          ? `Compacted: ${mb(r.beforeBytes)} → ${mb(r.afterBytes)} (reclaimed ${mb(saved)}).`
          : `Already compact (${mb(r.afterBytes)}).`,
      );
    } catch (e) {
      setStatus(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="prefs-section">
      <h3>Maintenance</h3>
      <div className="modal-sub">
        Remove catalog entries whose original is gone from <em>both</em> your library and
        the backup, or whose only file is empty (0 bytes). Only the catalog entry is deleted
        — never a file. Photos that may still be on an offline volume are left alone.
      </div>
      <div className="row">
        <button className="chip" disabled={busy} onClick={removeUnavailable}>
          Remove unavailable photos
        </button>
        <button className="chip" disabled={busy} onClick={removeEmpty}>
          Remove empty (0-byte) photos
        </button>
      </div>

      <h3 style={{ marginTop: 18 }}>Compact database</h3>
      <div className="modal-sub">
        Reclaim disk space left behind by deletions and defragment the catalog (SQLite
        VACUUM). Your data is unchanged. Worth running after large removals; it briefly
        needs ~double the catalog size in free space and the window may pause.
      </div>
      <div className="row">
        <button className="chip" disabled={busy} onClick={compact}>
          Compact database now
        </button>
      </div>
      {status && <div className="modal-sub">{status}</div>}
    </div>
  );
}

// A module's own settings (e.g. AI engine/model, or a publisher's API key/secret),
// surfaced as that module's tab. Generic over any module that registered settings panels.
function ModuleSettingsTab({ moduleId }: { moduleId: string }) {
  const panels = settingsPanelsForModule(moduleId);
  if (panels.length === 0) {
    return (
      <div className="prefs-section">
        <div className="panel-empty">This module has no settings.</div>
      </div>
    );
  }
  return (
    <div className="prefs-section">
      {panels.map((render, i) => (
        <section key={i} className="editor-section">
          {render()}
        </section>
      ))}
    </div>
  );
}
