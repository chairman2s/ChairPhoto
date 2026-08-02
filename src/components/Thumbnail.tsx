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
  status,
  metadataReady = true,
}: {
  photoId: number;
  bust?: number;
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
  const url =
    convertFileSrc(String(photoId), "thumb") + (bust ? `?v=${bust}` : "");
  return (
    <img
      className="thumb"
      src={url}
      loading="lazy"
      alt=""
      onError={() => setFailed(true)}
    />
  );
}
