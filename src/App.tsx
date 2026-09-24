import { noteGridCommit } from "./modules/shellTiming";
import { Profiler, useCallback, useEffect, useRef, useState, useMemo } from "react";
import { confirm } from "@tauri-apps/plugin-dialog";
import {
  analyzeBurstSharpness,
  trashPhotos,
  enqueueOperations,
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
  invalidateListCache,
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
  listTrash,
  listVolumes,
  reconcileNow,
  StorageStatus,
  summarizePendingIdentity,
} from "./modules/api";
import { useLibrarySession } from "./modules/librarySession";
import { CatalogGrid } from "./components/CatalogGrid";
import { Splash, BOOT_STAGES } from "./components/Splash";
import { TagEditor } from "./components/TagEditor";
import { PhotoInspector } from "./components/PhotoInspector";
import { ZoomableImage } from "./components/ZoomableImage";
import { CompareView, MAX_PANES, CompareMode } from "./components/CompareView";
import { advanceDuel, DuelState, duelRound, duelTotalRounds, initialDuel } from "./modules/compareDuel";
import { shellTarget } from "./modules/shellTarget";
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
  toolbarActionGroups,
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
import { StackProposalsDialog } from "./components/StackProposalsDialog";
import { CullSession } from "./components/CullSession";
import { TrashDialog } from "./components/TrashDialog";
import { CatalogSwitcher } from "./components/CatalogSwitcher";
import { DevelopSurface } from "./components/darkroom/DevelopSurface";
import { parseEdit } from "./modules/editing";
import { ImportBatch, listImportBatches, listRecentCatalogs } from "./modules/api";
import {
  TitleBar,
  IconRail,
  Bench,
  CommandPill,
  CollectionBrowser,
  Inspector,
  QuickTagGroups,
  INSPECTOR_TABS,
  railOrder,
  useNarrow,
  type InspectorTab,
} from "./components/shell";
import { useAppearance } from "./theme/controller";
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
  useAppearance();
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
  // Compare: the frames being compared, captured when the view is entered rather than
  // read live from the selection. Culling inside Compare rejects frames, and a rejected
  // frame can drop straight out of the current filter — so a live-derived pane list would
  // rearrange itself underneath the decision that caused it. The ids are held; their rows
  // are looked up fresh each render so badges stay current.
  // The whole selection at the moment Compare opened; shown MAX_PANES at a time from
  // `compareStart`, so a 27-frame selection is seven explicit rounds — never a silent
  // "first four only" (the batch range in the compare bar says exactly where you are).
  const [compareIds, setCompareIds] = useState<number[] | null>(null);
  const [compareStart, setCompareStart] = useState(0);
  const [compareFocusId, setCompareFocusId] = useState<number | null>(null);
  // Duel vs grid presentation for pools larger than one screen. Persisted per machine
  // like the other panel.* prefs; duel is the default — one ←/→ verdict per frame beats
  // scanning batches when the pile is big.
  const [compareMode, setCompareMode] = useState<CompareMode>(() => {
    try {
      return localStorage.getItem("panel.compareMode") === "grid" ? "grid" : "duel";
    } catch {
      return "duel";
    }
  });
  const [duelState, setDuelState] = useState<DuelState>(initialDuel());
  const [cachePreviews, setCachePreviews] = useState(true);

  // Side-panel layout: widths (px) and hidden state, persisted to localStorage.
  const [leftW, setLeftW] = useState(() => +(localStorage.getItem("panel.leftW") || 210));
  const [rightW, setRightW] = useState(() => +(localStorage.getItem("panel.rightW") || 316));
  const [leftHidden, setLeftHidden] = useState(
    () => localStorage.getItem("panel.leftHidden") === "1",
  );
  const [rightHidden, setRightHidden] = useState(
    () => localStorage.getItem("panel.rightHidden") === "1",
  );
  // ≤1024px: the side columns stop fitting next to the grid, so App switches them from
  // persistent grid tracks to transient overlays (see .body's inline style below and the
  // "Shell: narrow overlays" block in App.css). Open/closed state is intentionally NOT
  // persisted — narrow is a transient window shape, not a layout preference, so shrinking
  // the window must never clobber leftHidden/rightHidden above, and every narrow session
  // starts closed.
  const narrow = useNarrow();
  const [overlayLeft, setOverlayLeft] = useState(false);
  const [overlayRight, setOverlayRight] = useState(false);
  useEffect(() => {
    if (!narrow) {
      setOverlayLeft(false);
      setOverlayRight(false);
    }
  }, [narrow]);
  // The `[`/`]` shortcuts and the View menu's checkboxes both call these — narrow, they
  // toggle the transient overlay instead of the persisted panel-hidden state.
  const toggleLeftPanel = useCallback(() => {
    if (narrow) setOverlayLeft((v) => !v);
    else setLeftHidden((v) => !v);
  }, [narrow]);
  const toggleRightPanel = useCallback(() => {
    if (narrow) setOverlayRight((v) => !v);
    else setRightHidden((v) => !v);
  }, [narrow]);
  // The docked inspector's active tab (details / tags / versions / publish), persisted
  // like the panel widths above. Validated against the tab whitelist so a stale or
  // hand-edited localStorage value can never select a tab that doesn't exist.
  const [inspectorTab, setInspectorTab] = useState<InspectorTab>(() => {
    const v = localStorage.getItem("panel.inspectorTab");
    return INSPECTOR_TABS.includes(v as InspectorTab) ? (v as InspectorTab) : "details";
  });
  // Grid tile size (px), driven by the command pill's thumbnail-size slider — same
  // persisted-to-localStorage treatment as the panel widths above. Clamped to the slider's
  // own [120, 320] range so a hand-edited or stale localStorage value can't hand CatalogGrid
  // an out-of-range tileMin.
  const [thumbSize, setThumbSize] = useState(() => {
    const raw = +(localStorage.getItem("panel.thumbSize") || 160);
    return Math.min(320, Math.max(120, Number.isFinite(raw) ? raw : 160));
  });
  useEffect(() => {
    localStorage.setItem("panel.leftW", String(leftW));
    localStorage.setItem("panel.rightW", String(rightW));
    localStorage.setItem("panel.leftHidden", leftHidden ? "1" : "0");
    localStorage.setItem("panel.rightHidden", rightHidden ? "1" : "0");
    localStorage.setItem("panel.thumbSize", String(thumbSize));
    localStorage.setItem("panel.inspectorTab", inspectorTab);
  }, [leftW, rightW, leftHidden, rightHidden, thumbSize, inspectorTab]);

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
  // The open catalog's display name (basename of its .chairphoto path) — the title bar's
  // catalog pill. Recorded catalogs are ordered most-recently-opened first, and both
  // init_catalog and switch_catalog record the catalog they open before returning, so
  // index 0 is always the one now open.
  const [catalogName, setCatalogName] = useState("");
  const refreshCatalogName = useCallback(() => {
    listRecentCatalogs()
      .then((catalogs) => {
        const path = catalogs[0]?.catalogPath;
        if (path) setCatalogName(path.split("/").pop() || path);
      })
      .catch(() => {});
  }, []);
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
  // Trash count for the collection browser's Library section (CollectionBrowser.tsx).
  // `null` means "unknown" (not yet fetched, or the last fetch failed) — same reasoning
  // as identityDebtCount above: hides the badge rather than showing a stale/wrong number.
  // No dedicated backend count command exists (only `list_trash`, which returns full
  // Photo rows), so this reuses `listTrash().length` — the same "fetch the list, count
  // it" shape `checkReconcile` already uses for `listVolumes`/`listPendingOperations`
  // below — refreshed on the same cycles as `pendingCount`.
  const [trashCount, setTrashCount] = useState<number | null>(null);
  // Bundle export dialog state — set to the batch to export, null = closed.
  const [bundleExportBatch, setBundleExportBatch] = useState<ImportBatch | null>(null);
  // Bundle import dialog open/closed.
  const [showBundleImport, setShowBundleImport] = useState(false);
  // Import batches, for the title bar's "Export a bundle" submenu (TitleBar owns none of
  // this fetch — BatchesPanel/CommandPill/SmartAlbumEditor each keep their own copy the same
  // way, reloaded on the same `batchesKey` bump).
  const [importBatches, setImportBatches] = useState<ImportBatch[]>([]);
  useEffect(() => {
    if (!ready) return;
    listImportBatches().then(setImportBatches).catch(() => {});
  }, [ready, batchesKey]);
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
  // Auto-stack proposals (C3). Holds the photo ids the pass examines, snapshotted when the
  // dialog opens: the grid can refresh under it as groups are accepted, and re-proposing
  // over a moving set would renumber the groups being reviewed.
  const [stackTargets, setStackTargets] = useState<number[] | null>(null);
  // Cull session (C4). Holds the rows to cull, frozen at the start: culling changes the
  // fields the view is filtered by, so a live list would delete photos out from under the
  // cursor and skip frames unseen.
  const [cullPhotos, setCullPhotos] = useState<Photo[] | null>(null);
  const inCull = cullPhotos !== null && cullPhotos.length > 0;
  const closeModalAction = useCallback(() => setModalAction(null), []);
  // Right-click context menu on a grid tile.
  const [ctxMenu, setCtxMenu] = useState<{ x: number; y: number; photoId: number } | null>(null);
  const [showTrash, setShowTrash] = useState(false);

  // Full-surface views contributed by modules (e.g. Map). The active one, if its
  // module is still enabled — otherwise we fall back to the built-in Library view.
  const moduleViews = mainViews();
  const activeView = moduleViews.find((v) => v.id === activeViewId) ?? null;
  // The Basic Editor module gates the Develop view and per-version editing.
  const canEdit = activeEditRenderer() !== null;
  const inDevelop = develop && selection.activeId != null && canEdit;

  // The compared rows, resolved from the held ids against the current result. Rows are
  // looked up per render so a rating applied inside Compare shows up on its own pane; the
  // id list itself is frozen (see `compareIds`). A row that has vanished entirely — the
  // catalog switched, the photo was purged — is dropped rather than rendered blank.
  const compareBatchIds = !compareIds
    ? []
    : compareMode === "duel"
      ? duelState.done
        ? [compareIds[duelState.championIdx]]
        : [compareIds[duelState.championIdx], compareIds[duelState.challengerIdx]]
      : compareIds.slice(compareStart, compareStart + MAX_PANES);
  const comparePhotos = compareBatchIds
    .map((id) => photos.find((p) => p.id === id) ?? null)
    .filter((p): p is Photo => p !== null);
  const inCompare = compareIds !== null && comparePhotos.length > 0;
  const duelChampionId =
    compareIds && compareMode === "duel" ? compareIds[duelState.championIdx] ?? null : null;

  // Enter Compare on the current selection. Needs two frames to mean anything; the whole
  // selection becomes the pool, paged MAX_PANES at a time (more per screen would make each
  // pane too small to judge). Opens on the batch holding the active photo, so the view
  // starts on the frame the user was already looking at.
  const canCompare = selection.ids.length >= 2;
  const openCompare = useCallback(() => {
    const ids = selection.ids;
    if (ids.length < 2) return;
    const activeAt = selection.activeId != null ? ids.indexOf(selection.activeId) : -1;
    const start = activeAt >= 0 ? Math.floor(activeAt / MAX_PANES) * MAX_PANES : 0;
    setCompareIds(ids);
    setCompareStart(start);
    // A duel always starts at the top of the pool (round 1 of N-1); focus opens on the
    // challenger — the frame being judged against the standing champion.
    setDuelState(initialDuel());
    setCompareFocusId(
      compareMode === "duel" ? ids[1] : activeAt >= 0 ? ids[activeAt] : ids[start],
    );
    setLoupeInline(false);
  }, [selection.ids, selection.activeId, compareMode]);

  // Swap presentation mid-compare: the duel restarts from the top of the pool (its
  // judged/unjudged bookkeeping has no meaning across modes), the grid keeps its page.
  const switchCompareMode = useCallback(
    (m: CompareMode) => {
      setCompareMode(m);
      try {
        localStorage.setItem("panel.compareMode", m);
      } catch {
        // Private-mode storage failures lose only the preference, never the feature.
      }
      if (compareIds) {
        setDuelState(initialDuel());
        setCompareFocusId(m === "duel" ? compareIds[1] ?? compareIds[0] : compareIds[compareStart]);
      }
    },
    [compareIds, compareStart],
  );


  // Step one batch forward/back, clamped at the pool's ends; focus lands on the new
  // batch's first frame so the culling keys always have a target.
  const pageCompare = useCallback(
    (dir: -1 | 1) => {
      if (!compareIds) return;
      const next = compareStart + dir * MAX_PANES;
      if (next < 0 || next >= compareIds.length) return;
      setCompareStart(next);
      setCompareFocusId(compareIds[next]);
    },
    [compareIds, compareStart],
  );

  const closeCompare = useCallback(() => {
    setCompareIds(null);
    setCompareStart(0);
    setDuelState(initialDuel());
    setCompareFocusId(null);
  }, []);

  // Compare's focused pane, when it has one — the second "current photo" that the
  // inspector and pop-out have to be told about. See modules/shellTarget.ts.
  const focusedComparePhoto =
    inCompare && compareFocusId != null
      ? comparePhotos.find((p) => p.id === compareFocusId) ?? null
      : null;

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

  // Resolve the selection and Compare's focus into the one photo the inspector and any
  // pop-out loupe follow, plus the edit that may legitimately ride with it.
  const {
    photo: shellPhoto,
    broadcastId: loupeBroadcastId,
    editJson: loupeBroadcastEdit,
  } = shellTarget({
    selected,
    activeId: selection.activeId,
    compareFocus: focusedComparePhoto,
    activeEditJson: loupeEditJson,
  });

  // Keep any open pop-out loupe window in sync, and re-send when a loupe window
  // announces it just opened.
  // Also preload neighbours so navigation is instant (the AGENTS.md preload
  // invariant). We prefetch further ahead than behind, since culling moves forward:
  // the next 5 photos and the previous 2. Preloading stays keyed on the *selection*: it
  // exists for stepping through the grid, and the compared frames are already on screen.
  // The Darkroom's RAW preload targets (docs/plans/raw-foundation, slice 4): the photos
  // either side in the current order, next first. Kept referentially stable while the
  // selection and the list are unchanged.
  const developNeighbours = useMemo(() => {
    if (selection.activeId == null) return [];
    const idx = photos.findIndex((p) => p.id === selection.activeId);
    if (idx === -1) return [];
    return [photos[idx + 1]?.id, photos[idx - 1]?.id].filter((id): id is number => id != null);
  }, [selection.activeId, photos]);

  useEffect(() => {
    broadcastPhoto(loupeBroadcastId, loupeBroadcastEdit);
    if (selection.activeId == null) return;
    const idx = photos.findIndex((p) => p.id === selection.activeId);
    if (idx === -1) return;
    for (let d = 1; d <= 5; d++) prefetch(photos[idx + d]?.id);
    prefetch(photos[idx - 1]?.id);
    prefetch(photos[idx - 2]?.id);
  }, [selection.activeId, loupeBroadcastId, loupeBroadcastEdit, photos]);

  useEffect(() => {
    const unlisten = onLoupeReady(() =>
      broadcastPhoto(loupeBroadcastId, loupeBroadcastEdit),
    );
    return () => {
      unlisten.then((f) => f());
    };
  }, [loupeBroadcastId, loupeBroadcastEdit]);

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
        refreshCatalogName(); // now that init_catalog has recorded it as the most recent
        // The sidebar panels (albums / smart albums / batches) and the command pill fetch
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
    // A refresh means "something changed": drop the cached lists before re-reading them.
    // Module mutations arrive here through the change sink and do not pass through the
    // core invoke, so this is their invalidation.
    invalidateListCache();
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

  const refreshTrashCount = useCallback(async () => {
    try {
      const list = await listTrash();
      setTrashCount(list.length);
    } catch {
      // Same reasoning as refreshIdentityDebtCount: leave it as it was rather than
      // masquerading a transient IPC error as a confirmed 0.
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
    refreshTrashCount();
    const onFocus = () => {
      checkReconcile();
      refreshTrashCount();
    };
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, [ready, checkReconcile, refreshPending, refreshIdentityDebtCount, refreshTrashCount]);

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

  // Batch back-up (cluster B, B1/D6): the second half of the safety panel's "show me".
  // The panel filters the grid to a bucket; this queues what you then select, through the
  // same pending-operations queue the per-photo action uses — so the topbar badge and the
  // reconcile drain report it, and an offline NAS defers rather than fails.
  const backUpSelection = async () => {
    // Requires an explicit selection, unlike the analyse/propose actions that fall back to
    // the whole view. Those read; this one commits the library to copying every byte it
    // names — on the owner's catalog an empty-selection fallback would silently queue
    // 165,093 photos. It is additive and safe, and still not a thing to start by accident.
    const targets = selection.ids;
    if (targets.length === 0) {
      setStatus("Select the photos to back up first — Back up does not act on the whole view.");
      return;
    }
    try {
      const queued = await enqueueOperations("backup", targets);
      const already = targets.length - queued;
      setStatus(
        `Queued ${queued} for backup` +
          (already ? ` (${already} already waiting)` : "") +
          ". They copy when the NAS is reachable.",
      );
      await refreshPending();
      runReconcile();
    } catch (e) {
      setStatus(`Could not queue backup: ${e}`);
    }
  };

  // Auto-stack proposals (C3): same scoping as burst analysis — the selection, else the
  // whole view. Opening only *proposes*; each group is accepted individually in the dialog.
  const openStackProposals = () => {
    const targets = selection.ids.length ? selection.ids : photos.map((p) => p.id);
    if (targets.length === 0) {
      setStatus("No photos to group — scan or select some first.");
      return;
    }
    setStackTargets(targets);
  };

  // Cull session (C4): the selection, else the whole view — the same scoping as the other
  // two culling actions. Opening freezes the rows; the grid is refreshed once, on exit.
  const startCullSession = () => {
    const rows = selection.ids.length
      ? photos.filter((p) => selection.ids.includes(p.id))
      : photos;
    if (rows.length === 0) {
      setStatus("Nothing to cull — scan or select some photos first.");
      return;
    }
    setCullPhotos(rows);
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
      invalidateListCache(); // the lists belong to the catalog that just closed
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
      // Trash is per-catalog too — same reasoning as identityDebtCount just above.
      setTrashCount(null);
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
      // Same reasoning again: per-catalog, closes over no filter/tag state.
      refreshTrashCount();
      // Same reasoning: switch_catalog has already recorded the new catalog as the most
      // recent by the time this event fires, so re-reading the list gets the new name.
      refreshCatalogName();
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, [library.reset, refreshIdentityDebtCount, refreshTrashCount, refreshCatalogName]);

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

  // Resolve a comparison: the keeper becomes a pick, every other compared frame a reject.
  // Reversible (U clears a pick state) and it touches no bytes — reject is a filterable
  // metadata state, not deletion. Compare stays open on the result so the outcome is
  // visible and can be undone in place rather than being an unseen side effect.
  // One duel verdict: reject the loser, advance the challenger (a right-side win moves it
  // to the champion slot), and on the final round pick the tournament's winner.
  const duelVerdict = useCallback(
    async (winner: "left" | "right") => {
      if (!compareIds) return;
      const result = advanceDuel(compareIds, duelState, winner);
      if (!result) return;
      await setPickState(result.loserId, "reject");
      if (result.winnerId != null) await setPickState(result.winnerId, "pick");
      setDuelState(result.next);
      setCompareFocusId(
        result.next.done
          ? compareIds[result.next.championIdx] ?? null
          : compareIds[result.next.challengerIdx] ?? null,
      );
      await refresh();
    },
    [compareIds, duelState, refresh],
  );

  const keepInCompare = useCallback(
    async (keeperId: number) => {
      // Batch-scoped: the keeper's rivals are the frames on screen, not the whole pool —
      // with a 27-frame pool, "keep" must never silently reject 26 photos. After the
      // round, advance to the next batch so a big selection flows as K, K, K…
      if (compareMode === "duel") {
        // In a duel a "keep" is a verdict for that pane's side.
        await duelVerdict(keeperId === duelChampionId ? "left" : "right");
        return;
      }
      const batch = (compareIds ?? []).slice(compareStart, compareStart + MAX_PANES);
      if (!batch.includes(keeperId)) return;
      await setPickState(keeperId, "pick");
      for (const id of batch) {
        if (id !== keeperId) await setPickState(id, "reject");
      }
      setCompareFocusId(keeperId);
      await refresh();
      pageCompare(1);
    },
    [compareIds, compareStart, refresh, pageCompare, compareMode, duelVerdict, duelChampionId],
  );

  // The ONE write path for culling marks (rate / pick / label): apply the verb to every
  // targeted photo, then refresh. Shared by the keyboard culling shortcuts and the bench's
  // marking controls, so the two surfaces cannot drift apart (pinned by the "one code
  // path" block in components/shell/__tests__/commandInventory.test.tsx). Auto-advance is
  // deliberately NOT in here: it is the keyboard branch's own behavior — pressing a number
  // key steps to the next photo, clicking a bench star must not move the selection.
  const applyToSelection = useCallback(
    async (fn: (id: number) => Promise<unknown>) => {
      for (const id of selection.targets) await fn(id);
      await refresh();
    },
    [selection.targets, refresh],
  );

  // The bench's marking clicks. Outside Compare they are applyToSelection, verbatim.
  // Inside Compare they drive the focused pane — the same photo whose marks the bench is
  // showing (`shellPhoto`, via shellTarget) — mirroring Compare's keyboard branch: mark
  // the one pane and refresh, never the whole selection at once.
  const applyMark = useCallback(
    async (fn: (id: number) => Promise<unknown>) => {
      if (inCompare) {
        if (shellPhoto) {
          await fn(shellPhoto.id);
          await refresh();
        }
        return;
      }
      await applyToSelection(fn);
    },
    [inCompare, shellPhoto, applyToSelection, refresh],
  );

  // Keyboard culling. Active whenever a photo is selected and focus isn't in an input.
  useEffect(() => {
    const handler = async (e: KeyboardEvent) => {
      if (activeView || inDevelop || inCull) return; // a full-surface view owns input
      const target = e.target as HTMLElement;
      if (target.tagName === "INPUT" || target.tagName === "TEXTAREA") return;

      const key = e.key.toLowerCase();

      // Panel visibility: `[` the tags/collections panel, `]` the inspector. The same
      // toggles as More ⋯ → View, which stay; these work in Compare too — the panels
      // frame every Library surface. Narrow, both remap to the transient overlay instead
      // of the persisted hidden state (see toggleLeftPanel/toggleRightPanel above).
      if (key === "[") {
        toggleLeftPanel();
        e.preventDefault();
        return;
      }
      if (key === "]") {
        toggleRightPanel();
        e.preventDefault();
        return;
      }

      // --- Compare owns the keyboard while it is open --------------------------
      // Deliberately ahead of every grid shortcut: culling keys must act on the FOCUSED
      // PANE, not on the selection. Falling through to the grid handler would rate all
      // the compared frames at once, which is the opposite of choosing between them.
      if (inCompare) {
        const ids = comparePhotos.map((p) => p.id);
        const at = compareFocusId != null ? ids.indexOf(compareFocusId) : -1;
        const focused = at >= 0 ? ids[at] : ids[0];
        if (e.key === "Escape" || key === "c") {
          closeCompare();
        } else if (compareMode === "duel" && e.key === "ArrowRight") {
          // Duel verdicts: the arrows name the winning side, not a focus move.
          await duelVerdict("right");
        } else if (compareMode === "duel" && e.key === "ArrowLeft") {
          await duelVerdict("left");
        } else if (e.key === "PageDown") {
          if (compareMode === "grid") pageCompare(1);
        } else if (e.key === "PageUp") {
          if (compareMode === "grid") pageCompare(-1);
        } else if (e.key === "ArrowRight" || e.key === "ArrowDown") {
          setCompareFocusId(ids[(Math.max(at, 0) + 1) % ids.length]);
        } else if (e.key === "ArrowLeft" || e.key === "ArrowUp") {
          setCompareFocusId(ids[(Math.max(at, 0) - 1 + ids.length) % ids.length]);
        } else if (key === "k") {
          await keepInCompare(focused);
        } else if (key >= "0" && key <= "5") {
          await setRating(focused, parseInt(key, 10));
          await refresh();
        } else if (key === "p" || key === "x" || key === "u") {
          await setPickState(focused, key === "p" ? "pick" : key === "x" ? "reject" : "none");
          await refresh();
        } else if (key in COLOR_KEYS) {
          await setLabel(focused, COLOR_KEYS[key]);
          await refresh();
        } else {
          return;
        }
        e.preventDefault();
        return;
      }

      // Enter Compare from the grid. Needs two or more selected frames.
      if (key === "c" && canCompare) {
        openCompare();
        e.preventDefault();
        return;
      }

      // Ctrl/Cmd+A: select every photo in the current view (works with nothing selected yet).
      if ((e.ctrlKey || e.metaKey) && key === "a") {
        library.selectAll();
        e.preventDefault();
        return;
      }

      if (!selected) return; // the shortcuts below act on the active photo
      // Culling applies to the whole selection (batch) — through the same
      // applyToSelection write path the bench's marking controls use — and advances only
      // if culling one. The advance is the keyboard's own, over the rows this handler was
      // created with: not the ones the refresh inside applyToSelection just produced,
      // from which the photo that was just rated may have dropped out of the current
      // filter (`stepActive` closes over this render's rows).
      const targets = selection.targets;
      const applyAll = async (fn: (id: number) => Promise<unknown>) => {
        await applyToSelection(fn);
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
    applyToSelection,
    library.selectAll,
    library.stepActive,
    refresh,
    activeView,
    inDevelop,
    // Pre-existing gap surfaced in review: the first guard line reads `inCull`, but the
    // dep list never named it — so opening a cull session left the stale closure (inCull
    // captured false) attached, and the grid handler kept firing behind the session:
    // wrong-target marks against the pre-session selection, a full refresh per keystroke,
    // and a silent stepActive on the grid cursor. CullSession owns the keyboard while
    // open; this dependency is what makes the guard actually stand down.
    inCull,
    toggleLeftPanel,
    toggleRightPanel,
    // Compare's branch reads all of these; without them the listener would keep acting on
    // the pane list and focus it was created with — rating the wrong frame after a step.
    inCompare,
    comparePhotos,
    compareFocusId,
    canCompare,
    openCompare,
    closeCompare,
    keepInCompare,
    pageCompare,
    compareMode,
    duelVerdict,
  ]);

  // The bench's single progress readout, folded from the three title-bar renderers this
  // redesign removed — same labels, verbatim; priority import > scan > develop.
  // `total: null` = indeterminate (the bar shows a fixed partial fill).
  const benchProgress = importProgress
    ? {
        label: importProgress.total
          ? `Importing ${importProgress.done}/${importProgress.total}`
          : "Importing …",
        done: importProgress.done,
        total: importProgress.total || null,
      }
    : scanProgress
      ? {
          label:
            scanProgress.phase === "metadata"
              ? `Reading metadata ${scanProgress.done.toLocaleString()}/${scanProgress.total.toLocaleString()}`
              : scanProgress.phase === "finalizing"
                ? "Finalizing…"
                : `Indexing ${scanProgress.done.toLocaleString()}…`,
          done: scanProgress.done,
          total: scanProgress.total > 0 ? scanProgress.total : null,
        }
      : developStatus
        ? {
            label:
              developStatus.phase === "rendering"
                ? `Rendering ${developStatus.editor}…`
                : `Editing in ${developStatus.editor}…`,
            done: 0,
            total: null,
          }
        : null;

  // Hoisted so the narrow-overlay and desktop-column renders of .leftcol (below) can
  // share one element instead of two copies of the same prop block.
  const collectionBrowserPanel = (
    <CollectionBrowser
      isAllScope={
        scope.tagId == null &&
        scope.albumId == null &&
        scope.batchId == null &&
        scope.smartAlbumId == null
      }
      onSelectAll={library.clearScope}
      trashCount={trashCount}
      onOpenTrash={() => setShowTrash(true)}
      tagPanel={{
        tags,
        activeTagId: scope.tagId,
        onSelectTag: library.selectTag,
        onEditTag: setEditingTag,
        onMoveTag: (tagId, newParentId) =>
          moveTag(tagId, newParentId)
            .then(refresh)
            .catch((e) => setStatus(`Move failed: ${e}`)),
        onSetPrivate: (tagId, isPrivate, recursive) =>
          setTagPrivate(tagId, isPrivate, recursive)
            .then((n) => {
              setStatus(
                `${n} tag${n === 1 ? "" : "s"} marked ${isPrivate ? "private" : "public"}.`,
              );
              return refresh();
            })
            .catch((e) => setStatus(`Privacy change failed: ${e}`)),
        onTagsChanged: refresh,
        selectedPhotoIds: selection.ids,
        onStatus: setStatus,
      }}
      albumsPanel={{
        activeAlbumId: scope.albumId,
        onSelectAlbum: library.selectAlbum,
        selectionCount: selection.ids.length,
        onAddSelection: addSelectionToAlbum,
        reloadKey: albumsKey,
      }}
      smartAlbumsPanel={{
        activeSmartAlbumId: scope.smartAlbumId,
        onSelectSmartAlbum: library.selectSmartAlbum,
        // The panel owns the SmartAlbumEditor modal; this just selects the album
        // being edited so the grid previews its rule.
        onEditRule: (album) => library.selectSmartAlbum(album.id),
        reloadKey: smartAlbumsKey,
      }}
      batchesPanel={{
        activeBatchId: scope.batchId,
        onSelectBatch: library.selectBatch,
        onExportBatch: setBundleExportBatch,
        reloadKey: batchesKey,
      }}
    />
  );

  // Hoisted for the same reason as collectionBrowserPanel above — one element shared by
  // .rightcol's narrow-overlay and desktop-column renders. `onHide` closes the transient
  // overlay when narrow, rather than setting the persisted rightHidden a desktop session
  // would otherwise lose.
  const inspectorPanel = (
    <Inspector
      tab={inspectorTab}
      onTab={setInspectorTab}
      onHide={() => (narrow ? setOverlayRight(false) : setRightHidden(true))}
      photo={shellPhoto}
      quickTags={
        <QuickTagGroups
          reloadKey={groupsKey}
          selectionCount={selection.ids.length}
          onAssign={assignToSelection}
          onManage={() => setShowGroups(true)}
        />
      }
    >
      <PhotoInspector
        tab={inspectorTab}
        photo={shellPhoto}
        onChanged={() => {
          refresh();
          refreshPending();
          setGroupsKey((k) => k + 1); // inspector tagging updates "Recently used"
        }}
        allTags={tags}
        status={shellPhoto ? statuses.get(shellPhoto.id) ?? null : null}
        // `activeVersion` belongs to the selected photo; while Compare is showing a
        // different frame there is no active version to speak of, and passing one
        // would attribute another photo's edit to this one.
        activeVersionId={shellPhoto?.id === selection.activeId ? activeVersion?.id ?? null : null}
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
        // Same gate the Bench's Publish button uses: an active grid selection.
        onPublish={selection.activeId != null ? () => setShowPublish(true) : undefined}
      />
    </Inspector>
  );

  return (
    <div className="app">
      <Splash stage={bootStage} />
      <TitleBar
        catalogName={catalogName}
        photoCount={library.total}
        onOpenCatalogs={() => setShowCatalogSwitcher(true)}
        pendingCount={pendingCount}
        onReconcile={runReconcile}
        identityDebtCount={identityDebtCount}
        onOpenIdentityDebt={() => setShowIdentityDebt(true)}
        ready={ready}
        onImportCard={() => setShowImport(true)}
        onImportBundle={() => setShowBundleImport(true)}
        onRescan={onScan}
        cachePreviews={cachePreviews}
        onCachePreviews={setCachePreviews}
        exportableBatches={importBatches}
        onExportBundle={setBundleExportBatch}
        canExport={selection.targets.length > 0}
        onExport={() => setShowExport(true)}
        onPopOutLoupe={() => openLoupeWindow()}
        loupeOn={loupeInline}
        loupeEnabled={!!selected && activeView === null}
        onToggleLoupe={() => setLoupeInline((v) => !v)}
        selectionCount={selection.ids.length}
        onAnalyseBurst={runBurstAnalysis}
        onProposeStacks={openStackProposals}
        onCullSession={startCullSession}
        moduleActionGroups={toolbarActionGroups()}
        onModuleAction={(action) =>
          isModalAction(action) ? setModalAction(action) : activateToolbarAction(action.id)
        }
        onOpenPrefs={() => setShowPrefs(true)}
        // Narrow, the View menu's checkboxes track the transient overlay instead of the
        // persisted hidden state — negated because TitleBar always renders `checked={!x}`.
        leftHidden={narrow ? !overlayLeft : leftHidden}
        onToggleLeft={toggleLeftPanel}
        rightHidden={narrow ? !overlayRight : rightHidden}
        onToggleRight={toggleRightPanel}
      />

      <div
        className="body"
        style={{
          // Narrow: both side tracks collapse to 0 unconditionally — leftHidden/
          // rightHidden stop driving the grid template at all, so the desktop prefs
          // they hold survive a narrow session untouched (the panels themselves move to
          // overlays below; see the leftcol/rightcol rendering and App.css's "Shell:
          // narrow overlays" block).
          gridTemplateColumns: narrow
            ? `52px 0 1fr 0`
            : `52px ${leftHidden ? 0 : leftW}px 1fr ${rightHidden ? 0 : rightW}px`,
        }}
      >
        <IconRail
          active={activeView?.id ?? (inDevelop ? "develop" : "library")}
          canDevelop={canEdit}
          developEnabled={!!selected}
          moduleViews={railOrder(moduleViews)}
          onSelect={(id) => {
            if (id === "library") {
              setActiveViewId(null);
              setDevelop(false);
            } else if (id === "develop") {
              setActiveViewId(null);
              setDevelop(true);
            } else {
              setActiveViewId(id);
              setDevelop(false);
            }
          }}
          onOpenPrefs={() => setShowPrefs(true)}
        />
        {narrow ? (
          overlayLeft && (
            <div className="overlay-scrim" onClick={() => setOverlayLeft(false)}>
              <div className="leftcol overlay" onClick={(e) => e.stopPropagation()}>
                {collectionBrowserPanel}
              </div>
            </div>
          )
        ) : (
          !leftHidden && (
            <div className="leftcol">
              <div
                className="col-resizer col-resizer-right"
                onMouseDown={startResize("left")}
                title="Drag to resize"
              />
              {collectionBrowserPanel}
            </div>
          )
        )}
        <main className={`grid-wrap ${inDevelop ? "develop-wrap" : ""}`}>
          {/* Only the grid and the inline loupe have anything to filter/sort/size — hidden
              in Develop, module views and Compare, which is exactly the rest of the stage
              ternary's branches (mirrors it rather than tracking its own state). Docked as
              its own row above the stage so it never obstructs the top row of tiles. */}
          {!(inDevelop && selected) && !activeView && !inCompare && (
          <CommandPill
              filters={FILTERS}
              filter={scope.filter}
              onFilter={library.setFilter}
              activeTagLabel={tags.find((t) => t.id === scope.tagId)?.name ?? null}
              onClearTag={() => library.selectTag(null)}
              activeAlbumId={scope.albumId}
              onClearAlbum={() => library.selectAlbum(null)}
              activeSmartAlbumId={scope.smartAlbumId}
              onClearSmartAlbum={() => library.selectSmartAlbum(null)}
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
              thumbSize={thumbSize}
              onThumbSize={setThumbSize}
            />
          )}
          <div className="stage">
            {inDevelop && selected ? (
              <DevelopSurface
                photoId={selected.id}
                neighbours={developNeighbours}
                photoW={selected.width}
                photoH={selected.height}
                activeVersionId={activeVersion?.id ?? null}
                activeEditJson={activeVersion?.editJson ?? null}
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
            ) : inCompare ? (
              <CompareView
                photos={comparePhotos}
                focusedId={compareFocusId}
                softThreshold={softThreshold}
                poolTotal={compareIds?.length ?? comparePhotos.length}
                poolOffset={compareStart}
                onPage={pageCompare}
                mode={compareMode}
                onMode={switchCompareMode}
                duel={
                  compareMode === "duel"
                    ? {
                        championId: duelChampionId,
                        round: duelRound(duelState),
                        totalRounds: duelTotalRounds(compareIds?.length ?? 0),
                        done: duelState.done,
                      }
                    : undefined
                }
                onFocus={setCompareFocusId}
                onKeep={keepInCompare}
                onExit={closeCompare}
              />
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
                        title="Soft in burst — dimmer than the rest of its cluster. The inspector's Culling signals section shows the cluster, the median and the exact cutoff."
                      >
                        soft-in-burst
                      </span>
                    )}
                    {selected.burstFlag === "sharpest-of-burst" && (
                      <span
                        className="loupe-tag loupe-version"
                        title="Sharpest of burst — the highest-scoring frame in its cluster. The inspector's Culling signals section shows the cluster and the scores."
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
              <Profiler id="grid" onRender={(_id, phase, actual) => noteGridCommit(phase, actual)}>
              <CatalogGrid
                photos={photos}
                selectedId={selection.activeId}
                selectedIds={selection.ids}
                statuses={statuses}
                onVisibleRange={library.setVisibleRange}
                thumbBusts={thumbBusts}
                softThreshold={softThreshold}
                tileMin={thumbSize}
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
              </Profiler>
            )}
          </div>
          {!inDevelop && !activeView && (
            <Bench
              progress={benchProgress}
              status={status}
              total={library.total}
              selectedCount={selection.ids.length}
              active={shellPhoto}
              onRate={(n) => void applyMark((id) => setRating(id, n))}
              onPick={(s) => void applyMark((id) => setPickState(id, s))}
              onLabel={(name) => void applyMark((id) => setLabel(id, name))}
              selectionThumbs={selection.photos.slice(0, 3)}
              thumbBusts={thumbBusts}
              canCompare={canCompare}
              compareOn={inCompare}
              onCompare={() => (inCompare ? closeCompare() : openCompare())}
              onStack={openStackProposals}
              onCull={startCullSession}
              canExport={selection.targets.length > 0}
              onExport={() => setShowExport(true)}
              canPublish={selection.activeId != null}
              onPublish={() => setShowPublish(true)}
              onBackUpSelection={backUpSelection}
              canBackUpSelection={ready && selection.ids.length > 0}
              onAnalyseBurst={runBurstAnalysis}
              ready={ready}
              onClearSelection={library.clearSelection}
            />
          )}
        </main>
        {!inDevelop &&
          (narrow ? (
            overlayRight && (
              <div className="overlay-scrim" onClick={() => setOverlayRight(false)}>
                <div className="rightcol overlay" onClick={(e) => e.stopPropagation()}>
                  {inspectorPanel}
                </div>
              </div>
            )
          ) : (
            !rightHidden && (
              <div className="rightcol">
                <div
                  className="col-resizer col-resizer-left"
                  onMouseDown={startResize("right")}
                  title="Drag to resize"
                />
                {inspectorPanel}
              </div>
            )
          ))}
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
          onShowStorageTier={(tier) => {
            // Filter first, then close: the panel's whole promise is that the number it
            // showed you and the photos you land on are the same set.
            library.setStorageTier(tier);
            setShowPrefs(false);
          }}
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

      {showTrash && (
        <TrashDialog
          onClose={() => setShowTrash(false)}
          onChanged={() => {
            refresh();
            refreshPending();
            refreshTrashCount();
          }}
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
                title="Hide it everywhere, reversibly. Nothing is deleted and nothing is written to disk."
                onClick={() => {
                  close();
                  // The selection if this photo is part of one, else just this photo —
                  // right-clicking a tile outside the selection is about that tile.
                  const ids = selection.ids.includes(id) ? selection.ids : [id];
                  trashPhotos(ids)
                    .then((s) => {
                      const extra = s.cascaded ? ` (+${s.cascaded} stacked)` : "";
                      setStatus(`Moved ${s.trashed} to the trash${extra}.`);
                      refresh();
                    })
                    .catch((e) => setStatus(`Could not trash: ${e}`));
                }}
              >
                Move to trash
              </button>
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

      {inCull && cullPhotos && (
        <CullSession
          photos={cullPhotos}
          onExit={(stats) => {
            setCullPhotos(null);
            setStatus(
              `Cull session: ${stats.visited} reviewed, ${stats.picked} picked, ` +
                `${stats.rejected} rejected, ${stats.remaining} left.`,
            );
            refresh();
          }}
        />
      )}
      {stackTargets && (
        <StackProposalsDialog
          photoIds={stackTargets}
          onClose={() => setStackTargets(null)}
          onApplied={refresh}
        />
      )}
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
