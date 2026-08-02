import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { ZoomableImage } from "./components/ZoomableImage";
import { announceReady, onPhoto, type LoupePhoto } from "./modules/loupe";
import { getSetting, listTags, pluginFeatures, renderEdit } from "./modules/api";
import { parseEdit } from "./modules/editing";
import { FaceOverlay, type FaceOverlayApi } from "./modules/plugins/faces";
import "./App.css";

// Root component for the popped-out loupe window (rendered when the URL hash is
// "#loupe"). It shows the photo (and version) the main window tells it to, and
// follows along as the selection changes there — so you can park it on a second
// screen.
export default function LoupeWindow() {
  const [photo, setPhoto] = useState<LoupePhoto>({ photoId: null, editJson: null });
  // The rendered edit (data URL) when a version is active; "" = show the Original.
  const [editedSrc, setEditedSrc] = useState("");
  const photoId = photo.photoId;
  // Whether the faces backend feature is present in this build.
  const [facesEnabled, setFacesEnabled] = useState(false);

  // Build a stable FaceOverlayApi shim that the FaceOverlay can use without
  // access to the full ChairPhotoAPI (which lives in App.tsx's module host).
  const faceApiRef = useRef<FaceOverlayApi>({
    listTags: () => listTags(),
    // The module host namespaces module settings as "<moduleId>.<key>", so this
    // shim must apply the same prefix or lookups silently miss (raw getSetting
    // bypasses the host).
    getSetting: (key) => getSetting(`faces.${key}`),
    // notifyChange in the pop-out window does nothing — the main window will
    // pick up catalog changes via its own polling / event listeners.
    notifyChange: () => {},
    // The overlay confirms/rejects/draws faces, so it needs the command channel.
    // This mirrors what host.ts hands a real module; the dependency is not new,
    // it was previously hidden behind core wrappers in api.ts.
    invoke: (command, args) => invoke(command, args),
  });

  useEffect(() => {
    pluginFeatures()
      .then((features) => setFacesEnabled(features.includes("faces")))
      .catch(() => setFacesEnabled(false));
  }, []);

  useEffect(() => {
    const unlisten = onPhoto(setPhoto);
    announceReady();
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  // Render the active version's edit, mirroring the main window's inline loupe
  // (same renderEdit backend command — no module host needed here). Falls back to
  // the unedited preview when there's no version or the render fails (e.g. the
  // edit engine is compiled out).
  useEffect(() => {
    const { photoId, editJson } = photo;
    if (photoId == null || editJson == null) {
      setEditedSrc("");
      return;
    }
    let cancelled = false;
    renderEdit(photoId, JSON.stringify(parseEdit(editJson)), 0)
      .then((url) => !cancelled && setEditedSrc(url))
      .catch(() => !cancelled && setEditedSrc(""));
    return () => {
      cancelled = true;
    };
  }, [photo]);

  // Zoom-in render for the active version: the same edit over the native-size preview
  // tier, so zooming a (cropped) version magnifies real pixels — the fast render can
  // be smaller than the window. Fetched lazily by ZoomableImage on first zoom.
  const { photoId: hiPhotoId, editJson: hiEditJson } = photo;
  const hiSrcOverride = useCallback(() => {
    if (hiPhotoId == null || hiEditJson == null) return Promise.resolve("");
    return renderEdit(hiPhotoId, JSON.stringify(parseEdit(hiEditJson)), 0, true);
  }, [hiPhotoId, hiEditJson]);

  return (
    // The .loupe-window class is used by FaceOverlay's DOM traversal to locate
    // the .zoom-container and its <img> for letterbox geometry computation.
    <div className="loupe-window">
      {photoId == null ? (
        <div className="loupe-empty">No photo selected</div>
      ) : (
        // Wrap ZoomableImage in a relative-positioned container so the face
        // overlay can be absolutely positioned over it.
        <div style={{ position: "relative", display: "flex", flexDirection: "column", flex: 1, minHeight: 0 }}>
          <ZoomableImage
            photoId={photoId}
            srcOverride={editedSrc || undefined}
            hiSrcOverride={editedSrc ? hiSrcOverride : undefined}
          />
          {facesEnabled && (
            <div style={{ position: "absolute", inset: 0, pointerEvents: "none" }}>
              <FaceOverlay photoId={photoId} api={faceApiRef.current} />
            </div>
          )}
        </div>
      )}
    </div>
  );
}
