import type { ChairPhotoModule } from "../registry";
import { renderEdit } from "../api";

// The Basic Editor module (H5b): non-destructive crop + exposure/tone over photo
// versions. Its backend render engine is the `edit` Cargo feature; if compiled out, the
// host won't let the module enable. It registers the loupe **edit renderer** so a
// selected version shows its edited result; the editor UI itself (the "Develop" main-window
// view) opens from the view switcher or the Versions panel's Edit button. See docs/editing.md.
export const basicEditorModule: ChairPhotoModule = {
  id: "basic-editor",
  name: "Basic Editor",
  version: "0.1.0",
  description:
    "Non-destructive crop (with social aspect presets) and exposure/tone, saved as photo versions. Originals are never modified.",
  backendFeature: "edit",
  // No `permissions` on purpose, not by omission (#48): this module never calls
  // `api.invoke`. It renders through `renderEdit`, a core wrapper in `modules/api.ts`,
  // which is part of the host API rather than the module-owned command surface.
  onLoad(api) {
    api.registerEditRenderer({
      id: "basic-editor",
      // Render a version's edit record to a full-size data URL for the loupe.
      render: (photoId, record) => renderEdit(photoId, JSON.stringify(record), 0),
      // Zoom-in render: same record over the native-size preview tier, so the loupe
      // has real pixels to magnify (a cropped render of the fast 2048 proxy can be
      // smaller than the window).
      renderHi: (photoId, record) => renderEdit(photoId, JSON.stringify(record), 0, true),
    });
  },
};
