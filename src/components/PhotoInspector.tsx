import { type ReactNode, useEffect, useState, useSyncExternalStore } from "react";
import type { Photo, Publication, Tag } from "../modules/registry";
import {
  AvailableEditor,
  availableEditors,
  backupPhoto,
  cancelRapidraw,
  createTag,
  developInEditor,
  editInRapidraw,
  enqueueOperation,
  getPhoto,
  getPhotoTags,
  getSetting,
  importDeveloped,
  listPublications,
  onRapidrawProgress,
  rapidrawAvailable,
  listStackChildren,
  listVersions,
  offloadPhoto,
  restorePhoto,
  setLabel,
  setPickState,
  setRating,
  setSetting,
  StorageStatus,
  suggestTagsByTime,
  TagWithCount,
  unstackPhoto,
} from "../modules/api";
import { COLOR_LABELS } from "../modules/labels";
import { convertFileSrc } from "@tauri-apps/api/core";

// ── Per-photo RapidRAW round-trip state ──────────────────────────────────────
// The RapidRAW round-trip is long-lived and per-photo: after launch the promise can stay
// pending (the forwarded / Done-later case) while the backend watches for the output. That
// in-flight state MUST be keyed by photo id, not held in a single component instance —
// otherwise navigating from the editing photo A to photo B would show A's busy/Cancel on B
// and let B's Cancel button no-op against B's id.
//
// So we keep a module-scope map of photoId → { phase, note } driven by the single
// `rapidraw:progress` event stream (subscribed once, below), and let each DevelopSection read
// only its own photo's entry via useSyncExternalStore. State transitions clear the right
// photo's entry regardless of which photo is currently displayed.
type RapidrawPhase = "editing" | "waiting" | "importing";
interface RapidrawEntry {
  phase: RapidrawPhase;
  note: string;
}
const rapidrawState = new Map<number, RapidrawEntry>();
const rapidrawListeners = new Set<() => void>();

function notifyRapidraw() {
  for (const l of rapidrawListeners) l();
}
function setRapidrawEntry(photoId: number, entry: RapidrawEntry | null) {
  if (entry) rapidrawState.set(photoId, entry);
  else rapidrawState.delete(photoId);
  notifyRapidraw();
}

// Subscribe once to the backend progress stream and fold it into the per-photo map. The
// terminal phases (done/error/cancelled) clear the in-flight entry for THAT photo id — so a
// promise resolving while a different photo is shown still clears the correct entry. The note
// text mirrors the previous inline messages.
let rapidrawSubscribed = false;
function ensureRapidrawSubscription() {
  if (rapidrawSubscribed) return;
  rapidrawSubscribed = true;
  onRapidrawProgress((p) => {
    switch (p.phase) {
      case "editing":
        setRapidrawEntry(p.photoId, {
          phase: "editing",
          note: "Editing in RapidRAW… click Done there to import the result. If it opened in an existing RapidRAW window, edit there and click Done — or Cancel to stop waiting.",
        });
        break;
      case "waiting":
        setRapidrawEntry(p.photoId, {
          phase: "waiting",
          note: "Waiting for RapidRAW's exported result… click Done in RapidRAW, or Cancel to stop waiting.",
        });
        break;
      case "importing":
        setRapidrawEntry(p.photoId, { phase: "importing", note: "Importing the result…" });
        break;
      case "done":
      case "error":
      case "cancelled":
        // Terminal: clear this photo's in-flight entry. The launcher's promise handler
        // sets the (photo-scoped) completion note.
        setRapidrawEntry(p.photoId, null);
        break;
    }
  }).catch(() => {});
}

// Read a single photo's live RapidRAW entry (re-renders that DevelopSection when it changes).
function useRapidrawEntry(photoId: number): RapidrawEntry | null {
  return useSyncExternalStore(
    (onChange) => {
      rapidrawListeners.add(onChange);
      return () => rapidrawListeners.delete(onChange);
    },
    () => rapidrawState.get(photoId) ?? null,
  );
}

const WINDOW_OPTIONS: { label: string; secs: number }[] = [
  { label: "30s", secs: 30 },
  { label: "1m", secs: 60 },
  { label: "2m", secs: 120 },
  { label: "5m", secs: 300 },
  { label: "10m", secs: 600 },
];
import { MetadataPanel } from "./MetadataPanel";
import { IptcPanel } from "./IptcPanel";
import { VersionsPanel } from "./VersionsPanel";
import { PublishedPanel } from "./PublishedPanel";
import { panelsForSlot, useHostContributions } from "../modules/host";

const LABELS = COLOR_LABELS;

const PICKS: { key: "pick" | "reject" | "none"; label: string }[] = [
  { key: "pick", label: "Pick" },
  { key: "reject", label: "Reject" },
  { key: "none", label: "None" },
];

// Collapsed-row summary per storage status (dot colour + short label).
const STORAGE_META: Record<StorageStatus, { label: string; color: string }> = {
  localOnly: { label: "Local only", color: "#F59E0B" },
  backedUp: { label: "Backed up", color: "#10B981" },
  archived: { label: "On NAS", color: "#60A5FA" },
  offline: { label: "NAS offline", color: "#F87171" },
  missing: { label: "Missing", color: "#F87171" },
};

// A collapsible inspector section (the bottom accordion zone). The label acts as a
// toggle; open/closed state is persisted per section in localStorage so it survives
// photo navigation and restarts. Sections default to collapsed — `summary` is shown
// right-aligned on the collapsed row (e.g. "Backed up", a version count).
function Section({
  id,
  label,
  summary,
  children,
}: {
  id: string;
  label: string;
  summary?: ReactNode;
  children: ReactNode;
}) {
  const [open, setOpen] = useState(() => {
    const v = localStorage.getItem(`inspector.section.${id}`);
    return v === null ? false : v === "1";
  });
  const toggle = () =>
    setOpen((o) => {
      localStorage.setItem(`inspector.section.${id}`, o ? "0" : "1");
      return !o;
    });
  return (
    <div className={`field ${open ? "" : "field-collapsed"}`}>
      <label className="field-head" onClick={toggle}>
        <span className="field-caret">{open ? "▾" : "▸"}</span>
        {label}
        {!open && summary != null && summary !== "" && (
          <span className="field-head-sum">{summary}</span>
        )}
      </label>
      {open && <div className="field-body">{children}</div>}
    </div>
  );
}

// Shows the photos stacked under this one (e.g. a camera JPEG under its RAW). Each can
// be un-stacked (returns it to the grid). Only rendered when the photo has children.
// The whole stack group — the master ("Original") + its derivatives — shown for ANY member
// (whether the master or a stacked child is currently selected), with the active one
// highlighted. Click a row to view it; Unstack removes a derivative back to the grid.
function StackSection({
  photo,
  onChanged,
  onViewPhoto,
}: {
  photo: Photo;
  onChanged: () => void;
  onViewPhoto: (p: Photo) => void;
}) {
  const [master, setMaster] = useState<Photo | null>(null);
  const [children, setChildren] = useState<Photo[]>([]);

  // The stack's master: this photo's parent if it's a child, else itself if it has children.
  const masterId =
    photo.stackParentId != null
      ? photo.stackParentId
      : (photo.stackCount ?? 0) > 0
        ? photo.id
        : null;

  useEffect(() => {
    let alive = true;
    if (masterId == null) {
      setMaster(null);
      setChildren([]);
      return;
    }
    const loadMaster = masterId === photo.id ? Promise.resolve(photo) : getPhoto(masterId);
    Promise.all([loadMaster, listStackChildren(masterId)])
      .then(([m, cs]) => {
        if (alive) {
          setMaster(m);
          setChildren(cs);
        }
      })
      .catch(() => {
        if (alive) {
          setMaster(null);
          setChildren([]);
        }
      });
    return () => {
      alive = false;
    };
  }, [masterId, photo.id, photo.stackCount]);

  if (masterId == null || master == null) return null;
  const members = [master, ...children];
  return (
    <Section id="stack" label="Stack" summary={children.length}>
      <div className="stack-note">Original + its developed / derived versions — click to view.</div>
      {members.map((m) => {
        const active = m.id === photo.id;
        const isMaster = m.id === master.id;
        return (
          <div className={`stack-row ${active ? "stack-row-active" : ""}`} key={m.id}>
            <img
              className={`stack-thumb ${active ? "" : "stack-thumb-click"}`}
              src={convertFileSrc(String(m.id), "thumb")}
              alt=""
              title={active ? "Currently viewing" : "View in the loupe"}
              onClick={() => !active && onViewPhoto(m)}
            />
            <span className="stack-name" title={m.path}>
              {isMaster ? "Original — " : ""}
              {m.path.split("/").pop()}
            </span>
            {!active && (
              <button className="chip" title="View in the loupe" onClick={() => onViewPhoto(m)}>
                View
              </button>
            )}
            {!isMaster && (
              <button
                className="chip"
                title="Remove from stack — returns it to the grid as its own photo"
                onClick={() => unstackPhoto(m.id).then(onChanged)}
              >
                Unstack
              </button>
            )}
          </div>
        );
      })}
    </Section>
  );
}

// "Edit in…" — send the photo to an external develop app (darktable/RawTherapee/ART). When
// the editor closes, its develop sidecar is rendered via the editor CLI and the result is
// stacked under this photo (appears in the Stack section above). Only editors whose GUI is
// available are shown.
function DevelopSection({ photo, onChanged }: { photo: Photo; onChanged: () => void }) {
  const [editors, setEditors] = useState<AvailableEditor[]>([]);
  const [rapidraw, setRapidraw] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  // Sidecar-editor status note (darktable/RawTherapee/ART). RapidRAW's in-flight status is
  // per-photo and lives in the module-scope map; only its terminal (done/cancelled/error)
  // note is kept here, keyed by photo id so it's shown against the right photo.
  const [note, setNote] = useState<string | null>(null);
  // RapidRAW's live in-flight state for THIS photo (editing/waiting/importing), driven by the
  // rapidraw:progress stream — so the button disables and Cancel shows only on the photo that
  // is actually editing, never on whatever photo happens to be displayed.
  const rapidrawEntry = useRapidrawEntry(photo.id);
  useEffect(() => {
    ensureRapidrawSubscription();
    availableEditors()
      .then((es) => setEditors(es.filter((e) => e.gui)))
      .catch(() => setEditors([]));
    rapidrawAvailable()
      .then((s) => setRapidraw(s.available))
      .catch(() => setRapidraw(false));
  }, []);
  if (editors.length === 0 && !rapidraw) return null;

  const develop = (key: string, label: string) => {
    setBusy(key);
    setNote(`Editing in ${label}… the result imports when you close it.`);
    developInEditor(photo.id, key)
      .then((id) => {
        if (id != null) {
          setNote(null);
          onChanged();
        } else {
          setNote(`No changes detected. If ${label} was already open, edit there, then "Import result".`);
        }
      })
      .catch((e) => setNote(String(e)))
      .finally(() => setBusy(null));
  };
  const importResult = (key: string) => {
    setBusy(key);
    importDeveloped(photo.id, key)
      .then(() => {
        setNote(null);
        onChanged();
      })
      .catch((e) => setNote(String(e)))
      .finally(() => setBusy(null));
  };

  // "Edit in RapidRAW" round-trip. Unlike the sidecar editors this is request/response: we
  // hand RapidRAW an output path and it exports there on Done. If it forwarded the session to
  // an already-open window, the promise stays pending while the app watches for the output —
  // the Cancel button (cancelRapidraw) lets the user abandon that wait. In-flight state is
  // driven by the rapidraw:progress stream into the per-photo map (see above); the promise
  // handler only sets the terminal note, scoped to the photo it launched for.
  const runRapidraw = () => {
    // Optimistically mark editing so the affordance appears immediately (the "editing" event
    // will confirm it); clear any stale terminal note for this photo.
    const pid = photo.id;
    setNote(null);
    setRapidrawEntry(pid, {
      phase: "editing",
      note: "Editing in RapidRAW… click Done there to import the result. If it opened in an existing RapidRAW window, edit there and click Done — or Cancel to stop waiting.",
    });
    editInRapidraw(pid)
      .then((id) => {
        // Only surface the terminal note if THIS photo is still displayed; either way the
        // in-flight entry was already cleared by the terminal progress event.
        if (id != null) {
          if (photo.id === pid) setNote(null);
          onChanged();
        } else if (photo.id === pid) {
          setNote("Cancelled — nothing was imported.");
        }
      })
      .catch((e) => {
        // The backend already emitted an "error" phase that cleared the entry; ensure it's
        // gone (e.g. the reject-before-any-event case) and show the error on this photo.
        setRapidrawEntry(pid, null);
        if (photo.id === pid) setNote(String(e));
      });
  };
  const cancelRapidrawWait = () => {
    // Always target THIS section's photo id — it's the one whose in-flight entry is showing.
    cancelRapidraw(photo.id).catch(() => {});
  };

  const summary = [...editors.map((e) => e.label), ...(rapidraw ? ["RapidRAW"] : [])].join(" · ");
  const rapidrawBusy = rapidrawEntry != null;
  const displayNote = rapidrawEntry?.note ?? note;

  return (
    <Section id="develop" label="Edit in" summary={summary}>
      {editors.map((e) => (
        <div className="develop-row" key={e.key}>
          <button className="chip" disabled={busy !== null} onClick={() => develop(e.key, e.label)}>
            {busy === e.key ? `${e.label}…` : e.label}
          </button>
          {e.cli && (
            <button
              className="chip"
              disabled={busy !== null}
              title="Render the developed result from the current sidecar and stack it"
              onClick={() => importResult(e.key)}
            >
              Import result
            </button>
          )}
        </div>
      ))}
      {rapidraw && (
        <div className="develop-row">
          <button
            className="chip"
            disabled={busy !== null || rapidrawBusy}
            title="Open the photo in RapidRAW; the edited export is stacked under the original when you click Done"
            onClick={runRapidraw}
          >
            {rapidrawBusy ? "RapidRAW…" : "RapidRAW"}
          </button>
          {rapidrawBusy && (
            <button
              className="chip"
              title="Stop waiting for RapidRAW's exported result"
              onClick={cancelRapidrawWait}
            >
              Cancel
            </button>
          )}
        </div>
      )}
      {displayNote && <div className="develop-note">{displayNote}</div>}
    </Section>
  );
}

// Right sidebar: culling controls and tag assignment for the selected photo.
// Calls back to the parent after any catalog change so the grid can refresh.
// Two zones: a primary zone (rating / pick / colour label / tags — always visible)
// and a collapsed accordion of secondary sections beneath it.
export function PhotoInspector({
  photo,
  onChanged,
  allTags,
  status,
  activeVersionId,
  onSelectVersion,
  canEditVersions,
  onEditVersion,
  clipboardCount,
  selectionCount,
  onCopyTags,
  onPasteTags,
  onAssignTag,
  onRemoveTag,
  onRotate,
  onViewPhoto,
}: {
  photo: Photo | null;
  onChanged: () => void;
  /** Non-destructively rotate this photo by `delta` degrees (±90 / 180). */
  onRotate: (photoId: number, delta: number) => void;
  /** Open an off-grid photo (a stacked child) in the loupe. */
  onViewPhoto: (p: Photo) => void;
  /** The whole tag tree, for add-box autocomplete. */
  allTags: TagWithCount[];
  /** The photo's storage status, for the backup/offload/restore actions. */
  status: StorageStatus | null;
  /** The selected version (null = Original), for the Versions panel highlight. */
  activeVersionId: number | null;
  onSelectVersion: (v: import("../modules/api").PhotoVersion | null) => void;
  /** Whether the Basic Editor module is enabled (gates the per-version Edit button). */
  canEditVersions: boolean;
  onEditVersion: (v: import("../modules/api").PhotoVersion) => void;
  /** Number of tags currently on the tag clipboard (for the Paste button). */
  clipboardCount: number;
  /** Size of the current selection (Paste applies to all selected). */
  selectionCount: number;
  /** Copy these tag ids (this photo's tags) to the clipboard. */
  onCopyTags: (tagIds: number[]) => void;
  /** Paste the clipboard's tags onto the current selection. */
  onPasteTags: () => void;
  /** Assign a tag to the whole selection (not just the active photo). */
  onAssignTag: (tagId: number) => Promise<void> | void;
  /** Remove a tag from the whole selection (not just the active photo). */
  onRemoveTag: (tagId: number) => Promise<void> | void;
}) {
  const [tags, setTags] = useState<Tag[]>([]);
  const [newTag, setNewTag] = useState("");
  const [highlight, setHighlight] = useState(0);
  const [nearby, setNearby] = useState<TagWithCount[]>([]);
  const [windowSec, setWindowSec] = useState(120);
  const [storageMsg, setStorageMsg] = useState("");
  const [versionCount, setVersionCount] = useState(0);
  const [publications, setPublications] = useState<Publication[]>([]);
  useHostContributions(); // re-render when modules contribute/remove inspector panels

  // Load the persisted time window once.
  useEffect(() => {
    getSetting("nearby_window_seconds")
      .then((v) => {
        const n = v ? parseInt(v, 10) : NaN;
        if (!Number.isNaN(n)) setWindowSec(n);
      })
      .catch(() => {});
  }, []);

  useEffect(() => {
    setStorageMsg("");
    if (!photo) return setTags([]);
    getPhotoTags(photo.id).then(setTags).catch(() => setTags([]));
  }, [photo]);

  // Tags from photos taken around the same time (session tags like Events/Places).
  useEffect(() => {
    if (!photo) return setNearby([]);
    suggestTagsByTime(photo.id, windowSec).then(setNearby).catch(() => setNearby([]));
  }, [photo, windowSec]);

  // Version / publication counts for the collapsed accordion-row summaries.
  useEffect(() => {
    let alive = true;
    if (!photo) {
      setVersionCount(0);
      setPublications([]);
      return;
    }
    listVersions(photo.id)
      .then((vs) => alive && setVersionCount(vs.length))
      .catch(() => alive && setVersionCount(0));
    listPublications(photo.id)
      .then((ps) => alive && setPublications(ps))
      .catch(() => alive && setPublications([]));
    return () => {
      alive = false;
    };
  }, [photo]);

  const changeWindow = (secs: number) => {
    setWindowSec(secs);
    setSetting("nearby_window_seconds", String(secs)).catch(() => {});
  };

  if (!photo) {
    return (
      <aside className="panel inspector">
        <div className="panel-empty">Select a photo</div>
      </aside>
    );
  }

  const refreshTags = () => getPhotoTags(photo.id).then(setTags).catch(() => {});

  // Adds go through onAssignTag so they hit the whole selection, not just the
  // active photo (mirrors onRemoveTag); this panel's list still shows the active one.
  const addTag = async () => {
    const path = newTag.trim();
    if (!path) return;
    const tagId = await createTag(path);
    await onAssignTag(tagId);
    setNewTag("");
    setHighlight(0);
    await refreshTags();
    onChanged();
  };

  // Assign an existing tag picked from the autocomplete dropdown.
  const pickSuggestion = async (tagId: number) => {
    await onAssignTag(tagId);
    setNewTag("");
    setHighlight(0);
    await refreshTags();
    onChanged();
  };

  // Autocomplete for the add-tag box. Matches the query against tag names/paths and
  // ranks the **deepest** (lowest-in-tree) match first, so the most specific tag is
  // offered before its ancestors. Excludes already-assigned tags.
  const query = newTag.trim().toLowerCase();
  const assignedIds = new Set(tags.map((t) => t.id));
  const depth = (t: TagWithCount) => t.fullPath.split("/").length;
  const suggestions = query
    ? allTags
        .filter(
          (t) =>
            !assignedIds.has(t.id) &&
            (t.fullPath.toLowerCase().includes(query) || t.name.toLowerCase().includes(query)),
        )
        .sort((a, b) => {
          const byDepth = depth(b) - depth(a); // lowest in the tree first
          if (byDepth) return byDepth;
          const aStarts = a.name.toLowerCase().startsWith(query) ? 0 : 1;
          const bStarts = b.name.toLowerCase().startsWith(query) ? 0 : 1;
          if (aStarts !== bStarts) return aStarts - bStarts;
          return a.fullPath.localeCompare(b.fullPath);
        })
        .slice(0, 8)
    : [];
  const clampedHighlight = Math.min(highlight, Math.max(0, suggestions.length - 1));

  // Storage actions. Back up runs now if the NAS is reachable, else queues (the "both"
  // entry-point: manual + the auto-enqueue on import). Offload/Restore act immediately.
  const onBackup = async () => {
    setStorageMsg("Backing up…");
    try {
      await backupPhoto(photo.id);
      setStorageMsg("Backed up");
    } catch {
      await enqueueOperation("backup", photo.id).catch(() => {});
      setStorageMsg("Queued (NAS offline)");
    }
    onChanged();
  };
  const onOffload = async () => {
    setStorageMsg("Offloading…");
    try {
      await offloadPhoto(photo.id);
      setStorageMsg("Local copy freed");
    } catch (e) {
      setStorageMsg(String(e));
    }
    onChanged();
  };
  const onRestore = async () => {
    setStorageMsg("Restoring…");
    try {
      await restorePhoto(photo.id);
      setStorageMsg("Restored to local");
    } catch (e) {
      setStorageMsg(String(e));
    }
    onChanged();
  };

  // Assign an existing tag (e.g. from the time-based suggestions) and drop it from
  // the nearby list.
  const addExisting = async (tagId: number) => {
    await onAssignTag(tagId);
    setNearby((prev) => prev.filter((t) => t.id !== tagId));
    await refreshTags();
    onChanged();
  };

  // Header summary: camera · lens · ƒ/x · shutter · ISO (whatever the file carried).
  const fmtAperture = (a: number) => (Number.isInteger(a) ? String(a) : a.toFixed(1));
  const exifLine = [
    photo.cameraModel,
    photo.lens,
    photo.aperture != null ? `ƒ/${fmtAperture(photo.aperture)}` : null,
    photo.shutterSpeed,
    photo.iso != null ? `ISO ${photo.iso}` : null,
  ]
    .filter(Boolean)
    .join(" · ");

  const storageMeta = status ? STORAGE_META[status] : null;
  const platforms = [...new Set(publications.map((p) => p.platform))]
    .map((p) => p.charAt(0).toUpperCase() + p.slice(1))
    .join(", ");

  return (
    <aside className="panel inspector">
      <div className="inspector-header">
        <div className="inspector-filename" title={photo.path}>
          {photo.path.split("/").pop()}
        </div>
        {exifLine && (
          <div className="inspector-exif" title={exifLine}>
            {exifLine}
          </div>
        )}
      </div>

      <div className="ins-primary">
        <div className="ins-block">
          <div className="ins-label">Rating</div>
          <div className="stars">
            {[1, 2, 3, 4, 5].map((n) => (
              <button
                key={n}
                className={`star ${n <= photo.rating ? "star-on" : ""}`}
                onClick={() => setRating(photo.id, n === photo.rating ? 0 : n).then(onChanged)}
              >
                ★
              </button>
            ))}
          </div>
        </div>

        <div className="ins-block">
          <div className="ins-label">Pick</div>
          <div className="pick-seg">
            {PICKS.map((p) => (
              <button
                key={p.key}
                className={`pick-btn pick-${p.key} ${photo.pickState === p.key ? "pick-on" : ""}`}
                onClick={() => setPickState(photo.id, p.key).then(onChanged)}
              >
                {p.label}
              </button>
            ))}
          </div>
        </div>

        <div className="ins-block">
          <div className="ins-label">Color label</div>
          <div className="label-swatches">
            {LABELS.map((l) => (
              <button
                key={l.name}
                className={`swatch ${photo.label === l.name ? "swatch-on" : ""}`}
                style={{ background: l.color }}
                title={l.name}
                onClick={() =>
                  setLabel(photo.id, photo.label === l.name ? "" : l.name).then(onChanged)
                }
              />
            ))}
            <button
              className="swatch swatch-none"
              title="Clear label"
              onClick={() => photo.label && setLabel(photo.id, "").then(onChanged)}
            />
          </div>
        </div>

        <div className="ins-block">
          <div className="ins-label">Tags</div>
          <div className="tag-list">
            {tags.map((t) => (
              <span key={t.id} className="assigned-tag" title={t.fullPath}>
                {t.name}
                <button
                  className="tag-remove"
                  onClick={() =>
                    // Remove from the whole selection, not just the active photo, then
                    // refresh this panel's list (shows the active photo's tags).
                    Promise.resolve(onRemoveTag(t.id)).then(() => {
                      refreshTags();
                      onChanged();
                    })
                  }
                >
                  ×
                </button>
              </span>
            ))}
            {tags.length === 0 && <span className="panel-empty">none</span>}
          </div>
          <div className="row tag-copypaste">
            <button
              className="chip"
              onClick={() => onCopyTags(tags.map((t) => t.id))}
              disabled={tags.length === 0}
              title="Copy this photo's tags"
            >
              Copy tags
            </button>
            <button
              className="chip"
              onClick={onPasteTags}
              disabled={clipboardCount === 0}
              title={
                clipboardCount === 0
                  ? "Copy tags from a photo first"
                  : `Paste ${clipboardCount} tag(s) onto ${selectionCount > 1 ? `${selectionCount} selected` : "this photo"}`
              }
            >
              Paste{clipboardCount > 0 ? ` ${clipboardCount}` : ""}
              {selectionCount > 1 ? ` → ${selectionCount}` : ""}
            </button>
          </div>
          <div className="tag-add">
            <div className="row">
              <input
                className="tag-input"
                placeholder="Add tag…"
                value={newTag}
                onChange={(e) => {
                  setNewTag(e.target.value);
                  setHighlight(0);
                }}
                onKeyDown={(e) => {
                  if (e.key === "ArrowDown" && suggestions.length) {
                    e.preventDefault();
                    setHighlight((h) => Math.min(h + 1, suggestions.length - 1));
                  } else if (e.key === "ArrowUp" && suggestions.length) {
                    e.preventDefault();
                    setHighlight((h) => Math.max(h - 1, 0));
                  } else if (e.key === "Escape") {
                    setNewTag("");
                  } else if (e.key === "Enter") {
                    // Enter picks the highlighted suggestion; with none, creates the
                    // typed path (existing behaviour).
                    if (suggestions.length) pickSuggestion(suggestions[clampedHighlight].id);
                    else addTag();
                  }
                }}
              />
              <button
                className="chip"
                onClick={() =>
                  suggestions.length
                    ? pickSuggestion(suggestions[clampedHighlight].id)
                    : addTag()
                }
              >
                +
              </button>
            </div>
            {suggestions.length > 0 && (
              <ul className="tag-suggest">
                {suggestions.map((s, i) => {
                  const cut = s.fullPath.length - s.name.length;
                  return (
                    <li key={s.id}>
                      <button
                        className={`tag-suggest-item ${i === clampedHighlight ? "tag-suggest-on" : ""}`}
                        // onMouseDown (not onClick) so it fires before the input blurs.
                        onMouseDown={(e) => {
                          e.preventDefault();
                          pickSuggestion(s.id);
                        }}
                        onMouseEnter={() => setHighlight(i)}
                      >
                        <span className="tag-suggest-anc">{s.fullPath.slice(0, cut)}</span>
                        <span className="tag-suggest-leaf">{s.name}</span>
                      </button>
                    </li>
                  );
                })}
              </ul>
            )}
          </div>

          <div className="nearby">
            <div className="nearby-header">
              <span className="nearby-label">From nearby photos</span>
              <select
                className="window-select"
                value={windowSec}
                onChange={(e) => changeWindow(parseInt(e.target.value, 10))}
                title="Time window for nearby suggestions"
              >
                {WINDOW_OPTIONS.map((o) => (
                  <option key={o.secs} value={o.secs}>
                    ±{o.label}
                  </option>
                ))}
              </select>
            </div>
            {nearby.length > 0 ? (
              <div className="nearby-list">
                {nearby.map((t) => (
                  <button
                    key={t.id}
                    className="chip nearby-chip"
                    title={`${t.fullPath} — on ${t.photoCount} nearby photo(s)`}
                    onClick={() => addExisting(t.id)}
                  >
                    + {t.name} <span className="nearby-count">{t.photoCount}</span>
                  </button>
                ))}
              </div>
            ) : (
              <span className="panel-empty">none in window</span>
            )}
          </div>
        </div>

        {/* Inspector panels contributed by enabled modules (e.g. AI tagging) — part of
            the primary zone, per the redesign (AI TAGS sits under TAGS). */}
        {panelsForSlot("inspector").map((panel) => (
          <div className="ins-block" key={panel.id}>
            <div className="ins-label">{panel.label}</div>
            {panel.render()}
          </div>
        ))}
      </div>

      <div className="ins-sections">
        <Section id="orientation" label="Orientation">
          <div className="row">
            <button
              className="chip"
              title="Rotate left 90° (non-destructive — the original file is never changed)"
              onClick={() => onRotate(photo.id, -90)}
            >
              ↺ Left
            </button>
            <button
              className="chip"
              title="Rotate right 90° (non-destructive — the original file is never changed)"
              onClick={() => onRotate(photo.id, 90)}
            >
              ↻ Right
            </button>
            <button
              className="chip"
              title="Rotate 180° (non-destructive)"
              onClick={() => onRotate(photo.id, 180)}
            >
              180°
            </button>
          </div>
        </Section>

        <DevelopSection key={photo.id} photo={photo} onChanged={onChanged} />

        <StackSection photo={photo} onChanged={onChanged} onViewPhoto={onViewPhoto} />

        <Section
          id="storage"
          label="Storage"
          summary={
            storageMeta && (
              <>
                <span className="sum-dot" style={{ background: storageMeta.color }} />
                {storageMeta.label}
              </>
            )
          }
        >
          <div className="row">
            {status === "localOnly" && (
              <button className="chip" onClick={onBackup}>
                Back up
              </button>
            )}
            {status === "backedUp" && (
              <button className="chip" onClick={onOffload}>
                Offload local
              </button>
            )}
            {(status === "archived" || status === "offline") && (
              <button className="chip" onClick={onRestore}>
                Restore local
              </button>
            )}
            <span className="iptc-status">{storageMsg || status || ""}</span>
          </div>
        </Section>

        <Section id="versions" label="Versions" summary={versionCount > 0 ? versionCount : null}>
          <VersionsPanel
            photoId={photo.id}
            onChanged={() => {
              listVersions(photo.id)
                .then((vs) => setVersionCount(vs.length))
                .catch(() => {});
              onChanged();
            }}
            activeVersionId={activeVersionId}
            onSelectVersion={onSelectVersion}
            canEdit={canEditVersions}
            onEditVersion={onEditVersion}
          />
        </Section>

        <Section id="published" label="Published to" summary={platforms || null}>
          <PublishedPanel
            photoId={photo.id}
            activeVersionId={activeVersionId}
            onChanged={() => {
              listPublications(photo.id).then(setPublications).catch(() => {});
              onChanged();
            }}
          />
        </Section>

        <Section id="iptc" label="IPTC">
          <IptcPanel photoId={photo.id} />
        </Section>

        <Section id="metadata" label="Metadata">
          <MetadataPanel photoId={photo.id} />
        </Section>
      </div>
    </aside>
  );
}
