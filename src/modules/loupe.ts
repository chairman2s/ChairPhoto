// Pop-out loupe window: open it, and sync what it displays across windows.
//
// Both windows share the same backend process (and the same open catalog), so the
// loupe window only needs to know *which* photo (and version) to show. The main
// window emits `loupe:photo` whenever its selection or active version changes; the
// loupe window listens. A module can instead put a card up (`loupe:card`, via
// `ChairPhotoAPI.showInLoupe`) — the loupe shows the card while one is set. When the
// loupe window opens it emits `loupe:ready` so the main window re-sends the current
// selection and the host replays the card (handling the race where it opens
// mid-session).

import { emit, listen, type UnlistenFn } from "@tauri-apps/api/event";
import { WebviewWindow } from "@tauri-apps/api/webviewWindow";
import type { LoupeCard } from "./registry";

const LOUPE_LABEL = "loupe";
export const EVENT_PHOTO = "loupe:photo";
export const EVENT_CARD = "loupe:card";
export const EVENT_READY = "loupe:ready";

/** Open the pop-out loupe window (or focus it if already open). */
export async function openLoupeWindow(): Promise<void> {
  const existing = await WebviewWindow.getByLabel(LOUPE_LABEL);
  if (existing) {
    await existing.setFocus();
    return;
  }
  new WebviewWindow(LOUPE_LABEL, {
    url: "index.html#loupe",
    title: "ChairPhoto — Loupe",
    width: 1280,
    height: 800,
  });
}

/** What the loupe window should display: a photo, optionally a version of it. */
export interface LoupePhoto {
  photoId: number | null;
  /** The active version's edit record (JSON string), or null for the Original. */
  editJson: string | null;
}

/** Tell any open loupe window which photo (and version) to display. */
export function broadcastPhoto(photoId: number | null, editJson: string | null = null): void {
  emit(EVENT_PHOTO, { photoId, editJson } satisfies LoupePhoto);
}

/** Loupe window: subscribe to photo/version changes. Returns an unlisten function. */
export function onPhoto(handler: (photo: LoupePhoto) => void): Promise<UnlistenFn> {
  return listen<LoupePhoto>(EVENT_PHOTO, (e) => handler(e.payload));
}

/** Tell any open loupe window to show a module's card instead of the photo (null = photo). */
export function broadcastCard(card: LoupeCard | null): void {
  emit(EVENT_CARD, card);
}

/** Loupe window: subscribe to card changes. Returns an unlisten function. */
export function onCard(handler: (card: LoupeCard | null) => void): Promise<UnlistenFn> {
  return listen<LoupeCard | null>(EVENT_CARD, (e) => handler(e.payload));
}

/** Main window: respond to a loupe window announcing it is ready. */
export function onLoupeReady(handler: () => void): Promise<UnlistenFn> {
  return listen(EVENT_READY, () => handler());
}

/** Loupe window: announce readiness so the main window resends its selection. */
export function announceReady(): void {
  emit(EVENT_READY);
}
