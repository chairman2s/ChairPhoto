//! The Faces module (`src/modules/plugins/faces.tsx`), first half (#129): the settings panel
//! (models, inference, indexing speed, people root, threshold, "Index faces" with its job),
//! the inspector's Faces block, and the loupe's face overlay. Compiled with the `faces`
//! feature, like its backend (`chairphoto_core::app::faces`, `plugins::faces`).
//!
//! - [`state::FacesState`] — everything catalog-derived and every write, off the UI thread,
//!   bound to the catalog it was read from; the indexing job it follows.
//! - [`settings::FacesSettings`], [`inspector::FacesInspector`], [`overlay::FaceOverlay`] —
//!   the three views; [`picker::PersonPicker`] is shared by the last two.
//! - [`logic`] — geometry, picker rows and result lines (unit-tested).
//!
//! **Seams for #130** (People view, clusters, matching review): the People main view and the
//! matching job ("Run matching", `faces:match_progress`/`faces:match_done`) add to
//! [`FacesInstance::contributions`] and [`state::FacesState`]; the state already notes a
//! running match (it disables "Index faces") and re-reads the shown faces when one ends.
//!
//! All inference is local; the only network use is "Download models", on the user's click.

pub mod inspector;
pub mod logic;
pub mod overlay;
pub mod picker;
pub mod settings;
pub mod state;

use super::{view as view_factory, Contributions, Module, ModuleHost, ModuleInstance, ModuleMeta, Panel, PanelSlot, SettingsPanel};
use gpui_kit::{App, AppContext as _, Entity};

pub const FACES_MODULE_ID: &str = "faces";

pub struct FacesModule;

pub struct FacesInstance {
    state: Entity<state::FacesState>,
    host: ModuleHost,
}

impl Module for FacesModule {
    fn meta(&self) -> ModuleMeta {
        ModuleMeta::new(FACES_MODULE_ID, "Faces")
            .description(
                "Local face detection and recognition: index faces, confirm or correct who is who in the loupe and \
                 the inspector, and turn faces into ordinary person tags. Nothing leaves your computer; the models \
                 download once, when you ask.",
            )
            .backend_feature("faces")
    }

    fn load(&self, host: ModuleHost, cx: &mut App) -> Result<Box<dyn ModuleInstance>, String> {
        let app = host.model().read(cx).state().clone();
        let (settings, model, shell) = (host.settings(), host.model().clone(), host.shell().clone());
        let state = cx.new(|cx| state::FacesState::new(app, settings, model, shell, cx));
        Ok(Box::new(FacesInstance { state, host }))
    }
}

impl FacesInstance {
    pub fn state(&self) -> &Entity<state::FacesState> {
        &self.state
    }
}

impl ModuleInstance for FacesInstance {
    fn contributions(&self) -> Contributions {
        let state = self.state.clone();
        let settings = view_factory(move |window, cx| settings::FacesSettings::new(state.clone(), window, cx));
        let state = self.state.clone();
        let inspector = view_factory(move |_, cx| inspector::FacesInspector::new(state.clone(), cx));
        let (state, images) = (self.state.clone(), self.host.images().cloned());
        let overlay = view_factory(move |window, cx| overlay::FaceOverlay::new(state.clone(), images.clone(), window, cx));
        Contributions {
            settings: vec![SettingsPanel { id: "faces-settings".into(), view: settings }],
            panels: vec![
                Panel { id: "faces-inspector".into(), label: "Faces".into(), slot: PanelSlot::Inspector, view: inspector },
                Panel { id: "faces-overlay".into(), label: "Faces".into(), slot: PanelSlot::Loupe, view: overlay },
            ],
            ..Default::default()
        }
    }

    /// Disabled: stop following (the core job, if any, runs on detached, as in React).
    fn on_unload(&mut self, cx: &mut App) {
        self.state.update(cx, |s, cx| s.unload(cx));
    }
}
