// The Darkroom's Lens section (docs/plans/lens-corrections): one switch for the camera's
// own lens-correction tables. Shown only on the RAW, and only when the file carries a
// table this build applies.
import type { LensInfo } from "../../modules/api";

/** What the switch corrects, in words: the tables this file carries. */
export function lensHint(info: LensInfo): string {
  const fixes = [
    info.vignetting && "brightens the corners the lens darkened",
    info.distortion && "straightens lines the lens bent",
    info.chromatic && "removes colour fringing at the edges",
  ].filter(Boolean) as string[];
  const list = fixes.length > 1 ? `${fixes.slice(0, -1).join(", ")} and ${fixes[fixes.length - 1]}` : fixes[0] ?? "";
  return `${info.source} tables: ${list}.`;
}

export function LensRail({
  info,
  on,
  onToggle,
}: {
  info: LensInfo;
  on: boolean;
  onToggle: (on: boolean) => void;
}) {
  return (
    <div className="develop-section">
      <div className="panel-head develop-group-label">Lens</div>
      <div className="editor-aspects">
        <button
          className={`chip ${on ? "chip-on" : ""}`}
          onClick={() => onToggle(!on)}
          title="Apply the correction the camera recorded for this lens, focal length and aperture"
        >
          {on ? "Correction on" : "Correction off"}
        </button>
      </div>
      <div className="editor-hint">{lensHint(info)}</div>
    </div>
  );
}
