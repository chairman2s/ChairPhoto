// Chooses the Develop surface: the classic EditorView, or the in-progress Darkroom when
// the `editor.darkroom` early-preview setting is on (Preferences → Editors). Slice 8 of
// docs/plans/darkroom/04-slices.md removes this chooser and the Darkroom becomes the
// only surface. The setting is read when Develop opens, so a toggle applies on the
// next open rather than mid-edit.
import { useEffect, useRef, useState } from "react";
import { EditorView } from "../EditorView";
import { developClose, getSetting, type PhotoVersion } from "../../modules/api";
import { DarkroomView } from "./DarkroomView";

export function DevelopSurface(props: {
  photoId: number;
  photoW: number | null;
  photoH: number | null;
  activeVersionId: number | null;
  /** The active version's record, for the Darkroom's starting state. */
  activeEditJson: string | null;
  onPickVersion: (v: PhotoVersion | null) => void;
  onSavedActive: (editJson: string) => void;
  onChanged: () => void;
  onBack: () => void;
  /** The photos either side in the Library's current order, next first — the Darkroom
   *  preloads their RAW working images (docs/plans/raw-foundation, slice 4). */
  neighbours: number[];
  /** The filmstrip: the Library's order, names for tooltips, and how to move to a photo. */
  strip: {
    ids: number[];
    names: Map<number, string>;
    covers: Map<number, string | null>;
    onSelect: (id: number) => void;
  };
  /** The version this photo's Library thumbnail shows, if one is its cover. */
  coverVersionId: number | null;
}) {
  const [darkroom, setDarkroom] = useState<boolean | null>(null);
  useEffect(() => {
    let alive = true;
    getSetting("editor.darkroom")
      .then((v) => alive && setDarkroom(v === "1"))
      .catch(() => alive && setDarkroom(false));
    return () => {
      alive = false;
    };
  }, []);
  // The Develop session lives as long as Develop does, not as long as one photo's view:
  // stepping to the next photo remounts the Darkroom (keyed per photo) but keeps the
  // session, so a preloaded neighbour is adopted instead of released. Leaving Develop
  // releases the working images.
  // Leaving also refreshes the Library: edits made here may have changed a cover's look,
  // and the grid's thumbnail URLs carry the cover token from the photo rows.
  const onChangedRef = useRef(props.onChanged);
  onChangedRef.current = props.onChanged;
  useEffect(
    () => () => {
      developClose().catch(() => {});
      onChangedRef.current();
    },
    [],
  );
  if (darkroom === null) return null; // one frame while the setting loads
  if (darkroom) {
    return (
      <DarkroomView
        // One view per photo: nothing — working state, history, pending autosave — can
        // cross from one photo to the next.
        key={props.photoId}
        photoId={props.photoId}
        photoW={props.photoW}
        photoH={props.photoH}
        activeVersionId={props.activeVersionId}
        initialEditJson={props.activeEditJson}
        onPickVersion={props.onPickVersion}
        onChanged={props.onChanged}
        onBack={props.onBack}
        onSavedActive={props.onSavedActive}
        neighbours={props.neighbours}
        strip={props.strip}
        coverVersionId={props.coverVersionId}
      />
    );
  }
  const {
    activeEditJson: _activeEditJson,
    neighbours: _neighbours,
    strip: _strip,
    coverVersionId: _coverVersionId,
    ...editorProps
  } = props;
  return <EditorView {...editorProps} />;
}
