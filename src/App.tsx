import { useCallback, useEffect, useRef, useState } from "react";
import { confirm } from "@tauri-apps/plugin-dialog";
import {
  analyzeBurstSharpness,
  applyAutoTags,
  assignTag,
  cacheImages,
  CullingFilter,
  ingestFromCard,
  DeepLinkView,
  getPhotoByUuid,
  initCatalog,
  onCatalogSwitched,
  onDeepLinkPhoto,
  onDeepLinkTag,
  PhotoVersion,
  listTags,
  moveTag,
  setTagPrivate,
  rotatePhoto,
  onCacheProgress,
  onDevelopProgress,
  onImportProgress,
  onScanProgress,
  rescanLibrary,
  revealPhoto,
  videoServerPort,
  relocatePhoto,
  removePhotoFromCatalog,
  removeTag,
  restorePhoto,
  pickFile,
  getLibraryRoot,
  setLabel,
  setPickState,
  setRating,
  TagWithCount,
} from "./modules/api";
import type { Photo, ToolbarAction } from "./modules/registry";

// Human label for a photo's storage state (shown in the right-click menu / loupe).
function storageLabel(s?: StorageStatus): string {
  switch (s) {
    case "localOnly":
      return "On local disk";
    case "backedUp":
      return "Local + NAS backup";
    case "archived":
      return "On NAS";
    case "offline":
      return "On NAS (offline)";
    case "missing":
      return "Missing — no copy found";
    default:
      return "";
  }
}
import {
  addPhotosToAlbum,
  applyOffloadPolicy,
  getSetting,
  listPendingOperations,
  listVolumes,
  reconcileNow,
  StorageStatus,
  summarizePendingIdentity,
} from "./modules/api";
import { useLibrarySession } from "./modules/librarySession";
import { CatalogGrid } from "./components/CatalogGrid";
import { Splash, BOOT_STAGES } from "./components/Splash";
import { TagPanel } from "./components/TagPanel";
import { AlbumsPanel } from "./components/AlbumsPanel";
import { SmartAlbumsPanel } from "./components/SmartAlbumsPanel";
import { BatchesPanel } from "./components/BatchesPanel";
import { FilterBar } from "./components/FilterBar";
import { TagEditor } from "./components/TagEditor";
import { PhotoInspector } from "./components/PhotoInspector";
import { ZoomableImage } from "./components/ZoomableImage";
import { QuickTagBar } from "./components/QuickTagBar";
import { TagGroupsManager } from "./components/TagGroupsManager";
import { broadcastPhoto, onLoupeReady, openLoupeWindow } from "./modules/loupe";
import { prefetch, isVideoPath, videoUrl, setVideoPort } from "./modules/previewCache";
import {
  activeEditRenderer,
  initHost,
  mainViews,
  panelsForSlot,
  setActiveVersion as setHostActiveVersion,
  setChangeSink,
  setEditingTagContext,
  setFilterContext,
  setNavSink,
  setSelection,
  toolbarActions,
  activateToolbarAction,
  useHostContributions,
} from "./modules/host";
import { ModuleActionModal, ModuleContent, isModalAction } from "./modules/ModuleContent";
import { BUNDLED_MODULES } from "./modules/bundled";
import { useOwnedSubscription } from "./modules/ownedEvents";
import { Preferences } from "./components/Preferences";
import { IdentityDebtPanel } from "./components/IdentityDebtPanel";
import { ExportPanel } from "./components/ExportPanel";
import { PublishDialog } from "./components/PublishDialog";
import { ImportPanel } from "./components/ImportPanel";
import { BundleExportDialog } from "./components/BundleExportDialog";
import { BundleImportDialog } from "./components/BundleImportDialog";
import { CatalogSwitcher } from "./components/CatalogSwitcher";
import { EditorView } from "./components/EditorView";
import { parseEdit } from "./modules/editing";
import { ImportBatch } from "./modules/api";
import "./App.css";

const FILTERS: CullingFilter[] = ["all", "unrated", "pick", "reject", "edited"];

// Single-key culling shortcuts, ported from the old app's review shortcuts.
const COLOR_KEYS: Record<string, string> = {
  r: "Red",
  y: "Yellow",
  g: "Green",
  b: "Blue",
  v: "Purple",
  n: "",
};

export default function App() {
  // App reads only contributions state (mainViews/activeEditRenderer/toolbarActions/
  // panelsForSlot("loupe")) — it *writes* selection/filterContext/editingTag via
  // setSelection/setHostActiveVersion/setFilterContext/setEditingTagContext but never
  // reads them back, so a narrower subscription here (vs. the old useHost() union) means
  // a selection change no longer re-renders App and its whole subtree (issue #16 AC1).
  useHostContributions();
  const [ready, setReady] = useState(false);
  // Startup splash: the current boot stage (null = boot finished, splash fades out).
  const [bootStage, setBootStage] = useState<string | null>(BOOT_STAGES[0]);
  // The splash ends only when BOTH the init chain (catalog+modules) and the first photo
  // list are in — whichever finishes last dismisses it.
  const bootDone = useRef({ init: false, photos: false });
  const finishBootPart = (part: "init" | "photos") => {
    bootDone.current[part] = true;
    if (bootDone.current.init && bootDone.current.photos) setBootStage(null);
  };
  const [status, setStatus] = useState("");
  // Per-photo thumbnail cache-bust, bumped after a photo's file is recovered
  // (relocate / retrieve-from-NAS) so its tile refreshes instead of staying black.
  const [thumbBusts, setThumbBusts] = useState<Map<number, number>>(new Map());
  const bustThumb = useCallback((id: number) => {
    setThumbBusts((m) => new Map(m).set(id, (m.get(id) ?? 0) + 1));
  }, []);
  // Non-destructive orientation fix: rotate the displayed image by ±90° (or 180°), then
  // bust the photo's cached image so the grid tile and loupe re-fetch the new orientation.
  const rotateSelected = useCallback(
    (id: number, delta: number) => {
      rotatePhoto(id, delta)
        .then(() => bustThumb(id))
        .catch((e) => setStatus(`Rotate failed: ${e}`));
    },
    [bustThumb],
  );
  // Editing: the selected version (null = Original), the rendered loupe src for it, and
  // the version currently open in the editor modal.
  const [activeVersion, setActiveVersion] = useState<PhotoVersion | null>(null);
  const [editedSrc, setEditedSrc] = useState<string>("");
  const [develop, setDevelop] = useState(false); // the Develop (editor) view is active
  const [tags, setTags] = useState<TagWithCount[]>([]);
  const [albumsKey, setAlbumsKey] = useState(0); // bump to refresh album counts
  const [batchesKey, setBatchesKey] = useState(0); // bump to refresh batches
  const [smartAlbumsKey, setSmartAlbumsKey] = useState(0); // bump to refresh smart-album counts
  // Conservative default: 15.0 (mirrors SOFT_THRESHOLD_DEFAULT in facets.rs).
  const [softThreshold, setSoftThreshold] = useState<number>(15.0);

  // --- the library view -----------------------------------------------------
  // What the grid is asking for (the scope, and the one typed query derived from it), what
  // it got back (rows, total, storage badges, and the refresh generation that stops a slow
  // response from an older filter repainting a newer grid), and what is selected. The
  // shell owns none of that state any more: see modules/librarySession.ts (issue #15) and
  // modules/libraryQuery.ts (issue #10). What stays here is the composition — which panel
  // gets which verb, and which surface is on screen.
  const library = useLibrarySession();
  const { photos, statuses, scope, selection } = library;
  // Normally the active photo is the selected grid tile. A stacked child (hidden from the
  // grid) can also be opened for viewing via `library.viewPhoto`; the session holds it
  // aside so the loupe/inspector can show it even though it isn't in `photos`.
  const selected = selection.active;
  const [loupeInline, setLoupeInline] = useState(false);
  const [cachePreviews, setCachePreviews] = useState(true);

  // Side-panel layout: widths (px) and hidden state, persisted to localStorage.
  const [leftW, setLeftW] = useState(() => +(localStorage.getItem("panel.leftW") || 250));
  const [rightW, setRightW] = useState(() => +(localStorage.getItem("panel.rightW") || 300));
  const [leftHidden, setLeftHidden] = useState(
    () => localStorage.getItem("panel.leftHidden") === "1",
  );
  const [rightHidden, setRightHidden] = useState(
    () => localStorage.getItem("panel.rightHidden") === "1",
  );
  useEffect(() => {
    localStorage.setItem("panel.leftW", String(leftW));
    localStorage.setItem("panel.rightW", String(rightW));
    localStorage.setItem("panel.leftHidden", leftHidden ? "1" : "0");
    localStorage.setItem("panel.rightHidden", rightHidden ? "1" : "0");
  }, [leftW, rightW, leftHidden, rightHidden]);

  // Drag a column edge to resize. `side` picks which width to adjust; the right
  // column grows when dragged left, so its delta is inverted.
  const startResize = (side: "left" | "right") => (e: React.MouseEvent) => {
    e.preventDefault();
    const startX = e.clientX;
    const startW = side === "left" ? leftW : rightW;
    const onMove = (ev: MouseEvent) => {
      const delta = side === "left" ? ev.clientX - startX : startX - ev.clientX;
      const w = Math.max(140, Math.min(640, startW + delta));
      if (side === "left") setLeftW(w);
      else setRightW(w);
    };
    const onUp = () => {
      document.removeEventListener("mousemove", onMove);
      document.removeEventListener("mouseup", onUp);
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
    };
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
    document.addEventListener("mousemove", onMove);
    document.addEventListener("mouseup", onUp);
  };
  const [editingTag, setEditingTag] = useState<TagWithCount | null>(null);
  const [showCatalogSwitcher, setShowCatalogSwitcher] = useState(false);
  const [showPrefs, setShowPrefs] = useState(false);
  const [showExport, setShowExport] = useState(false);
  const [showPublish, setShowPublish] = useState(false);
  const [showImport, setShowImport] = useState(false);
  const [showIdentityDebt, setShowIdentityDebt] = useState(false);
  // Total pending-identity-debt count (issue #50) — badges the topbar entry point.
  // `null` means "unknown" (not yet fetched, or the last fetch failed) — distinct from a
  // confirmed 0, so a transient IPC error can never masquerade as "no debt" and quietly
  // remove the panel's only entry point.
  const [identityDebtCount, setIdentityDebtCount] = useState<number | null>(null);
  // Bundle export dialog state — set to the batch to export, null = closed.
  const [bundleExportBatch, setBundleExportBatch] = useState<ImportBatch | null>(null);
  // Bundle import dialog open/closed.
  const [showBundleImport, setShowBundleImport] = useState(false);
  const [importProgress, setImportProgress] = useState<{ done: number; total: number } | null>(
    null,
  );
  const [scanProgress, setScanProgress] = useState<{
    phase: string;
    done: number;
    total: number;
  } | null>(null);
  const [developStatus, setDevelopStatus] = useState<{ phase: string; editor: string } | null>(
    null,
  );
  const [pendingCount, setPendingCount] = useState(0);
  const reconciling = useRef(false); // guards against overlapping background drains
  const [tagClipboard, setTagClipboard] = useState<number[]>([]); // copied tag ids
  const [showGroups, setShowGroups] = useState(false);
  const [groupsKey, setGroupsKey] = useState(0); // bump to refresh the quick-tag bar
  const [activeViewId, setActiveViewId] = useState<string | null>(null); // null = Library
  // A module toolbar action currently open as a modal (its render(close)/mount(el, close)
  // overlay). `closeModalAction` is stable so ModuleActionModal's mount effect — which
  // keys on it — runs once per opened modal rather than once per App render.
  const [modalAction, setModalAction] = useState<ToolbarAction | null>(null);
  const closeModalAction = useCallback(() => setModalAction(null), []);
  // Right-click context menu on a grid tile.
  const [ctxMenu, setCtxMenu] = useState<{ x: number; y: number; photoId: number } | null>(null);

  // Full-surface views contributed by modules (e.g. Map). The active one, if its
  // module is still enabled — otherwise we fall back to the built-in Library view.
  const moduleViews = mainViews();
  const activeView = moduleViews.find((v) => v.id === activeViewId) ?? null;
  // The Basic Editor module gates the Develop view and per-version editing.
  const canEdit = activeEditRenderer() !== null;
  const inDevelop = develop && selection.activeId != null && canEdit;

  // Open any stack member in the inline loupe. Which photo is *selected* is the session's
  // business (a stacked child is off-grid, so it holds it aside); which surface is on
  // screen is the shell's.
  const viewPhotoInLoupe = (p: Photo) => {
    library.viewPhoto(p);
    setLoupeInline(true);
  };

  // The version to broadcast to the pop-out loupe: the active version's edit record,
  // guarded to the selected photo — on photo change the broadcast effect can fire
  // before the reset-version effect, so a stale other-photo version must not leak.
  const loupeEditJson =
    activeVersion && activeVersion.photoId === selection.activeId
      ? activeVersion.editJson
      : null;

  // Keep any open pop-out loupe window in sync with the current selection and the
  // active version (so picking a version updates the pop-out too), and
  // re-send when a loupe window announces it just opened.
  // Also preload neighbours so navigation is instant (the AGENTS.md preload
  // invariant). We prefetch further ahead than behind, since culling moves forward:
  // the next 5 photos and the previous 2.
  useEffect(() => {
    broadcastPhoto(selection.activeId, loupeEditJson);
    if (selection.activeId == null) return;
    const idx = photos.findIndex((p) => p.id === selection.activeId);
    if (idx === -1) return;
    for (let d = 1; d <= 5; d++) prefetch(photos[idx + d]?.id);
    prefetch(photos[idx - 1]?.id);
    prefetch(photos[idx - 2]?.id);
  }, [selection.activeId, loupeEditJson, photos]);

  useEffect(() => {
    const unlisten = onLoupeReady(() => broadcastPhoto(selection.activeId, loupeEditJson));
    return () => {
      unlisten.then((f) => f());
    };
  }, [selection.activeId, loupeEditJson]);

  // --- chairphoto://<uuid>[/loupe|/develop] deep links ----------------------
  // A link (e.g. from an Obsidian note) opens/focuses the app on that photo —
  // in the Library grid by default, or straight into the loupe / Develop view.
  // Links can arrive before the catalog is open, so buffer the uuid and handle
  // it once `ready`. Window focus itself is handled Rust-side (single-instance).
  const pendingDeepLink = useRef<{ uuid: string; view: DeepLinkView } | null>(null);
  const [deepLinkTick, setDeepLinkTick] = useState(0);
  const [deepLinkTarget, setDeepLinkTarget] = useState<{
    photo: Photo;
    view: DeepLinkView;
  } | null>(null);
  useEffect(() => {
    const unlisten = onDeepLinkPhoto((uuid, view) => {
      pendingDeepLink.current = { uuid, view };
      setDeepLinkTick((t) => t + 1);
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  useEffect(() => {
    if (!ready || !pendingDeepLink.current) return;
    const { uuid, view } = pendingDeepLink.current;
    pendingDeepLink.current = null;
    getPhotoByUuid(uuid)
      .then((p) => {
        // The photo may be outside the current scope — widen to the whole library so the
        // grid contains it. That changes the query, hence `refresh`'s identity, so the
        // list effect re-fetches.
        setActiveViewId(null); // back to Library
        setDevelop(false);
        library.clearScope();
        setDeepLinkTarget({ photo: p, view }); // selection happens once the grid has it
      })
      .catch(() => setStatus(`Deep link: no photo ${uuid} in this catalog`));
  }, [ready, deepLinkTick]);

  // Select the staged target once the (now unfiltered) photo list contains it,
  // then apply the requested surface. A stacked child never appears in the grid —
  // view it off-grid via `viewPhotoInLoupe`. The stackParentId guard keeps the stale-list
  // race (this effect fires once with the old filtered list) from prematurely
  // falling back for a grid photo.
  useEffect(() => {
    if (!deepLinkTarget) return;
    const { photo: target, view } = deepLinkTarget;
    const applyView = () => {
      // /develop falls back to the Library if no editor module is enabled
      // (inDevelop requires canEdit); /loupe opens the inline loupe.
      if (view === "loupe") setLoupeInline(true);
      else if (view === "develop") setDevelop(true);
    };
    if (photos.some((p) => p.id === target.id)) {
      library.select(target.id);
      applyView();
      setDeepLinkTarget(null);
    } else if (target.stackParentId != null) {
      viewPhotoInLoupe(target); // already opens the inline loupe
      applyView();
      setDeepLinkTarget(null);
    }
    // otherwise: keep waiting — the unfiltered refresh hasn't landed yet.
  }, [photos, deepLinkTarget]);

  // --- chairphoto://tag/<uuid> deep links -----------------------------------
  // A link (e.g. from an Obsidian tag note) filters the Library to that tag.
  // Tags are matched by their stable uuid against the loaded tag tree, so the
  // link survives renames/moves. Buffer until the tree is in.
  const pendingTagLink = useRef<string | null>(null);
  const [tagLinkTick, setTagLinkTick] = useState(0);
  useEffect(() => {
    const unlisten = onDeepLinkTag((uuid) => {
      pendingTagLink.current = uuid;
      setTagLinkTick((t) => t + 1);
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, []);
  useEffect(() => {
    if (!ready || !pendingTagLink.current || tags.length === 0) return;
    const uuid = pendingTagLink.current;
    pendingTagLink.current = null;
    const tag = tags.find((t) => t.uuid === uuid);
    if (!tag) {
      setStatus(`Deep link: no tag ${uuid} in this catalog`);
      return;
    }
    setActiveViewId(null); // back to Library
    setDevelop(false);
    library.selectTag(tag.id); // clears the other primary scopes
  }, [ready, tagLinkTick, tags]);

  // Open the default catalog once on startup, then start the plugin host. Each step
  // reports its boot stage to the splash overlay.
  useEffect(() => {
    initCatalog()
      // Populate auto-tags (e.g. monochrome) for photos imported before the rule.
      .then(() => {
        setBootStage(BOOT_STAGES[1]); // Updating auto-tags…
        return applyAutoTags().catch(() => {});
      })
      .then(() => {
        setBootStage(BOOT_STAGES[2]); // Starting modules…
        setReady(true);
        // The sidebar panels (albums / smart albums / batches) and the FilterBar fetch
        // their lists in mount effects, which fire BEFORE this initCatalog() chain has
        // opened the catalog — those first fetches fail ("No catalog is open") and are
        // swallowed. Bump their reload keys now that the catalog is open, mirroring the
        // catalog-switch path.
        setAlbumsKey((k) => k + 1);
        setBatchesKey((k) => k + 1);
        setSmartAlbumsKey((k) => k + 1);
        setGroupsKey((k) => k + 1);
        return initHost(BUNDLED_MODULES);
      })
      .then(() => {
        setBootStage(BOOT_STAGES[3]); // Loading photos…
        finishBootPart("init");
      })
      .catch((e) => {
        setStatus(`Failed to open catalog: ${e}`);
        setBootStage(null); // never leave the splash hanging on a failed boot
      });
    // Learn the loopback video-server port so the <video> player can stream files.
    videoServerPort()
      .then(setVideoPort)
      .catch(() => {});
  }, []);

  // Keep the plugin host's notion of the selection in sync, so modules (e.g. AI
  // tagging) can act on the active photo or the whole multi-selection.
  useEffect(() => {
    setSelection(selection.photos, selection.activeId);
  }, [selection.photos, selection.activeId]);

  // Keep the host's active-version in sync so publishing modules can default to the
  // version the user is viewing.
  useEffect(() => {
    setHostActiveVersion(activeVersion?.id ?? null);
  }, [activeVersion]);

  // Keep the host's filter context in sync so modules (e.g. Statistics) can reflect
  // the active sidebar scope (tag / album / import batch).
  useEffect(() => {
    setFilterContext({ tagId: scope.tagId, albumId: scope.albumId, batchId: scope.batchId });
  }, [scope.tagId, scope.albumId, scope.batchId]);

  // Keep the host's editing-tag in sync so "tag-editor" slot panels (e.g. the
  // Obsidian tag note) know which tag the tag editor modal is showing.
  useEffect(() => {
    setEditingTagContext(editingTag);
  }, [editingTag]);

  // Dismiss the right-click menu on Escape.
  useEffect(() => {
    if (!ctxMenu) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setCtxMenu(null);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [ctxMenu]);

  // Re-run the library query and reload the tag tree. The photo rows, their badges and the
  // stale-response guard live in `useLibrarySession`; the tag tree is the shell's own.
  const refreshLibrary = library.refresh;
  const refresh = useCallback(async () => {
    const [, nextTags] = await Promise.all([refreshLibrary(), listTags()]);
    setTags(nextTags);
  }, [refreshLibrary]);

  // --- recovery actions for a photo whose original can't be shown ----------
  // Shared by the right-click menu and the loupe's "unavailable" state.
  const photoName = (id: number) =>
    photos.find((p) => p.id === id)?.path.split("/").pop() ?? `photo ${id}`;

  const relocatePhotoAction = async (id: number) => {
    const root = await getLibraryRoot().catch(() => undefined);
    const picked = await pickFile(root);
    if (!picked) return;
    try {
      await relocatePhoto(id, picked);
      bustThumb(id);
      await refresh();
      setStatus("Photo relocated to its new file.");
    } catch (e) {
      setStatus(`Couldn't relocate: ${e}`);
    }
  };

  const retrieveFromNasAction = async (id: number) => {
    setStatus("Retrieving from NAS…");
    try {
      await restorePhoto(id);
      bustThumb(id);
      await refresh();
      setStatus("Retrieved from NAS.");
    } catch (e) {
      setStatus(`Couldn't retrieve from NAS: ${e}`);
    }
  };

  const removeFromCatalogAction = async (id: number) => {
    const ok = await confirm(
      `Remove "${photoName(id)}" from the catalog? This deletes its catalog entry ` +
        "(tags, rating, versions) but never deletes the file on disk or the NAS.",
      { title: "Remove from catalog", kind: "warning" },
    );
    if (!ok) return;
    try {
      await removePhotoFromCatalog(id);
      await refresh();
      setStatus("Removed from catalog (files left untouched).");
    } catch (e) {
      setStatus(`Couldn't remove: ${e}`);
    }
  };

  // Add the current selection (or active photo) to an album.
  const addSelectionToAlbum = async (albumId: number) => {
    const targets = selection.targets;
    if (!targets.length) return;
    await addPhotosToAlbum(albumId, targets);
    setStatus(`Added ${targets.length} to album`);
    setAlbumsKey((k) => k + 1);
    await refresh();
  };

  const firstRefresh = useRef(true);
  useEffect(() => {
    if (!ready) return;
    // Load the soft-threshold setting whenever the catalog or filter changes. It's a
    // catalog setting (may differ per-catalog) so we read it here, not on module init.
    getSetting("sharpness.soft_threshold").then((raw) => {
      const v = raw != null ? parseFloat(raw) : NaN;
      if (isFinite(v) && v > 0) setSoftThreshold(v);
      else setSoftThreshold(15.0);
    }).catch(() => {});
    const p = refresh();
    if (firstRefresh.current) {
      firstRefresh.current = false;
      // First photo list is in (or failed) — release the splash's "photos" half.
      p.finally(() => finishBootPart("photos"));
    }
  }, [ready, refresh]);

  // Assign a tag to the whole current selection (or the active photo) — used by the
  // quick-tag bar.
  const assignToSelection = async (tagId: number) => {
    for (const id of selection.targets) await assignTag(id, tagId);
    await refresh();
    setGroupsKey((k) => k + 1); // refresh the quick-tag bar's "Recently used" group
  };

  // Remove a tag from the whole selection (mirrors assignToSelection). The inspector's
  // tag list shows the active photo's tags, but the remove action applies to every
  // selected photo so multi-select edits don't silently hit only the active one.
  const removeFromSelection = async (tagId: number) => {
    for (const id of selection.targets) await removeTag(id, tagId);
    await refresh();
    setGroupsKey((k) => k + 1);
  };

  // Copy/paste tags between photos: snapshot one photo's tag ids, then apply them to the
  // current selection.
  const copyTags = (tagIds: number[]) => {
    setTagClipboard(tagIds);
    setStatus(
      tagIds.length ? `Copied ${tagIds.length} tag(s)` : "That photo has no tags to copy",
    );
  };
  const pasteTagsToSelection = async () => {
    if (tagClipboard.length === 0) return;
    const targets = selection.targets;
    if (targets.length === 0) return;
    for (const id of targets) for (const tagId of tagClipboard) await assignTag(id, tagId);
    await refresh();
    setGroupsKey((k) => k + 1);
    setStatus(`Pasted ${tagClipboard.length} tag(s) onto ${targets.length} photo(s)`);
  };

  // Let modules (e.g. AI tagging) ask the app to refresh after they change data.
  // Also refresh the quick-tag bar so module tagging updates "Recently used".
  useEffect(() => {
    setChangeSink(() => {
      refresh();
      setGroupsKey((k) => k + 1);
      // Smart-album membership is derived, so any data change can shift its counts.
      setSmartAlbumsKey((k) => k + 1);
    });
  }, [refresh]);

  // Let a module (Tag Graph) navigate the Library: filter by a tag, or select a photo.
  // selectPhotoSilent sets the selection without switching the active view (used by the
  // Map filmstrip so the pop-out loupe follows without leaving the map).
  useEffect(() => {
    setNavSink({
      filterByTag: (tagId) => {
        setActiveViewId(null); // back to Library
        library.selectTag(tagId);
      },
      selectPhoto: (photoId) => {
        setActiveViewId(null);
        library.selectQuiet(photoId);
      },
      selectPhotoSilent: (photoId) => {
        // Update app-wide selection (loupe follows), but do NOT change the active view.
        library.selectQuiet(photoId);
      },
    });
  }, [library.selectTag, library.selectQuiet]);

  const refreshPending = useCallback(async () => {
    try {
      // Count only actionable (pending) ops — a permanently-failed op shouldn't keep
      // the badge lit or trigger the prompt forever.
      const ops = await listPendingOperations();
      setPendingCount(ops.filter((o) => o.status === "pending").length);
    } catch {
      setPendingCount(0);
    }
  }, []);

  // Cheap count-only refresh (issue #50) — never pulls the full pending-identity list
  // just to badge the topbar entry point.
  const refreshIdentityDebtCount = useCallback(async () => {
    try {
      const s = await summarizePendingIdentity();
      setIdentityDebtCount(s.total);
    } catch {
      // Leave the count as it was: an IPC error means "unknown", not "zero". Resetting
      // to 0 here would hide the topbar chip and silently remove the panel's only entry
      // point on a transient failure.
    }
  }, []);

  // Drain the backup queue in the background (no blocking dialog). Guarded so
  // overlapping triggers (focus events) don't start parallel drains.
  const runReconcile = useCallback(async () => {
    if (reconciling.current) return;
    reconciling.current = true;
    try {
      const before = (await listPendingOperations()).filter((o) => o.status === "pending").length;
      if (before > 0) setStatus(`Backing up ${before} to NAS…`);
      const result = await reconcileNow();
      if (!result.skippedOffline && result.ran + result.failed > 0) {
        setStatus(`Backed up ${result.ran}` + (result.failed ? `, ${result.failed} failed` : ""));
      }
      // Now that backups are current, apply the "keep last N days local" policy: offload
      // older, backed-up photos to free local space. No-op when the policy is off or the
      // NAS is unreachable.
      if (!result.skippedOffline) {
        try {
          const n = await applyOffloadPolicy();
          if (n > 0) setStatus(`Offloaded ${n} older photo(s) to the NAS`);
        } catch {
          /* offload is best-effort; never block the UI */
        }
      }
      await Promise.all([refresh(), refreshPending()]);
    } catch (e) {
      setStatus(`Backup failed: ${e}`);
    } finally {
      reconciling.current = false;
    }
  }, [refresh, refreshPending]);

  // On launch / window focus: if a backup volume is reachable and ops are queued, run
  // the backup in the background (no modal). Refresh the pending count regardless.
  const checkReconcile = useCallback(async () => {
    try {
      const [ops, volumes] = await Promise.all([listPendingOperations(), listVolumes()]);
      const pending = ops.filter((o) => o.status === "pending").length;
      setPendingCount(pending);
      const backupReachable = volumes.some((v) => v.kind === "backup" && v.reachable);
      if (pending > 0 && backupReachable) runReconcile();
    } catch {
      /* ignore */
    }
  }, [runReconcile]);

  // Detect the NAS on launch and whenever the window regains focus.
  useEffect(() => {
    if (!ready) return;
    refreshPending();
    checkReconcile();
    refreshIdentityDebtCount();
    const onFocus = () => checkReconcile();
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, [ready, checkReconcile, refreshPending, refreshIdentityDebtCount]);

  const onScan = async () => {
    setStatus("Scanning library…");
    try {
      const result = await rescanLibrary();
      setStatus(
        `Scanned ${result.scanned}, imported ${result.imported} (${result.created} new)` +
          (result.errors ? `, ${result.errors} errors` : ""),
      );
      await refresh();
      refreshPending(); // scan auto-enqueues backups for new photos
      // The scanner is the primary producer of identity debt (unwritable/unreachable
      // sidecars found during indexing) — refresh the badge here too, not just at boot
      // and on panel close, or debt from this scan stays invisible until restart.
      refreshIdentityDebtCount();
      setBatchesKey((k) => k + 1); // a scan may have created a new import batch
      // Pre-cache so browsing is instant. Thumbnails always; previews if opted in.
      // Progress is shown via the cache:progress listener below.
      cacheImages(cachePreviews)
        .then(() => setStatus("Cache ready"))
        .catch((e) => setStatus(`Cache failed: ${e}`));
    } catch (e) {
      setStatus(`Scan failed: ${e}`);
    }
  };

  // Burst-relative sharpness analysis (H16e): run over the current selection, or
  // all visible photos if nothing is selected. Groups into H15b clusters, flags
  // soft-in-burst / sharpest-of-burst, then refreshes the grid.
  const runBurstAnalysis = async () => {
    // Unlike the tagging actions, the fallback here is the whole *view*, not the active
    // photo: "analyse this burst" with nothing selected means the grid in front of you.
    const targets = selection.ids.length ? selection.ids : photos.map((p) => p.id);
    if (targets.length === 0) {
      setStatus("No photos to analyse — scan or select some first.");
      return;
    }
    setStatus(`Analysing burst sharpness for ${targets.length} photos…`);
    try {
      const result = await analyzeBurstSharpness(targets);
      setStatus(
        `Burst analysis done — ${result.clusters} cluster(s), ` +
          `${result.flaggedBest} best frame(s), ${result.flaggedSoft} soft-in-burst.`,
      );
      await refresh();
    } catch (e) {
      setStatus(`Burst analysis failed: ${e}`);
    }
  };

  // Surface batch-cache progress in the status bar. `useOwnedSubscription` owns the async
  // registration (issue #13): one that resolves after this effect is cleaned up is stopped
  // rather than left running.
  useOwnedSubscription(
    () =>
      onCacheProgress((p) => {
        setStatus(
          p.done < p.total ? `Caching ${p.done}/${p.total}…` : `Cache ready (${p.total})`,
        );
      }),
    [],
  );

  // Card import runs in the background (the dialog closes immediately) — track its copy
  // progress for the topbar indicator.
  useEffect(() => {
    const unlisten = onImportProgress((p) => setImportProgress(p));
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  // Throttle for live-scan grid refreshes: don't hammer listPhotos on every
  // COMMIT_EVERY (500-file) event — that would refetch the whole list hundreds of times
  // for a large NAS scan. Both Phase A ("indexing") and Phase B ("metadata") emit at the
  // same COMMIT_EVERY=500 cadence, so both need a 2-second throttle. "finalizing" emits
  // only a single event and is fine without throttling. "done" always refreshes immediately
  // to clear any residual placeholder tiles.
  const lastIndexingRefresh = useRef(0);
  const lastMetadataRefresh = useRef(0);

  // Folder/NAS scans run on a background connection (see run_blocking_scan); stream their
  // progress to the topbar. Refresh the grid on every "indexing" commit (throttled) so
  // newly-inserted Phase A rows appear as placeholder tiles in real time. "metadata" commits
  // are also throttled (same COMMIT_EVERY cadence as indexing). The "done" event does a final
  // unconditional refresh that flips any residual placeholders to real tiles.
  // Same owned registration as the cache listener above (issue #13).
  useOwnedSubscription(
    () =>
      onScanProgress((p) => {
        if (p.phase === "done") {
          setScanProgress(null);
          lastIndexingRefresh.current = 0; // reset so the next scan starts fresh
          lastMetadataRefresh.current = 0;
          refresh();
        } else {
          setScanProgress(p);
          if (p.phase === "indexing") {
            const now = Date.now();
            if (now - lastIndexingRefresh.current >= 2000) {
              lastIndexingRefresh.current = now;
              refresh();
            }
          } else if (p.phase === "metadata") {
            // Phase B emits at the same COMMIT_EVERY=500 cadence as Phase A — throttle
            // identically to prevent the same per-event query storm.
            const now = Date.now();
            if (now - lastMetadataRefresh.current >= 2000) {
              lastMetadataRefresh.current = now;
              refresh();
            }
          } else {
            // "finalizing" and any other single-shot phases: refresh immediately.
            refresh();
          }
        }
      }),
    [refresh],
  );

  // External-develop round-trip status (darktable/RawTherapee/ART) for the topbar.
  useEffect(() => {
    const unlisten = onDevelopProgress((p) => {
      setDevelopStatus(["done", "nochange", "error"].includes(p.phase) ? null : p);
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  // On catalog:switched (emitted by switch_catalog after teardown + reinit), reset ALL
  // transient React state so nothing from the previous catalog bleeds into the new one,
  // then refresh the photo list and tags against the new catalog.
  useEffect(() => {
    const unlisten = onCatalogSwitched(() => {
      // The library session: scope, selection, Shift anchor, and the rows themselves.
      // Every id in there names something in the catalog that just closed. Dropping the
      // rows immediately also stops the previous catalog's photos being on screen (with
      // stale ids) while the new query resolves, and disowns a refresh still running
      // against it. See modules/librarySession.ts.
      library.reset();
      // View state
      setActiveViewId(null);
      setDevelop(false);
      setLoupeInline(false);
      setActiveVersion(null);
      setEditedSrc("");
      // Progress indicators
      setScanProgress(null);
      setImportProgress(null);
      setDevelopStatus(null);
      setPendingCount(0);
      // Identity debt is per-catalog, so the previous catalog's count must not keep
      // showing as current: `null` (not `0`) so the chip reads "(?)" rather than
      // disappearing while the real count is unknown — same reasoning as an IPC error in
      // `refreshIdentityDebtCount` above.
      // `ready` only flips true once at boot, so the `[ready, ...]` mount effect that
      // normally calls `refreshIdentityDebtCount` never re-fires on a catalog switch;
      // call it directly below instead of relying on that effect.
      setIdentityDebtCount(null);
      // The tag tree is the shell's, not the session's — clear it here for the same
      // reason the session clears its rows.
      setTags([]);
      // Reload keys — bump so sidebar panels re-query the new catalog.
      setAlbumsKey((k) => k + 1);
      setBatchesKey((k) => k + 1);
      setSmartAlbumsKey((k) => k + 1);
      setGroupsKey((k) => k + 1);
      // Tag clipboard / dialogs — clear to prevent stale IDs crossing catalogs.
      setTagClipboard([]);
      setEditingTag(null);
      // Status
      setStatus("Catalog opened.");
      // NOTE: do NOT call refresh() here. `library.reset()` invalidates the query, so the
      // dependency-chain useEffect([ready, refresh]) above fires once the resets have
      // committed and `refresh` has stabilised on a clean scope. Calling refresh() here
      // would capture the OLD closure (stale filter state) and query the new catalog with
      // tag/album IDs that belonged to the previous catalog, producing a flash of wrong data.
      //
      // `refreshIdentityDebtCount`, unlike `refresh`, closes over no filter/tag state, so
      // it's safe to call directly here rather than wait on an effect — nothing else
      // calls it on a catalog switch, since `ready` never flips back to false.
      refreshIdentityDebtCount();
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, [library.reset, refreshIdentityDebtCount]);

  // Reset the active version to Original whenever the selected photo changes.
  useEffect(() => {
    setActiveVersion(null);
    setEditedSrc("");
  }, [selection.activeId]);

  // Render the active version's edit for the loupe (via the editing module's renderer).
  // Falls back to the unedited preview when there's no version, no renderer, or on error.
  useEffect(() => {
    const renderer = activeEditRenderer();
    const photoId = selection.activeId;
    if (photoId == null || activeVersion == null || !renderer) {
      setEditedSrc("");
      return;
    }
    let cancelled = false;
    renderer
      .render(photoId, parseEdit(activeVersion.editJson) as Record<string, unknown>)
      .then((url) => !cancelled && setEditedSrc(url))
      .catch(() => !cancelled && setEditedSrc(""));
    return () => {
      cancelled = true;
    };
  }, [selection.activeId, activeVersion]);

  // Zoom-in render for the active version (ZoomableImage fetches it lazily on first
  // zoom): the same edit over the native-size preview, so a cropped version has real
  // pixels to magnify. Falls back to zooming the fast render if the module's renderer
  // doesn't offer a hi-res variant.
  const renderHiVersion = useCallback(() => {
    const renderHi = activeEditRenderer()?.renderHi;
    const photoId = selection.activeId;
    if (photoId == null || activeVersion == null || !renderHi) {
      return Promise.resolve("");
    }
    return renderHi(photoId, parseEdit(activeVersion.editJson) as Record<string, unknown>);
  }, [selection.activeId, activeVersion]);

  // Kick off a background import from a card. The ImportPanel collects the source folder
  // and optional name, then closes; progress shows in the topbar via import:progress.
  const startImport = useCallback(
    async (source: string, name: string, selected?: string[]) => {
      setImportProgress({ done: 0, total: 0 });
      setStatus("Importing from card…");
      try {
        const r = await ingestFromCard(source, name || undefined, selected);
        setStatus(
          `Imported ${r.created} new of ${r.scanned} on card` +
            (r.skipped ? `, ${r.skipped} already imported` : "") +
            (r.errors ? `, ${r.errors} errors` : ""),
        );
        await refresh();
        refreshPending();
        setBatchesKey((k) => k + 1);
      } catch (e) {
        setStatus(`Import failed: ${e}`);
      } finally {
        setImportProgress(null);
      }
    },
    [refresh, refreshPending],
  );

  // Keyboard culling. Active whenever a photo is selected and focus isn't in an input.
  useEffect(() => {
    const handler = async (e: KeyboardEvent) => {
      if (activeView || inDevelop) return; // a full-surface view owns input
      const target = e.target as HTMLElement;
      if (target.tagName === "INPUT" || target.tagName === "TEXTAREA") return;

      const key = e.key.toLowerCase();

      // Ctrl/Cmd+A: select every photo in the current view (works with nothing selected yet).
      if ((e.ctrlKey || e.metaKey) && key === "a") {
        library.selectAll();
        e.preventDefault();
        return;
      }

      if (!selected) return; // the shortcuts below act on the active photo
      // Culling applies to the whole selection (batch) — advance only if culling one.
      const targets = selection.targets;
      const applyAll = async (fn: (id: number) => Promise<unknown>) => {
        for (const id of targets) await fn(id);
        await refresh();
        // Auto-advance after a cull decision, over the rows this handler was created
        // with — not the ones the refresh above just produced, from which the photo that
        // was just rated may have dropped out of the current filter.
        if (targets.length === 1) library.stepActive(1);
      };

      if (e.key === "Enter") {
        setLoupeInline((v) => !v);
      } else if (e.key === "Escape") {
        setLoupeInline(false);
      } else if (e.key === "ArrowRight" || e.key === "ArrowDown") {
        // Shift extends the selection from the anchor; plain moves a single selection.
        library.stepActive(1, e.shiftKey);
      } else if (e.key === "ArrowLeft" || e.key === "ArrowUp") {
        library.stepActive(-1, e.shiftKey);
      } else if (key >= "0" && key <= "5") {
        await applyAll((id) => setRating(id, parseInt(key, 10)));
      } else if (key === "p") {
        await applyAll((id) => setPickState(id, "pick"));
      } else if (key === "x") {
        await applyAll((id) => setPickState(id, "reject"));
      } else if (key === "u") {
        await applyAll((id) => setPickState(id, "none"));
      } else if (key in COLOR_KEYS) {
        await applyAll((id) => setLabel(id, COLOR_KEYS[key]));
      } else {
        return;
      }
      e.preventDefault();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
    // `selectAll`/`stepActive` close over the rows and the active photo, so they stand in
    // for the `photos`/`selectedId` dependencies the handler used to name itself.
  }, [
    selected,
    selection.targets,
    library.selectAll,
    library.stepActive,
    refresh,
    activeView,
    inDevelop,
  ]);

  return (
    <div className="app">
      <Splash stage={bootStage} />
      <header className="topbar">
        <button
          className={`chip panel-toggle ${leftHidden ? "" : "chip-on"}`}
          onClick={() => setLeftHidden((v) => !v)}
          title={leftHidden ? "Show left panel" : "Hide left panel"}
        >
          ⬛ Tags
        </button>
        <span className="brand">ChairPhoto</span>
        <span className="topbar-sep" aria-hidden />
        <button className="btn-primary" onClick={onScan} disabled={!ready} title="Re-index your library folder">
          Rescan library
        </button>
        <button className="btn-ghost" onClick={() => setShowImport(true)} disabled={!ready} title="Copy from a card into your library">
          Import card
        </button>
        <button className="btn-ghost" onClick={() => setShowBundleImport(true)} disabled={!ready} title="Import a .chairphoto bundle from another machine">
          Import bundle
        </button>
        <label className="cache-opt" title="Pre-generate full previews on import (slower import, instant loupe)">
          <input
            type="checkbox"
            checked={cachePreviews}
            onChange={(e) => setCachePreviews(e.target.checked)}
          />
          Cache previews
        </label>
        {(moduleViews.length > 0 || activeViewId !== null || canEdit) && (
          <div className="view-switcher">
            <div className="seg">
              <button
                className={`seg-item ${activeView === null && !inDevelop ? "on" : ""}`}
                onClick={() => {
                  setActiveViewId(null);
                  setDevelop(false);
                }}
              >
                {/* Grid glyph — matches the app's 13px stroke icon language. */}
                <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
                  <rect x="3" y="3" width="7" height="7" />
                  <rect x="14" y="3" width="7" height="7" />
                  <rect x="14" y="14" width="7" height="7" />
                  <rect x="3" y="14" width="7" height="7" />
                </svg>
                Library
              </button>
              {canEdit && (
                <button
                  className={`seg-item ${inDevelop ? "on" : ""}`}
                  onClick={() => {
                    setActiveViewId(null);
                    setDevelop(true);
                  }}
                  disabled={!selected}
                  title="Develop the selected photo (crop & tone)"
                >
                  {/* Sliders glyph. */}
                  <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
                    <line x1="21" y1="6" x2="14" y2="6" />
                    <line x1="10" y1="6" x2="3" y2="6" />
                    <line x1="21" y1="18" x2="12" y2="18" />
                    <line x1="8" y1="18" x2="3" y2="18" />
                    <line x1="14" y1="4" x2="14" y2="8" />
                    <line x1="8" y1="16" x2="8" y2="20" />
                  </svg>
                  Develop
                </button>
              )}
              {moduleViews.map((v) => (
                <button
                  key={v.id}
                  className={`seg-item ${activeViewId === v.id ? "on" : ""}`}
                  onClick={() => {
                    setActiveViewId(v.id);
                    setDevelop(false);
                  }}
                  title={v.label}
                >
                  {v.icon}
                  {v.label}
                </button>
              ))}
            </div>
          </div>
        )}
        <span className="topbar-sep" aria-hidden />
        <div className="loupe-controls">
          <button
            className={`btn-ghost ${loupeInline ? "chip-on" : ""}`}
            onClick={() => setLoupeInline((v) => !v)}
            disabled={!selected || activeView !== null}
            title="Toggle large view (Enter)"
          >
            Loupe
          </button>
          <button
            className="btn-ghost"
            onClick={() => openLoupeWindow()}
            title="Open loupe in a separate window (move it to another screen)"
          >
            Pop out ⧉
          </button>
        </div>
        {pendingCount > 0 && (
          <button
            className="chip chip-on"
            onClick={runReconcile}
            title="Back up photos waiting for the NAS"
          >
            ⤓ Back up ({pendingCount})
          </button>
        )}
        {(identityDebtCount === null || identityDebtCount > 0) && (
          <button
            className="chip chip-on"
            onClick={() => setShowIdentityDebt(true)}
            title={
              identityDebtCount === null
                ? "Identity debt count could not be checked — open to see the current queue"
                : "Photo copies whose sidecar doesn't carry their identity yet. Most of this " +
                  "is normally Unreachable (an offline volume), not a failure."
            }
          >
            Identity debt ({identityDebtCount === null ? "?" : identityDebtCount})
          </button>
        )}
        <span className="topbar-sep" aria-hidden />
        <button
          className="btn-ghost"
          onClick={() => setShowExport(true)}
          disabled={selection.targets.length === 0}
          title="Export the selected photo(s) to files (RAW + XMP, or JPEG)"
        >
          Export
        </button>
        <button
          className="btn-ghost"
          onClick={() => setShowPublish(true)}
          disabled={selection.activeId == null}
          title="Publish the selected photo to Instagram, Flickr, SmugMug…"
        >
          Publish
        </button>
        <button
          className="btn-ghost"
          onClick={runBurstAnalysis}
          disabled={!ready}
          title={
            selection.ids.length
              ? `Rank burst sharpness for ${selection.ids.length} selected photo(s)`
              : "Rank burst sharpness for all visible photos (H16e)"
          }
        >
          Analyse burst
        </button>
        {toolbarActions().map((action) => (
          <button
            key={action.id}
            className="btn-ghost module-action"
            onClick={() =>
              isModalAction(action)
                ? setModalAction(action)
                : activateToolbarAction(action.id)
            }
            title={action.label}
          >
            {action.icon ? `${action.icon} ${action.label}` : action.label}
          </button>
        ))}
        <button
          className="btn-ghost"
          onClick={() => setShowCatalogSwitcher(true)}
          title="Open or create a catalog"
        >
          Catalogs
        </button>
        <button className="btn-ghost" onClick={() => setShowPrefs(true)} title="Preferences (storage, AI, modules)">
          ⚙ Preferences
        </button>
        {importProgress && (
          <div
            className="import-progress"
            title="Importing from card"
            role="progressbar"
            aria-valuenow={importProgress.done}
            aria-valuemax={importProgress.total || undefined}
          >
            <span className="import-progress-label">
              Importing{" "}
              {importProgress.total
                ? `${importProgress.done}/${importProgress.total}`
                : "…"}
            </span>
            <div className="progress-track">
              <div
                className="progress-fill"
                style={{
                  width: importProgress.total
                    ? `${Math.round((importProgress.done / importProgress.total) * 100)}%`
                    : "30%",
                }}
              />
            </div>
          </div>
        )}
        <span className="topbar-sep" aria-hidden />
        <button
          className={`chip panel-toggle ${rightHidden ? "" : "chip-on"}`}
          onClick={() => setRightHidden((v) => !v)}
          title={rightHidden ? "Show inspector panel" : "Hide inspector panel"}
        >
          Inspector ⬛
        </button>
        {scanProgress && (
          <div
            className="import-progress"
            title="Scanning the library / NAS"
            role="progressbar"
            aria-valuenow={scanProgress.done}
            aria-valuemax={scanProgress.total || undefined}
          >
            <span className="import-progress-label">
              {scanProgress.phase === "metadata"
                ? `Reading metadata ${scanProgress.done.toLocaleString()}/${scanProgress.total.toLocaleString()}`
                : scanProgress.phase === "finalizing"
                  ? "Finalizing…"
                  : `Indexing ${scanProgress.done.toLocaleString()}…`}
            </span>
            <div className="progress-track">
              <div
                className="progress-fill"
                style={{
                  width:
                    scanProgress.total > 0
                      ? `${Math.round((scanProgress.done / scanProgress.total) * 100)}%`
                      : "40%",
                }}
              />
            </div>
          </div>
        )}
        {developStatus && (
          <div className="import-progress" title="External develop round-trip">
            <span className="import-progress-label">
              {developStatus.phase === "rendering"
                ? `Rendering ${developStatus.editor}…`
                : `Editing in ${developStatus.editor}…`}
            </span>
            <div className="progress-track">
              <div className="progress-fill" style={{ width: "40%" }} />
            </div>
          </div>
        )}
        <span className="status">{status}</span>
      </header>

      <FilterBar
        filters={FILTERS}
        filter={scope.filter}
        onFilter={library.setFilter}
        activeTagLabel={tags.find((t) => t.id === scope.tagId)?.name ?? null}
        onClearTag={() => library.selectTag(null)}
        activeAlbumId={scope.albumId}
        onClearAlbum={() => library.selectAlbum(null)}
        activeBatchId={scope.batchId}
        onClearBatch={() => library.selectBatch(null)}
        activeFacets={scope.facets}
        onToggleFacet={library.toggleFacet}
        storageTier={scope.storageTier}
        onStorageTier={library.setStorageTier}
        photoSort={scope.sort}
        onPhotoSort={library.setSort}
        activeCamera={scope.camera}
        onCamera={library.setCamera}
        activeLens={scope.lens}
        onLens={library.setLens}
        activeLabels={scope.labels}
        onToggleLabel={library.toggleLabel}
        reloadKey={groupsKey}
      />

      <div
        className="body"
        style={{
          gridTemplateColumns: `${leftHidden ? 0 : leftW}px 1fr ${
            rightHidden ? 0 : rightW
          }px`,
        }}
      >
        {!leftHidden && (
        <div className="leftcol">
          <div
            className="col-resizer col-resizer-right"
            onMouseDown={startResize("left")}
            title="Drag to resize"
          />
          <TagPanel
            tags={tags}
            activeTagId={scope.tagId}
            onSelectTag={library.selectTag}
            onEditTag={setEditingTag}
            onMoveTag={(tagId, newParentId) =>
              moveTag(tagId, newParentId)
                .then(refresh)
                .catch((e) => setStatus(`Move failed: ${e}`))
            }
            onSetPrivate={(tagId, isPrivate, recursive) =>
              setTagPrivate(tagId, isPrivate, recursive)
                .then((n) => {
                  setStatus(
                    `${n} tag${n === 1 ? "" : "s"} marked ${isPrivate ? "private" : "public"}.`,
                  );
                  return refresh();
                })
                .catch((e) => setStatus(`Privacy change failed: ${e}`))
            }
            onTagsChanged={refresh}
          />
          <AlbumsPanel
            activeAlbumId={scope.albumId}
            onSelectAlbum={library.selectAlbum}
            selectionCount={selection.ids.length}
            onAddSelection={addSelectionToAlbum}
            reloadKey={albumsKey}
          />
          <SmartAlbumsPanel
            activeSmartAlbumId={scope.smartAlbumId}
            onSelectSmartAlbum={library.selectSmartAlbum}
            // The panel owns the SmartAlbumEditor modal; this just selects the album being
            // edited so the grid previews its rule.
            onEditRule={(album) => library.selectSmartAlbum(album.id)}
            reloadKey={smartAlbumsKey}
          />
          <BatchesPanel
            activeBatchId={scope.batchId}
            onSelectBatch={library.selectBatch}
            onExportBatch={setBundleExportBatch}
            reloadKey={batchesKey}
          />
        </div>
        )}
        <main className={`grid-wrap ${inDevelop ? "develop-wrap" : ""}`}>
          {inDevelop && selected ? (
            <EditorView
              photoId={selected.id}
              photoW={selected.width}
              photoH={selected.height}
              activeVersionId={activeVersion?.id ?? null}
              onPickVersion={setActiveVersion}
              onSavedActive={(editJson) =>
                setActiveVersion((cur) => (cur ? { ...cur, editJson } : cur))
              }
              onChanged={() => {
                refresh();
              }}
              onBack={() => setDevelop(false)}
            />
          ) : activeView ? (
            <div className="module-view">
              <ModuleContent view={activeView} />
            </div>
          ) : loupeInline && selected ? (
            <div className="loupe-inline">
              <div className="loupe-bar">
                <button className="chip" onClick={() => setLoupeInline(false)}>
                  ‹ Back to grid (Esc)
                </button>
                {selection.extraPhoto && selection.stackOrigin != null && (
                  <button
                    className="chip"
                    title="Return to the original this is stacked under"
                    onClick={library.backToOriginal}
                  >
                    ‹ Back to original
                  </button>
                )}
                <button
                  className="chip"
                  title="Rotate left (non-destructive)"
                  onClick={() => rotateSelected(selected.id, -90)}
                >
                  ↺
                </button>
                <button
                  className="chip"
                  title="Rotate right (non-destructive)"
                  onClick={() => rotateSelected(selected.id, 90)}
                >
                  ↻
                </button>
                <span className="loupe-filename">
                  {selected.path.split("/").pop()}
                  {selected.pickState === "reject" && (
                    <span className="loupe-tag reject"> rejected</span>
                  )}
                  {selected.pickState === "pick" && (
                    <span className="loupe-tag pick"> pick</span>
                  )}
                  {selected.rating > 0 && (
                    <span className="loupe-tag"> {"★".repeat(selected.rating)}</span>
                  )}
                  {selected.sharpness != null && selected.sharpness < softThreshold && (
                    <span
                      className="loupe-tag loupe-soft"
                      title={`Sharpness score ${selected.sharpness.toFixed(1)} is below threshold ${softThreshold} (method: ${selected.sharpnessMethod ?? "tile"})`}
                    >
                      soft
                    </span>
                  )}
                  {selected.burstFlag === "soft-in-burst" && (
                    <span
                      className="loupe-tag loupe-soft"
                      title="Soft in burst — below 60% of cluster median sharpness"
                    >
                      soft-in-burst
                    </span>
                  )}
                  {selected.burstFlag === "sharpest-of-burst" && (
                    <span
                      className="loupe-tag loupe-version"
                      title="Sharpest of burst — best frame in this cluster"
                    >
                      ♛ sharpest of burst
                    </span>
                  )}
                  {activeVersion && (
                    <span className="loupe-tag loupe-version"> · {activeVersion.name}</span>
                  )}
                </span>
                <span className="loupe-hint">
                  scroll zoom · drag pan · dbl-click 100% · P pick · X reject · F faces · ← →
                </span>
              </div>
              {isVideoPath(selected.path) ? (
                <div className="loupe-video-wrap">
                  {/* keyed by id so switching photos reloads the source */}
                  <video
                    key={selected.id}
                    className="loupe-video"
                    src={videoUrl(selected.id)}
                    controls
                    autoPlay
                  />
                </div>
              ) : (
                // Wrap the ZoomableImage in a relative-positioned container so that
                // loupe-slot module panels (e.g. the face overlay) can position
                // themselves absolutely over the image. The wrapper inherits the same
                // flex-1 sizing that .zoom-container already has.
                <div style={{ position: "relative", display: "flex", flexDirection: "column", flex: 1, minHeight: 0 }}>
                  <ZoomableImage
                    photoId={selected.id}
                    bust={thumbBusts.get(selected.id)}
                    srcOverride={editedSrc || undefined}
                    hiSrcOverride={editedSrc ? renderHiVersion : undefined}
                    unavailableActions={
                      <div className="loupe-actions">
                        <button className="chip" onClick={() => relocatePhotoAction(selected.id)}>
                          Relocate…
                        </button>
                        <button className="chip" onClick={() => retrieveFromNasAction(selected.id)}>
                          Retrieve from NAS
                        </button>
                        <button
                          className="chip ctx-item-danger"
                          onClick={() => removeFromCatalogAction(selected.id)}
                        >
                          Remove from catalog
                        </button>
                      </div>
                    }
                  />
                  {/* Loupe-slot panels from enabled modules (e.g. face overlay). Each
                      panel is expected to render an absolute-positioned overlay. */}
                  {panelsForSlot("loupe").map((panel) => (
                    <div key={panel.id} style={{ position: "absolute", inset: 0, pointerEvents: "none" }}>
                      <ModuleContent view={panel} />
                    </div>
                  ))}
                </div>
              )}
            </div>
          ) : (
            <CatalogGrid
              photos={photos}
              selectedId={selection.activeId}
              selectedIds={selection.ids}
              statuses={statuses}
              onVisibleRange={library.setVisibleRange}
              thumbBusts={thumbBusts}
              softThreshold={softThreshold}
              emptyMessage={
                scope.storageTier === "nas"
                  ? "No NAS-only photos yet. Older photos move here when offloaded — set a day count in Preferences → Storage → Local / NAS tiering, or click “Offload older now”."
                  : scope.storageTier === "local" ||
                      scope.filter !== "all" ||
                      scope.tagId != null ||
                      scope.albumId != null ||
                      scope.batchId != null ||
                      scope.smartAlbumId != null ||
                      scope.facets.length > 0 ||
                      scope.labels.length > 0
                    ? "No photos match the current filters."
                    : undefined
              }
              onSelect={(p, mods) => library.select(p.id, mods)}
              onOpen={(p) => {
                library.select(p.id);
                setLoupeInline(true);
              }}
              onContextMenu={(p, e) => {
                library.select(p.id);
                setCtxMenu({ x: e.clientX, y: e.clientY, photoId: p.id });
              }}
            />
          )}
          {!inDevelop && !activeView && !(loupeInline && selected) && library.total > 0 && (
            <div className="grid-statusbar">
              {/* The MATCHING count, which a windowed fetch would no longer equal
                  `photos.length` (issue #10). */}
              <span className="grid-statusbar-count">{library.total.toLocaleString()}</span>
              <span>photos</span>
              {selection.ids.length > 1 && (
                <span className="grid-statusbar-sel">
                  {selection.ids.length.toLocaleString()} selected
                </span>
              )}
            </div>
          )}
        </main>
        {!inDevelop && !rightHidden && (
        <div className="rightcol">
          <div
            className="col-resizer col-resizer-left"
            onMouseDown={startResize("right")}
            title="Drag to resize"
          />
          <PhotoInspector
            photo={selected}
            onChanged={() => {
              refresh();
              refreshPending();
              setGroupsKey((k) => k + 1); // inspector tagging updates "Recently used"
            }}
            allTags={tags}
            status={selected ? statuses.get(selected.id) ?? null : null}
            activeVersionId={activeVersion?.id ?? null}
            onSelectVersion={setActiveVersion}
            canEditVersions={canEdit}
            onEditVersion={(v) => {
              setActiveVersion(v);
              setDevelop(true);
            }}
            clipboardCount={tagClipboard.length}
            selectionCount={selection.ids.length}
            onCopyTags={copyTags}
            onPasteTags={pasteTagsToSelection}
            onAssignTag={assignToSelection}
            onRemoveTag={removeFromSelection}
            onRotate={rotateSelected}
            onViewPhoto={viewPhotoInLoupe}
          />
          <QuickTagBar
            reloadKey={groupsKey}
            selectionCount={selection.ids.length}
            onAssign={assignToSelection}
            onManage={() => setShowGroups(true)}
          />
        </div>
        )}
      </div>

      {editingTag && (
        <TagEditor
          tagId={editingTag.id}
          tagName={editingTag.name}
          tagPath={editingTag.fullPath}
          tagDescription={editingTag.description}
          onClose={() => setEditingTag(null)}
          onChanged={refresh}
        />
      )}

      {showCatalogSwitcher && (
        <CatalogSwitcher
          onClose={() => setShowCatalogSwitcher(false)}
        />
      )}

      {showPrefs && (
        <Preferences
          onClose={() => setShowPrefs(false)}
          onLibraryRootChanged={() => {
            refresh();
            refreshPending();
            // A root change can leave stale/newly-unreachable copies behind — refresh the
            // badge here too, not just at boot/scan/panel-close.
            refreshIdentityDebtCount();
            setStatus("Library folder changed — click Rescan library to index it.");
          }}
        />
      )}

      {showIdentityDebt && (
        <IdentityDebtPanel
          onClose={() => {
            setShowIdentityDebt(false);
            refreshIdentityDebtCount(); // a repair pass may have cleared some debt
          }}
        />
      )}

      {showImport && (
        <ImportPanel
          onClose={() => setShowImport(false)}
          onImport={(source, name, selected) => {
            setShowImport(false);
            void startImport(source, name, selected);
          }}
        />
      )}

      {showExport && (
        <ExportPanel
          photoIds={selection.targets}
          versionId={activeVersion?.id ?? null}
          versionName={activeVersion?.name ?? null}
          activeBatch={scope.batch}
          onExportBatch={(batch) => {
            setShowExport(false);
            setBundleExportBatch(batch);
          }}
          onClose={() => setShowExport(false)}
        />
      )}

      {showPublish && <PublishDialog onClose={() => setShowPublish(false)} />}

      {bundleExportBatch && (
        <BundleExportDialog
          batchId={bundleExportBatch.id}
          batchLabel={
            bundleExportBatch.sourceLabel.replace(/\/+$/, "").split("/").pop() ||
            bundleExportBatch.sourceLabel ||
            "(ingest)"
          }
          onClose={() => setBundleExportBatch(null)}
          onExport={(result) => {
            setStatus(
              `Bundle exported: ${result.exported} original${result.exported === 1 ? "" : "s"}` +
                (result.skippedOffline > 0
                  ? ` (${result.skippedOffline} offline/missing)`
                  : "") +
                (result.errors > 0 ? `, ${result.errors} error(s)` : ""),
            );
          }}
          onClear={() => setImportProgress(null)}
        />
      )}

      {showBundleImport && (
        <BundleImportDialog
          onClose={() => setShowBundleImport(false)}
          onImport={(result) => {
            setStatus(
              result.merge.photosAdded > 0
                ? `Bundle imported: ${result.merge.photosAdded} new photo${result.merge.photosAdded === 1 ? "" : "s"}` +
                    (result.merge.tagsCreated > 0
                      ? `, ${result.merge.tagsCreated} tag${result.merge.tagsCreated === 1 ? "" : "s"} created`
                      : "") +
                    (result.errors > 0 ? `, ${result.errors} error(s)` : "")
                : "Bundle imported — all photos already present (no duplicates added).",
            );
            refresh().catch(() => {});
            refreshPending().catch(() => {});
            // A bundle import can queue new identity debt for extracted copies whose
            // sidecar can't be written immediately (e.g. onto read-only storage) — refresh
            // the badge here too, not just at boot/scan/panel-close.
            refreshIdentityDebtCount().catch(() => {});
            setBatchesKey((k) => k + 1);
          }}
          onClear={() => setImportProgress(null)}
        />
      )}

      {ctxMenu && (() => {
        const id = ctxMenu.photoId;
        const st = statuses.get(id);
        // A backup might exist unless the photo is provably local-only or fully missing.
        // (Status is derived from volume kind, so a deleted-local photo still reads
        // "backedUp" — keep Retrieve enabled for it.)
        const canRetrieve = st !== undefined && st !== "localOnly" && st !== "missing";
        const close = () => setCtxMenu(null);
        return (
          <div
            className="ctx-backdrop"
            onClick={close}
            onContextMenu={(e) => {
              e.preventDefault();
              close();
            }}
          >
            <div
              className="ctx-menu"
              style={{ left: ctxMenu.x, top: ctxMenu.y }}
              onClick={(e) => e.stopPropagation()}
            >
              <div className="ctx-header">
                <span className="ctx-header-name" title={photoName(id)}>{photoName(id)}</span>
                {st && <span className="ctx-header-status">{storageLabel(st)}</span>}
              </div>
              <button
                className="ctx-item"
                onClick={() => {
                  close();
                  revealPhoto(id).catch((e) =>
                    setStatus(`Couldn't reveal: ${e} (the file may be offline)`),
                  );
                }}
              >
                Reveal in Files
              </button>
              <button className="ctx-item" onClick={() => { close(); relocatePhotoAction(id); }}>
                Relocate…
              </button>
              <button
                className="ctx-item"
                disabled={!canRetrieve}
                title={canRetrieve ? undefined : "No NAS backup to retrieve"}
                onClick={() => { close(); retrieveFromNasAction(id); }}
              >
                Retrieve from NAS
              </button>
              <div className="ctx-sep" />
              <button
                className="ctx-item ctx-item-danger"
                onClick={() => { close(); removeFromCatalogAction(id); }}
              >
                Remove from catalog
              </button>
            </div>
          </div>
        );
      })()}

      {modalAction && <ModuleActionModal action={modalAction} close={closeModalAction} />}

      {showGroups && (
        <TagGroupsManager
          onClose={() => {
            setShowGroups(false);
            setGroupsKey((k) => k + 1); // refresh quick-tag bar with any changes
          }}
        />
      )}
    </div>
  );
}
