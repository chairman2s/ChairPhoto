// Face Tagging module — H13d: Loupe overlay + review UI + settings panel.
//
// Architecture:
//   • `backendFeature: "faces"` — requires the Rust `faces` Cargo feature.
//   • Inspector panel ("Faces"): lists all faces for the active photo with per-face
//     confirm / reject / reassign / ignore actions.
//   • Loupe overlay (slot: "loupe"): face rectangles + name chips overlaid on the
//     displayed photo, computing letterbox geometry from the image's natural size and
//     the container's rendered dimensions.
//   • Settings panel: people root path, similarity threshold, model download status,
//     "Index faces" + "Run matching" actions with progress display and cancel.
//
// Coordinate mapping:
//   The backend stores bboxes normalized 0–1 relative to the oriented image (same
//   coordinate space as the `preview://` image served to the loupe). The loupe renders
//   the image with CSS `object-fit: contain` (letterboxing). To map bbox coordinates to
//   screen coordinates we need the letterbox offset and the scale factor:
//
//     letterbox: intrinsic image W/H vs. container W/H.
//     scale_x = min(cW / iW, cH / iH)   (uniform because object-fit: contain)
//     rendered_w = iW * scale_x
//     rendered_h = iH * scale_x
//     offset_x = (cW - rendered_w) / 2
//     offset_y = (cH - rendered_h) / 2
//
//   Then (fit-zoom, in container px):
//     fit_x = offset_x + bbox.x * rendered_w
//     fit_y = offset_y + bbox.y * rendered_h
//     fit_w =            bbox.w * rendered_w
//     fit_h =            bbox.h * rendered_h
//
//   Zoom/pan: ZoomableImage applies `translate(tx, ty) scale(s)` to the <img>, whose
//   transform-origin (its own center, since it is a centered flex child) coincides with
//   the container center C. The overlay parses that transform and maps every fit-zoom
//   point through the same affine map so the rectangles track the image exactly:
//     screen_p = C + (fit_p − C) * s + (tx, ty)
//     screen_w = fit_w * s,  screen_h = fit_h * s

import { useCallback, useEffect, useRef, useState } from "react";
import type { ChairPhotoAPI, ChairPhotoModule, Tag } from "../registry";
import { useHostSelection } from "../host";
import "./faces.css";
import { thumbnailUrl, createTag } from "../api";

// ── Backend commands (owned by this module) ───────────────────────────────────
// Per the module contract, a module's own commands go through `ChairPhotoAPI.invoke`
// rather than core's `api.ts`, so the command names travel with the module.
//
// The helpers take the narrowest host-API slice they need rather than the whole
// `ChairPhotoAPI`. That keeps `FaceOverlayApi` — the deliberately small shim
// `LoupeWindow` passes in — able to satisfy them without pulling in the full contract.

/** The host-API subset the command helpers need: just `invoke`. */
type FacesCommandApi = Pick<ChairPhotoAPI, "invoke">;

/** The subset the two progress subscriptions need: just the optional `onEvent`. */
type FacesEventApi = Pick<ChairPhotoAPI, "onEvent">;

/**
 * One detected face row, as returned by the backend. Bounding box is normalized
 * (0–1) relative to the full oriented image (the same coordinate space as the
 * displayed preview image).
 */
interface FaceBbox {
  x: number;
  y: number;
  w: number;
  h: number;
}

/** State of a face row in the catalog. */
type FaceState =
  | "unassigned"
  | "suggested"
  | "confirmed"
  | "rejected"
  | "ignored";

/** Source that produced the assignment/suggestion. `drawn` = user-drawn box for an
 * undetected face (no embedding; becomes `manual` once a person is assigned). */
type FaceSource = "detect" | "seed" | "match" | "manual" | "xmp" | "drawn";

/** A face row returned for display (loupe overlay + review panel). */
interface FaceRow {
  id: number;
  photoId: number;
  bbox: FaceBbox;
  detectConfidence: number;
  /** Assigned person tag id, or null if unassigned. */
  personTagId: number | null;
  /** Leaf name of the person tag (null when unassigned). */
  personName: string | null;
  state: FaceState;
  matchConfidence: number | null;
  source: FaceSource;
}

/** Progress event for `faces_index_photos` (`faces:progress`). */
interface FacesProgress {
  done: number;
  total: number;
  /** Which indexing run this belongs to. Events from a superseded job carry its old id —
   * filter on this to ignore them. */
  job: number;
}

/**
 * Progress event for `faces_run_matching` (`faces:match_progress`).
 *
 * Matching used to share `faces:progress` and be identified by having no `job` field.
 * It is a real job now, so it has its own id and its own event: two job families cannot
 * be told apart by a missing field once both have one.
 */
interface FacesMatchProgress {
  done: number;
  total: number;
  /** Pipeline step label, e.g. "clustering unknowns". `total` restarts each step. */
  phase: string;
  /** Which matching run this belongs to. */
  job: number;
}

/** Per-model presence report, as returned inside `FacesModelsStatus`. */
interface FacesModelReport {
  key: string;
  filename: string;
  present: boolean;
  path: string;
  detail: string | null;
}

/** Model download status returned by `faces_models_status`. */
interface FacesModelsStatus {
  /** True only when every model is present and verified — the engine is usable. */
  ready: boolean;
  models: FacesModelReport[];
}

/** List all face rows for a photo (used by the loupe overlay and review panel). */
const facesForPhoto = (api: FacesCommandApi, photoId: number) =>
  api.invoke<FaceRow[]>("faces_for_photo", { photoId });

/** Accept a face suggestion: sets state=confirmed, assigns the person tag to the photo. */
const faceConfirm = (api: FacesCommandApi, faceId: number) =>
  api.invoke<void>("faces_accept", { faceId });

/** Reject a face suggestion: sets state=rejected so it is never re-proposed. */
const faceReject = (api: FacesCommandApi, faceId: number) =>
  api.invoke<void>("faces_reject", { faceId });

/** Ignore a face (photobombers etc.): excluded from centroids and suggestions. */
const faceIgnore = (api: FacesCommandApi, faceId: number) =>
  api.invoke<void>("faces_ignore", { faceId });

/**
 * Reassign a face to a different person tag (by tag id). Sets state=confirmed and
 * assigns the new person tag to the photo; removes any previous person-tag assignment
 * for this face if different.
 */
const faceReassign = (api: FacesCommandApi, faceId: number, tagId: number) =>
  api.invoke<void>("faces_assign", { faceId, tagId });

/**
 * Insert a manually drawn face box for a face the detector missed. `bbox` is
 * normalized 0–1 in oriented-image space. Returns the new face id (unassigned,
 * source='drawn'); assign a person via `faceReassign`.
 */
const facesAddManual = (api: FacesCommandApi, photoId: number, bbox: FaceBbox) =>
  api.invoke<number>("faces_add_manual", {
    photoId,
    x: bbox.x,
    y: bbox.y,
    w: bbox.w,
    h: bbox.h,
  });

/** Delete a drawn, still-unassigned face box (a mis-draw). Drawn boxes only. */
const facesDeleteDrawn = (api: FacesCommandApi, faceId: number) =>
  api.invoke<void>("faces_delete_drawn", { faceId });

/**
 * Return download + verification status for the face model files (YuNet + AuraFace).
 * `ready` is true only when both models are present and SHA-256-verified.
 */
const facesModelsStatus = (api: FacesCommandApi) =>
  api.invoke<FacesModelsStatus>("faces_models_status");

/** Trigger a model download, returning the post-download status. Emits no progress:
 *  `faces_download_models` (commands/faces.rs:26) takes no AppHandle and `models::ensure_all`
 *  has no emitter, so there is nothing to subscribe to for this call. */
const facesDownloadModels = (api: FacesCommandApi) =>
  api.invoke<FacesModelsStatus>("faces_download_models");

/** Where face inference actually runs + the indexing-speed plan (Faces settings panel). */
interface FacesInferenceInfo {
  /** "cuda" | "cpu" | "unbuilt" (no inference has run yet this session). */
  ep: "cuda" | "cpu" | "unbuilt";
  /** Whether the running binary was compiled with the `faces-cuda` feature. */
  cudaBuilt: boolean;
  /** Effective `indexing.speed` setting. */
  speed: "background" | "full";
}

const facesInferenceInfo = (api: FacesCommandApi) =>
  api.invoke<FacesInferenceInfo>("faces_inference_info");

/** Persist `indexing.speed`. Takes effect on the next app start (pool built once). */
const facesSetIndexingSpeed = (api: FacesCommandApi, speed: "background" | "full") =>
  api.invoke<void>("faces_set_indexing_speed", { speed });

/** Terminal event for the face-indexing job (`faces:index_done`). */
interface FacesIndexDone {
  ok: boolean;
  done: number;
  total: number;
  /** Photos skipped because no reachable copy exists (NAS offline). Still queued. */
  offline: number;
  /** Photos whose preview couldn't be generated (unreadable file). Still queued. */
  failed: number;
  /** True when the run stopped early (cancel, superseded by a newer run, catalog switch). */
  aborted: boolean;
  /** Which run finished — compare against the id `facesIndexPhotos` returned. */
  job: number;
  error: string | null;
}

/** Terminal event for `faces_run_matching` (`faces:match_done`). */
interface FacesMatchDone {
  ok: boolean;
  /** The run's counters, or null when it failed before producing any. */
  outcome: MatchOutcome | null;
  /** True when the run stopped early (cancel, superseded by a newer run, catalog switch). */
  aborted: boolean;
  /** Which run finished — compare against the id `facesRunMatching` returned. */
  job: number;
  error: string | null;
}

/** Snapshot of a running face-matching job (`faces_match_status`), null when idle. */
interface FacesMatchJobStatus {
  job: number;
  done: number;
  total: number;
  phase: string;
}

/** Snapshot of a running face-indexing job (`faces_index_status`), null when idle. */
interface FacesJobStatus {
  job: number;
  done: number;
  total: number;
}

/** Counters returned by `faces_run_matching`, so the UI can say what a run did. */
interface MatchOutcome {
  /** Faces auto-seeded to confirmed (1 face + 1 person tag). */
  seeded: number;
  /** Faces suggested by constrained (Hungarian) matching against the photo's own tags. */
  constrained: number;
  /** Faces suggested by open nearest-centroid matching. */
  open: number;
  /** Faces assigned to an unnamed cluster. */
  clustered: number;
  /** Distinct people that had a usable centroid this run. */
  people: number;
}

/**
 * Start (or resume) the background face-indexing job for all unindexed photos.
 * Returns the new job's id as soon as the job has STARTED; progress streams via
 * `faces:progress` and completion arrives as a terminal `faces:index_done` event,
 * both carrying the job id. Starting a new job aborts a running one.
 */
const facesIndexPhotos = (api: FacesCommandApi) =>
  api.invoke<number>("faces_index_photos");

/** Snapshot of the running face-indexing job, or null when idle. */
const facesIndexStatus = (api: FacesCommandApi) =>
  api.invoke<FacesJobStatus | null>("faces_index_status");

/**
 * Start the seed/match/cluster pass over already-indexed faces. Should be called after
 * indexing.
 *
 * Returns the new job's id as soon as the job has STARTED — the counters arrive later on
 * the terminal `faces:match_done` event, not from this promise. Starting a new match
 * aborts a running one.
 */
const facesRunMatching = (api: FacesCommandApi) =>
  api.invoke<number>("faces_run_matching");

/** Snapshot of the running face-matching job, or null when idle. */
const facesMatchStatus = (api: FacesCommandApi) =>
  api.invoke<FacesMatchJobStatus | null>("faces_match_status");

/** Cancel an in-flight face-matching job. Its terminal event still arrives. */
const facesCancelMatch = (api: FacesCommandApi) =>
  api.invoke<void>("faces_match_cancel");

/**
 * Subscribe to face-indexing progress events (`faces:progress`).
 *
 * `onEvent` is an optional host-API member, so this resolves to `null` on a host that
 * predates it. Callers must treat null as "no progress updates will arrive" rather than
 * assuming a live subscription.
 */
const onFacesProgress = (
  api: FacesEventApi,
  handler: (p: FacesProgress) => void,
): Promise<(() => void) | null> =>
  api.onEvent?.<FacesProgress>("faces:progress", handler) ?? Promise.resolve(null);

/**
 * Subscribe to the face-indexing terminal event (`faces:index_done`).
 *
 * Resolves to `null` when the host has no `onEvent`; see `onFacesProgress`.
 */
const onFacesIndexDone = (
  api: FacesEventApi,
  handler: (p: FacesIndexDone) => void,
): Promise<(() => void) | null> =>
  api.onEvent?.<FacesIndexDone>("faces:index_done", handler) ?? Promise.resolve(null);

/**
 * Subscribe to face-matching progress events (`faces:match_progress`).
 *
 * Resolves to `null` when the host has no `onEvent`; see `onFacesProgress`.
 */
const onFacesMatchProgress = (
  api: FacesEventApi,
  handler: (p: FacesMatchProgress) => void,
): Promise<(() => void) | null> =>
  api.onEvent?.<FacesMatchProgress>("faces:match_progress", handler) ?? Promise.resolve(null);

/**
 * Subscribe to the face-matching terminal event (`faces:match_done`).
 *
 * Resolves to `null` when the host has no `onEvent`; see `onFacesProgress`.
 */
const onFacesMatchDone = (
  api: FacesEventApi,
  handler: (p: FacesMatchDone) => void,
): Promise<(() => void) | null> =>
  api.onEvent?.<FacesMatchDone>("faces:match_done", handler) ?? Promise.resolve(null);

/** Cancel an in-flight faces index job. */
const facesCancelJob = (api: FacesCommandApi) =>
  api.invoke<void>("faces_index_cancel");

// ── People-view summary types & queries (H13e) ───────────────────────────────

/** A named person with face/photo counts and a representative face for the avatar. */
interface PersonSummary {
  tagId: number;
  name: string;
  fullPath: string;
  faceCount: number;
  photoCount: number;
  /** Photo id to load the avatar thumbnail from. */
  avatarPhotoId: number;
  /** Normalized bbox of the representative face within that photo (0–1). */
  avatarBbox: FaceBbox;
}

/** An unnamed cluster with member count and a representative face. */
interface ClusterSummary {
  clusterId: number;
  memberCount: number;
  avatarPhotoId: number;
  avatarBbox: FaceBbox;
}

/** One suggested face row for the review queue. */
interface SuggestionEntry {
  faceId: number;
  photoId: number;
  bbox: FaceBbox;
  personTagId: number;
  personName: string;
  personFullPath: string;
  confidence: number;
}

/** Return all named people with face/photo counts + avatar data. */
const facesPeopleSummary = (api: FacesCommandApi) =>
  api.invoke<PersonSummary[]>("faces_people_summary");

/** Return all unnamed clusters with member count + avatar data. */
const facesClusterSummary = (api: FacesCommandApi) =>
  api.invoke<ClusterSummary[]>("faces_cluster_summary");

/** Return all suggested (unconfirmed) face rows, ordered by confidence descending. */
const facesSuggestionList = (api: FacesCommandApi) =>
  api.invoke<SuggestionEntry[]>("faces_suggestion_list");

// ── Minimal API surface required by FaceOverlay ───────────────────────────────
// This interface is a subset of ChairPhotoAPI. It is intentionally narrow so
// that FaceOverlay can be rendered from non-module contexts (e.g. LoupeWindow)
// by passing a small shim instead of the full ChairPhotoAPI.
//
// It extends FacesCommandApi because the overlay confirms/rejects/draws faces, so it
// genuinely needs `invoke`. That dependency always existed — it was just hidden while
// the wrappers lived in core `api.ts` and took no host handle.
export interface FaceOverlayApi extends FacesCommandApi {
  listTags(): Promise<Tag[]>;
  getSetting(key: string): Promise<string | null>;
  notifyChange(): void;
}

// ── Letterbox geometry helper ─────────────────────────────────────────────────

interface LetterboxGeom {
  /** Left offset of the rendered image within the container, as a fraction 0–1. */
  ox: number;
  /** Top offset of the rendered image within the container, as a fraction 0–1. */
  oy: number;
  /** Rendered image width as a fraction of the container width. */
  fw: number;
  /** Rendered image height as a fraction of the container height. */
  fh: number;
}

/**
 * Compute the letterbox geometry for `object-fit: contain`. The result is expressed
 * as fractions of the container so it can be applied via percentage CSS.
 */
function computeLetterbox(
  containerW: number,
  containerH: number,
  imageNaturalW: number,
  imageNaturalH: number,
): LetterboxGeom {
  if (containerW <= 0 || containerH <= 0 || imageNaturalW <= 0 || imageNaturalH <= 0) {
    return { ox: 0, oy: 0, fw: 1, fh: 1 };
  }
  const scaleX = containerW / imageNaturalW;
  const scaleY = containerH / imageNaturalH;
  const scale = Math.min(scaleX, scaleY);
  const rw = imageNaturalW * scale;
  const rh = imageNaturalH * scale;
  return {
    ox: (containerW - rw) / 2 / containerW,
    oy: (containerH - rh) / 2 / containerH,
    fw: rw / containerW,
    fh: rh / containerH,
  };
}

/** The zoom/pan transform ZoomableImage applies to the loupe <img>. */
interface ViewTransform {
  s: number;
  tx: number;
  ty: number;
}

const FIT_VIEW: ViewTransform = { s: 1, tx: 0, ty: 0 };

/** Parse `translate(Apx, Bpx) scale(S)` from the zoom-img inline style. */
function parseViewTransform(transform: string): ViewTransform {
  const t = transform.match(/translate\((-?[\d.]+)px,\s*(-?[\d.]+)px\)/);
  const m = transform.match(/scale\(([^)]+)\)/);
  return {
    s: m ? parseFloat(m[1]) || 1 : 1,
    tx: t ? parseFloat(t[1]) || 0 : 0,
    ty: t ? parseFloat(t[2]) || 0 : 0,
  };
}

/**
 * Map a normalized bbox (0–1 in image space) to CSS pixel positions within the
 * container, accounting for letterboxing and the current zoom/pan transform.
 */
function bboxToCss(
  bbox: FaceBbox,
  geom: LetterboxGeom,
  cw: number,
  ch: number,
  view: ViewTransform,
) {
  // Fit-zoom position in container px.
  const fx = (geom.ox + bbox.x * geom.fw) * cw;
  const fy = (geom.oy + bbox.y * geom.fh) * ch;
  const fw = bbox.w * geom.fw * cw;
  const fh = bbox.h * geom.fh * ch;
  // Same affine map as the img transform (origin = container center).
  const left = cw / 2 + (fx - cw / 2) * view.s + view.tx;
  const top = ch / 2 + (fy - ch / 2) * view.s + view.ty;
  return {
    left: Math.round(left),
    top: Math.round(top),
    width: Math.round(fw * view.s),
    height: Math.round(fh * view.s),
  };
}

/**
 * Inverse of `bboxToCss` for a single point: container px → normalized image
 * coordinates (may fall outside 0–1 when the point is in the letterbox area).
 */
function screenToImage(
  px: number,
  py: number,
  geom: LetterboxGeom,
  cw: number,
  ch: number,
  view: ViewTransform,
): { x: number; y: number } {
  const fx = (px - view.tx - cw / 2) / view.s + cw / 2;
  const fy = (py - view.ty - ch / 2) / view.s + ch / 2;
  return {
    x: (fx / cw - geom.ox) / geom.fw,
    y: (fy / ch - geom.oy) / geom.fh,
  };
}

/** In-progress manually drawn rectangle, in container px (anchor → current corner). */
interface DraftBox {
  x0: number;
  y0: number;
  x1: number;
  y1: number;
}

// ── Module-level settings keys ────────────────────────────────────────────────
// Declared here (before the first component that reads them) so both FaceOverlay
// and FacesInspectorPanel can reference them without a forward-declaration issue.

// Host-namespaced to "faces.people_root" / "faces.match_threshold" — must match
// PEOPLE_ROOT_SETTING / THRESHOLD_SETTING in src-tauri/src/plugins/faces/matcher.rs,
// which the Rust matching engine reads.
/**
 * Shown when the indexing terminal event cannot be subscribed to. `ChairPhotoAPI.onEvent`
 * is optional, and indexing is the one faces flow that cannot degrade without it: the
 * panel would enter "indexing" and never leave. An extracted build of this module should
 * declare a `minHostVersion` covering the host that introduced `onEvent`.
 */
const NO_EVENT_SUPPORT =
  "Face indexing needs backend event support, which this app version does not provide. " +
  "Update ChairPhoto to index faces.";

/** Distinct from the above: the API exists but this particular subscription failed. */
const EVENT_SUBSCRIBE_FAILED =
  "Could not subscribe to indexing events, so the run was not started. Try again.";

/** Reattach variant — nothing was started here, an existing run just cannot be followed. */
const REATTACH_SUBSCRIBE_FAILED =
  "An indexing run is in progress but could not be followed (event subscription failed). " +
  "It continues in the background.";

/** The validation status query itself failed, so backend state is unknown. */
const REATTACH_VALIDATE_FAILED =
  "Could not confirm indexing state, so this panel is not tracking it. " +
  "Any running job continues in the background.";

/** Reattach variant of the unsupported-host case: nothing was started here. */
const REATTACH_NO_EVENT_SUPPORT =
  "An indexing run is in progress but this app version cannot follow it (no backend event " +
  "support). It continues in the background.";

/** Outcome of acquiring the required `faces:index_done` listener. */
type DoneAcquire = "ok" | "unsupported" | "failed";

/** What the progress bar renders. Indexing has no `phase`; matching always does. */
type ProgressDisplay = { done: number; total: number; phase?: string };

/** A live subscription tagged with the lease token that installed it, so only its owner
 *  can release it. */
interface OwnedListener {
  owner: symbol;
  stop: () => void;
}

const SETTING_PEOPLE_ROOT = "people_root";
const SETTING_THRESHOLD = "match_threshold";
const DEFAULT_THRESHOLD = "0.45";

// ── Per-face chip colors ──────────────────────────────────────────────────────

function chipColor(state: FaceRow["state"]): string {
  switch (state) {
    case "confirmed": return "#10b981";
    case "suggested": return "#f59e0b";
    case "rejected":  return "#ef4444";
    case "ignored":   return "#64748b";
    default:          return "#3b82f6";
  }
}

// ── People-tag loading (shared by the overlay + inspector panel) ─────────────

/**
 * Load the tags offered in the person picker: descendants of the configured people
 * root (e.g. People/…), or all non-auto-rule tags when no root is set. Returns the
 * root too so callers can create new person tags under it.
 */
async function loadPeopleTags(
  api: FaceOverlayApi
): Promise<{ root: string; tags: Tag[] }> {
  const [tags, root] = await Promise.all([
    api.listTags(),
    api.getSetting(SETTING_PEOPLE_ROOT).catch(() => ""),
  ]);
  const peopleRoot = root?.trim() ?? "";
  return {
    root: peopleRoot,
    tags: peopleRoot
      ? tags.filter(
          (t) =>
            t.fullPath === peopleRoot ||
            t.fullPath.startsWith(peopleRoot + "/")
        )
      : tags.filter((t) => !t.autoRule),
  };
}

// ── PersonPicker — searchable person-tag list ────────────────────────────────
//
// Shared by the loupe-overlay reassign dropdown and the inspector-panel reassign
// row. A filter input on top (autofocused), the matching tags below; ArrowUp/Down
// move the highlight, Enter picks it, Escape cancels.

interface PersonPickerProps {
  tags: Tag[];
  /** Tag currently assigned to the face — rendered highlighted. */
  currentTagId: number | null;
  onPick(tag: Tag): void;
  onCancel(): void;
  /** When set, a typed name with no exact tag match offers a "Create" row. */
  onCreate?(name: string): void;
}

function PersonPicker({ tags, currentTagId, onPick, onCancel, onCreate }: PersonPickerProps) {
  const [query, setQuery] = useState("");
  const [highlight, setHighlight] = useState(0);
  const listRef = useRef<HTMLDivElement>(null);

  const q = query.trim().toLowerCase();
  const filtered = q
    ? tags.filter((t) => t.fullPath.toLowerCase().includes(q))
    : tags;
  // Offer creating a new person when the typed name matches no existing tag exactly
  // (leaf name or full path) — this is how a face gets assigned to someone new.
  const canCreate =
    !!onCreate &&
    q.length > 0 &&
    !filtered.some(
      (t) => t.name.toLowerCase() === q || t.fullPath.toLowerCase() === q
    );
  const rowCount = filtered.length + (canCreate ? 1 : 0);
  const clamped = Math.min(highlight, Math.max(0, rowCount - 1));

  // Keep the highlighted row visible while arrowing through a long list.
  useEffect(() => {
    listRef.current
      ?.children[clamped]?.scrollIntoView({ block: "nearest" });
  }, [clamped]);

  return (
    <>
      <input
        className="tag-input"
        type="text"
        autoFocus
        value={query}
        placeholder={onCreate ? "Search or create person…" : "Search person…"}
        spellCheck={false}
        onChange={(e) => { setQuery(e.target.value); setHighlight(0); }}
        onKeyDown={(e) => {
          if (e.key === "ArrowDown" && rowCount) {
            e.preventDefault();
            setHighlight((h) => Math.min(h + 1, rowCount - 1));
          } else if (e.key === "ArrowUp" && rowCount) {
            e.preventDefault();
            setHighlight((h) => Math.max(h - 1, 0));
          } else if (e.key === "Enter" && rowCount) {
            e.preventDefault();
            if (clamped < filtered.length) onPick(filtered[clamped]);
            else onCreate?.(query.trim());
          } else if (e.key === "Escape") {
            e.stopPropagation();
            onCancel();
          }
        }}
        style={{
          width: "100%",
          boxSizing: "border-box",
          margin: "2px 0 4px",
          fontSize: 12,
        }}
      />
      <div ref={listRef} style={{ maxHeight: 160, overflowY: "auto" }}>
        {filtered.map((tag, i) => (
          <button
            key={tag.id}
            className="fa-drop-item"
            style={{
              display: "block",
              width: "100%",
              textAlign: "left",
              padding: "4px 10px",
              fontSize: 12,
              color: tag.id === currentTagId ? "#10b981" : "#f8fafc",
              background: i === clamped ? "rgba(59,130,246,0.15)" : "none",
              border: "none",
              cursor: "pointer",
            }}
            onMouseEnter={() => setHighlight(i)}
            onClick={() => onPick(tag)}
          >
            {tag.fullPath}
          </button>
        ))}
        {canCreate && (
          <button
            className="fa-drop-item"
            style={{
              display: "block",
              width: "100%",
              textAlign: "left",
              padding: "4px 10px",
              fontSize: 12,
              color: "#3b82f6",
              background:
                clamped === filtered.length ? "rgba(59,130,246,0.15)" : "none",
              border: "none",
              cursor: "pointer",
            }}
            onMouseEnter={() => setHighlight(filtered.length)}
            onClick={() => onCreate?.(query.trim())}
          >
            ＋ Create “{query.trim()}”
          </button>
        )}
        {rowCount === 0 && (
          <div style={{ padding: "4px 10px", fontSize: 11, opacity: 0.6 }}>
            {tags.length === 0 ? "No tags found" : "No match"}
          </div>
        )}
      </div>
    </>
  );
}

// ── FaceOverlay — loupe overlay component ────────────────────────────────────
//
// Renders face rectangles + name chips over the loupe container. The overlay div is
// absolute-positioned to fill the `.zoom-container` (which has `position: relative`),
// so we can position face rects with percentage CSS coordinates using the letterbox math
// above.
//
// The image's natural dimensions are obtained by observing the <img> inside the
// `.zoom-container` once it loads. A ResizeObserver on the container keeps the geometry
// up to date on window resize.

interface FaceOverlayProps {
  /** The currently selected photo id (null = no selection). */
  photoId: number | null;
  /** Minimal API shim — subset of ChairPhotoAPI so FaceOverlay can be used in
   *  non-module contexts (e.g. the pop-out LoupeWindow). */
  api: FaceOverlayApi;
}

export function FaceOverlay({ photoId, api }: FaceOverlayProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const [faces, setFaces] = useState<FaceRow[]>([]);
  const [geom, setGeom] = useState<LetterboxGeom>({ ox: 0, oy: 0, fw: 1, fh: 1 });
  // Container size in px — needed to run bbox positions through the zoom transform.
  const [size, setSize] = useState({ w: 0, h: 0 });
  // Current zoom/pan transform on the loupe <img>; face boxes are mapped through it.
  const [view, setView] = useState<ViewTransform>(FIT_VIEW);
  // Per-face reassign dropdown open state: maps faceId → open
  const [reassigning, setReassigning] = useState<number | null>(null);
  const [peopleTags, setPeopleTags] = useState<Tag[]>([]);
  const [peopleRoot, setPeopleRoot] = useState("");
  // Manual face drawing (for faces the detector missed): "＋ face" arms draw mode,
  // then one drag on the image creates the box.
  const [drawMode, setDrawMode] = useState(false);
  const [draft, setDraft] = useState<DraftBox | null>(null);
  // Whether the face rectangles are drawn over the photo. Persisted so the
  // preference survives restarts (same pattern as inspector section state).
  const [showBoxes, setShowBoxes] = useState(
    () => localStorage.getItem("faces.showBoxes") !== "0",
  );
  const toggleBoxes = useCallback(() => {
    setShowBoxes((v) => {
      localStorage.setItem("faces.showBoxes", v ? "0" : "1");
      return !v;
    });
  }, []);

  // Load faces when the photo changes.
  useEffect(() => {
    if (photoId == null) {
      setFaces([]);
      return;
    }
    let alive = true;
    facesForPhoto(api, photoId)
      .then((rows) => { if (alive) setFaces(rows); })
      .catch(() => { if (alive) setFaces([]); });
    return () => { alive = false; };
  }, [photoId, api]);

  // Load people tags for the reassign dropdown. Re-fetched every time a picker
  // opens so tags created since mount (e.g. in the TagPanel) are offered too.
  const loadPeople = useCallback(async () => {
    try {
      const { root, tags } = await loadPeopleTags(api);
      setPeopleRoot(root);
      setPeopleTags(tags);
    } catch {
      // ignore
    }
  }, [api]);

  useEffect(() => { void loadPeople(); }, [loadPeople]);
  useEffect(() => {
    if (reassigning != null) void loadPeople();
  }, [reassigning, loadPeople]);

  // Compute letterbox geometry + track the zoom/pan transform.
  //
  // The overlay div fills the same dimensions as the .zoom-container sibling (both
  // are flex children of the same wrapper). We measure our own bounding rect for the
  // container dimensions, then find the <img> inside .zoom-container for
  // naturalWidth/naturalHeight.
  //
  // Zoom tracking: ZoomableImage applies `transform: translate(tx, ty) scale(s)` to
  // the <img> on every wheel/drag event. The MutationObserver below sees each style
  // change; we parse the transform into state so the face rectangles are re-mapped
  // through the same affine transform and stay glued to the image.
  useEffect(() => {
    const overlay = containerRef.current;
    if (!overlay) return;

    // Use closest() rather than a fixed parentElement depth so the traversal stays
    // correct even if App.tsx adds or removes wrapper levels around ZoomableImage.
    // Also supports .loupe-window (the pop-out loupe window) which uses the same
    // .zoom-container structure but without a .loupe-inline ancestor.
    const getImg = (): HTMLImageElement | null => {
      const loupeRoot =
        overlay.closest(".loupe-inline") ?? overlay.closest(".loupe-window");
      if (!loupeRoot) return null;
      const zoomContainer = loupeRoot.querySelector(".zoom-container");
      return zoomContainer
        ? (zoomContainer.querySelector("img") as HTMLImageElement | null)
        : null;
    };

    const measure = () => {
      const img = getImg();
      if (!img || !img.naturalWidth) return;
      const cw = overlay.clientWidth;
      const ch = overlay.clientHeight;
      if (cw <= 0 || ch <= 0) return;
      setSize((prev) => (prev.w === cw && prev.h === ch ? prev : { w: cw, h: ch }));
      setGeom(computeLetterbox(cw, ch, img.naturalWidth, img.naturalHeight));
    };

    // Read the current zoom/pan transform off the zoom-img. Bails to the previous
    // state object when unchanged so the MutationObserver → setState → render cycle
    // settles (React skips the re-render on identical state).
    const checkZoom = () => {
      const img = getImg();
      const next = img ? parseViewTransform(img.style.transform ?? "") : FIT_VIEW;
      setView((prev) =>
        prev.s === next.s && prev.tx === next.tx && prev.ty === next.ty ? prev : next
      );
    };

    // Re-measure when the image loads (may not be ready on first render). Attached
    // before the initial measure() so a load landing in between can't be missed.
    const listenToImg = (img: HTMLImageElement) => {
      img.addEventListener("load", measure);
      return () => img.removeEventListener("load", measure);
    };
    let cleanupImgListener: (() => void) | null = null;
    const img = getImg();
    if (img) cleanupImgListener = listenToImg(img);

    measure();
    checkZoom();

    // Re-measure on overlay resize (mirrors the zoom-container resize).
    const ro = new ResizeObserver(() => { measure(); checkZoom(); });
    ro.observe(overlay);

    // Observe DOM changes under the loupe root: catches img src changes, new <img>
    // elements, and crucially the `style` attribute change on the zoom-img when the
    // user zooms/pans (ZoomableImage updates transform on every wheel/drag event).
    // Works for both .loupe-inline (main window) and .loupe-window (pop-out).
    const loupeRoot =
      overlay.closest(".loupe-inline") ?? overlay.closest(".loupe-window");
    const mo = loupeRoot
      ? new MutationObserver(() => {
          const newImg = getImg();
          if (newImg) {
            cleanupImgListener?.();
            cleanupImgListener = listenToImg(newImg);
            measure();
          }
          checkZoom();
        })
      : null;
    mo?.observe(loupeRoot!, {
      childList: true,
      subtree: true,
      attributes: true,
      attributeFilter: ["src", "style"],
    });

    return () => {
      ro.disconnect();
      mo?.disconnect();
      cleanupImgListener?.();
    };
  }, []);

  // Actions — all optimistically remove/update the face row locally.
  const handleConfirm = useCallback(async (face: FaceRow) => {
    await faceConfirm(api, face.id).catch(() => {});
    setFaces((prev) => prev.map((f) => f.id === face.id ? { ...f, state: "confirmed" as const } : f));
    api.notifyChange();
  }, [api]);

  const handleReject = useCallback(async (face: FaceRow) => {
    await faceReject(api, face.id).catch(() => {});
    setFaces((prev) => prev.map((f) => f.id === face.id ? { ...f, state: "rejected" as const } : f));
    api.notifyChange();
  }, [api]);

  const handleIgnore = useCallback(async (face: FaceRow) => {
    await faceIgnore(api, face.id).catch(() => {});
    setFaces((prev) => prev.map((f) => f.id === face.id ? { ...f, state: "ignored" as const } : f));
    api.notifyChange();
  }, [api]);

  const handleReassign = useCallback(async (face: FaceRow, tagId: number, tagName: string) => {
    await faceReassign(api, face.id, tagId).catch(() => {});
    setFaces((prev) =>
      prev.map((f) =>
        f.id === face.id
          ? { ...f, state: "confirmed" as const, personTagId: tagId, personName: tagName }
          : f
      )
    );
    setReassigning(null);
    api.notifyChange();
  }, [api]);

  // Assign the face to a brand-new person: create the tag under the people root,
  // then run the normal reassign path with the fresh tag id.
  const handleCreatePerson = useCallback(async (face: FaceRow, name: string) => {
    const path = peopleRoot ? `${peopleRoot}/${name}` : name;
    try {
      const tagId = await createTag(path);
      const leaf = name.split("/").filter(Boolean).pop() ?? name;
      await handleReassign(face, tagId, leaf);
    } catch {
      // ignore — the picker stays open so the user can retry
    }
  }, [peopleRoot, handleReassign]);

  // Delete a mis-drawn box (only offered on source='drawn' faces).
  const handleDeleteDrawn = useCallback(async (face: FaceRow) => {
    await facesDeleteDrawn(api, face.id).catch(() => {});
    setFaces((prev) => prev.filter((f) => f.id !== face.id));
    setReassigning((v) => (v === face.id ? null : v));
    api.notifyChange();
  }, [api]);

  // Complete a manual draw: map the dragged rect back to normalized image
  // coordinates (inverting letterbox + zoom/pan), create the face row, and open
  // the person picker on it right away.
  const finishDraw = useCallback(async (d: DraftBox) => {
    if (photoId == null) return;
    const left = Math.min(d.x0, d.x1);
    const top = Math.min(d.y0, d.y1);
    const w = Math.abs(d.x1 - d.x0);
    const h = Math.abs(d.y1 - d.y0);
    if (w < 8 || h < 8) return; // accidental click, not a box
    const p0 = screenToImage(left, top, geom, size.w, size.h, view);
    const p1 = screenToImage(left + w, top + h, geom, size.w, size.h, view);
    const x = Math.min(Math.max(p0.x, 0), 1);
    const y = Math.min(Math.max(p0.y, 0), 1);
    const bw = Math.min(Math.max(p1.x, 0), 1) - x;
    const bh = Math.min(Math.max(p1.y, 0), 1) - y;
    if (bw <= 0.005 || bh <= 0.005) return; // entirely in the letterbox area
    try {
      const id = await facesAddManual(api, photoId, { x, y, w: bw, h: bh });
      const rows = await facesForPhoto(api, photoId);
      setFaces(rows);
      setReassigning(id);
      api.notifyChange();
    } catch {
      // ignore — nothing was inserted
    }
  }, [photoId, geom, size, view, api]);

  // One drag = one box. Listeners go on window so the drag survives leaving the
  // overlay; the geometry rect is captured at mousedown (stable during a drag —
  // draw mode blocks the pan/zoom handlers underneath).
  const beginDraw = useCallback((e: React.MouseEvent) => {
    if (photoId == null || e.button !== 0) return;
    e.preventDefault();
    e.stopPropagation();
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    const x0 = e.clientX - rect.left;
    const y0 = e.clientY - rect.top;
    setDraft({ x0, y0, x1: x0, y1: y0 });
    const move = (ev: MouseEvent) => {
      setDraft({ x0, y0, x1: ev.clientX - rect.left, y1: ev.clientY - rect.top });
    };
    const up = (ev: MouseEvent) => {
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
      setDraft(null);
      setDrawMode(false);
      void finishDraw({ x0, y0, x1: ev.clientX - rect.left, y1: ev.clientY - rect.top });
    };
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
  }, [photoId, finishDraw]);

  // Escape leaves draw mode without drawing.
  useEffect(() => {
    if (!drawMode) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        setDrawMode(false);
        setDraft(null);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [drawMode]);

  // F toggles the face frames while a photo is in the loupe. Plain key only —
  // Ctrl/Cmd+F etc. must stay free for the browser/webview.
  useEffect(() => {
    if (photoId == null) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.ctrlKey || e.metaKey || e.altKey) return;
      const target = e.target as HTMLElement;
      if (target.tagName === "INPUT" || target.tagName === "TEXTAREA") return;
      if (e.key.toLowerCase() === "f") {
        toggleBoxes();
        e.preventDefault();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [photoId, toggleBoxes]);

  // Always render the overlay root, even with no photo/faces: the observer effect
  // below runs once on mount, so the div must exist from the start — returning null
  // here until faces load would leave containerRef empty when the effect fires and
  // the ResizeObserver/MutationObserver would never attach (geometry would never be
  // measured and the overlay would stay hidden forever).
  return (
    <div
      ref={containerRef}
      className="fa-overlay-root"
      // The overlay fills the .zoom-container (absolute, pointer-events: none on root,
      // interactive per-face chip). overflow: hidden clips boxes that the zoom/pan
      // transform pushes outside the visible loupe area.
      style={{
        position: "absolute",
        inset: 0,
        // In draw mode the overlay takes the pointer itself (blocking pan/zoom
        // underneath) so a drag draws a box instead of panning the image.
        pointerEvents: drawMode ? "all" : "none",
        cursor: drawMode ? "crosshair" : undefined,
        zIndex: 10,
        overflow: "hidden",
        // Render only once the container has been measured — before that the pixel
        // positions would all collapse to 0,0.
        visibility: size.w > 0 ? "visible" : "hidden",
      }}
      onMouseDown={drawMode ? beginDraw : undefined}
    >
      {/* Manual face box: arm draw mode, then drag over the missed face. */}
      {photoId != null && (
        <div
          style={{
            position: "absolute",
            top: 8,
            left: 8,
            zIndex: 25,
            display: "flex",
            gap: 6,
            pointerEvents: "none",
          }}
        >
          <button
            className="fa-chip-btn"
            style={{
              pointerEvents: "all",
              padding: "3px 8px",
              background: drawMode ? "rgba(59,130,246,0.9)" : "rgba(15,23,42,0.85)",
              border: "1px solid var(--border)",
              borderRadius: 6,
            }}
            title={
              drawMode
                ? "Cancel drawing (Esc)"
                : "Draw a box around a face the detector missed"
            }
            onMouseDown={(e) => e.stopPropagation()}
            onClick={(e) => {
              e.stopPropagation();
              setDraft(null);
              setDrawMode((v) => !v);
            }}
          >
            {drawMode ? "✕ cancel" : "＋ face"}
          </button>
          {faces.length > 0 && (
            <button
              className="fa-chip-btn"
              style={{
                pointerEvents: "all",
                padding: "3px 8px",
                background: "rgba(15,23,42,0.85)",
                border: "1px solid var(--border)",
                borderRadius: 6,
              }}
              title={showBoxes ? "Hide face frames (F)" : "Show face frames (F)"}
              onMouseDown={(e) => e.stopPropagation()}
              onClick={(e) => {
                e.stopPropagation();
                toggleBoxes();
              }}
            >
              {showBoxes ? "hide faces" : "show faces"}
            </button>
          )}
        </div>
      )}

      {draft && (
        <div
          style={{
            position: "absolute",
            left: Math.min(draft.x0, draft.x1),
            top: Math.min(draft.y0, draft.y1),
            width: Math.abs(draft.x1 - draft.x0),
            height: Math.abs(draft.y1 - draft.y0),
            border: "2px dashed #3b82f6",
            background: "rgba(59,130,246,0.12)",
            borderRadius: 4,
            pointerEvents: "none",
            zIndex: 24,
          }}
        />
      )}

      {showBoxes && faces.map((face) => {
        const css = bboxToCss(face.bbox, geom, size.w, size.h, view);
        const color = chipColor(face.state);
        const isIgnored = face.state === "ignored";
        const isRejected = face.state === "rejected";
        const isReassigningThis = reassigning === face.id;
        const hasName = !!face.personName;
        const isSuggested = face.state === "suggested";

        return (
          <div
            key={face.id}
            className="fa-face-box"
            style={{
              position: "absolute",
              ...css,
              boxSizing: "border-box",
              border: `2px solid ${color}`,
              borderRadius: 4,
              // The rectangle itself must not eat events — wheel-zoom and drag-pan
              // have to reach the .zoom-container even when the cursor is over a
              // face. Only the chip (and dropdown) are interactive.
              pointerEvents: "none",
              cursor: "default",
              opacity: isIgnored ? 0.4 : 1,
            }}
          >
            {/* Name chip at the bottom of the box */}
            <div
              className="fa-face-chip"
              style={{
                position: "absolute",
                bottom: "calc(100% + 2px)",
                left: 0,
                display: "flex",
                alignItems: "center",
                gap: 3,
                background: "rgba(15, 23, 42, 0.88)",
                border: `1px solid ${color}`,
                borderRadius: 4,
                padding: "2px 5px",
                fontSize: 11,
                color: "#f8fafc",
                whiteSpace: "nowrap",
                userSelect: "none",
                zIndex: 20,
                pointerEvents: "all",
              }}
            >
              <span style={{ color, fontWeight: 600, maxWidth: 120, overflow: "hidden", textOverflow: "ellipsis" }}>
                {hasName
                  ? face.personName
                  : face.state === "unassigned"
                  ? "Unknown"
                  : face.state === "rejected"
                  ? "Rejected"
                  : face.state === "ignored"
                  ? "Ignored"
                  : "Unknown"}
              </span>
              {isSuggested && face.matchConfidence != null && (
                <span style={{ fontSize: 10, opacity: 0.75 }}>
                  {Math.round(face.matchConfidence * 100)}%
                </span>
              )}
              {/* Actions */}
              {!isIgnored && !isRejected && (
                <>
                  {isSuggested && (
                    <button
                      className="fa-chip-btn"
                      title="Confirm this suggestion"
                      onClick={(e) => { e.stopPropagation(); void handleConfirm(face); }}
                    >
                      ✓
                    </button>
                  )}
                  {(isSuggested || face.state === "unassigned") && (
                    <button
                      className="fa-chip-btn fa-chip-btn-danger"
                      title="Reject — won't be proposed again"
                      onClick={(e) => { e.stopPropagation(); void handleReject(face); }}
                    >
                      ✕
                    </button>
                  )}
                  <button
                    className="fa-chip-btn"
                    title="Reassign to a different person"
                    onClick={(e) => {
                      e.stopPropagation();
                      setReassigning((v) => (v === face.id ? null : face.id));
                    }}
                  >
                    ⇄
                  </button>
                  <button
                    className="fa-chip-btn"
                    title="Ignore this face (photobomber / background)"
                    onClick={(e) => { e.stopPropagation(); void handleIgnore(face); }}
                  >
                    –
                  </button>
                  {face.source === "drawn" && (
                    <button
                      className="fa-chip-btn fa-chip-btn-danger"
                      title="Delete this drawn box"
                      onClick={(e) => { e.stopPropagation(); void handleDeleteDrawn(face); }}
                    >
                      🗑
                    </button>
                  )}
                </>
              )}
            </div>

            {/* Reassign dropdown */}
            {isReassigningThis && (
              <div
                className="fa-reassign-drop"
                style={{
                  position: "absolute",
                  top: "100%",
                  left: 0,
                  zIndex: 30,
                  background: "rgba(15, 23, 42, 0.97)",
                  border: "1px solid var(--border)",
                  borderRadius: 6,
                  padding: "4px 6px 6px",
                  minWidth: 200,
                  pointerEvents: "all",
                  boxShadow: "0 8px 24px rgba(0,0,0,0.6)",
                }}
                onMouseDown={(e) => e.stopPropagation()}
              >
                <div style={{ fontSize: 10, opacity: 0.6, padding: "2px 4px 4px", textTransform: "uppercase", letterSpacing: "0.05em" }}>
                  Reassign to…
                </div>
                <PersonPicker
                  tags={peopleTags}
                  currentTagId={face.personTagId}
                  onPick={(tag) => void handleReassign(face, tag.id, tag.name)}
                  onCancel={() => setReassigning(null)}
                  onCreate={(name) => void handleCreatePerson(face, name)}
                />
              </div>
            )}
          </div>
        );
      })}

      {/* Close any open dropdown when clicking outside. pointerEvents must be set
          explicitly — it is inherited, and the overlay root disables it. */}
      {reassigning != null && (
        <div
          style={{ position: "fixed", inset: 0, zIndex: 9, pointerEvents: "all" }}
          onClick={() => setReassigning(null)}
        />
      )}
    </div>
  );
}

// ── FacesInspectorPanel — per-photo face review list ─────────────────────────

function FacesInspectorPanel({ api }: { api: ChairPhotoAPI }) {
  useHostSelection(); // re-render on photo change
  const photoId = api.getActivePhotoId();
  const [faces, setFaces] = useState<FaceRow[]>([]);
  const [loading, setLoading] = useState(false);
  const [reassigning, setReassigning] = useState<number | null>(null);
  const [peopleTags, setPeopleTags] = useState<Tag[]>([]);
  const [peopleRoot, setPeopleRoot] = useState("");
  // Guard against setState after unmount (e.g. user navigates away while a
  // loadFaces promise is in flight).
  const mountedRef = useRef(true);

  useEffect(() => {
    mountedRef.current = true;
    return () => { mountedRef.current = false; };
  }, []);

  const loadFaces = useCallback(() => {
    if (photoId == null) {
      setFaces([]);
      return;
    }
    setLoading(true);
    facesForPhoto(api, photoId)
      .then((rows) => {
        if (!mountedRef.current) return;
        setFaces(rows);
        setLoading(false);
      })
      .catch(() => {
        if (!mountedRef.current) return;
        setFaces([]);
        setLoading(false);
      });
  }, [photoId, api]);

  useEffect(() => { loadFaces(); }, [loadFaces]);

  // Re-fetched every time a picker opens so tags created since mount (e.g. in
  // the TagPanel) are offered too.
  const loadPeople = useCallback(async () => {
    try {
      const { root, tags } = await loadPeopleTags(api);
      if (!mountedRef.current) return;
      setPeopleRoot(root);
      setPeopleTags(tags);
    } catch {
      // ignore
    }
  }, [api]);

  useEffect(() => { void loadPeople(); }, [loadPeople]);
  useEffect(() => {
    if (reassigning != null) void loadPeople();
  }, [reassigning, loadPeople]);

  const handleConfirm = async (face: FaceRow) => {
    await faceConfirm(api, face.id).catch(() => {});
    loadFaces();
    api.notifyChange();
  };

  const handleReject = async (face: FaceRow) => {
    await faceReject(api, face.id).catch(() => {});
    loadFaces();
    api.notifyChange();
  };

  const handleIgnore = async (face: FaceRow) => {
    await faceIgnore(api, face.id).catch(() => {});
    loadFaces();
    api.notifyChange();
  };

  const handleReassign = async (face: FaceRow, tagId: number) => {
    await faceReassign(api, face.id, tagId).catch(() => {});
    setReassigning(null);
    loadFaces();
    api.notifyChange();
  };

  const handleCreatePerson = async (face: FaceRow, name: string) => {
    const path = peopleRoot ? `${peopleRoot}/${name}` : name;
    try {
      const tagId = await createTag(path);
      await handleReassign(face, tagId);
    } catch {
      // ignore — the picker stays open so the user can retry
    }
  };

  const handleDeleteDrawn = async (face: FaceRow) => {
    await facesDeleteDrawn(api, face.id).catch(() => {});
    setReassigning((v) => (v === face.id ? null : v));
    loadFaces();
    api.notifyChange();
  };

  if (photoId == null) {
    return <span className="panel-empty">No photo selected</span>;
  }
  if (loading) {
    return <span className="panel-empty">Loading…</span>;
  }
  if (faces.length === 0) {
    return (
      <div>
        <div className="develop-note">No faces indexed for this photo.</div>
        <div className="develop-note" style={{ marginTop: 4 }}>
          Use the Faces settings panel to index faces.
        </div>
      </div>
    );
  }

  return (
    <div className="fa-panel">
      {faces.map((face, idx) => {
        const isSuggested = face.state === "suggested";
        const isConfirmed = face.state === "confirmed";
        const isReassigningThis = reassigning === face.id;

        return (
          <div key={face.id} className="fa-panel-row">
            <div className="fa-panel-num">#{idx + 1}</div>
            <div className="fa-panel-body">
              <div className="fa-panel-name">
                <span style={{ color: chipColor(face.state), fontWeight: 600 }}>
                  {face.personName ?? "Unknown"}
                </span>
                {isSuggested && face.matchConfidence != null && (
                  <span className="fa-panel-conf">
                    {Math.round(face.matchConfidence * 100)}%
                  </span>
                )}
                <span className="fa-panel-state">{face.state}</span>
              </div>
              <div className="row" style={{ gap: 4, flexWrap: "wrap" }}>
                {isSuggested && (
                  <button className="chip" onClick={() => void handleConfirm(face)}>
                    ✓ confirm
                  </button>
                )}
                {(isSuggested || face.state === "unassigned") && (
                  <button className="chip" onClick={() => void handleReject(face)}>
                    ✕ reject
                  </button>
                )}
                {!isReassigningThis && (
                  <button
                    className="chip"
                    onClick={() => setReassigning(face.id)}
                    title="Assign to a different person tag"
                  >
                    ⇄ reassign
                  </button>
                )}
                {face.state !== "ignored" && (
                  <button className="chip" onClick={() => void handleIgnore(face)}>
                    – ignore
                  </button>
                )}
                {face.source === "drawn" && (
                  <button
                    className="chip"
                    title="Delete this drawn box"
                    onClick={() => void handleDeleteDrawn(face)}
                  >
                    🗑 delete
                  </button>
                )}
                {isConfirmed && (
                  <span style={{ fontSize: 11, color: "#10b981", alignSelf: "center" }}>
                    confirmed
                  </span>
                )}
              </div>
              {isReassigningThis && (
                <div style={{ marginTop: 4 }}>
                  <PersonPicker
                    tags={peopleTags}
                    currentTagId={face.personTagId}
                    onPick={(tag) => void handleReassign(face, tag.id)}
                    onCancel={() => setReassigning(null)}
                    onCreate={(name) => void handleCreatePerson(face, name)}
                  />
                  <button
                    className="chip"
                    style={{ marginTop: 4 }}
                    onClick={() => setReassigning(null)}
                  >
                    Cancel
                  </button>
                </div>
              )}
            </div>
          </div>
        );
      })}
    </div>
  );
}

// ── FacesSettings — module settings panel ────────────────────────────────────

/** Human-readable summary of how an index run ended, from its honest counters. */
function indexDoneMessage(d: FacesIndexDone): string {
  if (d.total === 0) {
    return "Indexing: nothing to do — all photos are already indexed.";
  }
  const skips: string[] = [];
  if (d.offline > 0) skips.push(`${d.offline} offline (connect the NAS and re-run)`);
  if (d.failed > 0) skips.push(`${d.failed} unreadable`);
  const skipNote = skips.length > 0 ? ` Skipped: ${skips.join(", ")} — still queued.` : "";
  if (d.aborted) {
    return `Indexing cancelled at ${d.done} of ${d.total} photos.${skipNote}`;
  }
  if (d.done < d.total) {
    return `Indexing finished: ${d.done} of ${d.total} photos processed.${skipNote}`;
  }
  return `Indexing complete: ${d.total} photo${d.total !== 1 ? "s" : ""} processed.`;
}

function FacesSettings({ api }: { api: ChairPhotoAPI }) {
  const [peopleRoot, setPeopleRoot] = useState("");
  const [threshold, setThreshold] = useState(DEFAULT_THRESHOLD);
  const [saved, setSaved] = useState(false);
  const [modelsStatus, setModelsStatus] = useState<FacesModelsStatus | null>(null);
  const [modelsLoading, setModelsLoading] = useState(false);
  const [modelsError, setModelsError] = useState("");
  const [jobPhase, setJobPhase] = useState<"idle" | "indexing" | "matching">("idle");
  // Persistent outcome of the last finished job — a toast alone vanishes too fast to read.
  const [lastResult, setLastResult] = useState("");
  const [progress, setProgress] = useState<ProgressDisplay | null>(null);
  const [jobError, setJobError] = useState("");
  // Listeners are stored with the lease token that installed them. Under StrictMode an
  // attempt can be torn down while its registration is still pending, so a global
  // "release the current listener" would let a stale attempt drop its replacement's — or
  // let a late registration overwrite and orphan a live one.
  const unlistenRef = useRef<OwnedListener | null>(null);
  const doneUnlistenRef = useRef<OwnedListener | null>(null);
  // Id of the indexing job this panel is following. Starting a new run aborts any
  // running one, whose progress/done events keep arriving with the OLD id — everything
  // event-driven filters on this so a superseded run can't hijack the panel state.
  const jobIdRef = useRef<number | null>(null);
  /** The running match's id, so its progress and terminal events can be told from a
   *  superseded run's. Separate from `jobIdRef`: the two job families number themselves
   *  independently, so an id alone does not say which family it belongs to. */
  const matchJobIdRef = useRef<number | null>(null);

  // False once unmounted, so a registration that resolves after cleanup is released
  // immediately instead of being stored into a ref nobody will drain again.
  const mountedRef = useRef(true);
  // Synchronous single-flight lease. `jobPhase` is React state and does not update until
  // after the awaits in the start/reattach paths, so a second click (or reattach racing a
  // click) would otherwise both pass the phase check and start two runs. `preparing` is
  // the reactive mirror so the buttons are honestly disabled during that window too.
  //
  // The slot holds a token rather than a boolean: under StrictMode an effect is mounted,
  // cleaned up and re-run, and a stale attempt's `finally` must not release the claim its
  // replacement now holds.
  const claimRef = useRef<symbol | null>(null);
  const [preparing, setPreparing] = useState(false);
  // True when a backend job is running that this panel cannot follow, because the
  // terminal event could not be subscribed to. Distinct from "indexing": we know work is
  // in flight but cannot see it finish, so conflicting actions stay disabled.
  const [untracked, setUntracked] = useState(false);
  // Whether a faces:progress listener is actually live, so the caption can say
  // "Indexing…" rather than sitting on "Starting…" when updates can never arrive.
  const [progressLive, setProgressLive] = useState(false);

  // Drop event subscriptions if the panel unmounts mid-job (the backend job itself
  // keeps running — it is detached by design).
  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      claimRef.current = null;
      unlistenRef.current?.stop();
      unlistenRef.current = null;
      doneUnlistenRef.current?.stop();
      doneUnlistenRef.current = null;
    };
  }, []);

  // Load persisted settings on mount.
  useEffect(() => {
    api.getSetting(SETTING_PEOPLE_ROOT).then((v) => { if (v) setPeopleRoot(v); }).catch(() => {});
    api.getSetting(SETTING_THRESHOLD).then((v) => { if (v) setThreshold(v); }).catch(() => {});
  }, [api]);

  // Tag list for the people-root type-ahead.
  const [allTags, setAllTags] = useState<Tag[]>([]);
  const [rootFocus, setRootFocus] = useState(false);
  const [rootHighlight, setRootHighlight] = useState(0);
  useEffect(() => {
    api.listTags().then(setAllTags).catch(() => {});
  }, [api]);

  // While the field is focused: typing filters all tag paths; an empty field
  // offers the root-level tags (the natural people-root candidates).
  const rootQuery = peopleRoot.trim().toLowerCase();
  const rootSuggestions = rootFocus
    ? (rootQuery
        ? allTags.filter((t) => t.fullPath.toLowerCase().includes(rootQuery))
        : allTags.filter((t) => !t.fullPath.includes("/"))
      ).slice(0, 8)
    : [];
  const clampedRootHighlight = Math.min(rootHighlight, Math.max(0, rootSuggestions.length - 1));

  const pickPeopleRoot = (path: string) => {
    setPeopleRoot(path);
    setSaved(false);
    setRootFocus(false);
  };

  // Load model status on mount.
  useEffect(() => {
    setModelsLoading(true);
    facesModelsStatus(api)
      .then((s) => { setModelsStatus(s); setModelsLoading(false); })
      .catch(() => { setModelsLoading(false); });
  }, [api]);

  // Inference info (GPU/CPU + indexing speed). Reloaded whenever a job finishes so the
  // EP line reflects where the run actually executed ("unbuilt" until the first run).
  const [inference, setInference] = useState<FacesInferenceInfo | null>(null);
  const [speedNote, setSpeedNote] = useState("");
  useEffect(() => {
    if (jobPhase !== "idle") return;
    facesInferenceInfo(api).then(setInference).catch(() => {});
  }, [jobPhase, api]);
  const handleSpeedChange = async (speed: "background" | "full") => {
    try {
      await facesSetIndexingSpeed(api, speed);
      setInference((i) => (i ? { ...i, speed } : i));
      setSpeedNote("Saved — applies on next app start");
    } catch (e) {
      setSpeedNote(String(e));
    }
  };

  /** True while `token` still speaks for the panel: mounted, and the lease neither
   *  cleared by unmount nor already handed to a replacement attempt. UI writes are gated
   *  on this; listener and lease release never are, since those must happen either way. */
  const stillOwns = useCallback(
    (token: symbol) => mountedRef.current && claimRef.current === token,
    [],
  );

  // Subscribe to faces progress events for the duration of an active job.
  //
  // Progress is cosmetic, so this never throws and never blocks the work it decorates:
  // a host without `onEvent`, or a registration that rejects, simply means no progress
  // display. Matching must still run in that case. (Model download emits nothing and
  // does not subscribe at all.)
  // Returns whether the caller still owns the lease afterwards. A missing or rejected
  // subscription is cosmetic and returns true; losing the lease or the component during
  // the await returns false, and the caller must NOT go on to invoke its command.
  const subscribeToProgress = useCallback(async (token: symbol): Promise<boolean> => {
    const stillOurs = () => stillOwns(token);
    if (unlistenRef.current) return stillOurs(); // already subscribed
    let unlisten: (() => void) | null = null;
    try {
      unlisten = await onFacesProgress(api, (p) => {
        // Drop stragglers from a superseded indexing run (its in-flight chunk keeps
        // emitting briefly after a new run aborts it).
        if (jobIdRef.current != null && p.job !== jobIdRef.current) return;
        setProgress({ done: p.done, total: p.total });
      });
    } catch {
      unlisten = null;
    }
    // The attempt may have been torn down and replaced while this registration was
    // pending; a listener nobody owns must be stopped, not stored.
    if (!stillOurs()) {
      unlisten?.();
      return false;
    }
    if (unlisten) {
      unlistenRef.current = { owner: token, stop: unlisten };
      setProgressLive(true);
    }
    return true;
  }, [api, stillOwns]);

  const unsubscribeProgress = useCallback((owner?: symbol) => {
    const cur = unlistenRef.current;
    if (!cur) return;
    if (owner !== undefined && cur.owner !== owner) return;
    cur.stop();
    unlistenRef.current = null;
    setProgressLive(false);
  }, []);

  /** Drop the terminal listener without touching job state. With an `owner` it releases
   *  only that attempt's listener; without one (unmount) it releases whatever is there. */
  const releaseDoneListener = useCallback((owner?: symbol) => {
    const cur = doneUnlistenRef.current;
    if (!cur) return;
    if (owner !== undefined && cur.owner !== owner) return;
    cur.stop();
    doneUnlistenRef.current = null;
  }, []);

  /**
   * Claim the single-flight slot shared by reattach, indexing and matching. Returns the
   * lease token, or null if another of those is mid-flight. The ref is what actually
   * excludes (synchronous); `preparing` only mirrors it into render so buttons disable
   * during the await. The token also scopes listener ownership.
   */
  const beginExclusive = useCallback((): symbol | null => {
    if (claimRef.current) return null;
    const token = Symbol("faces-job");
    claimRef.current = token;
    setPreparing(true);
    return token;
  }, []);

  /** Releases only if `token` still holds the lease, so a stale attempt cannot free a
   *  replacement's claim. Safe to call more than once with the same token. */
  const endExclusive = useCallback((token: symbol | null) => {
    if (!token || claimRef.current !== token) return;
    claimRef.current = null;
    setPreparing(false);
  }, []);

  /**
   * Acquire the `faces:index_done` listener. Unlike progress this is **required**:
   * it is the only thing that calls `finishIndexing`, so a run started without it would
   * sit in "indexing" forever. Returns "unsupported" when the host has no `onEvent` and
   * "failed" when registration rejects or the lease was lost; the caller must then refuse
   * to start.
   */
  const acquireDoneListener = useCallback(
    async (
      token: symbol,
      handler: (d: FacesIndexDone) => void,
    ): Promise<DoneAcquire> => {
      if (!api.onEvent) return "unsupported";
      let stop: (() => void) | null = null;
      try {
        stop = await onFacesIndexDone(api, handler);
      } catch {
        return "failed";
      }
      if (!stop) return "unsupported";
      // Install only while still holding the lease. If the attempt was torn down and
      // replaced during the await, this registration belongs to nobody: stop it rather
      // than overwriting (and orphaning) the replacement's listener.
      if (!mountedRef.current || claimRef.current !== token) {
        stop();
        return "failed";
      }
      releaseDoneListener(token);
      doneUnlistenRef.current = { owner: token, stop };
      return "ok";
    },
    [api, releaseDoneListener],
  );

  /**
   * Subscribe to `faces:match_progress`. Cosmetic, exactly like `subscribeToProgress`:
   * a host without `onEvent` just means no progress bar, and matching still runs.
   */
  const subscribeToMatchProgress = useCallback(async (token: symbol): Promise<boolean> => {
    const stillOurs = () => stillOwns(token);
    if (unlistenRef.current) return stillOurs();
    let unlisten: (() => void) | null = null;
    try {
      unlisten = await onFacesMatchProgress(api, (p) => {
        if (matchJobIdRef.current != null && p.job !== matchJobIdRef.current) return;
        setProgress({ done: p.done, total: p.total, phase: p.phase });
      });
    } catch {
      unlisten = null;
    }
    if (!stillOurs()) {
      unlisten?.();
      return false;
    }
    if (unlisten) {
      unlistenRef.current = { owner: token, stop: unlisten };
      setProgressLive(true);
    }
    return true;
  }, [api, stillOwns]);

  /**
   * Acquire the `faces:match_done` listener. Required for the same reason as the indexing
   * one: it is now the only place the run's counters arrive, so a match started without it
   * would sit in "matching" forever and never report what it did.
   */
  const acquireMatchDoneListener = useCallback(
    async (token: symbol, handler: (d: FacesMatchDone) => void): Promise<DoneAcquire> => {
      if (!api.onEvent) return "unsupported";
      let stop: (() => void) | null = null;
      try {
        stop = await onFacesMatchDone(api, handler);
      } catch {
        return "failed";
      }
      if (!stop) return "unsupported";
      if (!mountedRef.current || claimRef.current !== token) {
        stop();
        return "failed";
      }
      releaseDoneListener(token);
      doneUnlistenRef.current = { owner: token, stop };
      return "ok";
    },
    [api, releaseDoneListener],
  );

  const handleSave = async () => {
    await api.setSetting(SETTING_PEOPLE_ROOT, peopleRoot.trim());
    await api.setSetting(SETTING_THRESHOLD, threshold.trim() || DEFAULT_THRESHOLD);
    setSaved(true);
    setTimeout(() => setSaved(false), 2000);
  };

  const handleDownloadModels = async () => {
    setModelsError("");
    setModelsLoading(true);
    try {
      // No progress subscription here: this command emits no faces:progress events.
      await facesDownloadModels(api);
      const s = await facesModelsStatus(api);
      setModelsStatus(s);
    } catch (e: unknown) {
      setModelsError(`Download failed: ${String(e)}`);
    } finally {
      setModelsLoading(false);
    }
  };

  // The run is over, so both listeners are released regardless of which attempt installed
  // them — this is the job ending, not one attempt tidying up after itself.
  const finishIndexing = useCallback(() => {
    jobIdRef.current = null;
    unsubscribeProgress();
    releaseDoneListener();
    if (!mountedRef.current) return; // nothing left to render the idle state on
    setJobPhase("idle");
    setProgress(null);
  }, [unsubscribeProgress, releaseDoneListener]);

  // Handle OUR run's terminal event (callers filter by job id before calling this).
  const handleIndexDone = useCallback(
    (d: FacesIndexDone) => {
      finishIndexing();
      if (d.ok) {
        const msg = indexDoneMessage(d);
        setLastResult(msg);
        api.showToast(msg);
        api.notifyChange(); // new faces may now show in the loupe overlay
      } else {
        setJobError(`Indexing failed: ${d.error ?? "unknown error"}`);
      }
    },
    [finishIndexing, api]
  );

  // If an indexing job is already running when the panel mounts (started before a
  // tab switch, say), re-attach to it: without this the panel believes it's idle —
  // no progress display, and the button would happily start a second run that
  // aborts the first.
  useEffect(() => {
    // This attempt owns whatever it acquires up to the point of installing it. If the
    // effect is torn down before then, whoever notices first releases the listener — the
    // cleanup below, or the async body when it resumes and finds itself stale. Once
    // installed the listener is transferred to the component refs, which own it for the
    // life of the run; it is not released on a dependency restart.
    const own = { stale: false, acquired: false, installed: false };
    // Claim BEFORE the first status request. Otherwise the user can start Matching while
    // that request is in flight: Matching takes the slot, the status result then shows an
    // active index job, and reattach exits — leaving the two overlapping with no terminal
    // listener on the index run.
    const token = beginExclusive();
    if (!token) return;
    (async () => {
      try {
        const first = await facesIndexStatus(api)
          .then((s) => ({ ok: true as const, s }))
          .catch(() => ({ ok: false as const, s: null }));
        if (own.stale || !mountedRef.current) return;
        if (!first.ok) {
          // A rejected query is not evidence of idleness — something may be running.
          setUntracked(true);
          setJobError(REATTACH_VALIDATE_FAILED);
          return;
        }
        if (!first.s || jobIdRef.current != null) return;
        // The job could finish between the status read and the listener going live. Buffer
        // anything arriving while we decide, since jobIdRef is not set yet and the normal
        // filter would discard it. Note this cannot recover an event emitted BEFORE the
        // listener existed — the re-read below prevents a frozen UI, it cannot reconstruct
        // that run's result.
        let adopted: number | null = null;
        const buffered: FacesIndexDone[] = [];
        const attached = await acquireDoneListener(token, (d) => {
          if (adopted == null) {
            buffered.push(d);
            return;
          }
          if (d.job !== adopted) return;
          handleIndexDone(d);
        });
        if (attached === "ok") own.acquired = true;
        if (own.stale || !mountedRef.current) {
          if (own.acquired) releaseDoneListener(token);
          return;
        }
        if (attached !== "ok") {
          // A job is running that we cannot follow to completion. Say so and keep
          // conflicting actions disabled rather than presenting the panel as idle.
          setUntracked(true);
          // Reattach starts nothing, so neither message may say a run was refused.
          setJobError(
            attached === "unsupported"
              ? REATTACH_NO_EVENT_SUPPORT
              : REATTACH_SUBSCRIBE_FAILED,
          );
          return;
        }
        // Re-read once now the listener is live. This is the only extra round trip, and
        // it closes the gap without introducing a second definition of "finished".
        const now = await facesIndexStatus(api)
          .then((s) => ({ ok: true as const, s }))
          .catch(() => ({ ok: false as const, s: null }));
        if (own.stale || !mountedRef.current) {
          releaseDoneListener(token);
          return;
        }
        if (!now.ok) {
          // A failed query is not evidence of idleness. Something may still be running
          // that we are no longer tracking.
          releaseDoneListener(token);
          own.acquired = false;
          setUntracked(true);
          setJobError(REATTACH_VALIDATE_FAILED);
          return;
        }
        if (!now.s) {
          // Confirmed idle: the job ended while we were attaching. Replay its terminal
          // event if we caught it; do not install a snapshot of a dead job.
          const ours = buffered.find((d) => d.job === first.s!.job);
          releaseDoneListener(token);
          own.acquired = false;
          if (ours) handleIndexDone(ours);
          return;
        }
        // Adopt whatever is actually running now — same job, or a newer one that
        // superseded it. Either way the listener is live and can see it finish.
        adopted = now.s.job;
        jobIdRef.current = now.s.job;
        own.installed = true;
        setJobPhase("indexing");
        const ours = buffered.find((d) => d.job === now.s!.job);
        if (ours) {
          handleIndexDone(ours);
          return;
        }
        const stillOurs = await subscribeToProgress(token);
        // The run may have finished while that await was pending, in which case
        // finishIndexing already ran and saw no listener to drop.
        if (!stillOurs || jobIdRef.current !== adopted) {
          unsubscribeProgress(token);
          return;
        }
        setProgress({ done: now.s.done, total: now.s.total });
      } finally {
        endExclusive(token);
      }
    })();
    return () => {
      own.stale = true;
      // Release only a listener this attempt acquired but never installed. An installed
      // listener is deliberately handed to the component refs and lives on until the job
      // ends or the panel unmounts — it is tracking a real run.
      if (own.acquired && !own.installed) releaseDoneListener(token);
      // Free the lease so a replacement attempt can claim it; the async `finally` will
      // no-op because the token no longer matches.
      endExclusive(token);
    };
  }, [
    api,
    subscribeToProgress,
    unsubscribeProgress,
    acquireDoneListener,
    releaseDoneListener,
    beginExclusive,
    endExclusive,
    handleIndexDone,
  ]);

  // A matching job survives a panel remount just as an indexing one does, and the panel
  // has no adopt protocol for it. Rather than show idle — and let the user start a second
  // run that would abort the first — mark the panel untracked and let the poll below
  // re-enable it. Matching is short next to indexing, so adopting it in full would be a
  // lot of machinery for a brief window.
  useEffect(() => {
    let cancelled = false;
    facesMatchStatus(api)
      .then((s) => {
        if (s && !cancelled && mountedRef.current) setUntracked(true);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [api]);

  // While untracked, poll status at a low rate purely to re-enable the buttons once the
  // backend goes idle. This deliberately reports nothing — no result message, no
  // finishIndexing — so the terminal events remain the single definition of "finished".
  // Both job families are checked: either one still running means still untracked.
  useEffect(() => {
    if (!untracked) return;
    const id = setInterval(() => {
      Promise.all([facesIndexStatus(api), facesMatchStatus(api)])
        .then(([index, match]) => {
          if (!index && !match && mountedRef.current) {
            setUntracked(false);
            setJobError("");
          }
        })
        .catch(() => {});
    }, 5000);
    return () => clearInterval(id);
  }, [untracked, api]);

  // The backend command returns as soon as the job has STARTED (it runs detached so
  // the UI never blocks); completion arrives later as a faces:index_done event. The
  // panel therefore stays in "indexing" until that event, not until the invoke returns.
  const handleIndexFaces = async () => {
    // `jobPhase` is state and will not have updated by the time a second click lands
    // during the awaits below, so the synchronous lease is what actually prevents two
    // concurrent starts (the second would abort the first backend run).
    if (jobPhase !== "idle" || untracked) return;
    const token = beginExclusive();
    if (!token) return;
    setJobError("");
    setLastResult("");
    setProgress(null);
    jobIdRef.current = null;
    // Listeners go up BEFORE the invoke so a fast run (empty queue) can't finish in
    // the gap — but then our job id isn't known yet when an event arrives. Park done
    // events until the invoke resolves the id, then replay ours if it already came.
    const pendingDone: FacesIndexDone[] = [];
    try {
      // The terminal listener is a precondition, not decoration: it is the only path to
      // finishIndexing. Acquire it before entering the indexing state so a host without
      // usable events refuses the run instead of hanging in it.
      const attached = await acquireDoneListener(token, (d) => {
        if (jobIdRef.current === null) {
          pendingDone.push(d);
          return;
        }
        if (d.job !== jobIdRef.current) return;
        handleIndexDone(d);
      });
      if (attached !== "ok") {
        if (stillOwns(token)) {
          setJobError(attached === "unsupported" ? NO_EVENT_SUPPORT : EVENT_SUBSCRIBE_FAILED);
        }
        return;
      }
      setJobPhase("indexing");
      try {
        // If the lease or the component was lost during that await, a remount's reattach
        // may already have seen this panel idle. Starting the backend run now would leave
        // it untracked, so abort instead.
        if (!(await subscribeToProgress(token))) return;
        const job = await facesIndexPhotos(api); // job started — completion handled above
        // jobIdRef and the pending-event replay are shared panel state, not this
        // attempt's: publishing them from a stale token would overwrite a replacement's.
        if (!stillOwns(token)) return;
        jobIdRef.current = job;
        const ours = pendingDone.find((d) => d.job === job);
        if (ours) handleIndexDone(ours);
      } catch (e: unknown) {
        // Start-up failure (no catalog open / models missing) — no done event will come.
        // finishIndexing clears shared state, so it is only ours to call while we still
        // hold the lease; otherwise drop just this attempt's own listeners.
        if (stillOwns(token)) {
          finishIndexing();
          setJobError(`Indexing failed: ${String(e)}`);
        } else {
          unsubscribeProgress(token);
          releaseDoneListener(token);
        }
      }
    } finally {
      endExclusive(token);
    }
  };

  /** Clear everything a finished match owned. Mirrors `finishIndexing`. */
  const finishMatching = useCallback(() => {
    matchJobIdRef.current = null;
    setJobPhase("idle");
    setProgress(null);
    unsubscribeProgress();
    releaseDoneListener();
  }, [unsubscribeProgress, releaseDoneListener]);

  /** Turn a finished match into the panel's result line. */
  const handleMatchDone = useCallback(
    (d: FacesMatchDone) => {
      finishMatching();
      if (d.error) {
        setJobError(`Matching failed: ${d.error}`);
        return;
      }
      if (d.aborted) {
        setLastResult("Matching cancelled — partial results were kept.");
        api.notifyChange();
        return;
      }
      const o = d.outcome;
      if (!o) {
        setJobError("Matching finished without reporting what it did.");
        return;
      }
      const suggested = o.constrained + o.open;
      const msg =
        o.seeded + suggested + o.clustered === 0
          ? o.people === 0
            ? "Matching: no seeds found yet — check that the people root matches your person tags, and that indexing has run."
            : `Matching: nothing new to propose (${o.people} known people).`
          : `Matching: ${o.seeded} seeded, ${suggested} suggested, ${o.clustered} clustered (${o.people} known people).`;
      setLastResult(msg);
      api.showToast(msg);
      api.notifyChange(); // refresh inspector panels
    },
    [api, finishMatching],
  );

  // Like indexing, the command returns as soon as the job has STARTED; the counters
  // arrive later on faces:match_done. The panel stays in "matching" until that event.
  const handleRunMatching = async () => {
    // `untracked` means a faces job we cannot follow is still running; a second one
    // would conflict with it. The exclusive claim also covers the window where an
    // indexing start or reattach is mid-await and jobPhase is still "idle".
    if (jobPhase !== "idle" || untracked) return;
    const token = beginExclusive();
    if (!token) return;
    setJobError("");
    setLastResult("");
    setProgress(null);
    matchJobIdRef.current = null;
    // Same ordering problem as indexing: the listener has to be up before the invoke so a
    // fast run cannot finish in the gap, but our job id is not known until the invoke
    // resolves. Park terminal events until then, and replay ours if it already arrived.
    const pendingDone: FacesMatchDone[] = [];
    try {
      const attached = await acquireMatchDoneListener(token, (d) => {
        if (matchJobIdRef.current === null) {
          pendingDone.push(d);
          return;
        }
        if (d.job !== matchJobIdRef.current) return;
        handleMatchDone(d);
      });
      if (attached !== "ok") {
        if (stillOwns(token)) {
          setJobError(attached === "unsupported" ? NO_EVENT_SUPPORT : EVENT_SUBSCRIBE_FAILED);
        }
        return;
      }
      setJobPhase("matching");
      try {
        if (!(await subscribeToMatchProgress(token))) return;
        const job = await facesRunMatching(api); // job started — completion handled above
        if (!stillOwns(token)) return;
        matchJobIdRef.current = job;
        const ours = pendingDone.find((d) => d.job === job);
        if (ours) handleMatchDone(ours);
      } catch (e: unknown) {
        // Start-up failure (no catalog open) — no terminal event will come.
        if (stillOwns(token)) {
          finishMatching();
          setJobError(`Matching failed: ${String(e)}`);
        } else {
          unsubscribeProgress(token);
          releaseDoneListener(token);
        }
      }
    } finally {
      endExclusive(token);
    }
  };

  // Trips the running job's abort flag; the worker stops at its next item and then emits
  // its terminal event, which resets the UI — so don't reset jobPhase here. Which flag to
  // trip depends on which job is running: they have separate ones, so cancelling a match
  // must not stop an index.
  const handleCancel = async () => {
    if (jobPhase === "matching") {
      await facesCancelMatch(api).catch(() => {});
      setLastResult("Cancelling — stops at the next face…");
      return;
    }
    await facesCancelJob(api).catch(() => {});
    setLastResult("Cancelling — stops after the current photo…");
  };

  const pct =
    progress && progress.total > 0
      ? Math.round((progress.done / progress.total) * 100)
      : null;

  const modelsReady = modelsStatus?.ready ?? false;

  return (
    <div>
      <h3>Faces</h3>

      {/* ── Model status ───────────────────────────────────────── */}
      <div className="iptc-row">
        <div className="iptc-label">Models</div>
        <div>
          {modelsLoading ? (
            <span className="term-note">Checking…</span>
          ) : modelsStatus == null ? (
            <span className="term-note">Status unavailable</span>
          ) : modelsReady ? (
            <span className="term-note" style={{ color: "#10b981" }}>
              YuNet + AuraFace ready
            </span>
          ) : (
            <span className="term-note">
              {modelsStatus.models.map((m) => (
                <span key={m.key}>
                  {m.key}: {m.present ? "OK" : "missing"}
                  {m.detail ? ` (${m.detail})` : ""}
                  {" "}
                </span>
              ))}
            </span>
          )}
        </div>
      </div>
      {!modelsReady && (
        <div className="iptc-row">
          <span className="term-note">
            YuNet (Apache-2.0) and AuraFace-v1 (Apache-2.0) will be downloaded from their
            official sources into the app data directory.
          </span>
        </div>
      )}
      <div className="iptc-row" style={{ gap: 8 }}>
        <button
          className="chip chip-on"
          onClick={handleDownloadModels}
          disabled={modelsLoading || (modelsReady && !modelsError)}
        >
          {modelsLoading ? "Downloading…" : modelsReady ? "Re-download models" : "Download models"}
        </button>
        {modelsError && <span className="term-note" style={{ color: "#ef4444" }}>{modelsError}</span>}
      </div>

      {/* ── Inference: where face detection/embedding actually runs ── */}
      <div className="iptc-row">
        <div className="iptc-label">Inference</div>
        <div>
          {inference == null ? (
            <span className="term-note">Checking…</span>
          ) : inference.ep === "cuda" ? (
            <span className="term-note" style={{ color: "#10b981" }}>GPU (CUDA)</span>
          ) : inference.ep === "cpu" ? (
            <span className="term-note">
              CPU
              {inference.cudaBuilt ? " — CUDA build fell back (see terminal log)" : ""}
            </span>
          ) : (
            <span className="term-note">
              Idle — determined on first run ({inference.cudaBuilt ? "CUDA-capable build" : "CPU build"})
            </span>
          )}
        </div>
      </div>
      <div className="iptc-row">
        <label className="iptc-label">Indexing speed</label>
        <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
          <select
            className="tag-input"
            value={inference?.speed ?? "background"}
            onChange={(e) => handleSpeedChange(e.target.value as "background" | "full")}
            disabled={inference == null}
          >
            <option value="background">Background (keep desktop responsive)</option>
            <option value="full">Full (use all cores / GPU)</option>
          </select>
          {speedNote && <span className="term-note">{speedNote}</span>}
        </div>
      </div>

      {/* ── People root ─────────────────────────────────────────── */}
      <div className="iptc-row">
        <label className="iptc-label">People root tag</label>
        <div style={{ position: "relative" }}>
          <input
            className="tag-input"
            type="text"
            value={peopleRoot}
            onChange={(e) => { setPeopleRoot(e.target.value); setSaved(false); setRootHighlight(0); }}
            onFocus={() => { setRootFocus(true); setRootHighlight(0); }}
            onBlur={() => setRootFocus(false)}
            onKeyDown={(e) => {
              if (e.key === "ArrowDown" && rootSuggestions.length) {
                e.preventDefault();
                setRootHighlight((h) => Math.min(h + 1, rootSuggestions.length - 1));
              } else if (e.key === "ArrowUp" && rootSuggestions.length) {
                e.preventDefault();
                setRootHighlight((h) => Math.max(h - 1, 0));
              } else if (e.key === "Enter" && rootSuggestions.length) {
                e.preventDefault();
                pickPeopleRoot(rootSuggestions[clampedRootHighlight].fullPath);
              } else if (e.key === "Escape") {
                setRootFocus(false);
              }
            }}
            placeholder="e.g. People"
            spellCheck={false}
            style={{ width: "100%", boxSizing: "border-box" }}
          />
          {rootSuggestions.length > 0 && (
            <div
              style={{
                position: "absolute",
                top: "100%",
                left: 0,
                right: 0,
                zIndex: 20,
                background: "var(--bg-panel)",
                border: "1px solid var(--border)",
                borderRadius: 6,
                overflow: "hidden",
                boxShadow: "var(--shadow-pop)",
              }}
            >
              {rootSuggestions.map((tag, i) => (
                <button
                  key={tag.id}
                  style={{
                    display: "block",
                    width: "100%",
                    textAlign: "left",
                    padding: "5px 10px",
                    fontSize: 12,
                    background: i === clampedRootHighlight ? "rgba(59,130,246,0.15)" : "none",
                    border: "none",
                    color: "var(--text)",
                    cursor: "pointer",
                  }}
                  onMouseEnter={() => setRootHighlight(i)}
                  // onMouseDown (not onClick) so the pick lands before the input's blur.
                  onMouseDown={(e) => { e.preventDefault(); pickPeopleRoot(tag.fullPath); }}
                >
                  {tag.fullPath}
                </button>
              ))}
            </div>
          )}
        </div>
      </div>
      <div className="iptc-row">
        <span className="term-note">
          Person tags must be descendants of this root (e.g. <code>People/Friends/Jane</code>).
          Used to seed recognition from existing photo-level tags.
        </span>
      </div>

      {/* ── Match threshold ─────────────────────────────────────── */}
      <div className="iptc-row">
        <label className="iptc-label">Match threshold (0–1)</label>
        <input
          className="tag-input"
          type="number"
          min="0.1"
          max="0.99"
          step="0.05"
          value={threshold}
          onChange={(e) => { setThreshold(e.target.value); setSaved(false); }}
        />
      </div>
      <div className="iptc-row">
        <span className="term-note">
          Cosine similarity threshold for face matching (default 0.45). Lower = more
          proposals but more false positives; higher = fewer but more precise.
        </span>
      </div>

      <div className="iptc-actions">
        <button className="chip chip-on" onClick={handleSave}>
          {saved ? "Saved" : "Save settings"}
        </button>
      </div>

      {/* ── Index + match actions ────────────────────────────────── */}
      <div className="iptc-row" style={{ marginTop: 16, borderTop: "1px solid var(--border)", paddingTop: 12 }}>
        <div className="iptc-label" style={{ fontWeight: 600, color: "var(--text)" }}>
          Index & match
        </div>
      </div>

      <div className="iptc-row">
        <span className="term-note">
          "Index faces" detects + embeds all unindexed photos. "Run matching" seeds known
          people from your tags and proposes suggestions for the rest. Both run in the
          background; you can cancel at any time.
        </span>
      </div>

      <div style={{ display: "flex", gap: 8, flexWrap: "wrap", marginTop: 6 }}>
        <button
          className="chip chip-on"
          onClick={handleIndexFaces}
          disabled={jobPhase !== "idle" || untracked || preparing || !modelsReady}
          title={!modelsReady ? "Download models first" : "Detect and embed faces in unindexed photos"}
        >
          {jobPhase === "indexing" ? "Indexing…" : "Index faces"}
        </button>
        <button
          className="chip chip-on"
          onClick={handleRunMatching}
          disabled={jobPhase !== "idle" || untracked || preparing || !modelsReady}
          title={!modelsReady ? "Download models first" : "Seed + match + cluster from existing person tags"}
        >
          {jobPhase === "matching" ? "Matching…" : "Run matching"}
        </button>
        {/* Shown for either job. `handleCancel` trips whichever flag belongs to the one
            that is running — gating this on "indexing" alone would leave matching
            cancellable by the backend and by the docs, but not by the user. */}
        {jobPhase !== "idle" && (
          <button className="chip" onClick={handleCancel}>
            Cancel
          </button>
        )}
      </div>

      {/* Progress bar. Determinate rendering requires a LIVE listener, not just a
          prior snapshot: without one the counts can never advance. */}
      {jobPhase !== "idle" && (
        <div style={{ marginTop: 8 }}>
          {progressLive && progress && (
            <>
              <div style={{ fontSize: 12, opacity: 0.75, marginBottom: 4 }}>
                {jobPhase === "indexing" ? "Indexing" : "Matching"}
                {progress.phase ? ` — ${progress.phase}` : ""}: {progress.done} / {progress.total > 0 ? progress.total : "…"}
                {pct != null ? ` (${pct}%)` : ""}
              </div>
              {pct != null && (
                <div style={{ height: 4, background: "rgba(255,255,255,0.12)", borderRadius: 2 }}>
                  <div
                    style={{
                      height: "100%",
                      background: "#3b82f6",
                      borderRadius: 2,
                      width: `${pct}%`,
                      transition: "width 0.2s",
                    }}
                  />
                </div>
              )}
            </>
          )}
          {!(progressLive && progress) && (
            <div style={{ fontSize: 12, opacity: 0.6 }}>
              {progressLive
                ? "Starting…"
                : /* No progress listener, so counts will never arrive — say what is
                     running rather than implying it is about to begin. */
                  jobPhase === "indexing"
                  ? "Indexing…"
                  : "Matching…"}
            </div>
          )}
        </div>
      )}

      {/* Persistent outcome of the last run — stays until the next job starts. */}
      {lastResult && jobPhase === "idle" && (
        <div style={{ fontSize: 12, marginTop: 8, color: "var(--ok)" }}>
          {lastResult}
        </div>
      )}
      {lastResult && jobPhase !== "idle" && (
        <div style={{ fontSize: 12, marginTop: 8, opacity: 0.75 }}>
          {lastResult}
        </div>
      )}

      {jobError && (
        <div className="modal-error" style={{ marginTop: 8 }}>
          {jobError}
        </div>
      )}
    </div>
  );
}

// ── FaceOverlayWrapper — loupe slot panel ────────────────────────────────────
//
// The loupe slot panel receives no external photoId — it uses api.getActivePhotoId()
// like inspector panels do. It renders a transparent overlay that sits over the
// .zoom-container by grabbing a reference to the nearest .zoom-container ancestor
// using a portal-like approach. Since the slot: "loupe" render is placed INSIDE the
// .loupe-inline div (alongside the ZoomableImage), we can render a relative-positioned
// wrapper that fills the same space.

function FaceOverlayPanel({ api }: { api: ChairPhotoAPI }) {
  useHostSelection(); // re-render on active photo change
  const photoId = api.getActivePhotoId();
  return <FaceOverlay photoId={photoId} api={api} />;
}

// ── FaceAvatar — face-crop thumbnail ──────────────────────────────────────────
//
// Renders a square avatar by loading the photo thumbnail and cropping it to the
// face bbox using a CSS clip technique on a relative-positioned container.
// No new backend render path — we use the existing thumb:// thumbnail and clip
// client-side via CSS overflow + object-position. The face bbox is normalized 0–1
// relative to the full image, so we can compute CSS percentage offsets.
//
// Technique: set the <img> to a larger size (1/bbox.w × width, 1/bbox.h × height)
// and translate it so the face region aligns with the container. The outer div has
// overflow:hidden to act as the clip mask.

interface FaceAvatarProps {
  photoId: number;
  bbox: FaceBbox;
  size?: number;
}

function FaceAvatar({ photoId, bbox, size = 72 }: FaceAvatarProps) {
  // Guard against zero/tiny bboxes to avoid division by zero.
  const bw = Math.max(bbox.w, 0.01);
  const bh = Math.max(bbox.h, 0.01);

  // Scale factor: how much larger than `size` to make the full image so the face
  // region fills the avatar area.
  const scaleX = 1 / bw;
  const scaleY = 1 / bh;

  // Rendered full-image dimensions.
  const imgW = size * scaleX;
  const imgH = size * scaleY;

  // Translate so the face bbox top-left aligns with the container's top-left.
  const offsetX = -(bbox.x * imgW);
  const offsetY = -(bbox.y * imgH);

  return (
    <div
      style={{
        width: size,
        height: size,
        overflow: "hidden",
        borderRadius: "50%",
        background: "rgba(30, 41, 59, 0.8)",
        flexShrink: 0,
        position: "relative",
      }}
    >
      <img
        src={thumbnailUrl(photoId)}
        alt=""
        draggable={false}
        style={{
          position: "absolute",
          width: imgW,
          height: imgH,
          left: offsetX,
          top: offsetY,
          objectFit: "cover",
          pointerEvents: "none",
        }}
        onError={(e) => {
          (e.currentTarget as HTMLImageElement).style.visibility = "hidden";
        }}
      />
    </div>
  );
}

// ── PersonCard — one tile in the people wall ──────────────────────────────────

interface PersonCardProps {
  person: PersonSummary;
  onFilter(): void;
}

function PersonCard({ person, onFilter }: PersonCardProps) {
  return (
    <button
      className="fa-pv-card"
      onClick={onFilter}
      title={`Filter Library to ${person.fullPath}`}
      style={{
        display: "flex",
        flexDirection: "column",
        alignItems: "center",
        gap: 8,
        padding: "12px 10px 10px",
        background: "rgba(30, 41, 59, 0.5)",
        border: "1px solid var(--border)",
        borderRadius: 8,
        cursor: "pointer",
        textAlign: "center",
        width: 108,
        transition: "background 0.15s",
      }}
      onMouseEnter={(e) => { (e.currentTarget as HTMLButtonElement).style.background = "rgba(59, 130, 246, 0.18)"; }}
      onMouseLeave={(e) => { (e.currentTarget as HTMLButtonElement).style.background = "rgba(30, 41, 59, 0.5)"; }}
    >
      <FaceAvatar photoId={person.avatarPhotoId} bbox={person.avatarBbox} size={72} />
      <div style={{ fontSize: 12, fontWeight: 600, color: "var(--text)", maxWidth: 90, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
        {person.name}
      </div>
      <div style={{ fontSize: 11, opacity: 0.6, lineHeight: 1.3 }}>
        {person.photoCount} photo{person.photoCount !== 1 ? "s" : ""}
        <br />
        {person.faceCount} face{person.faceCount !== 1 ? "s" : ""}
      </div>
    </button>
  );
}

// ── NameClusterModal — dialog to bind a cluster to a person tag ───────────────

interface NameClusterModalProps {
  clusterId: number;
  memberCount: number;
  peopleTags: Tag[];
  peopleRootPath: string;
  api: ChairPhotoAPI;
  onDone(): void;
  onCancel(): void;
}

function NameClusterModal({
  clusterId,
  memberCount,
  peopleTags,
  peopleRootPath,
  api,
  onDone,
  onCancel,
}: NameClusterModalProps) {
  const [tagPath, setTagPath] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  // Filter peopleTags as the user types for a type-ahead list.
  const query = tagPath.trim().toLowerCase();
  const suggestions = query.length > 0
    ? peopleTags
        .filter((t) => t.fullPath.toLowerCase().includes(query))
        .slice(0, 8)
    : [];

  const handleSave = async () => {
    const path = tagPath.trim();
    if (!path) { setError("Enter a name or tag path."); return; }

    // Ensure the path is under the people root if one is configured.
    let finalPath = path;
    if (
      peopleRootPath &&
      !finalPath.startsWith(peopleRootPath + "/") &&
      finalPath !== peopleRootPath
    ) {
      finalPath = `${peopleRootPath}/${path}`;
    }

    setBusy(true);
    setError("");
    try {
      await api.invoke<void>("faces_name_cluster", { cluster: clusterId, tagPath: finalPath });
      api.notifyChange();
      onDone();
    } catch (e: unknown) {
      setError(`Failed: ${String(e)}`);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div
      style={{
        position: "fixed",
        inset: 0,
        zIndex: 1000,
        background: "rgba(0,0,0,0.55)",
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
      }}
      onClick={onCancel}
    >
      <div
        style={{
          background: "var(--bg-panel)",
          border: "1px solid var(--border)",
          borderRadius: 10,
          padding: "20px 22px",
          minWidth: 320,
          maxWidth: 420,
        }}
        onClick={(e) => e.stopPropagation()}
      >
        <div style={{ fontWeight: 700, fontSize: 15, marginBottom: 10, color: "var(--text)" }}>
          Name this cluster
        </div>
        <div style={{ fontSize: 12, opacity: 0.7, marginBottom: 14 }}>
          {memberCount} face{memberCount !== 1 ? "s" : ""} — type a person name to create or assign an
          existing tag{peopleRootPath ? ` under ${peopleRootPath}` : ""}.
        </div>

        <input
          className="tag-input"
          type="text"
          autoFocus
          value={tagPath}
          onChange={(e) => { setTagPath(e.target.value); setError(""); }}
          placeholder={peopleRootPath ? `e.g. ${peopleRootPath}/Jane` : "e.g. People/Jane"}
          onKeyDown={(e) => {
            if (e.key === "Enter") void handleSave();
            if (e.key === "Escape") onCancel();
          }}
          style={{ width: "100%", boxSizing: "border-box", marginBottom: 4 }}
        />

        {/* Type-ahead suggestions */}
        {suggestions.length > 0 && (
          <div
            style={{
              border: "1px solid var(--border)",
              borderRadius: 6,
              marginBottom: 6,
              overflow: "hidden",
            }}
          >
            {suggestions.map((tag) => (
              <button
                key={tag.id}
                style={{
                  display: "block",
                  width: "100%",
                  textAlign: "left",
                  padding: "5px 10px",
                  fontSize: 12,
                  background: "none",
                  border: "none",
                  color: "var(--text)",
                  cursor: "pointer",
                }}
                onMouseEnter={(e) => { (e.currentTarget as HTMLButtonElement).style.background = "rgba(59,130,246,0.15)"; }}
                onMouseLeave={(e) => { (e.currentTarget as HTMLButtonElement).style.background = "none"; }}
                onClick={() => setTagPath(tag.fullPath)}
              >
                {tag.fullPath}
              </button>
            ))}
          </div>
        )}

        {error && (
          <div style={{ color: "#ef4444", fontSize: 12, marginBottom: 8 }}>{error}</div>
        )}

        <div style={{ display: "flex", gap: 8, marginTop: 10 }}>
          <button
            className="chip chip-on"
            onClick={() => void handleSave()}
            disabled={busy}
          >
            {busy ? "Saving…" : "Confirm"}
          </button>
          <button className="chip" onClick={onCancel}>
            Cancel
          </button>
        </div>
      </div>
    </div>
  );
}

// ── ClusterCard — one tile in the unnamed clusters section ───────────────────

interface ClusterCardProps {
  cluster: ClusterSummary;
  onName(): void;
}

function ClusterCard({ cluster, onName }: ClusterCardProps) {
  return (
    <button
      className="fa-pv-card"
      onClick={onName}
      title={`Name this cluster (${cluster.memberCount} face${cluster.memberCount !== 1 ? "s" : ""})`}
      style={{
        display: "flex",
        flexDirection: "column",
        alignItems: "center",
        gap: 8,
        padding: "12px 10px 10px",
        background: "rgba(30, 41, 59, 0.3)",
        border: "1px dashed var(--border)",
        borderRadius: 8,
        cursor: "pointer",
        textAlign: "center",
        width: 108,
        transition: "background 0.15s",
        opacity: 0.8,
      }}
      onMouseEnter={(e) => { (e.currentTarget as HTMLButtonElement).style.opacity = "1"; }}
      onMouseLeave={(e) => { (e.currentTarget as HTMLButtonElement).style.opacity = "0.8"; }}
    >
      <FaceAvatar photoId={cluster.avatarPhotoId} bbox={cluster.avatarBbox} size={72} />
      <div style={{ fontSize: 11, opacity: 0.6, lineHeight: 1.3 }}>
        {cluster.memberCount} face{cluster.memberCount !== 1 ? "s" : ""}
        <br />
        <span style={{ fontSize: 10, color: "#f59e0b" }}>Tap to name</span>
      </div>
    </button>
  );
}

// ── SuggestionsQueue — review queue for suggested faces ───────────────────────

interface SuggestionsQueueProps {
  api: ChairPhotoAPI;
  onDone(): void;
}

function SuggestionsQueue({ api, onDone }: SuggestionsQueueProps) {
  const [suggestions, setSuggestions] = useState<SuggestionEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [confidenceThreshold, setConfidenceThreshold] = useState(0.8);
  const [confirming, setConfirming] = useState<Set<number>>(new Set());
  const [rejecting, setRejecting] = useState<Set<number>>(new Set());
  const [bulkBusy, setBulkBusy] = useState(false);

  const load = useCallback(() => {
    setLoading(true);
    facesSuggestionList(api)
      .then((rows) => { setSuggestions(rows); setLoading(false); })
      .catch(() => setLoading(false));
  }, [api]);

  useEffect(() => { load(); }, [load]);

  const handleConfirm = async (entry: SuggestionEntry) => {
    setConfirming((s) => new Set(s).add(entry.faceId));
    try {
      await faceConfirm(api, entry.faceId);
      setSuggestions((prev) => prev.filter((e) => e.faceId !== entry.faceId));
      api.notifyChange();
    } catch {
      // ignore individual errors
    } finally {
      setConfirming((s) => { const n = new Set(s); n.delete(entry.faceId); return n; });
    }
  };

  const handleReject = async (entry: SuggestionEntry) => {
    setRejecting((s) => new Set(s).add(entry.faceId));
    try {
      await faceReject(api, entry.faceId);
      setSuggestions((prev) => prev.filter((e) => e.faceId !== entry.faceId));
      api.notifyChange();
    } catch {
      // ignore
    } finally {
      setRejecting((s) => { const n = new Set(s); n.delete(entry.faceId); return n; });
    }
  };

  const aboveThreshold = suggestions.filter((e) => e.confidence >= confidenceThreshold);

  const handleConfirmAll = async () => {
    if (aboveThreshold.length === 0) return;
    setBulkBusy(true);
    for (const entry of aboveThreshold) {
      try { await faceConfirm(api, entry.faceId); } catch { /* skip */ }
    }
    setBulkBusy(false);
    api.notifyChange();
    load(); // reload remaining
  };

  if (loading) {
    return <div style={{ padding: 24, opacity: 0.6 }}>Loading suggestions…</div>;
  }

  if (suggestions.length === 0) {
    return (
      <div style={{ padding: 24, textAlign: "center" }}>
        <div style={{ fontSize: 14, opacity: 0.7, marginBottom: 12 }}>
          No pending suggestions.
        </div>
        <button className="chip" onClick={onDone}>
          Back to People
        </button>
      </div>
    );
  }

  return (
    <div className="fa-pv-root">
      {/* Header */}
      <div className="fa-pv-header">
        <button className="chip" style={{ marginRight: 12 }} onClick={onDone}>
          ← People
        </button>
        <span style={{ fontWeight: 700, fontSize: 15 }}>
          Review suggestions ({suggestions.length})
        </span>
      </div>

      {/* Bulk confirm above threshold */}
      <div style={{ display: "flex", alignItems: "center", gap: 10, padding: "10px 20px", borderBottom: "1px solid var(--border)" }}>
        <label style={{ fontSize: 12, opacity: 0.75 }}>
          Confidence threshold:
        </label>
        <input
          type="range"
          min={0}
          max={1}
          step={0.05}
          value={confidenceThreshold}
          onChange={(e) => setConfidenceThreshold(parseFloat(e.target.value))}
          style={{ width: 120 }}
        />
        <span style={{ fontSize: 12, opacity: 0.75, minWidth: 36 }}>
          {Math.round(confidenceThreshold * 100)}%
        </span>
        <button
          className="chip chip-on"
          onClick={() => void handleConfirmAll()}
          disabled={bulkBusy || aboveThreshold.length === 0}
          title={`Confirm all ${aboveThreshold.length} suggestions at ≥${Math.round(confidenceThreshold * 100)}% confidence`}
        >
          {bulkBusy ? "Confirming…" : `Confirm all ≥${Math.round(confidenceThreshold * 100)}% (${aboveThreshold.length})`}
        </button>
      </div>

      {/* Suggestion list */}
      <div style={{ overflowY: "auto", flex: 1 }}>
        {suggestions.map((entry) => (
          <div
            key={entry.faceId}
            style={{
              display: "flex",
              alignItems: "center",
              gap: 12,
              padding: "10px 20px",
              borderBottom: "1px solid rgba(255,255,255,0.05)",
              opacity: entry.confidence < confidenceThreshold ? 0.55 : 1,
            }}
          >
            <FaceAvatar photoId={entry.photoId} bbox={entry.bbox} size={52} />
            <div style={{ flex: 1, minWidth: 0 }}>
              <div style={{ fontWeight: 600, fontSize: 13, color: "#10b981" }}>
                {entry.personFullPath}
              </div>
              <div style={{ fontSize: 11, opacity: 0.7 }}>
                Confidence: {Math.round(entry.confidence * 100)}%
              </div>
            </div>
            <div style={{ display: "flex", gap: 6, flexShrink: 0 }}>
              <button
                className="chip"
                style={{ fontSize: 12, padding: "3px 9px", color: "#10b981", borderColor: "#10b981" }}
                onClick={() => void handleConfirm(entry)}
                disabled={confirming.has(entry.faceId) || rejecting.has(entry.faceId)}
              >
                ✓
              </button>
              <button
                className="chip"
                style={{ fontSize: 12, padding: "3px 9px", color: "#ef4444", borderColor: "#ef4444" }}
                onClick={() => void handleReject(entry)}
                disabled={confirming.has(entry.faceId) || rejecting.has(entry.faceId)}
              >
                ✕
              </button>
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}

// ── PeopleView — the main view (H1 slot) ─────────────────────────────────────
//
// Three sections:
//   1. Named people wall — grid of PersonCards; clicking filters the Library to
//      that person via api.filterByTag(tagId).
//   2. Unnamed clusters — grid of ClusterCards; clicking opens NameClusterModal.
//   3. "Review suggestions" — opens SuggestionsQueue inline.

type PeopleViewSection = "people" | "suggestions";

function PeopleView({ api }: { api: ChairPhotoAPI }) {
  const [section, setSection] = useState<PeopleViewSection>("people");
  const [people, setPeople] = useState<PersonSummary[]>([]);
  const [clusters, setClusters] = useState<ClusterSummary[]>([]);
  const [peopleTags, setPeopleTags] = useState<Tag[]>([]);
  const [peopleRootPath, setPeopleRootPath] = useState("");
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  // The cluster currently being named (null = modal closed).
  const [namingCluster, setNamingCluster] = useState<ClusterSummary | null>(null);
  // Suggestion count badge for the "Review" button.
  const [suggestionCount, setSuggestionCount] = useState<number | null>(null);

  const loadData = useCallback(() => {
    setLoading(true);
    setError("");
    Promise.all([
      facesPeopleSummary(api),
      facesClusterSummary(api),
      facesSuggestionList(api),
      api.listTags(),
      api.getSetting(SETTING_PEOPLE_ROOT).catch(() => ""),
    ])
      .then(([ppl, cls, suggestions, tags, root]) => {
        setPeople(ppl);
        setClusters(cls);
        setSuggestionCount(suggestions.length);
        const rootPath = root?.trim() ?? PEOPLE_ROOT_DEFAULT;
        setPeopleRootPath(rootPath);
        if (rootPath) {
          setPeopleTags(
            tags.filter(
              (t) =>
                t.fullPath === rootPath ||
                t.fullPath.startsWith(rootPath + "/")
            )
          );
        } else {
          setPeopleTags(tags.filter((t) => !t.autoRule));
        }
        setLoading(false);
      })
      .catch((e: unknown) => {
        setError(String(e));
        setLoading(false);
      });
  }, [api]);

  useEffect(() => { loadData(); }, [loadData]);

  if (section === "suggestions") {
    return (
      <SuggestionsQueue
        api={api}
        onDone={() => { setSection("people"); loadData(); }}
      />
    );
  }

  return (
    <div className="fa-pv-root">
      {/* Header bar */}
      <div className="fa-pv-header">
        <span style={{ fontWeight: 700, fontSize: 15 }}>People</span>
        <div style={{ marginLeft: "auto", display: "flex", gap: 8, alignItems: "center" }}>
          <button
            className="chip"
            onClick={loadData}
            title="Refresh"
            style={{ fontSize: 11 }}
          >
            Refresh
          </button>
          <button
            className="chip chip-on"
            onClick={() => setSection("suggestions")}
            title="Review unconfirmed suggestions"
            style={{ fontSize: 11 }}
          >
            Review suggestions{suggestionCount != null && suggestionCount > 0 ? ` (${suggestionCount})` : ""}
          </button>
        </div>
      </div>

      {loading && (
        <div style={{ padding: 32, textAlign: "center", opacity: 0.6 }}>Loading…</div>
      )}
      {error && (
        <div style={{ padding: 20, color: "#ef4444" }}>{error}</div>
      )}

      {!loading && !error && (
        <div style={{ overflowY: "auto", flex: 1, padding: "16px 20px" }}>
          {/* Named people wall */}
          {people.length > 0 ? (
            <>
              <div className="fa-pv-section-title">
                {people.length} {people.length === 1 ? "person" : "people"}
              </div>
              <div className="fa-pv-grid">
                {people.map((p) => (
                  <PersonCard
                    key={p.tagId}
                    person={p}
                    onFilter={() => api.filterByTag(p.tagId)}
                  />
                ))}
              </div>
            </>
          ) : (
            <div className="fa-pv-empty">
              <svg width="40" height="40" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
                <circle cx="12" cy="8" r="4" />
                <path d="M4 20c0-4 3.6-7 8-7s8 3 8 7" />
              </svg>
              <div>No named people yet.</div>
              <div style={{ fontSize: 11 }}>
                Index faces and run matching to populate this view.
              </div>
            </div>
          )}

          {/* Unnamed clusters section */}
          {clusters.length > 0 && (
            <div style={{ marginTop: 28 }}>
              <div className="fa-pv-section-title">
                Unnamed clusters ({clusters.length})
              </div>
              <div style={{ fontSize: 12, opacity: 0.6, marginBottom: 10 }}>
                Click a cluster to name the person.
              </div>
              <div className="fa-pv-grid">
                {clusters.map((cl) => (
                  <ClusterCard
                    key={cl.clusterId}
                    cluster={cl}
                    onName={() => setNamingCluster(cl)}
                  />
                ))}
              </div>
            </div>
          )}

          {/* Empty state when no people and no clusters */}
          {people.length === 0 && clusters.length === 0 && (
            <div style={{ marginTop: 12, fontSize: 12, opacity: 0.6 }}>
              After running face indexing and matching, named people and unnamed
              clusters will appear here.
            </div>
          )}
        </div>
      )}

      {/* Name-cluster modal */}
      {namingCluster != null && (
        <NameClusterModal
          clusterId={namingCluster.clusterId}
          memberCount={namingCluster.memberCount}
          peopleTags={peopleTags}
          peopleRootPath={peopleRootPath}
          api={api}
          onDone={() => {
            setNamingCluster(null);
            loadData();
          }}
          onCancel={() => setNamingCluster(null)}
        />
      )}
    </div>
  );
}

// People-root default (mirrors the backend constant).
const PEOPLE_ROOT_DEFAULT = "People";

// ── Module export ─────────────────────────────────────────────────────────────

export const facesModule: ChairPhotoModule = {
  id: "faces",
  name: "Face Tagging",
  version: "0.1.0",
  description:
    "Detect and recognize faces locally using YuNet + AuraFace (Apache-2.0, no cloud). " +
    "Bootstraps names from existing person tags. Overlay in the loupe shows face regions " +
    "with confirm / reject / reassign actions. Requires the faces backend feature.",
  backendFeature: "faces",

  onLoad(api) {
    // People main view: wall of named people + unnamed clusters + suggestion queue.
    api.registerMainView({
      id: "people",
      label: "People",
      icon: (
        <svg
          width="13"
          height="13"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="2"
          strokeLinecap="round"
          strokeLinejoin="round"
          aria-hidden
        >
          <circle cx="9" cy="7" r="4" />
          <path d="M1 21v-2a7 7 0 0 1 14 0v2" />
          <path d="M16 11c1.5-.5 3-.5 4 0.5a5.5 5.5 0 0 1 3 5v2" />
          <circle cx="18" cy="7" r="3" />
        </svg>
      ),
      render: () => <PeopleView api={api} />,
    });

    // Inspector panel: face list with review actions for the active photo.
    api.registerPanel({
      id: "faces-inspector",
      label: "Faces",
      slot: "inspector",
      render: () => <FacesInspectorPanel api={api} />,
    });

    // Loupe overlay: face rectangles + chips over the displayed photo.
    api.registerPanel({
      id: "faces-loupe-overlay",
      label: "Face overlay",
      slot: "loupe",
      render: () => <FaceOverlayPanel api={api} />,
    });

    // Settings panel: people root, threshold, model download, index/match actions.
    api.registerSettingsPanel(() => <FacesSettings api={api} />);
  },
};
