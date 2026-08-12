// Instagram publishing module: posts the selected version to Instagram by driving Chrome
// (the `instagram` backend feature), as one destination in the unified Publish dialog.
// Unlike Flickr/SmugMug there's no API key — auth is the Chrome login. Records the
// publication (marker "instagram") only on a confirmed post. See docs/instagram.md.

import { useCallback, useEffect, useState } from "react";
import type { ChairPhotoAPI, ChairPhotoModule } from "../registry";
import { listVersions, PhotoVersion } from "../api";
import { useHostSelection } from "../host";

// ── Backend commands (owned by this module) ───────────────────────────────────
// Per the module contract, a module's own commands go through `ChairPhotoAPI.invoke`
// rather than core's `api.ts`, so the command names travel with the module.

/** Outcome of an Instagram post attempt. */
type InstagramOutcome = "posted" | "awaitingReview" | "needsLogin";

/**
 * Render the selected version to an Instagram-sized JPEG and post it by driving Chrome.
 * `publish` clicks Share; otherwise the post is composed and left on screen for review.
 */
const postToInstagram = (
  api: ChairPhotoAPI,
  photoId: number,
  versionId: number | null,
  caption: string,
  publish: boolean,
) =>
  api.invoke<InstagramOutcome>("post_to_instagram", {
    photoId,
    versionId: versionId ?? null,
    caption,
    publish,
  });

/** A suggested Instagram caption for a photo: title + its keywords as #hashtags. */
const buildInstagramCaption = (api: ChairPhotoAPI, photoId: number) =>
  api.invoke<string>("build_instagram_caption", { photoId });

/**
 * Snapshot of the photo/version that was submitted for a supervised post.
 * Shown after the CDP automation stops before Share so the user can confirm
 * whether they completed the post by hand.
 */
interface PendingReview {
  photoId: number;
  versionId: number | null;
}

function InstagramForm({ api }: { api: ChairPhotoAPI }) {
  // Rendered inside PublishDialog, which (issue #16) now subscribes only to
  // contributions — it no longer re-renders on selection changes, so this form needs
  // its own subscription to pick up the active photo/version.
  useHostSelection();
  const photoId = api.getActivePhotoId();
  const [versions, setVersions] = useState<PhotoVersion[]>([]);
  const [versionId, setVersionId] = useState<number | null>(api.getActiveVersionId());
  const [caption, setCaption] = useState("");
  const [captionTouched, setCaptionTouched] = useState(false);
  const [autoPublish, setAutoPublish] = useState(false);
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState("");
  /** Set when the CDP run stopped before Share; waits for the user's answer. */
  const [pendingReview, setPendingReview] = useState<PendingReview | null>(null);

  const reload = useCallback(() => {
    if (photoId == null) {
      setVersions([]);
      return;
    }
    listVersions(photoId).then(setVersions).catch(() => setVersions([]));
    setVersionId(api.getActiveVersionId());
  }, [photoId, api]);

  useEffect(() => {
    reload();
  }, [reload]);

  // Prefill the caption (title + #hashtags) from the photo until the user edits it.
  useEffect(() => {
    if (photoId == null || captionTouched) return;
    let alive = true;
    buildInstagramCaption(api, photoId)
      .then((c) => alive && setCaption(c))
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [photoId, captionTouched, api]);

  const post = async () => {
    if (photoId == null) return;
    setBusy(true);
    setStatus("");
    setPendingReview(null);
    try {
      const outcome = await postToInstagram(api, photoId, versionId, caption, autoPublish);
      if (outcome === "needsLogin") {
        setStatus("Log in to Instagram in the Chrome window that opened, then Post again.");
      } else if (outcome === "awaitingReview") {
        // CDP stopped before Share. Capture the exact photo/version so the confirm
        // below records the right publication even if the user changes the picker.
        setPendingReview({ photoId, versionId });
        setStatus("Composed in Chrome — click Share in the browser, then confirm below.");
      } else {
        await api.recordPublication(photoId, versionId);
        api.notifyChange();
        api.showToast("Posted to Instagram.");
        setStatus("Posted to Instagram ✓");
      }
    } catch (e) {
      setStatus(String(e));
    } finally {
      setBusy(false);
    }
  };

  /** User confirms they manually clicked Share after the supervised run. */
  const confirmPosted = async () => {
    if (pendingReview == null) return;
    const { photoId: pid, versionId: vid } = pendingReview;
    setPendingReview(null);
    try {
      await api.recordPublication(pid, vid);
      api.notifyChange();
      api.showToast("Posted to Instagram.");
      setStatus("Posted to Instagram ✓");
    } catch (e) {
      setStatus(String(e));
    }
  };

  /** User says they did not complete the post — discard the pending confirmation. */
  const dismissReview = () => {
    setPendingReview(null);
    setStatus("Post not recorded.");
  };

  if (photoId == null) return <div className="panel-empty">Select a photo</div>;

  return (
    <div className="iptc-panel">
      <div className="field">
        <label>Version</label>
        <select
          className="folder-input"
          value={versionId ?? ""}
          onChange={(e) => setVersionId(e.target.value ? Number(e.target.value) : null)}
        >
          <option value="">Original</option>
          {versions.map((v) => (
            <option key={v.id} value={v.id}>
              {v.name}
            </option>
          ))}
        </select>
      </div>
      <div className="field">
        <label>Caption</label>
        <textarea
          className="folder-input"
          rows={4}
          style={{ resize: "vertical", fontFamily: "inherit" }}
          placeholder="Caption + #hashtags…"
          value={caption}
          onChange={(e) => {
            setCaption(e.target.value);
            setCaptionTouched(true);
          }}
        />
        <label className="term-export" style={{ marginTop: 6 }}>
          <input
            type="checkbox"
            checked={autoPublish}
            onChange={(e) => setAutoPublish(e.target.checked)}
          />
          Publish automatically (otherwise stop before Share so you can review)
        </label>
        <span className="term-note">
          Posts the selected image (1080px) by driving Chrome — a window opens; log in to
          Instagram there once. Browser automation can break when Instagram changes its site.
        </span>
      </div>
      <div className="iptc-actions">
        <button className="chip chip-on" onClick={post} disabled={busy || pendingReview != null}>
          {busy ? "Posting…" : "Post to Instagram"}
        </button>
        <span className="iptc-status">{status}</span>
      </div>
      {pendingReview != null && (
        <div className="field" style={{ marginTop: 8, padding: "8px 0", borderTop: "1px solid var(--border, #333)" }}>
          <span className="term-note" style={{ display: "block", marginBottom: 6 }}>
            Did you click Share in the Instagram tab?
          </span>
          <div className="row">
            <button className="chip chip-on" onClick={confirmPosted}>
              Yes, I posted it
            </button>
            <button className="chip" onClick={dismissReview}>
              No, skip
            </button>
          </div>
        </div>
      )}
    </div>
  );
}

export const instagramModule: ChairPhotoModule = {
  id: "instagram",
  name: "Instagram",
  version: "0.1.0",
  description:
    "Post a photo to Instagram by driving Chrome; records which version was posted. Supervised by default.",
  backendFeature: "instagram",
  publicationMarker: "instagram",
  onLoad(api) {
    api.registerPublishTarget({
      id: "instagram",
      label: "Instagram",
      render: () => <InstagramForm api={api} />,
    });
  },
};
