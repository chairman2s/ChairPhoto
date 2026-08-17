import type { Photo } from "./registry";

/**
 * Which photo the surfaces *outside* the main content area follow — the inspector and any
 * pop-out loupe window — and the edit record that may ride with it.
 *
 * This exists because Compare introduced a second notion of "the current photo". Its
 * focused pane is deliberately not the selection: culling one frame must not disturb the
 * set being compared, and leaving Compare should hand the multi-selection back intact. But
 * the inspector and the pop-out both key off the selection, so without a single place that
 * resolves the two, moving Compare's focus would change something a second screen never
 * reflects.
 *
 * Extracted from `App` because of `editJson`, which is the part that is easy to get wrong
 * and invisible when it is: the active *version* belongs to the selected photo, so pairing
 * it with any other frame renders one photo's edit on top of another.
 */
export function shellTarget(args: {
  /** The selected photo — what the shell follows when Compare is closed. */
  selected: Photo | null;
  /** The selected photo's id (may be set while its row is momentarily absent). */
  activeId: number | null;
  /** Compare's focused pane, or `null` when Compare is closed. */
  compareFocus: Photo | null;
  /** The active version's edit record, already guarded to the selected photo. */
  activeEditJson: string | null;
}): {
  /** The photo to show. */
  photo: Photo | null;
  /** The id to broadcast to a pop-out loupe (`null` = nothing to show). */
  broadcastId: number | null;
  /** The edit to render with it — only ever the active photo's own. */
  editJson: string | null;
} {
  const photo = args.compareFocus ?? args.selected;
  const broadcastId = photo?.id ?? args.activeId;
  return {
    photo,
    broadcastId,
    editJson: broadcastId === args.activeId ? args.activeEditJson : null,
  };
}
