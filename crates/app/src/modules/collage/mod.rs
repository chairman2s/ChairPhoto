//! The Collage module (`src/modules/plugins/collage.tsx`, ticket #125): one action, "Make
//! collage", that opens [`view::CollageDialog`] — a freeform canvas seeded by the justified
//! mosaic, seven templates, drag/resize/pan/zoom/swap, aspect, width, background, border,
//! corner radius, JPEG/PNG — and saves the composite into the library or a folder. Compiled
//! with the `collage` feature, like its backend.
//!
//! The canvas logic is `chairphoto_model::collage`; the backend is core `app::collage`, every
//! call run on a worker ([`crate::storage::Runner`]) and bound to the catalog the selection
//! was read from. [`CollageBackend`] is the seam tests replace the preview loader at.

pub mod view;

#[cfg(test)]
mod tests;

use super::dialog::DialogHost;
use super::{ActionKind, Contributions, Module, ModuleAction, ModuleHost, ModuleInstance, ModuleMeta};
use chairphoto_core::app::collage::{cached_preview, PreviewLoader};
use gpui_kit::{App, AppContext as _, Window};
use std::rc::Rc;

pub const COLLAGE_ID: &str = "collage";
pub const COLLAGE_ACTION: &str = "collage";

/// Where the composite's pixels come from: each photo's upright preview.
#[derive(Clone)]
pub struct CollageBackend {
    pub previews: PreviewLoader,
}

impl Default for CollageBackend {
    fn default() -> Self {
        CollageBackend { previews: cached_preview() }
    }
}

#[derive(Default)]
pub struct CollageModule {
    pub backend: CollageBackend,
}

struct CollageInstance {
    host: ModuleHost,
    backend: CollageBackend,
}

impl Module for CollageModule {
    fn meta(&self) -> ModuleMeta {
        ModuleMeta::new(COLLAGE_ID, "Collage")
            .description(
                "Composite the selected photos into one justified-mosaic image (JPEG/PNG) — pick the aspect, spacing, \
                 background, fit, border and rounded corners, reorder the tiles, and render.",
            )
            .backend_feature("collage")
    }

    fn load(&self, host: ModuleHost, _: &mut App) -> Result<Box<dyn ModuleInstance>, String> {
        Ok(Box::new(CollageInstance { host, backend: self.backend.clone() }))
    }
}

impl ModuleInstance for CollageInstance {
    fn contributions(&self) -> Contributions {
        let (host, backend) = (self.host.clone(), self.backend.clone());
        Contributions {
            actions: vec![ModuleAction {
                id: COLLAGE_ACTION.into(),
                label: "Make collage".into(),
                kind: ActionKind::Run(Rc::new(move |window: &mut Window, cx: &mut App| {
                    let dialog = DialogHost::new(host.model(), host.shell(), host.images().cloned(), cx);
                    open(dialog, backend.clone(), window, cx);
                })),
            }],
            ..Default::default()
        }
    }
}

/// Open the dialog over the current selection. No backdrop close: a drag that ends outside
/// the canvas must not discard the layout.
pub fn open(host: DialogHost, backend: CollageBackend, window: &mut Window, cx: &mut App) -> gpui_kit::Entity<view::CollageDialog> {
    let view = cx.new(|cx| view::CollageDialog::new(host, backend, window, cx));
    super::dialog::open("Make collage", 600., false, view.clone(), window, cx);
    view
}
