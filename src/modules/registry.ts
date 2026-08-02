// The plugin contract — pure types. The runtime host lives in `host.ts`; the
// command wrappers in `api.ts`. A module is one object implementing
// ChairPhotoModule; it may only touch ChairPhotoAPI. See docs/plugin-system.md.

import type { ReactNode } from "react";

export interface Photo {
  id: number;
  uuid: string;
  path: string;
  rating: number;
  label: string;
  pickState: "pick" | "reject" | "none";
  captureTime: string | null;
  /** Original pixel dimensions from EXIF (null if unknown). */
  width: number | null;
  height: number | null;
  /** Promoted EXIF summary fields (null when the file carried none). */
  cameraModel: string | null;
  lens: string | null;
  aperture: number | null;
  shutterSpeed: string | null;
  iso: number | null;
  /** Number of photos stacked under this one (e.g. a camera JPEG under its RAW). */
  stackCount?: number;
  /** The master this photo is stacked under (a derivative's original), or null. */
  stackParentId?: number | null;
  /** Phase A/B boundary for two-phase live scan (I6a): 1 = metadata ready for display;
   *  0 = awaiting metadata extraction (EXIF/IPTC/XMP, thumbnails). */
  metadataReady: number;
  /**
   * Tiled Laplacian-variance sharpness score from the ~1024–2048px preview (H16b).
   * `null` = not yet scored by the background indexer. Higher = sharper somewhere.
   */
  sharpness: number | null;
  /**
   * How the score was computed: `"tile"` | `"face"` | `"afpoint"` (H16c).
   * `null` when `sharpness` is `null`.
   */
  sharpnessMethod: string | null;
  /**
   * Burst-relative sharpness flag set by `analyze_burst_sharpness` (H16e).
   * `null` = not yet analysed (or single-photo cluster).
   * `"soft-in-burst"` = scored below ~60% of cluster median (a likely missed frame).
   * `"sharpest-of-burst"` = sharpest frame in its cluster (the keeper candidate).
   */
  burstFlag: string | null;
}

export interface Tag {
  id: number;
  uuid: string;
  name: string;
  fullPath: string;
  parentId: number | null;
  description: string;
  /** Non-null = an auto-tag (system-managed membership by this rule, e.g. "monochrome"). */
  autoRule: string | null;
  /** Private: withheld from external/cloud AI (people's names etc.); local AI still sees it. */
  private: boolean;
}

export interface TagWithCount extends Tag {
  photoCount: number;
}

/** A record that a photo (a specific version) was published to a platform. */
export interface Publication {
  id: number;
  photoId: number;
  /** The published version, or null for the Original (unedited base). */
  versionId: number | null;
  /** Snapshot of the version's name at publish time (null for the Original). */
  versionName: string | null;
  /** The platform marker (e.g. "instagram"), declared by the publishing module. */
  platform: string;
  url: string | null;
  publishedAt: number;
}

/** A UI panel contributed by a module, rendered at a fixed slot. */
export interface ModulePanel {
  id: string;
  label: string;
  /** "tag-editor" panels render inside the tag editor modal (read the tag being
   *  edited via getEditingTag()). */
  slot: "inspector" | "sidebar" | "loupe" | "tag-editor";
  render(): ReactNode;
}

/**
 * A photo's non-destructive edit record. Core treats this as opaque: it's a JSON
 * object that editing MODULES namespace by their own id (e.g.
 * `{ "basic-editor": { exposure: 0.3 } }`). Core never interprets the contents — it
 * only stores/serves it and routes it to the active renderer. See docs/plugin-system.md.
 */
export type EditRecord = Record<string, unknown>;

/**
 * The render hook: an editing module registers one renderer that turns a photo +
 * its edit record into a displayable image source (e.g. a data/blob/asset URL) for
 * the loupe and export. Core defines the contract but ships no engine; if no module
 * has registered a renderer, consumers fall back to the unedited preview.
 */
export interface EditRenderer {
  /** The owning module id. */
  id: string;
  render(photoId: number, record: EditRecord): Promise<string>;
  /** Optional higher-resolution render of the same record (from the native-size
   *  preview rather than the fast proxy) — the loupe requests it on zoom-in. */
  renderHi?(photoId: number, record: EditRecord): Promise<string>;
}

/**
 * A full-surface view contributed by a module (e.g. a Map). The app shows a view
 * switcher; selecting one replaces the central library area with `render()`. The
 * built-in grid/loupe is the default "Library" view.
 */
export interface MainView {
  id: string;
  label: string;
  /** Small icon shown in the view-switcher tab — prefer a 13px stroke SVG matching
   *  the app's icon language (a plain string also works but renders as text). */
  icon?: ReactNode;
  render(): ReactNode;
}

/**
 * A photo action contributed by a module (toolbar / context). The app shows each as a
 * topbar button. Two flavours, by which field is set:
 * - `onActivate` — a fire-and-forget action: clicking calls it with the host API.
 * - `render` — a modal action: clicking opens an overlay and renders `render(close)`
 *   (the form calls `close` to dismiss). Used by the Collage dialog. If both are set,
 *   `render` wins.
 */
export interface ToolbarAction {
  id: string;
  label: string;
  icon?: string;
  onActivate?(api: ChairPhotoAPI): void;
  render?(close: () => void): ReactNode;
}

/**
 * A publishing destination contributed by a module (Instagram, Flickr, SmugMug, …),
 * shown in the app's unified Publish dialog. `render` returns the destination's form
 * (it reads the active photo/version via the host API and does the upload + records the
 * publication). One per publishing module.
 */
export interface PublishTarget {
  id: string;
  label: string;
  render(): ReactNode;
}

/**
 * Stops an `onEvent` subscription. Contract-owned on purpose: the host adapts Tauri's
 * `UnlistenFn` to this, so no Tauri type appears in the module surface.
 */
export type Unsubscribe = () => void;

/** The only surface a module may use. The host injects an instance per module. */
export interface ChairPhotoAPI {
  // selection & data
  getSelectedPhotos(): Photo[];
  getActivePhotoId(): number | null;
  /** The tag open in the tag editor modal, or null — for "tag-editor" slot panels. */
  getEditingTag(): Tag | null;
  /** The version selected in the inspector/loupe, or null for the Original — so a
   *  publishing module can default to publishing exactly what the user is viewing. */
  getActiveVersionId(): number | null;
  listTags(): Promise<TagWithCount[]>;
  assignTag(photoId: number, tagId: number): Promise<void>;
  // publications: record that a photo (version null = Original) was published BY THIS
  // module. The platform marker is the module's `publicationMarker` (or its id) — the
  // host stamps it, so a module never passes (or can mistake) the platform string.
  recordPublication(photoId: number, versionId: number | null, url?: string): Promise<void>;
  listPublications(photoId: number): Promise<Publication[]>;
  deletePublication(id: number): Promise<void>;
  // backend (the module's own commands; gated to its Cargo feature)
  invoke<T>(command: string, args?: Record<string, unknown>): Promise<T>;
  /**
   * Subscribe to a backend event (e.g. a module's own `"<id>:progress"`), receiving the
   * already-unwrapped payload. Await the returned `Unsubscribe` and call it to stop.
   *
   * This is the only supported way for a module to observe backend events — a module must
   * never import Tauri's `listen` itself, so that nothing in the contract leaks Tauri types.
   *
   * **Optional member** (added after the initial host API): it is `undefined` on older
   * hosts. Check before calling, or declare a `minHostVersion` floor covering the first
   * host that shipped it.
   */
  onEvent?<T>(event: string, handler: (payload: T) => void): Promise<Unsubscribe>;
  // module-scoped settings (auto-namespaced by module id)
  getSetting(key: string): Promise<string | null>;
  setSetting(key: string, value: string): Promise<void>;
  // non-destructive edit record (opaque JSON; the module owns its namespaced section)
  getEditRecord(photoId: number): Promise<EditRecord | null>;
  setEditRecord(photoId: number, record: EditRecord): Promise<void>;
  // UI contributions
  registerPanel(panel: ModulePanel): void;
  registerAction(action: ToolbarAction): void;
  /** Register a publishing destination shown in the app's Publish dialog. */
  registerPublishTarget(target: PublishTarget): void;
  registerSettingsPanel(render: () => ReactNode): void;
  /** Register a full-surface main view, shown via the app's view switcher. */
  registerMainView(view: MainView): void;
  /** Register the edit renderer (one per app; the editing module provides it). */
  registerEditRenderer(renderer: EditRenderer): void;
  showToast(message: string): void;
  /** Ask the app to refresh (e.g. after a module assigns a tag). */
  notifyChange(): void;
  /** Return to the Library view and filter it to a tag (e.g. clicking a graph node). */
  filterByTag(tagId: number): void;
  /** Return to the Library view and select a photo by id. */
  selectPhoto(photoId: number): void;
  /**
   * Set the app-wide photo selection WITHOUT switching the active view. The pop-out
   * loupe (which follows the selection) will update; the current module view stays open.
   * Use this when a module wants to "preview" a photo via the loupe without leaving its
   * own surface (e.g. the Map filmstrip).
   */
  selectPhotoSilent(photoId: number): void;
  /** The Library's current sidebar scope (tag/album/batch filters), for modules that
   *  want to reflect it (e.g. Statistics). Values are null when not filtered. */
  getFilterContext(): { tagId: number | null; albumId: number | null; batchId: number | null };
}

/** A chairphoto module. Bundled and enabled/disabled at runtime. */
export interface ChairPhotoModule {
  id: string;
  name: string;
  version: string;
  description?: string;
  /** Cargo feature its Rust backend needs (e.g. "ai"); omit if pure-frontend. */
  backendFeature?: string;
  /**
   * Marker stamped on photos this module publishes (recorded in the `publications`
   * table, e.g. "instagram"). A publishing module must declare what its photos are
   * marked with; if omitted, the host falls back to the module `id`. Resolve it via
   * `getPublicationMarker(moduleId)` — the backend never invents a platform string.
   */
  publicationMarker?: string;
  /**
   * Other modules this one depends on. The host enables required modules FIRST (deps
   * before dependents) and refuses to enable this one if a requirement is missing, its
   * `backendFeature` isn't compiled, or its `version` doesn't satisfy the given range.
   * Disabling a module cascade-disables any enabled module that requires it.
   * `version` is a semver range understood by `satisfies()` in host.ts (omit/`*` = any,
   * exact `X.Y.Z`, `^X.Y.Z`, `>=X.Y.Z`). e.g. `[{ id: "localsend", version: "^0.1.0" }]`.
   */
  requires?: { id: string; version?: string }[];
  onLoad(api: ChairPhotoAPI): void;
  onUnload?(): void;
  onPhotoSelected?(photos: Photo[], api: ChairPhotoAPI): void;
}
