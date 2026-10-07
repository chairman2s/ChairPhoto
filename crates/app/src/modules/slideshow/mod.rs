//! The Slideshow module (`src/modules/plugins/slideshow.tsx`, ticket #125): one action, "Make
//! slideshow", that opens [`view::SlideshowDialog`] — the selection as a reorderable strip, the
//! duration, orientation × resolution, crossfade, Ken Burns, frame rate and output folder —
//! and renders an `.mp4` with ffmpeg. Compiled with the `slideshow` feature, like its backend.
//!
//! The render is core's slideshow job (`chairphoto_core::app::slideshow`): claimed and run on
//! a worker ([`crate::storage::Runner`]), bound to the catalog the selection was read from,
//! cancellable (Cancel, a newer render or a catalog switch kill ffmpeg). `slideshow:progress`
//! only moves the bar, and only for this dialog's job; the job's return value is the result.
//!
//! ffmpeg is a runtime tool: when it is missing the render answers so in the dialog and
//! nothing else in the app is affected ([`SlideshowBackend`] is the seam tests fake it at).

pub mod view;

#[cfg(test)]
mod tests;

use super::dialog::DialogHost;
use super::{ActionKind, Contributions, Module, ModuleAction, ModuleHost, ModuleInstance, ModuleMeta};
use chairphoto_core::app::slideshow::{export_frame, FrameWriter};
use gpui_kit::{App, AppContext as _, Window};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

pub const SLIDESHOW_ID: &str = "slideshow";
pub const SLIDESHOW_ACTION: &str = "slideshow";

/// Where the render's external pieces come from: the ffmpeg binary (looked up on the worker,
/// never the UI thread) and the frame writer.
#[derive(Clone)]
pub struct SlideshowBackend {
    /// The ffmpeg to run; `None` = not installed.
    pub ffmpeg: Arc<dyn Fn() -> Option<PathBuf> + Send + Sync>,
    pub frames: FrameWriter,
}

impl Default for SlideshowBackend {
    fn default() -> Self {
        SlideshowBackend {
            ffmpeg: Arc::new(|| chairphoto_core::slideshow::ffmpeg_path().map(PathBuf::from)),
            frames: export_frame(),
        }
    }
}

#[derive(Default)]
pub struct SlideshowModule {
    pub backend: SlideshowBackend,
}

struct SlideshowInstance {
    host: ModuleHost,
    backend: SlideshowBackend,
}

impl Module for SlideshowModule {
    fn meta(&self) -> ModuleMeta {
        ModuleMeta::new(SLIDESHOW_ID, "Slideshow")
            .description(
                "Render the selected photos into a slideshow movie (.mp4) — pick per-photo duration, resolution/aspect \
                 preset, crossfade transitions and Ken Burns pan/zoom, reorder the photos, and render via ffmpeg.",
            )
            .backend_feature("slideshow")
    }

    fn load(&self, host: ModuleHost, _: &mut App) -> Result<Box<dyn ModuleInstance>, String> {
        Ok(Box::new(SlideshowInstance { host, backend: self.backend.clone() }))
    }
}

impl ModuleInstance for SlideshowInstance {
    fn contributions(&self) -> Contributions {
        let (host, backend) = (self.host.clone(), self.backend.clone());
        Contributions {
            actions: vec![ModuleAction {
                id: SLIDESHOW_ACTION.into(),
                label: "Make slideshow".into(),
                kind: ActionKind::Run(Rc::new(move |window: &mut Window, cx: &mut App| {
                    let dialog = DialogHost::new(host.model(), host.shell(), host.images().cloned(), cx);
                    open(dialog, backend.clone(), window, cx);
                })),
            }],
            ..Default::default()
        }
    }
}

/// Open the dialog over the current selection.
pub fn open(host: DialogHost, backend: SlideshowBackend, window: &mut Window, cx: &mut App) -> gpui_kit::Entity<view::SlideshowDialog> {
    let view = cx.new(|cx| view::SlideshowDialog::new(host, backend, window, cx));
    super::dialog::open("Make slideshow", 620., true, view.clone(), window, cx);
    view
}
