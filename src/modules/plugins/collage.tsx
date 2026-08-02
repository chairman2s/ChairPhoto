import type { ChairPhotoModule } from "../registry";
import { CollageDialog } from "./CollageDialog";

// The Collage module (H12): composite the selected photos into one justified-mosaic image.
// Its compositing backend is the `collage` Cargo feature — if compiled out, the host won't
// let the module enable (backendFeature gate), so it stays inert in a build without it. It
// registers a single toolbar action whose `render(close)` opens the settings dialog. See
// docs/collage.md.
export const collageModule: ChairPhotoModule = {
  id: "collage",
  name: "Collage",
  version: "0.1.0",
  description:
    "Composite the selected photos into one justified-mosaic image (JPEG/PNG) — pick the aspect, spacing, background, fit, border and rounded corners, reorder the tiles, and render.",
  backendFeature: "collage",
  onLoad(api) {
    api.registerAction({
      id: "collage",
      label: "Make collage",
      icon: "▦",
      render: (close) => <CollageDialog api={api} onClose={close} />,
    });
  },
};
