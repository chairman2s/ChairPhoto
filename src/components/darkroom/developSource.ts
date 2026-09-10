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
