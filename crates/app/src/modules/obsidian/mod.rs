//! The Obsidian module (`src/modules/plugins/obsidian.tsx`, #128; docs/obsidian.md): a
//! companion note in an Obsidian vault for a photo or a tag, linked both ways —
//! `obsidian://new` / `obsidian://open` out, `chairphoto://<uuid>` (and `/loupe`) or
//! `chairphoto://tag/<uuid>` back, which the app already opens (`AppModel::open_url`).
//!
//! - [`state::ObsidianState`] — the settings, the active photo's and the edited tag's note
//!   records, Create / Open / Forget, off the UI thread and bound to their catalog.
//! - [`views`] — the inspector's "Note" panel, the tag editor's "Obsidian note" section and
//!   the settings panel.
//! - The notes themselves (names, initial text, URIs, the record's JSON) are
//!   `chairphoto_model::obsidian`, unit-tested against the React functions' output.
//!
//! No backend and no cargo feature, as in React: always compiled in, disabled until enabled
//! in Preferences → Modules. ChairPhoto never writes into the vault; Obsidian creates the
//! note from the URI, and no image is exported.

pub mod state;
pub mod views;

use super::{view as view_factory, Contributions, Module, ModuleHost, ModuleInstance, ModuleMeta, Panel, PanelSlot, SettingsPanel};
use chairphoto_model::obsidian::MODULE_ID;
use gpui_kit::{App, AppContext as _, Entity};

pub const OBSIDIAN_MODULE_ID: &str = MODULE_ID;

pub struct ObsidianModule;

pub struct ObsidianInstance {
    state: Entity<state::ObsidianState>,
}

impl Module for ObsidianModule {
    fn meta(&self) -> ModuleMeta {
        ModuleMeta::new(OBSIDIAN_MODULE_ID, "Obsidian").description(
            "Per-photo and per-tag notes in an Obsidian vault, linked both ways (obsidian:// out, chairphoto:// back).",
        )
    }

    fn load(&self, host: ModuleHost, cx: &mut App) -> Result<Box<dyn ModuleInstance>, String> {
        let app = host.model().read(cx).state().clone();
        let (settings, model, shell) = (host.settings(), host.model().clone(), host.shell().clone());
        let state = cx.new(|cx| state::ObsidianState::new(app, settings, model, shell, cx));
        Ok(Box::new(ObsidianInstance { state }))
    }
}

impl ObsidianInstance {
    pub fn state(&self) -> &Entity<state::ObsidianState> {
        &self.state
    }
}

impl ModuleInstance for ObsidianInstance {
    fn contributions(&self) -> Contributions {
        let state = self.state.clone();
        let settings = view_factory(move |window, cx| views::ObsidianSettings::new(state.clone(), window, cx));
        let state = self.state.clone();
        let note = view_factory(move |_, cx| views::NotePanel::new(state.clone(), cx));
        let state = self.state.clone();
        let tag_note = view_factory(move |_, cx| views::TagNotePanel::new(state.clone(), cx));
        Contributions {
            settings: vec![SettingsPanel { id: "obsidian-settings".into(), view: settings }],
            panels: vec![
                Panel { id: "obsidian-note".into(), label: "Note".into(), slot: PanelSlot::Inspector, view: note },
                Panel { id: "obsidian-tag-note".into(), label: "Obsidian note".into(), slot: PanelSlot::TagEditor, view: tag_note },
            ],
            ..Default::default()
        }
    }

    fn on_unload(&mut self, cx: &mut App) {
        self.state.update(cx, |s, cx| s.unload(cx));
    }
}
