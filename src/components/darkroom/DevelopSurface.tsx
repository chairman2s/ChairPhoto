// Chooses the Develop surface: the classic EditorView, or the in-progress Darkroom when
// the `editor.darkroom` early-preview setting is on (Preferences → Editors). Slice 8 of
// docs/plans/darkroom/04-slices.md removes this chooser and the Darkroom becomes the
// only surface. The setting is read when Develop opens, so a toggle applies on the
// next open rather than mid-edit.
import { useEffect, useState } from "react";
import { EditorView } from "../EditorView";
import { getSetting, type PhotoVersion } from "../../modules/api";
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
  if (darkroom === null) return null; // one frame while the setting loads
  if (darkroom) {
    return (
      <DarkroomView
        photoId={props.photoId}
        photoW={props.photoW}
        photoH={props.photoH}
        activeVersionId={props.activeVersionId}
        initialEditJson={props.activeEditJson}
        onPickVersion={props.onPickVersion}
        onChanged={props.onChanged}
        onBack={props.onBack}
        neighbours={props.neighbours}
      />
    );
  }
  const { activeEditJson: _activeEditJson, neighbours: _neighbours, ...editorProps } = props;
  return <EditorView {...editorProps} />;
}
