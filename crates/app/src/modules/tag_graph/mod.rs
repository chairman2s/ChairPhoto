//! The Tag graph module (`tag-graph`, `src/modules/plugins/tagGraph.tsx`): the tag vocabulary
//! as a radial edge-bundled graph, a main view on the icon rail. See docs/tag-graph.md and
//! docs/plans/gpui/tag-graph.md.
//!
//! - The logic is `chairphoto_model::tag_graph` (layout, scene, labels, session), sans-IO.
//! - [`view::TagGraphView`] runs it: loads `library_graph`, builds scenes and rasters off the
//!   UI thread, takes the input, renders the panels.
//! - [`raster`] strokes the base edges with tiny-skia; [`paint`] draws the canvas.
//!
//! **Communities only**: the Photo ↔ tag mode was dropped by the owner (#120). Labels are
//! horizontal and the edge layer is soft while zooming (also #120).
//!
//! **The pop-out loupe** (#110): while the Graph is on screen its inspector is mirrored there
//! (`GraphSession::loupe_card` through `ModuleHost::show_in_loupe`), and "Open loupe window"
//! opens it; leaving the Graph takes the card down.
//!
//! Compiled in behind the `tag-graph` cargo feature (default). The module needs no core
//! backend (`library_graph` is in every build), so it declares no `backend_feature`.

pub mod paint;
pub mod raster;
pub mod view;

#[cfg(test)]
mod tests;

use super::{view as view_factory, Contributions, MainView, Module, ModuleHost, ModuleInstance, ModuleMeta};
use chairphoto_core::app::{with_catalog, AppState};
use chairphoto_model::tag_graph::graph::LibraryGraph;
use gpui_kit::component::{Icon, IconName};
use gpui_kit::App;
use std::sync::Arc;

pub const TAG_GRAPH_ID: &str = "tag-graph";
/// The main view's id (`registerMainView({id: "tag-graph"})`).
pub const VIEW_ID: &str = "tag-graph";

gpui_kit::actions!(
    tag_graph,
    [
        /// Escape: deselect; with nothing selected, climb one branch level.
        Back,
    ]
);

/// Where the view's graph comes from. Blocking: it runs on a background thread.
pub type GraphSource = Arc<dyn Fn() -> Result<LibraryGraph, String> + Send + Sync>;

/// `library_graph` against the open catalog.
pub fn catalog_source(app: AppState) -> GraphSource {
    Arc::new(move || {
        with_catalog(&app, |c| c.library_graph())
            .map(|(tags, cameras, cooc, hierarchy, camera)| LibraryGraph::from_catalog(tags, cameras, cooc, hierarchy, camera))
    })
}

pub struct TagGraphModule;

struct TagGraphInstance {
    host: ModuleHost,
}

impl Module for TagGraphModule {
    fn meta(&self) -> ModuleMeta {
        ModuleMeta::new(TAG_GRAPH_ID, "Tag Graph").description(
            "Visualize your library as a graph — tag communities and camera nodes. Select a node to inspect it.",
        )
    }

    fn load(&self, host: ModuleHost, _: &mut App) -> Result<Box<dyn ModuleInstance>, String> {
        Ok(Box::new(TagGraphInstance { host }))
    }
}

impl ModuleInstance for TagGraphInstance {
    fn contributions(&self) -> Contributions {
        let host = self.host.clone();
        Contributions {
            main_views: vec![MainView {
                id: VIEW_ID.into(),
                label: "Graph".into(),
                icon: Some(Icon::new(IconName::Network)),
                view: view_factory(move |window, cx| {
                    let source = catalog_source(host.model().read(cx).state().clone());
                    view::TagGraphView::new(
                        host.model(),
                        host.shell().clone(),
                        host.images().cloned(),
                        source,
                        window,
                        cx,
                    )
                    .with_host(host.clone(), cx)
                }),
            }],
            ..Default::default()
        }
    }
}
