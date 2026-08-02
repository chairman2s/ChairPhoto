import { useEffect, useState } from "react";
import { previewUrl } from "../modules/previewCache";

// Shows a large loupe preview, served by the native `preview://` protocol. The
// webview caches by URL, so previously-viewed (or prefetched) photos appear
// instantly. Used by both the inline overlay and the popped-out loupe window.
export function PreviewImage({ photoId }: { photoId: number | null }) {
  const [failed, setFailed] = useState(false);

  // Reset the error state when the photo changes.
  useEffect(() => {
    setFailed(false);
  }, [photoId]);

  if (photoId == null) return <div className="loupe-empty">No photo selected</div>;
  if (failed) return <div className="loupe-empty">No preview available</div>;
  return (
    <img
      className="loupe-img"
      src={previewUrl(photoId)}
      alt=""
      onError={() => setFailed(true)}
    />
  );
}
