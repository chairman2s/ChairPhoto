// The Darkroom's source badge (docs/plans/raw-foundation, mockup 01-preparing): what the
// stage is rendering from, in words. Pure — DarkroomView feeds it the probe result.
import type { DevelopSource } from "../../modules/api";

export interface SourceBadge {
  /** Short text for the bar. */
  label: string;
  /** Hover text with the detail (decoder version, the reason a RAW is unsupported). */
  title: string;
  /** Visual weight: `raw` is the developed state, `warn` an honest exception, `plain` a JPEG. */
  tone: "raw" | "warn" | "plain";
}

export function badgeFor(s: DevelopSource): SourceBadge {
  switch (s.source) {
    case "preview":
      return s.preparing
        ? { label: "camera preview · preparing full quality", title: "The RAW is being decoded; the stage swaps to it when ready.", tone: "warn" }
        : { label: "camera preview", title: "The RAW engine is off (Preferences → Darkroom).", tone: "plain" };
    case "raw":
      return {
        label: `RAW · ${s.bits}-bit · ${formatMegapixels(s.megapixels)} MP`,
        title: `${s.camera} — decoded by LibRaw ${s.decoder}`,
        tone: "raw",
      };
    case "unsupported":
      return {
        label: `camera preview · RAW not supported yet${s.camera ? ` · ${s.camera}` : ""}`,
        title: `The bundled decoder cannot open this file yet (${s.reason}). Develop works on the camera's preview.`,
        tone: "warn",
      };
    case "jpeg":
      return { label: "JPEG · 8-bit", title: "Not a RAW: the file's own pixels are its full quality.", tone: "plain" };
    case "nodecoder":
      return {
        label: "camera preview · no RAW decoder in this build",
        title: "This build was compiled without the `raw` feature.",
        tone: "warn",
      };
  }
}

/** One decimal, trailing zero dropped: 66.45 → "66.5", 33.0 → "33", 9.62 → "9.6". The
 *  picture's real pixel count, not a brochure figure — a Sony A7R VI delivers 66.5 MP. */
export function formatMegapixels(mp: number): string {
  return (Math.round(mp * 10) / 10).toString();
}

/** What the stage renders from, reduced from the source events for one photo. */
export interface SourceState {
  /** The token to put in render URLs — undefined = the camera preview. */
  token: string | undefined;
  /** The engine the working record should be written for: 2 once the RAW is resident. */
  engine: 1 | 2;
  /** The latest source, for the badge. */
  source: DevelopSource | null;
}

export const INITIAL_SOURCE: SourceState = { token: undefined, engine: 1, source: null };

/** Fold a source event in. Events for another photo are ignored; a resident RAW yields its
 *  token and engine 2; anything else drops back to the preview path. Pure. */
export function reduceSource(prev: SourceState, e: DevelopSource & { photoId?: number }, photoId: number): SourceState {
  if (e.photoId !== undefined && e.photoId !== photoId) return prev;
  if (e.source === "raw" && e.token) return { token: e.token, engine: 2, source: e };
  return { token: undefined, engine: 1, source: e };
}
