import { useEffect, useState } from "react";
import { GlSpike } from "./darkroom/GlSpike";
import { RENDER_TIMING_KEY, RENDER_TIMING_SUMMARY_KEY } from "./darkroom/renderTiming";
import { confirm } from "@tauri-apps/plugin-dialog";
import {
  applyOffloadPolicy,
  AvailableEditor,
  availableEditors,
  findEmptyPhotos,
  findUnavailablePhotos,
  getLibraryRoot,
  purgeEmptyPhotos,
  developCacheClear,
  developCacheUsage,
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
  listTags,
  deleteTag,
  findSimilarTags,
  findOrphanTags,
  getSystemTheme,
  onThemeChanged,
  type OrphanTag,
  type SimilarTagPair,
  type TagWithCount,
  vacuumCatalog,
} from "../modules/api";
import { TagMergeModal } from "./TagMergeModal";
import {
  listModules,
  settingsPanelsForModule,
  useHostLifecycle,
  useHostSettingsPanels,
} from "../modules/host";
import { ModuleSettings } from "../modules/ModuleContent";
import { VolumesSection } from "./VolumesPanel";
import { SafetySection } from "./SafetyPanel";
import { ModulesSection } from "./ModulesPanel";
import { getAppearanceMode, setAppearanceMode } from "../theme/controller";
import { useOwnedSubscription } from "../modules/ownedEvents";
import type { AppearanceMode, SystemThemeResult } from "../theme/tokens";

// One preferences dialog. Fixed tabs: Storage (library root + volumes) and Modules
// (enable/disable). Then one tab per enabled module that contributes settings (AI, Flickr,
// SmugMug, …) — that's where a module's API keys / options live.
export function Preferences({
  onClose,
  onLibraryRootChanged,
  onShowStorageTier,
}: {
  onClose: () => void;
  /** Called after the library root is re-rooted, so the app can prompt a rescan. */
  onLibraryRootChanged: () => void;
  /** Filter the grid to a safety bucket. Closing is the caller's business. */
  onShowStorageTier?: (tier: import("../modules/api").StorageTier) => void;
}) {
  // Re-render when modules enable/disable (moduleTabs filters by m.enabled) or contribute
  // settings panels (settingsPanelsForModule) — not on selection or other contribution types.
  useHostLifecycle();
  useHostSettingsPanels();
  const moduleTabs = listModules().filter(
    (m) => m.enabled && settingsPanelsForModule(m.id).length > 0,
  );
  const [tab, setTab] = useState<string>("storage");
  const validIds = [
    "storage",
    "tags",
    "editors",
    "modules",
    "appearance",
    ...moduleTabs.map((m) => m.id),
  ];
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
              className={`prefs-tab ${active === "tags" ? "prefs-tab-on" : ""}`}
              onClick={() => setTab("tags")}
            >
              Tags
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
            <button
              className={`prefs-tab ${active === "appearance" ? "prefs-tab-on" : ""}`}
              onClick={() => setTab("appearance")}
            >
              Appearance
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
                <SafetySection onShowTier={onShowStorageTier} />
                <TieringSection onChanged={onLibraryRootChanged} />
                <MaintenanceSection onChanged={onLibraryRootChanged} />
              </>
            )}
            {active === "tags" && <TagMaintenanceSection onChanged={onLibraryRootChanged} />}
            {active === "editors" && (
              <>
                <EditorsSection />
                <DarkroomSection />
              </>
            )}
            {active === "modules" && <ModulesSection />}
            {active === "appearance" && <AppearanceSection />}
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
    </div>
  );
}

// Tag maintenance (A5): the operations that used to be hand-written SQL. Tidying, finding
// duplicate and unused tags, and merging from the results — a merge always goes through the
// same preview the tag tree's "Merge into…" uses, so there is one merge flow, not two.
function TagMaintenanceSection({ onChanged }: { onChanged: () => void }) {
  const [status, setStatus] = useState("");
  const [tags, setTags] = useState<TagWithCount[]>([]);
  const [duplicates, setDuplicates] = useState<SimilarTagPair[] | null>(null);
  const [orphans, setOrphans] = useState<OrphanTag[] | null>(null);
  const [busy, setBusy] = useState("");
  const [merging, setMerging] = useState<TagWithCount | null>(null);

  const reload = async () => {
    setTags(await listTags().catch(() => []));
  };
  useEffect(() => {
    void reload();
  }, []);

  const tagById = (id: number) => tags.find((t) => t.id === id) ?? null;

  const findDuplicates = async () => {
    setBusy("duplicates");
    setStatus("");
    try {
      setDuplicates(await findSimilarTags());
    } catch (e) {
      setStatus(String(e));
    } finally {
      setBusy("");
    }
  };

  const findUnused = async () => {
    setBusy("orphans");
    setStatus("");
    try {
      setOrphans(await findOrphanTags());
    } catch (e) {
      setStatus(String(e));
    } finally {
      setBusy("");
    }
  };

  const removeOrphan = async (orphan: OrphanTag) => {
    // Deleting a branch cascades to its children, which is not obvious from a list of paths.
    if (orphan.hasChildren) {
      const ok = await confirm(
        `Delete ${orphan.path} and every tag under it?\n\nNone of them hold photos, but the ` +
          `sub-tags are removed too.`,
      );
      if (!ok) return;
    }
    try {
      await deleteTag(orphan.id);
      setOrphans((prev) => prev?.filter((o) => o.id !== orphan.id) ?? null);
      setStatus(`Deleted ${orphan.path}.`);
      await reload();
      onChanged();
    } catch (e) {
      setStatus(String(e));
    }
  };

  return (
    <div className="prefs-section">
      <h3>Tidy tags</h3>
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

      <h3 style={{ marginTop: 18 }}>Duplicate tags</h3>
      <div className="modal-sub">
        Tags whose names look alike, most similar first. Found by name — two tags meaning the
        same thing under unlike names (<code>Bike</code> and <code>Velocipede</code>) will not
        appear here. Merging is your call: “Cycling” and “Cycles” may be a typo, or two real
        things.
      </div>
      <div className="row">
        <button className="chip" disabled={busy !== ""} onClick={() => void findDuplicates()}>
          {busy === "duplicates" ? "Looking…" : "Find duplicate tags"}
        </button>
      </div>
      {duplicates?.length === 0 && (
        <div className="modal-sub">No similar tag names found.</div>
      )}
      {duplicates && duplicates.length > 0 && (
        <div className="tag-maint-list">
          {duplicates.slice(0, 50).map((pair) => (
            <div key={`${pair.aId}-${pair.bId}`} className="tag-maint-row">
              <div className="tag-maint-pair">
                <span>{pair.aPath}</span>
                <span className="tag-count">{pair.aPhotos.toLocaleString()}</span>
                <span className="tag-maint-vs">vs</span>
                <span>{pair.bPath}</span>
                <span className="tag-count">{pair.bPhotos.toLocaleString()}</span>
              </div>
              <div className="modal-sub">{pair.reason}</div>
              <div className="row" style={{ gap: 6 }}>
                <button
                  className="chip"
                  disabled={!tagById(pair.aId)}
                  onClick={() => setMerging(tagById(pair.aId))}
                >
                  Merge {pair.aPath} away…
                </button>
                <button
                  className="chip"
                  disabled={!tagById(pair.bId)}
                  onClick={() => setMerging(tagById(pair.bId))}
                >
                  Merge {pair.bPath} away…
                </button>
              </div>
            </div>
          ))}
          {duplicates.length > 50 && (
            <div className="modal-sub">
              Showing the 50 most similar of {duplicates.length}.
            </div>
          )}
        </div>
      )}

      <h3 style={{ marginTop: 18 }}>Unused tags</h3>
      <div className="modal-sub">
        Tags holding no photos anywhere beneath them. An empty branch is marked as such —
        deleting it removes the tags under it too.
      </div>
      <div className="row">
        <button className="chip" disabled={busy !== ""} onClick={() => void findUnused()}>
          {busy === "orphans" ? "Looking…" : "Find unused tags"}
        </button>
      </div>
      {orphans?.length === 0 && <div className="modal-sub">Every tag is in use.</div>}
      {orphans && orphans.length > 0 && (
        <div className="tag-maint-list">
          {orphans.map((orphan) => (
            <div key={orphan.id} className="tag-maint-row">
              <div className="tag-maint-pair">
                <span>{orphan.path}</span>
                {orphan.hasChildren && <span className="tag-maint-vs">branch</span>}
                {orphan.isAutoTag && (
                  <span className="tag-maint-vs" title="Recreated by the auto-tag engine">
                    auto-tag
                  </span>
                )}
              </div>
              <div className="row" style={{ gap: 6 }}>
                <button className="chip" onClick={() => void removeOrphan(orphan)}>
                  Delete
                </button>
              </div>
            </div>
          ))}
        </div>
      )}

      {status && <div className="modal-sub">{status}</div>}

      {merging && (
        <TagMergeModal
          source={merging}
          tags={tags}
          onClose={() => setMerging(null)}
          onMerged={(report) => {
            setMerging(null);
            setStatus(
              `Merged ${report.sources.map((x) => x.path).join(", ")} into ${report.targetPath} — ` +
                `${report.photosRetagged.toLocaleString()} photo(s) moved.`,
            );
            setDuplicates(null);
            setOrphans(null);
            void reload();
            onChanged();
          }}
        />
      )}
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
/** The probe's last report, persisted so it can be read without the inspector. */
const GL_SPIKE_REPORT_KEY = "editor.glSpike.lastReport";
/** `"1"` renders Develop from the RAW working image (docs/plans/raw-foundation); read by the
 *  backend's `develop_open`. Off by default until slice 8. */
const RAW_ENGINE_KEY = "develop.rawEngine";
/** The `.rawf` decode cache's size limit in GB (backend default 20) and neighbour preload
 *  (default on) — docs/plans/raw-foundation, slice 4. Read by `develop_open`. */
const DECODE_CACHE_GB_KEY = "develop.decodeCacheGb";
const PRELOAD_KEY = "develop.preloadNeighbours";
const DEFAULT_DECODE_CACHE_GB = 20;

/** Bytes as "3.4 GB" / "512 MB". */
export function formatCacheBytes(bytes: number): string {
  const gb = bytes / 1024 ** 3;
  if (gb >= 1) return `${gb.toFixed(1)} GB`;
  return `${Math.round(bytes / 1024 ** 2)} MB`;
}

/** The decode-cache size a settings value means: a non-negative number of GB, else the
 *  default. */
export function parseCacheGb(v: string | null | undefined): number {
  const n = v == null || v.trim() === "" ? NaN : Number(v);
  return Number.isFinite(n) && n >= 0 ? n : DEFAULT_DECODE_CACHE_GB;
}

/** RAW engine settings: the decode cache's size, its current use, and neighbour preload. */
function RawCacheSettings() {
  const [gb, setGb] = useState<string>("");
  const [preload, setPreload] = useState<boolean | null>(null);
  const [usage, setUsage] = useState<number | null>(null);
  const [busy, setBusy] = useState(false);
  const refreshUsage = () => {
    developCacheUsage()
      .then(setUsage)
      .catch(() => setUsage(null));
  };
  useEffect(() => {
    getSetting(DECODE_CACHE_GB_KEY)
      .then((v) => setGb(String(parseCacheGb(v))))
      .catch(() => setGb(String(DEFAULT_DECODE_CACHE_GB)));
    getSetting(PRELOAD_KEY)
      .then((v) => setPreload(v !== "0"))
      .catch(() => setPreload(true));
    refreshUsage();
  }, []);
  const saveGb = async () => {
    const n = parseCacheGb(gb);
    setGb(String(n));
    await setSetting(DECODE_CACHE_GB_KEY, String(n)).catch(() => {});
  };
  return (
    <div style={{ marginLeft: 24, marginTop: 4 }}>
      <label style={{ display: "flex", gap: 8, alignItems: "center" }}>
        RAW decode cache
        <input
          type="number"
          min={0}
          step={1}
          value={gb}
          style={{ width: 70 }}
          onChange={(e) => setGb(e.target.value)}
          onBlur={saveGb}
          onKeyDown={(e) => {
            if (e.key === "Enter") void saveGb();
          }}
          aria-label="RAW decode cache size in GB"
        />
        GB
        <span className="modal-sub">
          {usage == null ? "" : `· ${formatCacheBytes(usage)} used`}
        </span>
        <button
          disabled={busy || !usage}
          onClick={async () => {
            setBusy(true);
            await developCacheClear().catch(() => 0);
            setBusy(false);
            refreshUsage();
          }}
          title="Delete every cached RAW decode. The next first open of each photo decodes it again."
        >
          Clear
        </button>
      </label>
      <div className="modal-sub" style={{ marginTop: 2 }}>
        Each RAW decoded in the Darkroom is kept on disk so opening it again is near-instant
        (about 200–400 MB per photo). Oldest entries go first when the limit is reached; 0
        keeps nothing.
      </div>
      <label style={{ display: "flex", gap: 8, alignItems: "center", cursor: "pointer", marginTop: 6 }}>
        <input
          type="checkbox"
          checked={preload ?? true}
          disabled={preload === null}
          onChange={async () => {
            const next = !(preload ?? true);
            setPreload(next);
            try {
              await setSetting(PRELOAD_KEY, next ? "1" : "0");
            } catch {
              setPreload(!next);
            }
          }}
        />
        Prepare the next and previous photo in the background
      </label>
    </div>
  );
}

/** Darkroom early-preview toggle (docs/plans/darkroom): swaps the Develop surface for
 *  the in-progress Darkroom. Read when Develop opens (DevelopSurface), so a change
 *  applies on the next open. Removed at slice 8, when the Darkroom becomes Develop. */
function DarkroomSection() {
  const [on, setOn] = useState<boolean | null>(null);
  // Render-timing log (GPU-smoothness work, docs/plans/darkroom/00-status.md): stamps
  // every stage render in the console and unlocks the WebGL probe below. Dev-only.
  const [timing, setTiming] = useState<boolean | null>(null);
  const [rawEngine, setRawEngine] = useState<boolean | null>(null);
  const [spike, setSpike] = useState(false);
  const [lastSpike, setLastSpike] = useState("");
  const [lastSummary, setLastSummary] = useState("");
  useEffect(() => {
    getSetting("editor.darkroom")
      .then((v) => setOn(v === "1"))
      .catch(() => setOn(false));
    getSetting(RAW_ENGINE_KEY)
      .then((v) => setRawEngine(v === "1"))
      .catch(() => setRawEngine(false));
    getSetting(RENDER_TIMING_KEY)
      .then((v) => setTiming(v === "1"))
      .catch(() => setTiming(false));
    getSetting(GL_SPIKE_REPORT_KEY)
      .then((v) => setLastSpike(v ?? ""))
      .catch(() => {});
    getSetting(RENDER_TIMING_SUMMARY_KEY)
      .then((v) => setLastSummary(v ?? ""))
      .catch(() => {});
  }, []);
  const toggle = async () => {
    const next = !(on ?? false);
    setOn(next);
    try {
      await setSetting("editor.darkroom", next ? "1" : "0");
    } catch {
      setOn(!next); // write failed — reflect reality
    }
  };
  const toggleTiming = async () => {
    const next = !(timing ?? false);
    setTiming(next);
    try {
      await setSetting(RENDER_TIMING_KEY, next ? "1" : "0");
    } catch {
      setTiming(!next);
    }
  };
  return (
    <div className="prefs-section">
      <h3 style={{ marginTop: 18 }}>Darkroom (early preview)</h3>
      <div className="modal-sub">
        Replace the Develop view with the in-progress Darkroom (develop by choosing:
        proof sheets, duels, and the tone strip). Under construction — expect a bare
        surface for now. Takes effect the next time Develop opens.
      </div>
      <label style={{ display: "flex", gap: 8, alignItems: "center", cursor: "pointer" }}>
        <input
          type="checkbox"
          checked={on ?? false}
          disabled={on === null}
          onChange={toggle}
        />
        Use the Darkroom as the Develop surface
      </label>
      <label
        style={{ display: "flex", gap: 8, alignItems: "center", cursor: "pointer", marginTop: 6 }}
        title="Develop decodes the RAW itself — all 14 bits, in linear light — and every slider renders from that instead of the camera's preview JPEG. Takes effect the next time a photo is opened in the Darkroom."
      >
        <input
          type="checkbox"
          checked={rawEngine ?? false}
          disabled={rawEngine === null}
          onChange={async () => {
            const next = !(rawEngine ?? false);
            setRawEngine(next);
            try {
              await setSetting(RAW_ENGINE_KEY, next ? "1" : "0");
            } catch {
              setRawEngine(!next);
            }
          }}
        />
        Render from the RAW (engine 2, in progress)
      </label>
      {rawEngine && <RawCacheSettings />}
      <label
        style={{ display: "flex", gap: 8, alignItems: "center", cursor: "pointer", marginTop: 6 }}
      >
        <input
          type="checkbox"
          checked={timing ?? false}
          disabled={timing === null}
          onChange={toggleTiming}
        />
        Log render timings to the console (dev)
      </label>
      {timing && (
        <div style={{ marginTop: 8 }}>
          <button onClick={() => setSpike(true)}>Run WebGL probe</button>
          {lastSpike && (
            <div className="modal-sub" style={{ marginTop: 4, wordBreak: "break-all" }}>
              Last probe: {lastSpike}
            </div>
          )}
          {lastSummary && (
            <div className="modal-sub" style={{ marginTop: 4, wordBreak: "break-all" }}>
              Last drag summary: {lastSummary}
            </div>
          )}
        </div>
      )}
      {spike && (
        <GlSpike
          onClose={() => setSpike(false)}
          onReport={(json) => {
            setLastSpike(json);
            setSetting(GL_SPIKE_REPORT_KEY, json).catch(() => {});
          }}
        />
      )}
    </div>
  );
}

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

// Which palette source drives the app's theme. "Follow Omarchy" is the product default:
// the palette is derived live from the user's Omarchy/system theme (theme/controller.ts)
// and tracks theme switches while this dialog is open. This is a per-machine preference
// (localStorage via theme/prefs.ts), not a catalog setting — it does not travel with the
// catalog between computers.
/** The Appearance tab's status line for a detection result — shared by the on-mount/
 *  mode-change read and the live `appearance:theme_changed` subscription below, so both
 *  paths render the identical wording. */
function appearanceStatusLine(r: SystemThemeResult): string {
  if (r.available) {
    return `Following Omarchy · ${r.themeName ?? "unnamed theme"} · ${r.palette?.mode ?? ""}`;
  }
  return (
    "Omarchy not detected — ChairPhoto Standard is in use. This is normal without Omarchy; " +
    "nothing is missing."
  );
}

function AppearanceSection() {
  const [mode, setMode] = useState<AppearanceMode>(() => getAppearanceMode());
  const [status, setStatus] = useState<string | null>(null);

  const choose = (next: AppearanceMode) => {
    setMode(next);
    setAppearanceMode(next);
  };

  // Read the live status on mount, and again whenever the user switches back to Follow —
  // mount already covers "already following" (the effect below fires on mount too), so
  // this one condition is both cases the spec calls out. Standard mode never shows the
  // line, so there is nothing to read for it.
  useEffect(() => {
    if (mode !== "follow-omarchy") return;
    let cancelled = false;
    getSystemTheme()
      .then((r) => {
        if (!cancelled) setStatus(appearanceStatusLine(r));
      })
      .catch(() => {
        if (!cancelled) setStatus(null);
      });
    return () => {
      cancelled = true;
    };
  }, [mode]);

  // Keep the line live while the dialog is open: a desktop theme switch updates it without
  // waiting for the tab to be revisited. Subscribed unconditionally (not gated on `mode`) —
  // it only ever drives `status`, which is rendered solely under the follow-omarchy branch
  // below, so an event arriving while on Standard is harmless.
  useOwnedSubscription(
    () => onThemeChanged((r) => setStatus(appearanceStatusLine(r))),
    [],
  );

  return (
    <div className="prefs-section">
      <h3>Appearance</h3>
      <div className="modal-sub">
        Choose the palette ChairPhoto renders with. This is a per-machine preference — it
        isn't saved to the catalog and doesn't travel with it between computers.
      </div>
      <div className="seg">
        <button
          className={`seg-item ${mode === "follow-omarchy" ? "on" : ""}`}
          onClick={() => choose("follow-omarchy")}
        >
          Follow Omarchy
        </button>
        <button
          className={`seg-item ${mode === "standard" ? "on" : ""}`}
          onClick={() => choose("standard")}
        >
          ChairPhoto Standard
        </button>
      </div>
      {mode === "follow-omarchy" && status && <div className="modal-sub">{status}</div>}
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
      <div className="modal-sub">
        This also drops retired storage the catalog no longer uses — currently a per-metadata
        column that was written on every scan and never read. On a large library that column
        is worth a few hundred MB, and removing it rewrites the metadata table, which is why
        it happens here rather than silently at startup.
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
      {panels.map((panel, i) => (
        <section key={i} className="editor-section">
          <ModuleSettings panel={panel} />
        </section>
      ))}
    </div>
  );
}
