// Typed wrappers around the Rust Tauri commands. Components call these rather than
// invoke() directly, so every *core* command name lives in exactly one file.
//
// Modules are the deliberate exception: a module's own commands are gated to its Cargo
// feature and belong to it, not to the core surface, so it reaches them through
// `ChairPhotoAPI.invoke<T>(name, args)` (see registry.ts) and its command names stay in
// the module. Do not add a module-owned command here.

import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { getVersion } from "@tauri-apps/api/app";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { onOpenUrl } from "@tauri-apps/plugin-deep-link";
import { openUrl, revealItemInDir } from "@tauri-apps/plugin-opener";
import type { Photo, Publication, Tag } from "./registry";

/**
 * Build the `thumb://` asset URL for a photo id. Wraps Tauri's `convertFileSrc`
 * so that modules can import from the host-API layer (`api.ts`) rather than
 * reaching past it into `@tauri-apps/api/core` directly.
 */
export const thumbnailUrl = (photoId: number): string =>
  convertFileSrc(String(photoId), "thumb");

/** Open a URL in the user's default browser (e.g. an OAuth authorize page). */
export const openExternal = (url: string) => openUrl(url);

/** Reveal a file in the OS file manager (e.g. a freshly rendered collage). */
export const revealInFolder = (path: string) => revealItemInDir(path);

/** Absolute on-disk path of a photo's best-available copy (errors if unreachable). */
export const photoPath = (photoId: number) => invoke<string>("photo_path", { photoId });
/** Reveal a photo in the OS file manager (opens its folder, selects the file). */
export const revealPhoto = async (photoId: number) => revealInFolder(await photoPath(photoId));

// Re-exported so components/modules can keep importing the type from the api module.
export type { Publication };

export interface TagWithCount extends Tag {
  photoCount: number;
}

export interface ScanResult {
  scanned: number;
  imported: number;
  created: number;
  errors: number;
  /** Ingest only: source files already present at the destination, skipped. */
  skipped: number;
}

export type CullingFilter = "all" | "unrated" | "pick" | "reject" | "edited";

/** Open the default catalog (rooted at $HOME). Returns the catalog file path. */
export const initCatalog = () => invoke<string>("init_catalog");

// --- multi-catalog support (I4) ---

/** A recently-accessed catalog entry (from the app-level recent_catalogs.json registry). */
export interface RecentCatalog {
  /** User-given name (inferred from filename when not provided). */
  name: string;
  /** Absolute path to the .chairphoto database file. */
  catalogPath: string;
  /** Library root (photo folder) this catalog is rooted at. */
  root: string;
  /** Unix timestamp of the last time this catalog was opened. */
  lastOpened: number;
}

/** List recently-accessed catalogs, ordered most-recently-opened first (up to 20). */
export const listRecentCatalogs = () =>
  invoke<RecentCatalog[]>("list_recent_catalogs");

/**
 * Switch to a catalog via the safe teardown → reinit lifecycle (I4b).
 * Aborts any in-flight scan, closes the current catalog, opens (or creates) the new one,
 * records it in the recent-catalogs registry, and emits `catalog:switched` so the
 * frontend can reset all React state.
 *
 * @param catalogPath  Absolute path to the .chairphoto file.
 * @param root         Library root (photo folder) for the catalog.
 * @param create       If true, create a new catalog; fails if the file already exists.
 * @param name         Optional human name to record (else inferred from the filename).
 */
export const switchCatalog = (
  catalogPath: string,
  root: string,
  create: boolean,
  name?: string,
) =>
  invoke<void>("switch_catalog", {
    catalogPath,
    root,
    create,
    name: name ?? null,
  });

/**
 * Subscribe to the `catalog:switched` event emitted by the backend after a successful
 * switch. The payload is the new catalog file path (string). Returns an unlisten function.
 * The frontend should reset all view state (selection, filters, albums, scan progress) and
 * refresh on receipt.
 */
export const onCatalogSwitched = (handler: (catalogPath: string) => void): Promise<UnlistenFn> =>
  listen<string>("catalog:switched", (e) => handler(e.payload));

// --- plugin host support ---

/** Plugin backend features compiled into this build (e.g. ["ai"]). */
export const pluginFeatures = () => invoke<string[]>("plugin_features");

/**
 * A discovered external-module manifest (`chairphoto-module.json`), as returned by the
 * backend discovery command (H8b). Fields mirror `ChairPhotoModule`; `moduleDir` is the
 * absolute directory on disk (injected by the backend), so the loader can resolve the
 * entrypoint to an asset URL via `convertFileSrc`. See docs/plugin-system.md (External
 * modules).
 */
export interface ExternalModuleManifest {
  id: string;
  name: string;
  version: string;
  description: string;
  /** JS entry module, relative to `moduleDir`; its default export is a ChairPhotoModule. */
  entrypoint: string;
  /** Cargo feature the module's backend needs, if any (absent = frontend-only). */
  backendFeature?: string;
  /** Inter-module dependencies (same shape as ChairPhotoModule.requires). */
  requires?: { id: string; version?: string }[];
  /** Lowest host (app) version this module supports. */
  minHostVersion: string;
  /** Absolute path to the module directory on disk (backend-injected). */
  moduleDir: string;
}

/** Discover external modules installed under `<app_data_dir>/modules/*` (H8b). Read-only —
 *  no code is executed; malformed manifests are skipped backend-side. */
export const listExternalModules = () =>
  invoke<ExternalModuleManifest[]>("list_external_modules");

/** Absolute path to the external-modules install directory (`<app_data_dir>/modules/`).
 *  The directory may not exist yet; shown in the Modules panel as an install hint (H8d). */
export const getModulesDir = () => invoke<string>("get_modules_dir");

/**
 * Turn an absolute file path into an `asset:`-protocol URL loadable from the WebView (the
 * asset-protocol scope in tauri.conf.json must allow the path). Used to dynamically
 * `import()` an external module's entrypoint. Wraps `convertFileSrc` so modules/host code
 * import from the api layer rather than reaching into `@tauri-apps/api/core`.
 */
export const assetUrl = (path: string): string => convertFileSrc(path);

/** The running host (app) version, e.g. "0.1.0" — used to gate a module's minHostVersion. */
export const appVersion = () => getVersion();

/** Loopback port serving catalog videos (for the <video> player). */
export const videoServerPort = () => invoke<number>("video_server_port");

export const getSetting = (key: string) =>
  invoke<string | null>("get_setting", { key });

export const setSetting = (key: string, value: string) =>
  invoke<void>("set_setting", { key, value });

/**
 * Open a native folder picker, returning the chosen absolute path, or null if the
 * user cancelled. `defaultPath` pre-navigates the dialog when given.
 */
export const pickFolder = async (defaultPath?: string): Promise<string | null> => {
  const picked = await open({
    directory: true,
    multiple: false,
    defaultPath: defaultPath || undefined,
  });
  // With multiple:false the plugin returns a string (or null on cancel).
  return typeof picked === "string" ? picked : null;
};

/**
 * Open a native file picker, returning the chosen absolute path, or null if the user
 * cancelled. `defaultPath` pre-navigates the dialog when given. Used by "Relocate…" to
 * point a photo at a moved original.
 */
export const pickFile = async (defaultPath?: string): Promise<string | null> => {
  const picked = await open({
    directory: false,
    multiple: false,
    defaultPath: defaultPath || undefined,
  });
  return typeof picked === "string" ? picked : null;
};

/**
 * Open a native file picker filtered to `.chairphoto` bundle files, returning the chosen
 * absolute path, or null if the user cancelled. `defaultPath` pre-navigates the dialog.
 */
export const pickBundleFile = async (defaultPath?: string): Promise<string | null> => {
  const picked = await open({
    directory: false,
    multiple: false,
    defaultPath: defaultPath || undefined,
    filters: [{ name: "ChairPhoto Bundle", extensions: ["chairphoto"] }],
  });
  return typeof picked === "string" ? picked : null;
};

/** Recursively scan a folder into the open catalog. Read-only on photo files. */
export const scanFolder = (folder: string) =>
  invoke<ScanResult>("scan_folder_cmd", { folder });

/** Index an existing archive that lives on the NAS, in place (no copy). Those photos
 *  appear under the "On NAS" tier. For the initial bring-your-NAS-archive scan. */
export const scanNasFolder = (folder: string) =>
  invoke<ScanResult>("scan_nas_folder_cmd", { folder });

/** Import from a card: copy into <library root>/YYYY/MM/DD, index, batch, auto-queue
 *  backup. The destination is always the library root. `name` optionally labels the
 *  import batch (else the source folder is used). */
/** One photo on a card/source folder, flagged if it's already in the library. */
export interface CardPhoto {
  path: string;
  name: string;
  size: number;
  captureTime: string | null;
  isDuplicate: boolean;
}

/** List the photos on a card/source folder (with duplicate flags) for the import dialog. */
export const listCardPhotos = (source: string) =>
  invoke<CardPhoto[]>("list_card_photos_cmd", { source });

/** Import from a card. `selected` (full paths) restricts to a subset; omit for all. */
export const ingestFromCard = (source: string, name?: string, selected?: string[]) =>
  invoke<ScanResult>("ingest_from_card_cmd", {
    source,
    name: name || null,
    selected: selected ?? null,
  });

/** The current library root (catalog root = local volume base). */
export const getLibraryRoot = () => invoke<string>("get_library_root");
/** Re-root the catalog at a library folder (re-scan needed afterward). */
export const setLibraryRoot = (path: string) =>
  invoke<void>("set_library_root", { path });
/** Rescan the whole library (the catalog root) in place. */
export const rescanLibrary = () => invoke<ScanResult>("rescan_library");

/** Storage-tier filter for the library: all photos, on-disk only, or NAS-only. */
export type StorageTier = "all" | "local" | "nas";

/**
 * Photo sort order.
 * - `"date"` (default) — oldest-first by capture time.
 * - `"sharpness_asc"` — least-sharp first (suspect frames up front for culling; unscored last).
 * - `"sharpness_desc"` — sharpest-first (unscored last).
 */
export type PhotoSort = "date" | "sharpness_asc" | "sharpness_desc";

/**
 * Which photos the library view wants, in what order, and (optionally) which slice.
 *
 * The same object as `PhotoQuery` in `src-tauri/src/catalog/query.rs`, field for field
 * (issue #10) — it used to be eleven positional arguments that TypeScript, the Tauri
 * command and the SQL builder each had to spell identically. Two things keep the sides
 * honest rather than merely parallel: the string unions below are Rust enums, and the Rust
 * struct is `deny_unknown_fields`, so a field added here and not there fails loudly at the
 * boundary instead of being silently ignored (a silently ignored filter looks like "the
 * grid forgot my selection").
 *
 * Every field is optional; the defaults mean "the whole library, oldest first".
 */
export interface PhotoQuery {
  /** Restrict to a tag *and its descendants*. */
  tagId?: number | null;
  /** Restrict to an album — also switches the sort to the album's own member order. */
  albumId?: number | null;
  /** Restrict to one import batch. */
  batchId?: number | null;
  /** Evaluate a saved smart-album rule live. */
  smartAlbumId?: number | null;
  /** Derived boolean facets, ANDed in (e.g. `has-gps`, `published:flickr`). */
  facets?: string[];
  cullingFilter?: CullingFilter;
  storageTier?: StorageTier;
  /** Exact camera model, from `distinctPhotoValues("camera")`. */
  camera?: string | null;
  /** Exact lens, from `distinctPhotoValues("lens")`. */
  lens?: string | null;
  /** Colour labels to keep, OR-combined ("" = No label). Empty = no label filter. */
  labels?: string[];
  sort?: PhotoSort;
  /** The slice to fetch. Omit for every matching row. */
  window?: PhotoWindow | null;
}

/** A half-open slice `[offset, offset + limit)` of the ordered result. */
export interface PhotoWindow {
  offset: number;
  limit: number;
}

/**
 * One window of a query's result, plus the size of the whole matching set.
 *
 * `total` is what sizes the scrollbar and the "N photos" readout; it is the count of
 * matching photos, not of `photos`. A window is meaningful because the backend's ordering
 * is total (every sort ends in the photo's primary key), so "rows 500..1000" names the
 * same rows on every call.
 */
export interface PhotoPage {
  photos: Photo[];
  /** Ordered position of `photos[0]` in the whole matching set. */
  offset: number;
  total: number;
}

/** Run a library query. Omit `window` to get every matching row. */
export const listPhotos = (query: PhotoQuery = {}) =>
  invoke<PhotoPage>("list_photos", { query });

/** Distinct camera models / lenses in the catalog, for the filter-bar dropdowns. */
export const distinctPhotoValues = (kind: "camera" | "lens") =>
  invoke<string[]>("distinct_photo_values", { kind });

/** A derived, internal-only filter (e.g. has-GPS). Never exported. */
export interface Facet {
  key: string;
  label: string;
}
export const listFacets = () => invoke<Facet[]>("list_facets");

/** Pre-generate cached images for the whole catalog. Reports progress via events. */
export const cacheImages = (includePreviews: boolean) =>
  invoke<void>("cache_images", { includePreviews });

export interface CacheProgress {
  done: number;
  total: number;
}

/** Subscribe to batch-cache progress. Returns an unlisten function. */
export const onCacheProgress = (handler: (p: CacheProgress) => void): Promise<UnlistenFn> =>
  listen<CacheProgress>("cache:progress", (e) => handler(e.payload));

export interface ImportProgress {
  done: number;
  total: number;
}

/** Subscribe to card-import copy progress. Returns an unlisten function. */
export const onImportProgress = (handler: (p: ImportProgress) => void): Promise<UnlistenFn> =>
  listen<ImportProgress>("import:progress", (e) => handler(e.payload));

export interface ScanProgress {
  /** "indexing" | "metadata" | "finalizing" | "done" */
  phase: string;
  done: number;
  /** 0 = indeterminate (the discovery phase has no known total yet). */
  total: number;
}

/** Subscribe to folder/NAS scan progress (`scan:progress`). Returns an unlisten function. */
export const onScanProgress = (handler: (p: ScanProgress) => void): Promise<UnlistenFn> =>
  listen<ScanProgress>("scan:progress", (e) => handler(e.payload));

export const getPreview = (photoId: number) =>
  invoke<string>("get_preview", { photoId });

/** Non-destructively rotate a photo's displayed orientation by `delta` degrees
 *  clockwise (±90, or 180). The original file is never modified. Returns the new
 *  absolute rotation (0/90/180/270). */
export const rotatePhoto = (photoId: number, delta: number) =>
  invoke<number>("rotate_photo", { photoId, delta });

/** One photo by id (used to load a stack's master from a child). */
export const getPhoto = (photoId: number) =>
  invoke<import("./registry").Photo>("get_photo", { photoId });

/** One photo by its stable uuid (a chairphoto://<uuid> deep-link target). */
export const getPhotoByUuid = (uuid: string) =>
  invoke<import("./registry").Photo>("get_photo_by_uuid", { uuid });

/** Which surface a chairphoto:// link asks for: the Library grid (default), the
 *  inline loupe, or the Develop editor. */
export type DeepLinkView = "grid" | "loupe" | "develop";

/** Subscribe to chairphoto://<uuid>[/loupe|/develop] deep links. Fires for the
 *  URL the app was launched with too (onOpenUrl checks getCurrent() internally),
 *  and for URLs forwarded from a second launch by the single-instance plugin. */
export const onDeepLinkPhoto = (
  handler: (uuid: string, view: DeepLinkView) => void,
): Promise<UnlistenFn> =>
  onOpenUrl((urls) => {
    for (const u of urls) {
      // Accept chairphoto://UUID and chairphoto:///UUID, with an optional
      // /loupe or /develop suffix, any casing (URI schemes are case-insensitive;
      // uuids are stored lowercase, so normalize).
      const m = u
        .trim()
        .match(/^chairphoto:\/{2,3}([0-9a-fA-F-]{36})(?:\/(loupe|develop))?\/?$/i);
      if (m) handler(m[1].toLowerCase(), (m[2]?.toLowerCase() as DeepLinkView) ?? "grid");
    }
  });

/** Subscribe to chairphoto://tag/<uuid> deep links (e.g. from an Obsidian tag note) —
 *  the app filters the Library to that tag. Same launch/second-instance semantics as
 *  onDeepLinkPhoto; "tag" isn't 36 hex chars so the two matchers never overlap. */
export const onDeepLinkTag = (handler: (uuid: string) => void): Promise<UnlistenFn> =>
  onOpenUrl((urls) => {
    for (const u of urls) {
      const m = u.trim().match(/^chairphoto:\/{2,3}tag\/([0-9a-fA-F-]{36})\/?$/i);
      if (m) handler(m[1].toLowerCase());
    }
  });

/** Photos stacked under a master (e.g. the camera JPEG under its RAW). */
export const listStackChildren = (photoId: number) =>
  invoke<import("./registry").Photo[]>("list_stack_children", { photoId });

/** Stack `childId` under `parentId` (the child is hidden from the grid). */
export const stackPhoto = (childId: number, parentId: number) =>
  invoke<void>("stack_photo", { childId, parentId });

/** Remove a photo from its stack — it returns to the grid as a top-level photo. */
export const unstackPhoto = (childId: number) =>
  invoke<void>("unstack_photo", { childId });

/** Stack every derivative JPEG under its sibling RAW; returns the count newly stacked. */
export const pairRawJpegStacks = () => invoke<number>("pair_raw_jpeg_stacks");

// --- external develop (darktable / RawTherapee / ART) ----------------------
export interface AvailableEditor {
  key: string;
  label: string;
  /** GUI command runnable → can offer "Edit in …". */
  gui: boolean;
  /** CLI command runnable → can auto-render the result. */
  cli: boolean;
  sidecar: string;
}
/** Which external develop editors are configured/available. */
export const availableEditors = () => invoke<AvailableEditor[]>("available_editors");

/** Launch the editor GUI on a photo; when it closes, render + stack the developed result.
 *  For darktable, AI-restore outputs (denoised DNG / upscaled TIFF) written during the
 *  session are also stacked under the original as they appear.
 *  Returns the new stacked child's id, or null if nothing changed (use importDeveloped). */
export const developInEditor = (photoId: number, editorKey: string) =>
  invoke<number | null>("develop_in_editor", { photoId, editorKey });

/** Render the developed result from the current sidecar and stack it (manual fallback).
 *  For darktable this also adopts any AI-restore outputs next to the original. */
export const importDeveloped = (photoId: number, editorKey: string) =>
  invoke<number>("import_developed", { photoId, editorKey });

export interface DevelopProgress {
  /** waiting | rendering | stacked | done | nochange | error */
  phase: string;
  editor: string;
}
/** Subscribe to external-develop progress (`develop:progress`). */
export const onDevelopProgress = (handler: (p: DevelopProgress) => void): Promise<UnlistenFn> =>
  listen<DevelopProgress>("develop:progress", (e) => handler(e.payload));

// --- Edit in RapidRAW (request/response round-trip) -------------------------
export interface RapidRawStatus {
  /** The RapidRAW binary is detected (PATH or override) → offer the action. */
  available: boolean;
  /** The output format that will be produced (tiff | png | jpg). */
  format: string;
}
/** Whether RapidRAW is configured/available. */
export const rapidrawAvailable = () => invoke<RapidRawStatus>("rapidraw_available");

/** Launch RapidRAW on a photo; on Done, import + stack the exported result under the original.
 *  Returns the new stacked child's id, or null if the wait was cancelled. In the single-instance
 *  forwarded case the promise stays pending (the app watches for the output) until Done or
 *  cancelRapidraw. */
export const editInRapidraw = (photoId: number) =>
  invoke<number | null>("edit_in_rapidraw", { photoId });

/** Abandon an in-flight RapidRAW wait for a photo (forwarded session / closed without Done). */
export const cancelRapidraw = (photoId: number) =>
  invoke<void>("cancel_rapidraw", { photoId });

export interface RapidRawProgress {
  photoId: number;
  /** editing | waiting | importing | done | error | cancelled */
  phase: string;
  message: string;
}
/** Subscribe to RapidRAW round-trip progress (`rapidraw:progress`). */
export const onRapidrawProgress = (handler: (p: RapidRawProgress) => void): Promise<UnlistenFn> =>
  listen<RapidRawProgress>("rapidraw:progress", (e) => handler(e.payload));

// Image generation (especially RAW preview extraction) is expensive, so we cap
// how many run at once. The grid can ask for 100 thumbnails; only `MAX_CONCURRENT`
// reach the backend at a time, the rest queue. This keeps the app responsive and
// avoids spawning dozens of exiftool processes simultaneously.
const MAX_CONCURRENT = 6;
let active = 0;
const queue: Array<() => void> = [];

function withLimit<T>(task: () => Promise<T>): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const run = () => {
      active++;
      task()
        .then(resolve, reject)
        .finally(() => {
          active--;
          queue.shift()?.();
        });
    };
    if (active < MAX_CONCURRENT) run();
    else queue.push(run);
  });
}

export const getThumbnail = (photoId: number) =>
  withLimit(() => invoke<string>("get_thumbnail", { photoId }));

/** Thumbnail (data URL) for an arbitrary file path — import-dialog card previews. */
export const cardThumbnail = (path: string) =>
  withLimit(() => invoke<string>("card_thumbnail", { path }));

export const setRating = (photoId: number, rating: number) =>
  invoke<Photo>("set_rating", { photoId, rating });

export const setLabel = (photoId: number, label: string) =>
  invoke<Photo>("set_label", { photoId, label });

export const setPickState = (photoId: number, pickState: Photo["pickState"]) =>
  invoke<Photo>("set_pick_state", { photoId, pickState });

export const listTags = () => invoke<TagWithCount[]>("list_tags");

/** Existing tags from photos taken near this one in time (photoCount = neighbour freq). */
export const suggestTagsByTime = (photoId: number, windowSeconds?: number) =>
  invoke<TagWithCount[]>("suggest_tags_by_time", { photoId, windowSeconds });

// --- tag groups (fast tagging) ---

export interface TagGroup {
  id: number;
  name: string;
}

export const listTagGroups = () => invoke<TagGroup[]>("list_tag_groups");
export const createTagGroup = (name: string) =>
  invoke<number>("create_tag_group", { name });
export const renameTagGroup = (groupId: number, name: string) =>
  invoke<void>("rename_tag_group", { groupId, name });
export const deleteTagGroup = (groupId: number) =>
  invoke<void>("delete_tag_group", { groupId });
export const getGroupMembers = (groupId: number) =>
  invoke<Tag[]>("get_group_members", { groupId });
/** Tags most recently applied by hand, newest first — backs the "Recently used" group. */
export const recentlyUsedTags = (limit = 10) =>
  invoke<Tag[]>("recently_used_tags", { limit });
export const addTagToGroup = (groupId: number, path: string) =>
  invoke<number>("add_tag_to_group", { groupId, path });
export const removeTagFromGroup = (groupId: number, tagId: number) =>
  invoke<void>("remove_tag_from_group", { groupId, tagId });

// --- storage volumes ---

export type VolumeKind = "local" | "backup";

export interface Volume {
  id: number;
  uuid: string;
  name: string;
  basePath: string;
  kind: VolumeKind;
  reachable: boolean;
}

export const listVolumes = () => invoke<Volume[]>("list_volumes");
export const addVolume = (name: string, basePath: string, kind: VolumeKind) =>
  invoke<number>("add_volume", { name, basePath, kind });
export const removeVolume = (volumeId: number) =>
  invoke<void>("remove_volume", { volumeId });

/** Per-photo storage status, derived from where its copies live. */
export type StorageStatus =
  | "localOnly"
  | "backedUp"
  | "archived"
  | "offline"
  | "missing";

/** Batch storage status for the grid: [photoId, status] pairs. */
export const photoStatuses = (photoIds: number[]) =>
  invoke<[number, StorageStatus][]>("photo_statuses", { photoIds });

// --- edit record (non-destructive editing contract) ---

/** A photo's edit record as a JSON string, or null if it has none. */
export const getEditRecord = (photoId: number) =>
  invoke<string | null>("get_edit_record", { photoId });
/** Replace a photo's edit record (empty clears it). Must be valid JSON. */
export const setEditRecord = (photoId: number, editJson: string) =>
  invoke<void>("set_edit_record", { photoId, editJson });

/**
 * Render the photo's preview proxy with an edit record applied (crop + tone), returning
 * a `data:image/jpeg;base64,…` URL. `maxEdge` caps size for fast live preview (0 = full).
 * Never touches the original file. Requires the `edit` backend feature.
 */
export const renderEdit = (photoId: number, editJson: string, maxEdge = 0, hiRes = false) =>
  invoke<string>("render_edit", { photoId, editJson, maxEdge, hiRes });

/**
 * Render several edit records against one photo's proxy in a single call (the preset
 * browser's thumbnails). The proxy is decoded once backend-side; per-record failures
 * come back as null. Returns data URLs in input order.
 */
export const renderEditBatch = (photoId: number, editJsons: string[], maxEdge = 320) =>
  invoke<(string | null)[]>("render_edit_batch", { photoId, editJsons, maxEdge });

// --- LUTs (user-supplied .cube files, referenced by edit records by filename) ---

/** Filenames of the .cube LUTs available in the app's luts folder. */
export const listLuts = () => invoke<string[]>("list_luts");
/** Validate + copy a .cube file into the luts folder; returns the bare filename. */
export const importLut = (path: string) => invoke<string>("import_lut", { path });
/** Remove a LUT. Edit records referencing it keep rendering, minus the LUT. */
export const deleteLut = (file: string) => invoke<void>("delete_lut", { file });

// --- photo versions (crop/exposure variants, see docs/editing.md) ---

export interface PhotoVersion {
  id: number;
  photoId: number;
  name: string;
  editJson: string;
  position: number;
}

export const listVersions = (photoId: number) =>
  invoke<PhotoVersion[]>("list_versions", { photoId });
export const createVersion = (photoId: number, name: string) =>
  invoke<number>("create_version", { photoId, name });
export const renameVersion = (versionId: number, name: string) =>
  invoke<void>("rename_version", { versionId, name });
export const setVersionEdit = (versionId: number, editJson: string) =>
  invoke<void>("set_version_edit", { versionId, editJson });
export const deleteVersion = (versionId: number) =>
  invoke<void>("delete_version", { versionId });
export const duplicateVersion = (versionId: number) =>
  invoke<number>("duplicate_version", { versionId });
export const reorderVersions = (photoId: number, orderedIds: number[]) =>
  invoke<void>("reorder_versions", { photoId, orderedIds });
/** Version counts for many photos at once (grid badge): `[photoId, count]` pairs. */
export const versionCounts = (photoIds: number[]) =>
  invoke<[number, number][]>("version_counts", { photoIds });

// --- publications (where a photo was posted + which version, see docs/publications.md) ---
// The `Publication` type lives in the module contract (registry.ts) and is re-exported
// above. Modules normally use api.recordPublication/listPublications/deletePublication on
// the injected ChairPhotoAPI (which stamps the module's marker); these raw wrappers take
// an explicit platform and back both that API and the core UI.

export const listPublications = (photoId: number) =>
  invoke<Publication[]>("list_publications", { photoId });

/** Record (or update) that a photo's version (null = Original) was published to a
 *  platform. `platform` must be non-empty. Upserts on (photo, platform). */
export const recordPublication = (
  photoId: number,
  versionId: number | null,
  platform: string,
  url?: string | null,
) =>
  invoke<number>("record_publication", {
    photoId,
    versionId: versionId ?? null,
    platform,
    url: url ?? null,
  });

export const deletePublication = (id: number) =>
  invoke<void>("delete_publication", { id });

// --- storage lifecycle (backup / offload / restore + reconcile queue) ---

export const backupPhoto = (photoId: number) => invoke<void>("backup_photo", { photoId });
export const offloadPhoto = (photoId: number) => invoke<void>("offload_photo", { photoId });
export const restorePhoto = (photoId: number) => invoke<void>("restore_photo", { photoId });

/** Forget a photo whose original is gone — deletes the catalog row only (never files). */
export const removePhotoFromCatalog = (photoId: number) =>
  invoke<void>("remove_photo_from_catalog", { photoId });

/** Re-point a photo at a moved original (must be under the library root). */
export const relocatePhoto = (photoId: number, newPath: string) =>
  invoke<void>("relocate_photo", { photoId, newPath });

export interface UnavailablePhoto {
  id: number;
  path: string;
}
/** Preview: catalog photos with no reachable, existing copy (local or backup). */
export const findUnavailablePhotos = () =>
  invoke<UnavailablePhoto[]>("find_unavailable_photos");
/** Remove all such photos from the catalog (deletes rows only — never files). */
export const purgeUnavailablePhotos = () =>
  invoke<UnavailablePhoto[]>("purge_unavailable_photos");

/** Preview: photos whose only copy is a 0-byte (empty/corrupt) file. */
export const findEmptyPhotos = () => invoke<UnavailablePhoto[]>("find_empty_photos");
/** Remove those empty-file photos from the catalog (rows only — never files). */
export const purgeEmptyPhotos = () => invoke<UnavailablePhoto[]>("purge_empty_photos");

export interface VacuumResult {
  beforeBytes: number;
  afterBytes: number;
}
/** Compact the catalog (SQLite VACUUM) — reclaim space from deletions + defragment. */
export const vacuumCatalog = () => invoke<VacuumResult>("vacuum_catalog");

/** Setting key for the "keep last N days on local disk" offload policy. */
export const OFFLOAD_AGE_SETTING = "offload_age_days";
/** Apply the age-based offload policy now; returns how many photos were offloaded. */
export const applyOffloadPolicy = () => invoke<number>("apply_offload_policy");

export interface PendingOperation {
  id: number;
  kind: string;
  photoId: number;
  status: string;
  error: string;
  createdAt: number;
}
export interface DrainSummary {
  ran: number;
  failed: number;
  skippedOffline: boolean;
}
export const listPendingOperations = () =>
  invoke<PendingOperation[]>("list_pending_operations");
export const enqueueOperation = (kind: string, photoId: number) =>
  invoke<number>("enqueue_operation", { kind, photoId });
export const reconcileNow = () => invoke<DrainSummary>("reconcile_now");

// --- identity debt (sidecar UUID / import-batch binding, see CONTEXT.md § Identity) ---
//
// Debt is per COPY, not per photo: the same photo can owe two independent debts on two
// different volumes. `Unreachable` is a normal state (an unmounted volume owes exactly as
// much as a failed write), not a failure — never render it as an error.

/** One of the CONTEXT.md § Identity states a queued copy can be in. `bound` never appears
 *  here — a copy is removed from the queue the moment it's bound. `dismissed` is a decision
 *  rather than a failure: the row is kept for the record, never retried, and not counted as
 *  debt (see `resolveIdentityConflict`). */
export type IdentityDebtState = "unreachable" | "unwritable" | "conflict" | "dismissed";

/** One field a COPY still owes (`identifier` = xmp:Identifier, `import_batch` =
 *  chairphoto:ImportBatch), with its own retry history. A copy can owe up to both,
 *  queued and retried independently. */
export interface PendingIdentityField {
  field: "identifier" | "import_batch";
  state: IdentityDebtState;
  attempts: number;
  /** Human-readable detail (e.g. the write error, or the conflicting UUID found). The
   *  single most useful fact on a Conflict field — render it. Survives a dismissal, so a
   *  user deciding whether to restore can still see what the conflict was. */
  error: string;
  lastAttemptAt: number;
  /** When this field was dismissed, or 0. Per FIELD: a copy on the active page can carry a
   *  dismissed field while still owing the other one. */
  dismissedAt: number;
}

/** One COPY still owing at least one sidecar field. Matches `pending_sidecar_identity`
 *  grouped by (photoId, volumeId, relativePath) — CONTEXT.md's "Copy", and the same unit
 *  `PendingIdentitySummary.total` counts. A copy owing both `identifier` and
 *  `import_batch` is ONE of these, with both entries in `fields` — never two rows: a list
 *  that instead returned one row per (copy, field) could exceed `total`, e.g. rendering
 *  "Showing 1–4 of 3".
 *
 *  Backend fields `uuid`, `value`, `targetPath`, and `queuedAt` exist in Rust but are
 *  deliberately not shipped here — the panel never renders them, and doing so was ~a
 *  third of its IPC payload for nothing. `targetPath` is derivable client-side from
 *  `volumeId` + `relativePath` if ever needed. */
export interface PendingIdentity {
  photoId: number;
  /** The photo's catalog-root-relative logical path, for display. */
  path: string;
  volumeId: number;
  /** This copy's path relative to its volume's base — pair with `volumeId` to show
   *  "which volume" and "which path" independently. */
  relativePath: string;
  /** Every sidecar field this copy still owes — 1 or 2 entries, never 0. */
  fields: PendingIdentityField[];
}

/** Cheap counts over the whole pending-identity queue — safe to call to show a badge
 *  without pulling every row (the queue can hold tens of thousands of rows). Both counts
 *  are in COPIES (`photoId` + `volumeId` + `relativePath`), not queue rows: a copy owing
 *  both `identifier` and `import_batch` is one copy, not two. */
export interface PendingIdentitySummary {
  /** Every queued copy still owing something, any field, any state. A copy whose every
   *  field has been dismissed is in `dismissed` instead — the two are disjoint. */
  total: number;
  /** Of `total`, how many have at least one un-dismissed field in `conflict` — need a
   *  human, not a retry. */
  conflicts: number;
  /** Copies whose every queued field was dismissed: kept on the record, never retried, and
   *  deliberately not debt (CONTEXT.md § Identity). List them with
   *  `listPendingIdentity(…, true)` to restore one. */
  dismissed: number;
}

export interface IdentityRepairSummary {
  /** Now bound; cleared from the queue. */
  bound: number;
  /** Still not reachable; left queued. */
  unreachable: number;
  /** The sidecar carries a different identity — needs a human (#33), not a retry. NOT a
   *  failure: left untouched ("when uncertain, preserve"), same as before the pass. */
  conflicts: number;
  /** Retried and is still genuinely failing (unwritable sidecar); left queued. */
  failed: number;
  /** Rows somebody else decided while the pass held them — a conflict resolved, a copy
   *  dismissed, a scan re-recording the same copy. The pass discarded its own result for
   *  those rather than writing over the newer decision, so they are counted here and in
   *  none of the four above. Not a failure. */
  superseded: number;
  /** Queue rows when the pass started — its denominator. In ROWS, not copies: the pass
   *  retries each owed field, so a copy owing both its UUID and its import batch is two
   *  here and one in `PendingIdentitySummary.total`. */
  total: number;
  /** True when the pass stopped early — cancelled, superseded by a newer pass, or ended by
   *  a catalog switch. Every count above is then partial, and must be labelled as such
   *  rather than presented as a finished result. */
  aborted: boolean;
}

/** Progress event for a running repair pass (`identity:repair_progress`). `job` is what
 *  tells this pass's events from a superseded one's stragglers — filter on it. */
export interface IdentityRepairProgress {
  done: number;
  total: number;
  job: number;
}

/** Terminal event for a repair pass (`identity:repair_done`). The pass's RESULT — the
 *  command itself only returns a job id. `ok: false` with an `error` means the pass could
 *  not run at all; a pass that ran and was stopped is `ok: true` with `summary.aborted`. */
export interface IdentityRepairDone {
  ok: boolean;
  job: number;
  summary: IdentityRepairSummary;
  error: string | null;
}

/** A repair pass in flight, as `identityRepairStatus()` reports it. */
export interface IdentityRepairStatus {
  job: number;
  done: number;
  total: number;
}

/** One page of the identity-debt list, one row per copy (never per field — see
 *  `PendingIdentity`'s doc), ordered by each copy's natural key. The queue can hold tens
 *  of thousands of rows (74,488 on the 100k harness shape in #20), so this always pages
 *  via `limit`/`offset` rather than returning the whole queue in one IPC payload — pair
 *  with `summarizePendingIdentity()` for the total (same unit: copies), and a virtualized
 *  list for the page itself.
 *
 *  `includeDismissed` widens the page from the active queue (a slice of `summary.total`) to
 *  every copy including dismissed ones (a slice of `total + dismissed`). It is the only way
 *  back to a dismissal, so pair it with the `restore` action rather than offering it as a
 *  bare "show more". */
export const listPendingIdentity = (limit: number, offset: number, includeDismissed = false) =>
  invoke<PendingIdentity[]>("list_pending_identity", { limit, offset, includeDismissed });
/** Total debt + conflict counts, without transferring every row. Independent of
 *  `listPendingIdentity` — call/await it separately so a slow list fetch never delays the
 *  cheap header count. */
export const summarizePendingIdentity = () =>
  invoke<PendingIdentitySummary>("summarize_pending_identity");
/** Start a repair pass over the queued copies. Unreachable/unwritable/conflicted copies
 *  stay queued; dismissed ones are skipped entirely.
 *
 *  Returns the pass's **job id**, not its result: the queue reached 74,488 rows on the 100k
 *  harness shape in #20 and each row can be a network round trip, so the pass reports
 *  through `onIdentityRepairProgress` and finishes with `onIdentityRepairDone` (#34).
 *  Install both listeners BEFORE calling this — a pass over an empty queue finishes before
 *  this promise resolves. Starting a second pass supersedes the first. */
export const repairPendingIdentity = () => invoke<number>("repair_pending_identity");
/** Stop the running repair pass at its next copy. No-op when none is running. */
export const cancelIdentityRepair = () => invoke<void>("identity_repair_cancel");
/** The repair pass in flight, or `null` when idle — so a remounted panel re-attaches to a
 *  pass instead of reopening as if nothing were happening. */
export const identityRepairStatus = () =>
  invoke<IdentityRepairStatus | null>("identity_repair_status");
/** Subscribe to repair-pass progress. Returns an unlisten function. */
export const onIdentityRepairProgress = (
  handler: (p: IdentityRepairProgress) => void,
): Promise<UnlistenFn> =>
  listen<IdentityRepairProgress>("identity:repair_progress", (e) => handler(e.payload));
/** Subscribe to the repair pass's terminal event. Returns an unlisten function. */
export const onIdentityRepairDone = (
  handler: (d: IdentityRepairDone) => void,
): Promise<UnlistenFn> =>
  listen<IdentityRepairDone>("identity:repair_done", (e) => handler(e.payload));

/** What to do about one conflicted copy — CONTEXT.md § Identity's vocabulary verbatim.
 *  There is no default: the backend rejects anything that isn't one of these four, so a
 *  missing or mistyped choice can never fall through to the destructive one. */
export type IdentityConflictAction = "adopt" | "overwrite" | "dismiss" | "restore";

/** What one `resolveIdentityConflict` actually did. Report it — never infer the result
 *  from the action that was requested. */
export interface IdentityConflictOutcome {
  action: IdentityConflictAction;
  photoId: number;
  /** The photo's UUID after the resolution. Only `adopt` changes it. */
  catalogUuid: string;
  /** The identifier the sidecar carried before: the adopted value, or the one `overwrite`
   *  destroyed. Empty for `dismiss`/`restore`, which read no file. */
  previousSidecarUuid: string;
  /** Other copies of this photo an `adopt` re-checked, because the photo's identity
   *  changed under them. */
  recheckedCopies: number;
  /** Where `overwrite` preserved the previous sidecar. `null` when an older backup was
   *  already there and was deliberately left alone. */
  sidecarBackup: string | null;
}

/** Resolve one conflicted copy (issue #33). Identified by copy — `photoId` + `volumeId` +
 *  `relativePath` — because debt is per copy: resolving one says nothing about the same
 *  photo's other copies.
 *
 *  `adopt` changes the catalog's UUID for this photo, which is what catalog merge matches
 *  on and what `chairphoto://<uuid>` links address; `overwrite` destroys the identifier in
 *  the file (after backing the sidecar up). Both are consequential enough that the UI must
 *  make the user pick one by name, and confirm the destructive one.
 *
 *  Rejections come back as messages naming what was refused — notably an adopt of an
 *  identity another photo already holds, which is checked before anything is written. */
export const resolveIdentityConflict = (
  photoId: number,
  volumeId: number,
  relativePath: string,
  action: IdentityConflictAction,
) =>
  invoke<IdentityConflictOutcome>("resolve_identity_conflict", {
    photoId,
    volumeId,
    relativePath,
    action,
  });

// --- export (one-way) ---

export type ExportPreset = "handOff" | "showOff" | "instagram";

export interface ExportResult {
  exported: number;
  skippedOffline: number;
  errors: number;
}

export const exportPhotos = (
  photoIds: number[],
  preset: ExportPreset,
  destDir: string,
  hashtagGroupId?: number | null,
  hashtagLimit?: number | null,
  /** The version active in the UI; Show-off renders it at full resolution. */
  versionId?: number | null,
) =>
  invoke<ExportResult>("export_photos", {
    photoIds,
    preset,
    destDir,
    hashtagGroupId: hashtagGroupId ?? null,
    hashtagLimit: hashtagLimit ?? null,
    versionId: versionId ?? null,
  });

// --- hashtag bundles (core; used by the Export panel) ---

/** Assemble a reach-hashtag bundle from a tag group (preview/copy for export). */
export const assembleHashtagBundle = (groupId: number, limit?: number | null) =>
  invoke<string[]>("assemble_hashtag_bundle", { groupId, limit: limit ?? null });


// --- import batches ("negative film roll") ---

export interface ImportBatch {
  id: number;
  uuid: string;
  sourceLabel: string;
  note: string;
  createdAt: number;
  photoCount: number;
}

export const listImportBatches = () => invoke<ImportBatch[]>("list_import_batches");

// --- bundle export / import (Epic F1) ---

/** Lightweight pre-import summary returned by previewBundle. */
export interface BundlePreview {
  /** Human label for the import batch (e.g. the source folder name). */
  batchLabel: string;
  /** Stable UUID of the import batch. */
  batchUuid: string;
  /** Total photos in the bundle. */
  total: number;
  /** Photos not yet in the catalog (will be added on import). */
  newCount: number;
  /** Photos already present in the catalog (merge is a no-op for them). */
  existing: number;
}

/** What the bundle writer returned after a successful export. */
export interface BundleWriteResult {
  /** Number of photo originals successfully added to the zip. */
  exported: number;
  /** Number of photos whose original was offline/missing (metadata-only in bundle). */
  skippedOffline: number;
  /** Number of photos that encountered a non-fatal write error. */
  errors: number;
}

/** What the bundle importer returned after a successful import. */
export interface BundleImportResult {
  /** Originals copied from the bundle to the local library. */
  copied: number;
  /** Originals skipped because a same-size file already existed. */
  skippedDuplicate: number;
  /** Originals that encountered a non-fatal extraction error. */
  errors: number;
  /** What the additive merge did. */
  merge: {
    photosAdded: number;
    photosExisting: number;
    tagsCreated: number;
    termsAdded: number;
    assignmentsAdded: number;
    batchAdded: boolean;
  };
}

/**
 * Peek at a bundle file and return a lightweight pre-merge summary so the user
 * can confirm before committing to the full import. No data is written.
 */
export const previewBundle = (bundlePath: string) =>
  invoke<BundlePreview>("preview_bundle", { bundlePath });

/**
 * Export one import batch as a `.chairphoto` bundle zip to `destPath`.
 * Progress is streamed as `import:progress` events (same shape as ingest).
 * Returns the number of originals exported and how many were offline/skipped.
 */
export const exportBundle = (batchId: number, destPath: string) =>
  invoke<BundleWriteResult>("export_bundle", { batchId, destPath });

/**
 * Import a `.chairphoto` bundle: unpack originals into the library root, index
 * them, run the additive merge, and auto-enqueue backup. Progress is streamed
 * as `import:progress` events (same shape as ingest from card).
 */
export const importBundle = (bundlePath: string) =>
  invoke<BundleImportResult>("import_bundle_cmd", { bundlePath });

// --- albums (manual collections) ---

export interface Album {
  id: number;
  uuid: string;
  name: string;
  note: string;
  photoCount: number;
}

export const listAlbums = () => invoke<Album[]>("list_albums");
export const createAlbum = (name: string) =>
  invoke<number>("create_album", { name });
export const renameAlbum = (albumId: number, name: string) =>
  invoke<void>("rename_album", { albumId, name });
export const deleteAlbum = (albumId: number) =>
  invoke<void>("delete_album", { albumId });
export const addPhotosToAlbum = (albumId: number, photoIds: number[]) =>
  invoke<void>("add_photos_to_album", { albumId, photoIds });
export const removePhotosFromAlbum = (albumId: number, photoIds: number[]) =>
  invoke<void>("remove_photos_from_album", { albumId, photoIds });

// --- smart albums (saved rules, evaluated live; see docs/smart-albums.md) ---

/** A saved rule resolving to a photo set. `ruleJson` is the opaque-to-core Rule JSON
 *  string (the shared contract in docs/smart-albums.md). `photoCount` is the live count. */
export interface SmartAlbum {
  id: number;
  uuid: string;
  name: string;
  ruleJson: string;
  photoCount: number;
}

export const listSmartAlbums = () => invoke<SmartAlbum[]>("list_smart_albums");
export const createSmartAlbum = (name: string, ruleJson: string) =>
  invoke<number>("create_smart_album", { name, ruleJson });
export const renameSmartAlbum = (smartAlbumId: number, name: string) =>
  invoke<void>("rename_smart_album", { smartAlbumId, name });
/** Replace a smart album's rule (the Rule JSON string). */
export const setSmartAlbumRule = (smartAlbumId: number, ruleJson: string) =>
  invoke<void>("set_smart_album_rule", { smartAlbumId, ruleJson });
export const deleteSmartAlbum = (smartAlbumId: number) =>
  invoke<void>("delete_smart_album", { smartAlbumId });
export const reorderSmartAlbums = (orderedIds: number[]) =>
  invoke<void>("reorder_smart_albums", { orderedIds });
/** Live match count for a rule (drives the builder's preview). */
export const smartAlbumCount = (ruleJson: string) =>
  invoke<number>("smart_album_count", { ruleJson });

// --- taxonomy: tag terms (translations & synonyms) ---

export interface TagTerm {
  id: number;
  tagId: number;
  text: string;
  language: string | null;
  isPrimary: boolean;
  export: boolean;
}

export const listTagTerms = (tagId: number) =>
  invoke<TagTerm[]>("list_tag_terms", { tagId });

export const addTagTerm = (
  tagId: number,
  text: string,
  language: string | null,
  isPrimary: boolean,
  exportFlag: boolean,
) =>
  invoke<number>("add_tag_term", {
    tagId,
    text,
    language,
    isPrimary,
    export: exportFlag,
  });

export const updateTagTerm = (
  termId: number,
  text: string,
  language: string | null,
  isPrimary: boolean,
  exportFlag: boolean,
) =>
  invoke<void>("update_tag_term", {
    termId,
    text,
    language,
    isPrimary,
    export: exportFlag,
  });

export const setTermExport = (termId: number, exportFlag: boolean) =>
  invoke<void>("set_term_export", { termId, export: exportFlag });

export const removeTagTerm = (termId: number) =>
  invoke<void>("remove_tag_term", { termId });

export const listLanguages = () => invoke<string[]>("list_languages");

export const tagExportPreview = (tagId: number, languages: string[]) =>
  invoke<string[]>("tag_export_preview", { tagId, languages });

export const createTag = (path: string) => invoke<number>("create_tag", { path });

export const setTagDescription = (tagId: number, description: string) =>
  invoke<void>("set_tag_description", { tagId, description });

/** Whether a tag is emitted on export (false = organizational; descendants still export). */
export const getTagExportable = (tagId: number) =>
  invoke<boolean>("get_tag_exportable", { tagId });

export const setTagExportable = (tagId: number, exportable: boolean) =>
  invoke<void>("set_tag_exportable", { tagId, exportable });

/** Whether a tag is private (withheld from external/cloud AI; local AI still sees it). */
export const getTagPrivate = (tagId: number) =>
  invoke<boolean>("get_tag_private", { tagId });

/** Mark a tag private or not. With `recursive`, applies to the tag and all descendants
 * (e.g. "People" + every name under it). Returns the number of tags changed. */
export const setTagPrivate = (tagId: number, isPrivate: boolean, recursive: boolean) =>
  invoke<number>("set_tag_private", { tagId, private: isPrivate, recursive });

/** Library-wide: remove redundant ancestor tags (a parent a child already implies).
 * Returns how many assignments were removed. */
export const tidyRedundantTags = () => invoke<number>("tidy_redundant_tags");

export const renameTag = (tagId: number, newName: string) =>
  invoke<void>("rename_tag", { tagId, newName });

export const deleteTag = (tagId: number) => invoke<void>("delete_tag", { tagId });

export const moveTag = (tagId: number, newParentId: number | null) =>
  invoke<void>("move_tag", { tagId, newParentId });

/** Re-apply auto-tags (e.g. monochrome) across the catalog. */
export const applyAutoTags = () => invoke<void>("apply_auto_tags");

export const assignTag = (photoId: number, tagId: number) =>
  invoke<void>("assign_tag", { photoId, tagId });

export const removeTag = (photoId: number, tagId: number) =>
  invoke<void>("remove_tag", { photoId, tagId });

export const getPhotoTags = (photoId: number) =>
  invoke<Tag[]>("get_photo_tags", { photoId });

export interface MetadataEntry {
  key: string;
  groupName: string;
  value: string;
}

export const getPhotoMetadata = (photoId: number) =>
  invoke<MetadataEntry[]>("get_photo_metadata", { photoId });

// --- authored IPTC fields ---

export interface IptcFields {
  description: string;
  headline: string;
  title: string;
  creator: string;
  copyright: string;
  credit: string;
  source: string;
  city: string;
  state: string;
  country: string;
  countryCode: string;
}

export const getIptc = (photoId: number) => invoke<IptcFields>("get_iptc", { photoId });

export const setIptc = (photoId: number, fields: IptcFields) =>
  invoke<void>("set_iptc", { photoId, fields });

// ── H16e — Burst-relative sharpness flagging ─────────────────────────────────

/**
 * Summary returned by `analyze_burst_sharpness` after flagging a photo set.
 */
export interface BurstAnalysisResult {
  /** Total photos considered (the input set). */
  total: number;
  /** Clusters formed by the H15b engine. */
  clusters: number;
  /** Photos flagged as `"soft-in-burst"` (below the threshold). */
  flaggedSoft: number;
  /** Photos crowned as `"sharpest-of-burst"` (one per multi-photo cluster). */
  flaggedBest: number;
  /** Photos whose burst flag was cleared (single-photo clusters). */
  cleared: number;
}

/**
 * Run burst-relative sharpness analysis over the given photo IDs (H16e).
 *
 * Groups photos into H15b burst clusters, then within each cluster:
 * - Flags photos below ~60% of the cluster's median sharpness as `"soft-in-burst"`.
 * - Crowns the sharpest frame `"sharpest-of-burst"`.
 *
 * The `burstFlag` field on `Photo` rows reflects the persisted result.
 * Call `listPhotos` (or a grid refresh) after this returns to show the updated badges.
 *
 * Unscored photos (sharpness IS NULL) are clustered but not flagged.
 * Single-photo clusters have their flags cleared (reset from any previous run).
 *
 * The threshold is read from the `sharpness.burst_soft_threshold` setting (default 0.60).
 */
export const analyzeBurstSharpness = (photoIds: number[]) =>
  invoke<BurstAnalysisResult>("analyze_burst_sharpness", { photoIds });
