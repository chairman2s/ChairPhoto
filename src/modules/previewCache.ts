// Preview prefetching via the native `preview://` protocol.
//
// Previews load through a custom URI scheme (see src/protocol.rs), so the webview
// fetches and caches them by URL — already-seen images redisplay instantly. To
// make *unseen* images fast too, we warm that cache ahead of navigation by asking
// the browser to load the URL into a detached Image(); when the user arrows to it,
// it's already decoded. This is the AGENTS.md preload invariant.

import { convertFileSrc } from "@tauri-apps/api/core";

/** The native URL for a photo's loupe preview. `bust` changes the URL after a
 *  rotation/recovery so the webview re-fetches instead of serving the cached image. */
export function previewUrl(photoId: number, bust?: number): string {
  return convertFileSrc(String(photoId), "preview") + (bust ? `?v=${bust}` : "");
}

/** The native URL for a photo's full-resolution zoom image (embedded full preview). */
export function zoomUrl(photoId: number, bust?: number): string {
  return convertFileSrc(String(photoId), "zoom") + (bust ? `?v=${bust}` : "");
}

// <video> on WebKitGTK is decoded by GStreamer, which can't read custom URI schemes, so
// videos are served by a loopback HTTP server (see src-tauri/src/protocol.rs). The port is
// fetched once at startup via setVideoPort().
let videoPort = 0;
export function setVideoPort(port: number): void {
  videoPort = port;
}

/** The loopback URL for a catalog video, for a <video> element. */
export function videoUrl(photoId: number): string {
  return `http://127.0.0.1:${videoPort}/${photoId}`;
}

/** Whether a catalog path is a video (matches the backend VIDEO_EXTENSIONS). */
export function isVideoPath(path: string): boolean {
  return /\.(mp4|m4v|mov|avi|webm|mkv)$/i.test(path);
}

// Detached Image objects we've kicked off, keyed by id, so we don't re-issue the
// same prefetch. Bounded so the set doesn't grow without limit during long sessions.
const prefetched = new Map<number, HTMLImageElement>();
const MAX_TRACKED = 60;

/** Warm the webview cache for a photo you expect to view soon. Fire-and-forget. */
export function prefetch(photoId: number | null | undefined): void {
  if (photoId == null || prefetched.has(photoId)) return;
  const img = new Image();
  img.src = previewUrl(photoId);
  prefetched.set(photoId, img);
  while (prefetched.size > MAX_TRACKED) {
    const oldest = prefetched.keys().next().value;
    if (oldest === undefined) break;
    prefetched.delete(oldest);
  }
}
