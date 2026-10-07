//! The Faces module (`src/modules/plugins/faces.tsx`): the settings panel (models, inference,
//! indexing speed, people root, threshold, "Index faces" and "Run matching" with their jobs),
//! the inspector's Faces block and the loupe's face overlay (#129), and the People main view
//! — named people, unnamed clusters with merge and split, the suggestion queue,
//! filter-by-person (#130). Compiled with the `faces` feature, like its backend
//! (`chairphoto_core::app::faces`, `plugins::faces`).
//!
//! - [`state::FacesState`] — everything catalog-derived for the per-photo views and every
//!   write, off the UI thread, bound to the catalog it was read from; the indexing and
//!   matching jobs it follows.
//! - [`people::People`] — the People view's reads and writes, bound the same way.
//! - [`settings::FacesSettings`], [`inspector::FacesInspector`], [`overlay::FaceOverlay`],
//!   [`people_view::PeopleView`] — the views; [`picker::PersonPicker`] is shared by the
//!   inspector and the overlay.
//! - [`logic`] — geometry, picker rows, result lines, naming paths and avatar crops
//!   (unit-tested).
//!
//! All inference is local; the only network use is "Download models", on the user's click.

pub mod inspector;
pub mod logic;
pub mod overlay;
pub mod people;
pub mod people_view;
pub mod picker;
pub mod settings;
pub mod state;

use super::{
    view as view_factory, Contributions, MainView, Module, ModuleHost, ModuleInstance, ModuleMeta, Panel, PanelSlot,
    SettingsPanel,
};
use gpui_kit::component::Icon;
use gpui_kit::{App, AppContext as _, Entity};

pub const FACES_MODULE_ID: &str = "faces";

pub struct FacesModule;

pub struct FacesInstance {
    state: Entity<state::FacesState>,
    people: Entity<people::People>,
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
        let (settings, model, shell, images) = (host.settings(), host.model().clone(), host.shell().clone(), host.images().cloned());
        let state =
            cx.new(|cx| state::FacesState::new(app.clone(), settings, model.clone(), shell.clone(), images, cx));
        let faces = state.clone();
        let people = cx.new(|cx| people::People::new(app, model, shell, faces, cx));
        Ok(Box::new(FacesInstance { state, people, host }))
    }
}

impl FacesInstance {
    pub fn state(&self) -> &Entity<state::FacesState> {
        &self.state
    }

    pub fn people(&self) -> &Entity<people::People> {
        &self.people
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
        let (people, images) = (self.people.clone(), self.host.images().cloned());
        let people_view =
            view_factory(move |window, cx| people_view::PeopleView::new(people.clone(), images.clone(), window, cx));
        Contributions {
            main_views: vec![MainView {
                id: people::PEOPLE_VIEW_ID.into(),
                label: "People".into(),
                icon: Some(Icon::new(gpui_kit::assets::IconName::UserGroup)),
                view: people_view,
            }],
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
        self.people.update(cx, |p, cx| p.unload(cx));
    }
}
