// Snapchat module: there is no usable API to post to a Snapchat story, so the workflow is
// "send the photo to the phone via LocalSend, then post the story by hand" — and ChairPhoto
// records it as published to Snapchat. It is a thin layer over the LocalSend backend feature
// (`localsend_send`) and reuses the shared SendToDevicePanel; the only differences from the
// plain LocalSend target are:
//   - onSent → api.recordPublication (the host stamps this module's marker "snapchat"), and
//   - a non-blocking 9:16 aspect preflight (Snapchat stories are vertical 1080×1920).
// It `requires` the localsend module (same backend feature, ^0.1.0). See docs/localsend.md.

import type { ChairPhotoModule, Photo } from "../registry";
import type { PhotoVersion } from "../api";
import { SendToDevicePanel } from "./SendToDevicePanel";

// Snapchat story aspect: vertical 9:16 (1080×1920).
const SNAP_ASPECT = 9 / 16;
// Tolerance for "close enough to 9:16" (~±3%).
const ASPECT_TOLERANCE = 0.03;

/**
 * Pure helper: the effective width/height aspect (width / height) of what would be sent.
 *
 * Priority:
 *   1. the selected version's crop, read generically from its editJson —
 *      an `aspect` label like "9:16" (W:H), else a normalized crop `w`/`h` applied to the
 *      photo's pixel dimensions;
 *   2. the photo's original pixel dimensions (photo.width / photo.height).
 *
 * Returns null when nothing usable is available (no crop and no known dimensions).
 */
export function effectiveAspect(
  photo: Pick<Photo, "width" | "height">,
  editJson: string | null | undefined,
): number | null {
  const crop = parseCrop(editJson);

  if (crop) {
    // An explicit aspect label ("W:H") wins — it is the user's stated intent.
    const labelled = parseAspectLabel(crop.aspect);
    if (labelled != null) return labelled;

    // Otherwise derive from the normalized crop rect over the photo's pixel dimensions.
    if (
      typeof crop.w === "number" &&
      typeof crop.h === "number" &&
      crop.w > 0 &&
      crop.h > 0 &&
      photo.width &&
      photo.height
    ) {
      const aspect = (photo.width * crop.w) / (photo.height * crop.h);
      if (isFinite(aspect) && aspect > 0) return aspect;
    }
  }

  if (photo.width && photo.height && photo.width > 0 && photo.height > 0) {
    return photo.width / photo.height;
  }
  return null;
}

/** True when `aspect` is within ±tolerance (fractional) of 9:16. */
export function isNearSnapAspect(aspect: number | null, tolerance = ASPECT_TOLERANCE): boolean {
  if (aspect == null || !isFinite(aspect) || aspect <= 0) return false;
  return Math.abs(aspect - SNAP_ASPECT) <= SNAP_ASPECT * tolerance;
}

/**
 * Non-blocking preflight: returns a warning string when the effective aspect isn't ~9:16,
 * else null. The user can still send.
 */
export function snapchatPreflight(photo: Photo, version: PhotoVersion | null): string | null {
  const aspect = effectiveAspect(photo, version?.editJson);
  if (aspect == null || isNearSnapAspect(aspect)) return null;
  return "Snapchat stories are vertical 9:16 (1080×1920) — make a 9:16 crop for best results.";
}

// --- internal: tolerant, generic readers of the version's editJson ---

interface LooseCrop {
  w?: number;
  h?: number;
  aspect?: string;
}

function parseCrop(editJson: string | null | undefined): LooseCrop | null {
  if (!editJson) return null;
  try {
    const parsed = JSON.parse(editJson) as { crop?: LooseCrop };
    const crop = parsed?.crop;
    if (crop && typeof crop === "object") return crop;
  } catch {
    // Malformed editJson → fall back to photo dimensions.
  }
  return null;
}

/** Parse a "W:H" aspect label (e.g. "9:16") into width/height. null if not parseable. */
function parseAspectLabel(label: string | undefined): number | null {
  if (!label) return null;
  const m = label.match(/^\s*(\d+(?:\.\d+)?)\s*:\s*(\d+(?:\.\d+)?)\s*$/);
  if (!m) return null;
  const w = Number(m[1]);
  const h = Number(m[2]);
  if (!isFinite(w) || !isFinite(h) || w <= 0 || h <= 0) return null;
  return w / h;
}

export const snapchatModule: ChairPhotoModule = {
  id: "snapchat",
  name: "Snapchat",
  version: "0.1.0",
  description:
    "Send a photo to your phone via LocalSend, then post the Snapchat story by hand — and record it as published to Snapchat. Warns when the crop isn't vertical 9:16.",
  backendFeature: "localsend",
  publicationMarker: "snapchat",
  requires: [{ id: "localsend", version: "^0.1.0" }],
  // Snapchat renders the same SendToDevicePanel but hands it ITS OWN `api`, so the calls
  // are made under the snapchat identity and must be declared here too (#48). Requiring
  // the localsend module does not inherit its permissions — grants do not cascade.
  permissions: { commands: ["localsend_discover", "localsend_send"] },
  onLoad(api) {
    api.registerPublishTarget({
      id: "snapchat",
      label: "Snapchat",
      render: () => (
        <SendToDevicePanel
          api={api}
          onSent={(photoId, versionId) => api.recordPublication(photoId, versionId)}
          preflight={snapchatPreflight}
        />
      ),
    });
  },
};
