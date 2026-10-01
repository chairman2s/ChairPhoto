//! The Map module (`src/modules/plugins/map.tsx`, ticket #119): photos on a slippy map by
//! GPS, clustered; a filmstrip of a marker's photos; geofences drawn on the map that tag the
//! photos inside them; the tile source and per-host tile consent; reverse geocoding into
//! IPTC location fields. Compiled with the `map` feature, like its backend
//! (`chairphoto_core::plugins::map`).
//!
//! - [`state::MapState`] — the catalog-derived data and every catalog write, off the UI thread.
//! - [`view::MapView`] — the main view: one canvas plus overlays.
//! - [`tiles`] — tile textures: the fetch/decode seam and the view's LRU (`drop_image` on
//!   eviction).
//! - [`settings`] — the settings panel and the inspector's Geocode panel.
//! - [`logic`] — consent, drawing, hit tests, zoom steps (unit-tested).
//!
//! The design is `docs/plans/gpui/map.md`; the privacy decision (#118) is: ask before the
//! first tile request to each host, remember the answer, plain background until allowed.

pub mod logic;
pub mod settings;
pub mod state;
pub mod tiles;
pub mod view;

use super::{
    view as view_factory, Contributions, MainView, Module, ModuleHost, ModuleInstance, ModuleMeta, Panel, PanelSlot,
    SettingsPanel,
};
use gpui_kit::{App, AppContext as _, Entity};

pub const MAP_MODULE_ID: &str = "map";
/// The main view's id (React's `registerMainView({ id: "map" })`).
pub const MAP_VIEW_ID: &str = "map";

pub struct MapModule;

struct MapInstance {
    state: Entity<state::MapState>,
    host: ModuleHost,
}

impl Module for MapModule {
    fn meta(&self) -> ModuleMeta {
        ModuleMeta::new(MAP_MODULE_ID, "Map")
            .description(
                "Plot your photos on an OpenStreetMap map by GPS location. Click a marker or cluster to browse \
                 photos in a filmstrip; clicking a thumb selects it without leaving the map. Draw geofences to \
                 auto-tag photos by location. Map tiles load only after you allow their server.",
            )
            .backend_feature("map")
    }

    fn load(&self, host: ModuleHost, cx: &mut App) -> Result<Box<dyn ModuleInstance>, String> {
        let app = host.model().read(cx).state().clone();
        let (settings, model) = (host.settings().clone(), host.model().clone());
        let state = cx.new(|cx| state::MapState::new(app, settings, model, cx));
        Ok(Box::new(MapInstance { state, host }))
    }
}

impl ModuleInstance for MapInstance {
    fn contributions(&self) -> Contributions {
        let (state, shell, images) = (self.state.clone(), self.host.shell().clone(), self.host.images().cloned());
        let map = view_factory(move |window, cx| {
            view::MapView::new(state.clone(), shell.clone(), images.clone(), window, cx)
        });
        let state = self.state.clone();
        let settings = view_factory(move |window, cx| settings::MapSettings::new(state.clone(), window, cx));
        let (state, shell) = (self.state.clone(), self.host.shell().clone());
        let geocode = view_factory(move |_, cx| settings::GeocodePanel::new(state.clone(), shell.clone(), cx));
        Contributions {
            main_views: vec![MainView { id: MAP_VIEW_ID.into(), label: "Map".into(), icon: None, view: map }],
            settings: vec![SettingsPanel { id: "map-settings".into(), view: settings }],
            panels: vec![Panel { id: "map-geocode".into(), label: "Geocode".into(), slot: PanelSlot::Inspector, view: geocode }],
            ..Default::default()
        }
    }
}
