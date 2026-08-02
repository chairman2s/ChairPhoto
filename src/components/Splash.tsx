import { useEffect, useRef, useState } from "react";

// Startup splash: covers the window from first paint until the catalog, modules and the
// first photo list are in, reporting the real boot stage ("Opening catalog…" etc.) with a
// stepped progress bar. When `stage` goes null the overlay fades out and unmounts itself.

export const BOOT_STAGES = [
  "Opening catalog…",
  "Updating auto-tags…",
  "Starting modules…",
  "Loading photos…",
] as const;

export function Splash({ stage }: { stage: string | null }) {
  const [gone, setGone] = useState(false);
  // Keep the last real stage visible while fading out (stage is null by then).
  const lastStage = useRef<string>(BOOT_STAGES[0]);
  if (stage != null) lastStage.current = stage;

  const hiding = stage == null;
  useEffect(() => {
    if (!hiding) return;
    const t = setTimeout(() => setGone(true), 450); // matches the CSS transition
    return () => clearTimeout(t);
  }, [hiding]);
  if (gone) return null;

  const idx = BOOT_STAGES.indexOf(lastStage.current as (typeof BOOT_STAGES)[number]);
  const progress = hiding ? 100 : ((idx < 0 ? 0 : idx + 1) / (BOOT_STAGES.length + 1)) * 100;

  return (
    <div className={`splash ${hiding ? "splash-hide" : ""}`}>
      <div className="splash-logo" />
      <div className="splash-name">ChairPhoto</div>
      <div className="splash-bar">
        <div className="splash-bar-fill" style={{ width: `${progress}%` }} />
      </div>
      <div className="splash-stage">{hiding ? "Ready" : lastStage.current}</div>
    </div>
  );
}
