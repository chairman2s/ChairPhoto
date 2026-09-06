// The record the Darkroom stage renders (pure; DarkroomView's render loop and its tests).
import type { VersionEdit } from "../../modules/editing";

/** The stage shows the working record WITHOUT its crop — the crop is an interactive
 *  overlay, exactly like EditorView — and un-warped while the perspective handles are up,
 *  because the handles aim at the original's corners. Masses and the loupe print use the
 *  full record; they describe the finished print. */
export function stageJsonFor(working: VersionEdit, perspectiveMode: boolean): string {
  return JSON.stringify({
    ...working,
    crop: undefined,
    perspective: perspectiveMode ? undefined : working.perspective,
  });
}
