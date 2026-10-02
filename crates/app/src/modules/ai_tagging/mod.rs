//! The AI Tagging module (`src/modules/plugins/aiTagging.tsx`, #126): the inspector's "AI tags"
//! panel and the settings panel. Compiled with the `ai` feature, like its backend
//! (`chairphoto_core::app::ai`, `plugins::ai`).
//!
//! - [`state::AiState`] — settings, the active photo's suggestions, the runs and the bulk
//!   cloud confirm, off the UI thread and bound to the catalog they were read from.
//! - [`panel::AiPanel`], [`settings::AiSettings`] — the two views.
//! - [`logic`] — providers, curated models, the cost table, grouping, the region box and the
//!   cloud opt-in check (unit-tested).
//!
//! **Privacy.** Ollama (the default) is local. A cloud engine receives photos only once the
//! user chose it and saved its API key, and a cloud batch only after the cost confirm's
//! Proceed — see [`state`].

pub mod logic;
pub mod panel;
pub mod settings;
pub mod state;

use super::{view as view_factory, Contributions, Module, ModuleHost, ModuleInstance, ModuleMeta, Panel, PanelSlot, SettingsPanel};
use gpui_kit::{App, AppContext as _, Entity};

pub const AI_MODULE_ID: &str = "ai";

pub struct AiTaggingModule;

pub struct AiInstance {
    state: Entity<state::AiState>,
    host: ModuleHost,
}

impl Module for AiTaggingModule {
    fn meta(&self) -> ModuleMeta {
        ModuleMeta::new(AI_MODULE_ID, "AI Tagging")
            .description(
                "Suggest tags from your taxonomy (and propose new ones) using a vision model — local (Ollama) or cloud.",
            )
            .backend_feature("ai")
    }

    fn load(&self, host: ModuleHost, cx: &mut App) -> Result<Box<dyn ModuleInstance>, String> {
        let app = host.model().read(cx).state().clone();
        let (settings, model, shell) = (host.settings(), host.model().clone(), host.shell().clone());
        let state = cx.new(|cx| state::AiState::new(app, settings, model, shell, cx));
        Ok(Box::new(AiInstance { state, host }))
    }
}

impl AiInstance {
    pub fn state(&self) -> &Entity<state::AiState> {
        &self.state
    }
}

impl ModuleInstance for AiInstance {
    fn contributions(&self) -> Contributions {
        let state = self.state.clone();
        let settings = view_factory(move |window, cx| settings::AiSettings::new(state.clone(), window, cx));
        let (state, images) = (self.state.clone(), self.host.images().cloned());
        let panel = view_factory(move |window, cx| panel::AiPanel::new(state.clone(), images.clone(), window, cx));
        Contributions {
            settings: vec![SettingsPanel { id: "ai-settings".into(), view: settings }],
            panels: vec![Panel { id: "ai-tags".into(), label: "AI tags".into(), slot: PanelSlot::Inspector, view: panel }],
            ..Default::default()
        }
    }

    fn on_unload(&mut self, cx: &mut App) {
        self.state.update(cx, |s, cx| s.unload(cx));
    }
}
