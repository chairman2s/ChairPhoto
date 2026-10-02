//! The Smart Tagging module (`src/modules/plugins/smartTagging.tsx`, #126): the inspector's
//! "Similar tags" panel and the settings panel. Compiled with the `smarttags` feature, like its
//! backend (`chairphoto_core::app::smarttags`, `plugins::smarttags`).
//!
//! - [`state::SmarttagsState`] — the model, the index job it follows, the active photo's
//!   suggestions and every write, off the UI thread and bound to their catalog.
//! - [`views`] — the two views; [`logic`] — their lines (unit-tested).
//!
//! Fully local: the only network use is "Download model", on the user's click. A missing model
//! (or ONNX Runtime) degrades only this module: the panel offers the download or names the
//! broken path, and Index refuses until the model is there.

pub mod logic;
pub mod state;
pub mod views;

use super::{view as view_factory, Contributions, Module, ModuleHost, ModuleInstance, ModuleMeta, Panel, PanelSlot, SettingsPanel};
use gpui_kit::{App, AppContext as _, Entity};

pub const SMARTTAGS_MODULE_ID: &str = "smarttags";

pub struct SmartTaggingModule;

pub struct SmarttagsInstance {
    state: Entity<state::SmarttagsState>,
}

impl Module for SmartTaggingModule {
    fn meta(&self) -> ModuleMeta {
        ModuleMeta::new(SMARTTAGS_MODULE_ID, "Smart Tagging")
            .description("Suggest tags based on visual similarity using local CLIP embeddings.")
            .backend_feature("smarttags")
    }

    fn load(&self, host: ModuleHost, cx: &mut App) -> Result<Box<dyn ModuleInstance>, String> {
        let app = host.model().read(cx).state().clone();
        let (settings, model, shell) = (host.settings(), host.model().clone(), host.shell().clone());
        let state = cx.new(|cx| state::SmarttagsState::new(app, settings, model, shell, cx));
        Ok(Box::new(SmarttagsInstance { state }))
    }
}

impl SmarttagsInstance {
    pub fn state(&self) -> &Entity<state::SmarttagsState> {
        &self.state
    }
}

impl ModuleInstance for SmarttagsInstance {
    fn contributions(&self) -> Contributions {
        let state = self.state.clone();
        let settings = view_factory(move |window, cx| views::SmarttagsSettings::new(state.clone(), window, cx));
        let state = self.state.clone();
        let panel = view_factory(move |_, cx| views::SimilarTagsPanel::new(state.clone(), cx));
        Contributions {
            settings: vec![SettingsPanel { id: "smarttags-settings".into(), view: settings }],
            panels: vec![Panel {
                id: "smarttags-similar".into(),
                label: "Similar tags".into(),
                slot: PanelSlot::Inspector,
                view: panel,
            }],
            ..Default::default()
        }
    }

    /// Disabled: stop following (the core job, if any, runs on detached, as in React).
    fn on_unload(&mut self, cx: &mut App) {
        self.state.update(cx, |s, cx| s.unload(cx));
    }
}
