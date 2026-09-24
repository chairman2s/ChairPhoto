import { noteTileLoaded, noteTileMounted } from "../modules/shellTiming";
import { useEffect, useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import type { StorageStatus } from "../modules/api";

// A grid thumbnail served by the native `thumb://` protocol (see src/protocol.rs).
// The webview fetches the image directly — no base64 over IPC — and caches it by
// URL, so re-display is instant. The browser also bounds concurrent requests per
// host, so a large grid doesn't stampede the backend.
//
// `bust` cache-busts the URL: the protocol responds with `Cache-Control: max-age`,
// so after recovering a missing photo (relocate / retrieve-from-NAS) the same URL
// would re-serve the stale 404. Bumping `bust` forces a fresh fetch and clears the
// failed state so the tile stops showing "no preview".
export function Thumbnail({
  photoId,
  bust,
  cover,
  status,
  metadataReady = true,
}: {
  photoId: number;
  bust?: number;
  /** The photo's cover token (`Photo.coverToken`): part of the URL, so a new cover — or
   *  an edit to it — fetches a new thumbnail instead of the webview's cached one. */
  cover?: string | null;
  /** Storage state, used to label the placeholder when the thumbnail can't load. */
  status?: StorageStatus;
  /**
   * Phase A/B boundary flag from `photos.metadata_ready` (I6a).
   * When false the thumbnail hasn't been extracted yet — show a grey
   * placeholder with a spinner instead of firing a thumb:// request that
   * would immediately 404.
   */
  metadataReady?: boolean;
}) {
  // Shell-timing evidence (dev toggle): one mounted tile per Thumbnail instance.
  useEffect(() => {
    noteTileMounted();
  }, []);

  const [failed, setFailed] = useState(false);

  // A new bust value means "the underlying file may have changed" — try again.
  useEffect(() => setFailed(false), [bust]);

  // Phase A placeholder: thumbnail not yet available (Phase B will extract it).
  // Don't attempt a thumb:// request — there's nothing there yet.
  if (!metadataReady) {
    return (
      <div className="thumb thumb-placeholder">
        <span className="thumb-placeholder-spinner" aria-hidden />
      </div>
    );
  }

  if (failed) {
    const onNas = status === "archived" || status === "offline";
    const label = onNas ? "On NAS" : status === "missing" ? "Missing" : "No preview";
    return (
      <div className="thumb thumb-failed">
        <span className="thumb-failed-icon">{onNas ? "☁" : "⚠"}</span>
        <span>{label}</span>
      </div>
    );
  }
  const url = thumbUrl(photoId, bust, cover);
  return (
    <img
      className="thumb"
      src={url}
      loading="lazy"
      alt=""
      onLoad={noteTileLoaded}
      onError={() => setFailed(true)}
    />
  );
}

/** The `thumb://` URL for a photo: `v` busts after a file recovery, `c` follows the cover. */
export function thumbUrl(photoId: number, bust?: number, cover?: string | null): string {
  const q = new URLSearchParams();
  if (bust) q.set("v", String(bust));
  if (cover) q.set("c", cover);
  const qs = q.toString();
  return convertFileSrc(String(photoId), "thumb") + (qs ? `?${qs}` : "");
}
