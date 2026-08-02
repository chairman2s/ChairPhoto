import type { ChairPhotoModule } from "../registry";
import { SlideshowDialog } from "./SlideshowDialog";

// The Slideshow module (H11): render the selected photos into a slideshow movie (.mp4) via
// ffmpeg. Its render backend is the `slideshow` Cargo feature — if compiled out, the host won't
// let the module enable (backendFeature gate), so it stays inert in a build without it. It
// registers a single toolbar action whose `render(close)` opens the settings dialog. See
// docs/slideshow.md.
export const slideshowModule: ChairPhotoModule = {
  id: "slideshow",
  name: "Slideshow",
  version: "0.1.0",
  description:
    "Render the selected photos into a slideshow movie (.mp4) — pick per-photo duration, resolution/aspect preset, crossfade transitions and Ken Burns pan/zoom, reorder the photos, and render via ffmpeg.",
  backendFeature: "slideshow",
  onLoad(api) {
    api.registerAction({
      id: "slideshow",
      label: "Make slideshow",
      icon: "▶",
      render: (close) => <SlideshowDialog api={api} onClose={close} />,
    });
  },
};
