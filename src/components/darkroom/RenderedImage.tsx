// A native-protocol render (an `edit://` URL) with a placeholder until it has loaded and a
// quiet mark if it fails — the proof sheet's cells and the duel's panes. The URL is the
// whole request: the element only ever shows its latest src.
import { useState } from "react";

export function RenderedImage({
  src,
  loadingClass,
  loadingText,
}: {
  src: string;
  loadingClass: string;
  loadingText: string;
}) {
  const [state, setState] = useState<{ src: string; status: "loading" | "ok" | "error" }>({ src, status: "loading" });
  // A new URL starts loading again (derived state, reset during render).
  const status = state.src === src ? state.status : "loading";
  if (state.src !== src) setState({ src, status: "loading" });
  return (
    <>
      <img
        src={src}
        alt=""
        style={status === "ok" ? undefined : { display: "none" }}
        onLoad={() => setState({ src, status: "ok" })}
        onError={() => setState({ src, status: "error" })}
      />
      {status !== "ok" && <span className={loadingClass}>{status === "error" ? "—" : loadingText}</span>}
    </>
  );
}
