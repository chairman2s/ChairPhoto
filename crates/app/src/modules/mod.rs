//! First-party modules: the [`Module`] trait, what a module contributes, and the compiled-in
//! registry ([`registry::ModuleRegistry`]) that enables, disables and routes to them.
//!
//! The contract was decided in "Module trait: contribution points for first-party Rust
//! modules" (#104) and replaces `src/modules/registry.ts` + `host.ts`:
//!
//! - **A slim, object-safe trait.** A module is `Rc<dyn Module>`: metadata ([`ModuleMeta`])
//!   plus [`Module::load`], which builds the module's live state ([`ModuleInstance`]) when it
//!   is enabled. Nothing assumes the implementor is compiled in, so a future extension host
//!   (after parity, sandboxed) can implement the same trait.
//! - **Contributions are data plus view factories** ([`Contributions`]): panels at the four
//!   slots ([`PanelSlot`]: inspector, sidebar, loupe, tag editor), actions, publish targets,
//!   settings panels and main views. The shell renders them; a module never reaches into the
//!   shell's views.
//! - **Services.** Every [`CoreEvent`] reaches each *enabled* module's
//!   [`ModuleInstance::on_event`], in registration order; settings are namespaced
//!   `<module id>.<key>` ([`ModuleSettings`]); selection, the active photo and the Library
//!   scope are read and changed through the app's entities ([`ModuleHost`]).
//! - **Lifecycle.** [`Module::load`] on enable (an `Err` rolls the enable back and says why),
//!   [`ModuleInstance::on_unload`] on disable; the instance and every view it contributed are
//!   dropped with it.
//! - **Enable/disable** per module in the Modules panel ([`panel::ModulesPanel`]), persisted
//!   in the catalog setting `modules.enabled` in dependency order; `requires` enables
//!   dependencies first and cascade-disables dependents (ported from `host.ts`).
//!
//! **Dropped** with the JS host (#104): `onPhotoSelected` (read the shell's selection),
//! `api.fetch`, `getEditRecord`, the external loader, semver ranges on `requires`, permission
//! grants, and the edit-renderer contribution (the Basic Editor folds into the Darkroom).
//!
//! **Panics are bugs.** `host.ts` wrapped every module callback in try/catch because it ran
//! third-party JavaScript. These modules are first-party Rust: a recoverable failure to load
//! is `Err` from [`Module::load`]; a panic is a bug and fails like any other panic in the app.
//!
//! **Re-entrancy rule.** The registry calls `load`, `on_event`, `on_unload` and every view
//! factory while it holds no lease on itself, so a module may read the registry from them.
//! An enable or disable asked for from inside `load`, `on_event` or `on_unload` is not run
//! there (the module's instance is in use): it is logged and deferred until the callback
//! returns.

pub mod panel;
pub mod registry;
pub mod statistics;

#[cfg(any(test, feature = "dev-module"))]
pub mod dev_module;
#[cfg(feature = "collage")]
pub mod collage;
#[cfg(any(feature = "slideshow", feature = "collage"))]
pub mod dialog;
#[cfg(feature = "faces")]
pub mod faces;
#[cfg(feature = "map")]
pub mod map;
#[cfg(feature = "slideshow")]
pub mod slideshow;
#[cfg(feature = "tag-graph")]
pub mod tag_graph;

#[cfg(test)]
mod tests;

pub use registry::{ModuleInfo, ModuleRegistry, RequirementInfo};

use crate::image_store::ImageStore;
use crate::model::AppModel;
use crate::shell::ShellState;
use chairphoto_core::app::{with_catalog_as, AppState, CatalogIdentity, CoreEvent};
use gpui_kit::component::Icon;
use gpui_kit::{AnyView, App, AppContext as _, Context, Entity, Render, SharedString, Window};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

/// A first-party module. Object-safe: the registry holds `Rc<dyn Module>`.
pub trait Module: 'static {
    /// Who the module is and what it needs. Read once, at registration.
    fn meta(&self) -> ModuleMeta;

    /// Enable the module: build its live state and say what it contributes. Runs on the main
    /// thread after every module in [`ModuleMeta::requires`] has loaded; keep it short and
    /// move blocking work to a background task. `Err(why)` leaves the module disabled and
    /// shows `why` on the status line.
    fn load(&self, host: ModuleHost, cx: &mut App) -> Result<Box<dyn ModuleInstance>, String>;
}

/// A loaded (enabled) module. Dropped when the module is disabled, after [`on_unload`].
///
/// [`on_unload`]: ModuleInstance::on_unload
pub trait ModuleInstance: 'static {
    /// What the module adds to the shell. Read once, right after [`Module::load`].
    fn contributions(&self) -> Contributions;

    /// A core event. Every event reaches every enabled module, in registration order; match
    /// the ones you need. A job's events carry its job id: drop a superseded job's stragglers.
    fn on_event(&mut self, _event: &CoreEvent, _cx: &mut App) {}

    /// The module is being disabled: stop timers and background work it started. Its
    /// contributed views are dropped by the registry right after.
    fn on_unload(&mut self, _cx: &mut App) {}
}

/// A module's identity and requirements (`ChairPhotoModule` in registry.ts, minus the
/// version and semver: compiled-in modules ship with the app).
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleMeta {
    /// Stable id: the settings namespace (`<id>.<key>`) and the key in `modules.enabled`.
    /// Non-empty, no `.` or `,`, and not a host-owned namespace
    /// ([`registry::RESERVED_NAMESPACES`], checked by [`registry::validate_id`]). The `ai`,
    /// `faces` and `smarttags` ids share their namespace with their own backend
    /// ([`registry::BACKEND_NAMESPACES`]).
    pub id: SharedString,
    pub name: SharedString,
    pub description: SharedString,
    /// Module ids enabled before this one (and cascade-disabled with it).
    pub requires: Vec<SharedString>,
    /// The cargo feature its backend needs, e.g. `"faces"`. When the build lacks it, the
    /// Modules panel says "backend … not included in this build" and refuses to enable it.
    pub backend_feature: Option<SharedString>,
    /// What photos this module publishes are marked with in `publications.platform`;
    /// `None` = the module id (host.ts `getPublicationMarker`).
    pub publication_marker: Option<SharedString>,
}

impl ModuleMeta {
    pub fn new(id: impl Into<SharedString>, name: impl Into<SharedString>) -> Self {
        ModuleMeta {
            id: id.into(),
            name: name.into(),
            description: SharedString::default(),
            requires: Vec::new(),
            backend_feature: None,
            publication_marker: None,
        }
    }

    pub fn description(mut self, description: impl Into<SharedString>) -> Self {
        self.description = description.into();
        self
    }

    pub fn requires(mut self, id: impl Into<SharedString>) -> Self {
        self.requires.push(id.into());
        self
    }

    pub fn backend_feature(mut self, feature: impl Into<SharedString>) -> Self {
        self.backend_feature = Some(feature.into());
        self
    }

    pub fn publication_marker(mut self, marker: impl Into<SharedString>) -> Self {
        self.publication_marker = Some(marker.into());
        self
    }

    /// The marker this module stamps on what it publishes: the declared one, else its id.
    pub fn marker(&self) -> &str {
        self.publication_marker.as_deref().unwrap_or(&self.id)
    }
}

/// Builds a contributed view in the window that shows it. The registry calls it once per
/// window and keeps the view while the module stays enabled (panels, main views, settings
/// panels); modal actions and publish targets get a fresh view each time their dialog opens.
pub type ViewFactory = Rc<dyn Fn(&mut Window, &mut App) -> AnyView>;

/// A [`ViewFactory`] for a view type.
pub fn view<V: Render>(build: impl Fn(&mut Window, &mut Context<V>) -> V + 'static) -> ViewFactory {
    Rc::new(move |window, cx| cx.new(|cx| build(window, cx)).into())
}

/// Where a panel renders (`ModulePanel.slot`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PanelSlot {
    /// The inspector's tags tab, under the built-in blocks, one labelled block per panel.
    Inspector,
    /// The collection browser, below its sections.
    Sidebar,
    /// Over the loupe, absolutely positioned (the face overlay). Mounted by the loupe (#109).
    Loupe,
    /// Inside the tag editor (the tag being edited is the editor's). Mounted by #107.
    TagEditor,
}

impl PanelSlot {
    pub const ALL: [PanelSlot; 4] = [PanelSlot::Inspector, PanelSlot::Sidebar, PanelSlot::Loupe, PanelSlot::TagEditor];

    pub fn name(self) -> &'static str {
        match self {
            PanelSlot::Inspector => "inspector",
            PanelSlot::Sidebar => "sidebar",
            PanelSlot::Loupe => "loupe",
            PanelSlot::TagEditor => "tag-editor",
        }
    }
}

/// A panel at one of the four slots.
#[derive(Clone)]
pub struct Panel {
    pub id: SharedString,
    pub label: SharedString,
    pub slot: PanelSlot,
    pub view: ViewFactory,
}

/// What a module action does when chosen from More ⋯ → Modules.
#[derive(Clone)]
pub enum ActionKind {
    /// Fire and forget (`onActivate`).
    Run(Rc<dyn Fn(&mut Window, &mut App)>),
    /// Open a dialog with this view in it (`render(close)`: the view closes it with
    /// `window.close_dialog(cx)`).
    Modal(ViewFactory),
}

/// A module action (`ToolbarAction`), listed under its module in More ⋯ → Modules.
#[derive(Clone)]
pub struct ModuleAction {
    pub id: SharedString,
    pub label: SharedString,
    pub kind: ActionKind,
}

/// A destination in the Publish dialog (`PublishTarget`): its form reads the selection
/// through [`ModuleHost::shell`] and records publications with [`ModuleMeta::marker`].
#[derive(Clone)]
pub struct PublishTarget {
    pub id: SharedString,
    pub label: SharedString,
    pub view: ViewFactory,
}

/// A module's settings section (`SettingsPanel`), shown on the module's Preferences tab
/// (`crate::preferences`), which an enabled module with settings gets.
#[derive(Clone)]
pub struct SettingsPanel {
    pub id: SharedString,
    pub view: ViewFactory,
}

/// A full-surface view on the icon rail (`MainView`); the stage shows it as
/// `Surface::Module(id)`.
#[derive(Clone)]
pub struct MainView {
    pub id: SharedString,
    pub label: SharedString,
    /// The rail glyph; `None` = a generic dashboard glyph.
    pub icon: Option<Icon>,
    pub view: ViewFactory,
}

/// Everything one module adds to the shell.
#[derive(Clone, Default)]
pub struct Contributions {
    pub panels: Vec<Panel>,
    pub actions: Vec<ModuleAction>,
    pub publish_targets: Vec<PublishTarget>,
    pub settings: Vec<SettingsPanel>,
    pub main_views: Vec<MainView>,
}

/// What the registry hands a module when it loads: its settings, and the app entities that
/// hold what `ChairPhotoAPI` used to expose (selection, active photo, Library scope, status),
/// and the image layer for thumbnails (`thumb://` in the React app).
#[derive(Clone)]
pub struct ModuleHost {
    meta: ModuleMeta,
    app: AppState,
    restored: RestoredCatalog,
    model: Entity<AppModel>,
    shell: Entity<ShellState>,
    images: Option<Entity<ImageStore>>,
}

impl ModuleHost {
    pub(crate) fn new(
        meta: ModuleMeta,
        app: AppState,
        restored: RestoredCatalog,
        model: Entity<AppModel>,
        shell: Entity<ShellState>,
    ) -> Self {
        ModuleHost { meta, app, restored, model, shell, images: None }
    }

    pub(crate) fn with_images(mut self, images: Option<Entity<ImageStore>>) -> Self {
        self.images = images;
        self
    }

    pub fn meta(&self) -> &ModuleMeta {
        &self.meta
    }

    /// A handle on this module's settings, namespaced by its id and bound to the catalog the
    /// registry restored the enabled set from — the one open now, as far as the UI knows. Take
    /// a fresh handle after `catalog:switched`: the old one fails closed (see
    /// [`ModuleSettings`]), and a fresh one reads and writes the new catalog once the registry
    /// has restored from it.
    pub fn settings(&self) -> ModuleSettings {
        ModuleSettings { app: self.app.clone(), prefix: format!("{}.", self.meta.id), catalog: self.restored.get() }
    }

    /// The app model: the core [`AppState`], the status line, the open catalog.
    pub fn model(&self) -> &Entity<AppModel> {
        &self.model
    }

    /// The shell: the Library session (scope, selection, active photo) and the surface.
    pub fn shell(&self) -> &Entity<ShellState> {
        &self.shell
    }

    /// The app's image layer (thumbnails, previews), when the app has one: `None` in a
    /// registry built without it (tests of the registry alone).
    pub fn images(&self) -> Option<&Entity<ImageStore>> {
        self.images.as_ref()
    }
}

/// A module's settings: the catalog's key/value `settings` table, every key prefixed with
/// `<module id>.` (`getSetting`/`setSetting` in host.ts), so a module cannot read or write
/// another module's keys or the host's own ([`registry::RESERVED_NAMESPACES`]). A module whose
/// id is one of [`registry::BACKEND_NAMESPACES`] shares the namespace with its own backend.
///
/// **Bound to one catalog opening.** Settings are per catalog, and a module's instance outlives
/// a catalog switch, so a handle carries the [`CatalogIdentity`] the registry restored from when
/// it was taken ([`ModuleHost::settings`]). [`get`](Self::get) and [`set`](Self::set) run
/// through `with_catalog_as`: once another catalog is open — even before `catalog:switched`
/// reaches the UI — they fail closed with [`CATALOG_CHANGED`](chairphoto_core::app::CATALOG_CHANGED)
/// rather than read or write the new catalog's keys. A handle taken while no catalog's set has
/// been restored (between a switch and the new catalog's restore) fails with
/// [`SETTINGS_NOT_READY`].
///
/// **Blocking** (the catalog lock and SQLite): call [`get`](Self::get) and [`set`](Self::set)
/// from a background task, e.g. `cx.background_executor().spawn(..)`, never in a render or
/// an event handler.
#[derive(Clone)]
pub struct ModuleSettings {
    app: AppState,
    prefix: String,
    catalog: Option<CatalogIdentity>,
}

/// What a [`ModuleSettings`] handle taken before the open catalog's modules were restored
/// answers: it is bound to no catalog, so it touches none.
pub const SETTINGS_NOT_READY: &str = "The catalog's module settings are not read yet";

/// The catalog opening the registry last restored the enabled set from, shared by the
/// registry and every [`ModuleHost`] it hands out: what a settings handle is bound to. `None`
/// from a catalog switch until the new catalog's restore has read.
#[derive(Clone, Default)]
pub(crate) struct RestoredCatalog(Arc<Mutex<Option<CatalogIdentity>>>);

impl RestoredCatalog {
    pub(crate) fn get(&self) -> Option<CatalogIdentity> {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn set(&self, catalog: Option<CatalogIdentity>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = catalog;
    }
}

impl ModuleSettings {
    /// The stored key for `key`: `<module id>.<key>`.
    pub fn key(&self, key: &str) -> String {
        format!("{}{key}", self.prefix)
    }

    /// The catalog opening this handle reads and writes; `None` = none (it fails closed).
    pub fn catalog(&self) -> Option<CatalogIdentity> {
        self.catalog
    }

    pub fn get(&self, key: &str) -> Result<Option<String>, String> {
        let key = self.key(key);
        with_catalog_as(&self.app, self.bound()?, |c| c.get_setting(&key))
    }

    pub fn set(&self, key: &str, value: &str) -> Result<(), String> {
        let key = self.key(key);
        with_catalog_as(&self.app, self.bound()?, |c| c.set_setting(&key, value))
    }

    fn bound(&self) -> Result<CatalogIdentity, String> {
        self.catalog.ok_or_else(|| SETTINGS_NOT_READY.to_string())
    }
}

/// The first-party modules this build ships, in registration order: the order of
/// `BUNDLED_MODULES` in `src/modules/bundled.ts` as each is ported (#123–#129), each behind
/// its backend's cargo feature like the Tauri shell's features. A module whose backend is
/// compiled out may still register (metadata only) so the Modules panel can say so.
pub fn bundled() -> Vec<Rc<dyn Module>> {
    #[allow(unused_mut)]
    let mut modules: Vec<Rc<dyn Module>> = Vec::new();
    #[cfg(any(test, feature = "dev-module"))]
    modules.push(Rc::new(dev_module::DevModule));
    modules.push(Rc::new(statistics::StatisticsModule));
    #[cfg(feature = "collage")]
    modules.push(Rc::new(collage::CollageModule::default()));
    #[cfg(feature = "slideshow")]
    modules.push(Rc::new(slideshow::SlideshowModule::default()));
    #[cfg(feature = "map")]
    modules.push(Rc::new(map::MapModule));
    #[cfg(feature = "faces")]
    modules.push(Rc::new(faces::FacesModule));
    #[cfg(feature = "tag-graph")]
    modules.push(Rc::new(tag_graph::TagGraphModule));
    modules
}

/// The backend features compiled into this build (`plugin_features` in the Tauri shell):
/// what [`ModuleMeta::backend_feature`] is checked against.
pub fn compiled_features() -> Vec<SharedString> {
    #[allow(unused_mut)]
    let mut features: Vec<SharedString> = Vec::new();
    #[cfg(feature = "ai")]
    features.push("ai".into());
    #[cfg(feature = "edit")]
    features.push("edit".into());
    #[cfg(feature = "instagram")]
    features.push("instagram".into());
    #[cfg(feature = "flickr")]
    features.push("flickr".into());
    #[cfg(feature = "smugmug")]
    features.push("smugmug".into());
    #[cfg(feature = "localsend")]
    features.push("localsend".into());
    #[cfg(feature = "collage")]
    features.push("collage".into());
    #[cfg(feature = "slideshow")]
    features.push("slideshow".into());
    #[cfg(feature = "map")]
    features.push("map".into());
    #[cfg(feature = "faces")]
    features.push("faces".into());
    #[cfg(feature = "smarttags")]
    features.push("smarttags".into());
    features
}
