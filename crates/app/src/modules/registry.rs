//! [`ModuleRegistry`]: the compiled-in modules, which of them are enabled, what the enabled
//! ones contribute, and the views the shell built from those contributions. The runtime half
//! of `src/modules/host.ts`.
//!
//! **Enable** ([`ModuleRegistry::enable`], `enableModule`): refuse — with the reason on the
//! status line — a module whose own backend is compiled out or whose requirement is missing
//! or compiled out; enable each required module first (so `load` runs dependencies before
//! dependents); then `load` it. A failed `load` leaves it disabled.
//!
//! **Disable** ([`ModuleRegistry::disable`], `disableModule`): cascade-disable every enabled
//! module that requires it first, then `on_unload` and drop its instance, its contributions
//! and every view built from them.
//!
//! **Persistence.** The enabled set lives in the open catalog's `settings` table under
//! `modules.enabled`, comma-separated in dependency order, written off the UI thread after
//! every user toggle (a write a newer one overtook is skipped). It is read once, on the first
//! catalog read after startup ([`AppModelEvent::CatalogRead`]), as `initHost` did; a catalog
//! switch keeps the modules that are enabled (React did not re-read either), and the next
//! toggle writes the set into the catalog that is open then.
//!
//! **Events.** Every [`CoreEvent`] the app model routes ([`AppModelEvent::Core`]) reaches each
//! enabled module's `on_event`, in registration order. `appearance:theme_changed` is applied
//! to the theme by the router and does not reach modules.

use super::{
    ActionKind, Contributions, MainView, Module, ModuleAction, ModuleHost, ModuleInstance, ModuleMeta, Panel,
    PanelSlot, PublishTarget, SettingsPanel, ViewFactory,
};
use crate::model::{AppModel, AppModelEvent};
use crate::shell::ShellState;
use chairphoto_core::app::{with_catalog, AppState, CoreEvent};
use gpui_kit::component::WindowExt as _;
use gpui_kit::prelude::*;
use gpui_kit::{AnyView, App, Context, Entity, SharedString, Window, WindowId};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

/// The catalog setting holding the enabled set (host.ts `modules.enabled`).
pub const ENABLED_KEY: &str = "modules.enabled";

/// Settings namespaces the host owns: no module may take one of these as its id, or its
/// `ModuleSettings` would read and write the host's keys. Every `<prefix>.` key the core, the
/// model and the app use outside a module's own namespace is here (the source scan in
/// `modules::tests` keeps this list complete):
///
/// - `modules` — `modules.enabled`;
/// - `indexing` — `indexing.speed` (core `plugins/indexing.rs`);
/// - `sharpness` — `sharpness.soft_threshold`, `sharpness.burst_soft_threshold`;
/// - `editor` — external editors and RapidRaw (`editor.<key>.*`, `editor.rapidraw.*`),
///   `editor.renderTiming.lastShell`;
/// - `develop` — the Darkroom (`develop.decodeCacheGb`, `develop.preloadNeighbours`,
///   `develop.wbSlider`);
/// - `basic-editor` — the Darkroom's presets (`basic-editor.presets`; the Basic Editor
///   folded into the Darkroom, #104);
/// - `metrics` — `metrics.exportParity`;
/// - `geocode` — the map backend's `geocode.endpoint` (the Map module's id is `map`).
pub const RESERVED_NAMESPACES: &[&str] =
    &["modules", "indexing", "sharpness", "editor", "develop", "basic-editor", "metrics", "geocode"];

/// Namespaces a module shares with its own backend, deliberately, as in React: the module
/// whose id this is reads and writes the keys its core backend reads (`ai.*` burst settings,
/// `faces.*`, `smarttags.*`). Only that module may take the id.
pub const BACKEND_NAMESPACES: &[&str] = &["ai", "faces", "smarttags"];

/// Why `id` cannot be a module id, or `Ok`. Ids are settings namespaces (`<id>.<key>`) and
/// entries in a comma-separated list: a `.` would let `a` + `b.c` and `a.b` + `c` name the
/// same key, a `,` would split the list, and a [`RESERVED_NAMESPACES`] id would own host keys.
pub fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("a module id must not be empty".into());
    }
    if id.contains(['.', ',']) || id.chars().any(char::is_whitespace) {
        return Err(format!("module id {id:?} contains '.', ',' or whitespace"));
    }
    if RESERVED_NAMESPACES.contains(&id) {
        return Err(format!("module id {id:?} is a settings namespace the host owns"));
    }
    Ok(())
}

/// A declared requirement, resolved for the Modules panel (host.ts `RequirementInfo`).
#[derive(Debug, Clone, PartialEq)]
pub struct RequirementInfo {
    pub id: SharedString,
    /// The required module's name, or its id when it is not registered.
    pub name: SharedString,
    /// Registered and its backend compiled in.
    pub met: bool,
}

/// One row of the Modules panel (host.ts `ModuleInfo`, minus version, external flag and
/// permissions).
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleInfo {
    pub id: SharedString,
    pub name: SharedString,
    pub description: SharedString,
    pub enabled: bool,
    pub backend_available: bool,
    pub backend_feature: Option<SharedString>,
    pub requires: Vec<RequirementInfo>,
    /// Why it cannot be enabled now; `None` when it can, or is enabled.
    pub blocked_reason: Option<String>,
}

/// One enabled module's actions, for More ⋯ → Modules (host.ts `toolbarActionGroups`).
#[derive(Clone)]
pub struct ActionGroup {
    pub module_id: SharedString,
    pub module_name: SharedString,
    pub actions: Vec<ModuleAction>,
}

/// A view built from a contribution, for one slot.
#[derive(Clone)]
pub struct SlotView {
    pub module_id: SharedString,
    pub id: SharedString,
    pub label: SharedString,
    pub view: AnyView,
}

struct Live {
    instance: Rc<RefCell<Box<dyn ModuleInstance>>>,
    contributions: Contributions,
}

struct Entry {
    module: Rc<dyn Module>,
    meta: ModuleMeta,
    live: Option<Live>,
}

/// A contributed view, per window: a panel's view in the main window is not the same entity
/// as the same panel's view in a pop-out loupe window. A window's views are dropped when it
/// closes, and every view of a module when it is disabled.
#[derive(Clone, PartialEq, Eq, Hash)]
struct ViewKey {
    window: WindowId,
    module: SharedString,
    kind: &'static str,
    id: SharedString,
}

/// The registry entity. Mutate it through the associated functions that take
/// `&Entity<Self>` ([`enable`](Self::enable), [`disable`](Self::disable),
/// [`activate`](Self::activate), the `*_views` builders): they call into modules while
/// holding no lease on the registry.
pub struct ModuleRegistry {
    entries: Vec<Entry>,
    features: Vec<SharedString>,
    app: AppState,
    model: Entity<AppModel>,
    shell: Entity<ShellState>,
    restored: bool,
    views: HashMap<ViewKey, AnyView>,
    persist_generation: u64,
    /// The generation of the newest `modules.enabled` write that ran.
    persisted: Arc<Mutex<u64>>,
    /// How many module callbacks (`load`, `on_event`, `on_unload`) are running now. An enable
    /// or disable asked for from inside one is deferred until it returns ([`Self::enable`]).
    in_callback: u32,
}

impl ModuleRegistry {
    /// The production registry: [`super::bundled`] against [`super::compiled_features`].
    pub fn install(model: &Entity<AppModel>, shell: &Entity<ShellState>, cx: &mut App) -> Entity<Self> {
        Self::install_with(super::bundled(), super::compiled_features(), model, shell, cx)
    }

    /// Register `modules` (in order) and subscribe to the model: its first catalog read
    /// restores the enabled set, and its core events reach the enabled modules. A module with
    /// an invalid or duplicate id is left out, with a log line.
    pub fn install_with(
        modules: Vec<Rc<dyn Module>>,
        features: Vec<SharedString>,
        model: &Entity<AppModel>,
        shell: &Entity<ShellState>,
        cx: &mut App,
    ) -> Entity<Self> {
        let mut entries: Vec<Entry> = Vec::new();
        for module in modules {
            let meta = module.meta();
            if let Err(e) = validate_id(&meta.id) {
                eprintln!("modules: not registered: {e}");
                continue;
            }
            if entries.iter().any(|e| e.meta.id == meta.id) {
                eprintln!("modules: not registered: a second module with id {:?}", meta.id);
                continue;
            }
            entries.push(Entry { module, meta, live: None });
        }
        let app = model.read(cx).state().clone();
        let registry = cx.new(|_| ModuleRegistry {
            entries,
            features,
            app,
            model: model.clone(),
            shell: shell.clone(),
            restored: false,
            views: HashMap::new(),
            persist_generation: 0,
            persisted: Arc::new(Mutex::new(0)),
            in_callback: 0,
        });
        let weak = registry.downgrade();
        cx.on_window_closed(move |cx, closed| {
            if let Some(registry) = weak.upgrade() {
                registry.update(cx, |r, _| r.views.retain(|key, _| key.window != closed));
            }
        })
        .detach();
        let weak = registry.downgrade();
        cx.subscribe(model, move |_, event: &AppModelEvent, cx| {
            let Some(registry) = weak.upgrade() else { return };
            match event {
                AppModelEvent::CatalogRead => Self::restore(&registry, cx),
                AppModelEvent::Core(event) => Self::deliver(&registry, event, cx),
            }
        })
        .detach();
        registry
    }

    // --- queries ---------------------------------------------------------------------

    fn entry(&self, id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.meta.id.as_ref() == id)
    }

    fn entry_mut(&mut self, id: &str) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|e| e.meta.id.as_ref() == id)
    }

    /// Every registered module's metadata, in registration order.
    pub fn metas(&self) -> impl Iterator<Item = &ModuleMeta> {
        self.entries.iter().map(|e| &e.meta)
    }

    pub fn is_enabled(&self, id: &str) -> bool {
        self.entry(id).is_some_and(|e| e.live.is_some())
    }

    /// Enabled module ids, in registration order.
    pub fn enabled_ids(&self) -> Vec<SharedString> {
        self.entries.iter().filter(|e| e.live.is_some()).map(|e| e.meta.id.clone()).collect()
    }

    /// The module's backend is compiled in (or it needs none).
    pub fn backend_available(&self, meta: &ModuleMeta) -> bool {
        meta.backend_feature.as_ref().is_none_or(|f| self.features.contains(f))
    }

    /// Why module `id` cannot be enabled, or `None` if it can (host.ts `unmetRequirement`).
    /// Whether a requirement is *enabled* is not checked: enabling enables it.
    pub fn unmet_requirement(&self, id: &str) -> Option<String> {
        let Some(entry) = self.entry(id) else { return Some(format!("module \"{id}\" not found")) };
        if !self.backend_available(&entry.meta) {
            let feature = entry.meta.backend_feature.clone().unwrap_or_default();
            return Some(format!("backend \"{feature}\" not included in this build"));
        }
        for req in &entry.meta.requires {
            let Some(dep) = self.entry(req) else { return Some(format!("requires missing module \"{req}\"")) };
            if !self.backend_available(&dep.meta) {
                let feature = dep.meta.backend_feature.clone().unwrap_or_default();
                return Some(format!("requires \"{}\" whose backend \"{feature}\" is not in this build", dep.meta.name));
            }
        }
        None
    }

    /// The Modules panel's rows, in registration order (host.ts `listModules`).
    pub fn list(&self) -> Vec<ModuleInfo> {
        self.entries
            .iter()
            .map(|e| {
                let requires = e
                    .meta
                    .requires
                    .iter()
                    .map(|req| {
                        let dep = self.entry(req);
                        RequirementInfo {
                            id: req.clone(),
                            name: dep.map_or_else(|| req.clone(), |d| d.meta.name.clone()),
                            met: dep.is_some_and(|d| self.backend_available(&d.meta)),
                        }
                    })
                    .collect();
                let enabled = e.live.is_some();
                ModuleInfo {
                    id: e.meta.id.clone(),
                    name: e.meta.name.clone(),
                    description: e.meta.description.clone(),
                    enabled,
                    backend_available: self.backend_available(&e.meta),
                    backend_feature: e.meta.backend_feature.clone(),
                    requires,
                    blocked_reason: if enabled { None } else { self.unmet_requirement(&e.meta.id) },
                }
            })
            .collect()
    }

    fn live(&self) -> impl Iterator<Item = (&ModuleMeta, &Contributions)> {
        self.entries.iter().filter_map(|e| e.live.as_ref().map(|l| (&e.meta, &l.contributions)))
    }

    /// Enabled modules' panels at `slot`, in registration order (host.ts `panelsForSlot`).
    pub fn panels(&self, slot: PanelSlot) -> Vec<(SharedString, Panel)> {
        self.live()
            .flat_map(|(meta, c)| c.panels.iter().filter(|p| p.slot == slot).map(|p| (meta.id.clone(), p.clone())))
            .collect()
    }

    /// Enabled modules' actions grouped by module, omitting modules with none.
    pub fn action_groups(&self) -> Vec<ActionGroup> {
        self.live()
            .filter(|(_, c)| !c.actions.is_empty())
            .map(|(meta, c)| ActionGroup {
                module_id: meta.id.clone(),
                module_name: meta.name.clone(),
                actions: c.actions.clone(),
            })
            .collect()
    }

    /// Enabled modules' main views, in registration order (the rail orders them with
    /// `shell::sidebar::rail_order`).
    pub fn main_views(&self) -> Vec<(SharedString, MainView)> {
        self.live().flat_map(|(meta, c)| c.main_views.iter().map(|v| (meta.id.clone(), v.clone()))).collect()
    }

    /// Enabled modules' publish targets, for the Publish dialog.
    pub fn publish_targets(&self) -> Vec<(SharedString, PublishTarget)> {
        self.live().flat_map(|(meta, c)| c.publish_targets.iter().map(|t| (meta.id.clone(), t.clone()))).collect()
    }

    /// One enabled module's settings panels (none when it is disabled).
    pub fn settings_panels(&self, module_id: &str) -> Vec<SettingsPanel> {
        self.entry(module_id).and_then(|e| e.live.as_ref()).map(|l| l.contributions.settings.clone()).unwrap_or_default()
    }

    /// Enabled ids with each module after every module it requires (host.ts
    /// `enabledIdsInDepOrder`); a requirement cycle is broken where it closes.
    pub fn enabled_ids_in_dep_order(&self) -> Vec<SharedString> {
        let enabled: HashSet<SharedString> = self.enabled_ids().into_iter().collect();
        let mut out = Vec::new();
        let mut placed = HashSet::new();
        let mut visiting = HashSet::new();
        fn visit(
            r: &ModuleRegistry,
            id: &SharedString,
            enabled: &HashSet<SharedString>,
            placed: &mut HashSet<SharedString>,
            visiting: &mut HashSet<SharedString>,
            out: &mut Vec<SharedString>,
        ) {
            if placed.contains(id) || !enabled.contains(id) || visiting.contains(id) {
                return;
            }
            visiting.insert(id.clone());
            if let Some(e) = r.entry(id) {
                for req in &e.meta.requires {
                    visit(r, req, enabled, placed, visiting, out);
                }
            }
            visiting.remove(id);
            placed.insert(id.clone());
            out.push(id.clone());
        }
        for id in self.enabled_ids() {
            visit(self, &id, &enabled, &mut placed, &mut visiting, &mut out);
        }
        out
    }

    // --- enable / disable ------------------------------------------------------------

    /// Enable module `id` (and its requirements) and persist the enabled set. A no-op when
    /// it is unknown or already enabled; a refusal says why on the status line.
    ///
    /// Asked for from inside a module callback (a module disabling itself from `on_event`, say)
    /// it is not run there, where the module's instance is borrowed: it is logged and deferred
    /// until the callback has returned.
    pub fn enable(this: &Entity<Self>, id: &str, cx: &mut App) {
        if Self::defer_if_in_callback(this, id, true, cx) {
            return;
        }
        Self::enable_inner(this, id, true, &mut HashSet::new(), cx);
    }

    fn enable_inner(this: &Entity<Self>, id: &str, persist: bool, seen: &mut HashSet<String>, cx: &mut App) {
        let (module, meta, unmet) = {
            let r = this.read(cx);
            let Some(entry) = r.entry(id) else { return };
            if entry.live.is_some() {
                return;
            }
            (entry.module.clone(), entry.meta.clone(), r.unmet_requirement(id))
        };
        if !seen.insert(id.to_string()) {
            return; // a requirement cycle: stop recursing
        }
        if let Some(why) = unmet {
            Self::report(this, format!("Can't enable {}: {why}", meta.name), cx);
            this.update(cx, |_, cx| cx.notify());
            return;
        }
        for req in &meta.requires {
            Self::enable_inner(this, req, false, seen, cx);
            if !this.read(cx).is_enabled(req) {
                let why = this.read(cx).unmet_requirement(req).unwrap_or_else(|| "could not be enabled".into());
                Self::report(this, format!("Can't enable {}: dependency {req} {why}", meta.name), cx);
                this.update(cx, |_, cx| cx.notify());
                return;
            }
        }
        let host = {
            let r = this.read(cx);
            ModuleHost::new(meta.clone(), r.app.clone(), r.model.clone(), r.shell.clone())
        };
        match Self::in_module(this, cx, |cx| module.load(host, cx)) {
            Ok(instance) => {
                let contributions = instance.contributions();
                let live = Live { instance: Rc::new(RefCell::new(instance)), contributions };
                this.update(cx, |r, cx| {
                    if let Some(entry) = r.entry_mut(id) {
                        entry.live = Some(live);
                    }
                    if persist {
                        r.persist(cx);
                    }
                    cx.notify();
                });
            }
            Err(e) => {
                Self::report(this, format!("Can't enable {}: it failed to load: {e}", meta.name), cx);
                // A checkbox the user just ticked must re-render unticked.
                this.update(cx, |_, cx| cx.notify());
            }
        }
    }

    /// Disable module `id`, cascading to every enabled module that requires it, and persist
    /// the enabled set. A no-op when it is unknown or not enabled.
    /// Deferred like [`enable`](Self::enable) when asked for from inside a module callback.
    pub fn disable(this: &Entity<Self>, id: &str, cx: &mut App) {
        if Self::defer_if_in_callback(this, id, false, cx) {
            return;
        }
        Self::disable_inner(this, id, true, cx);
    }

    /// Run `f`, a call into a module, counted in `in_callback`.
    fn in_module<R>(this: &Entity<Self>, cx: &mut App, f: impl FnOnce(&mut App) -> R) -> R {
        this.update(cx, |r, _| r.in_callback += 1);
        let out = f(cx);
        this.update(cx, |r, _| r.in_callback -= 1);
        out
    }

    /// Inside a module callback: log, and run the enable/disable once the callback (and the
    /// effect cycle it is part of) has returned. Returns whether it deferred.
    fn defer_if_in_callback(this: &Entity<Self>, id: &str, enable: bool, cx: &mut App) -> bool {
        if this.read(cx).in_callback == 0 {
            return false;
        }
        let verb = if enable { "enable" } else { "disable" };
        eprintln!("modules: {verb} {id:?} asked for from inside a module callback; deferred until it returns");
        let (this, id) = (this.clone(), id.to_string());
        cx.defer(move |cx| {
            if enable {
                Self::enable(&this, &id, cx)
            } else {
                Self::disable(&this, &id, cx)
            }
        });
        true
    }

    fn disable_inner(this: &Entity<Self>, id: &str, persist: bool, cx: &mut App) {
        let (name, dependents) = {
            let r = this.read(cx);
            let Some(entry) = r.entry(id) else { return };
            if entry.live.is_none() {
                return;
            }
            let dependents: Vec<(SharedString, SharedString)> = r
                .entries
                .iter()
                .filter(|e| e.live.is_some() && e.meta.requires.iter().any(|req| req.as_ref() == id))
                .map(|e| (e.meta.id.clone(), e.meta.name.clone()))
                .collect();
            (entry.meta.name.clone(), dependents)
        };
        for (dep_id, dep_name) in dependents {
            Self::disable_inner(this, &dep_id, false, cx);
            Self::report(this, format!("Disabled {dep_name} (it requires {name})"), cx);
        }
        let live = this.update(cx, |r, _| {
            r.views.retain(|key, _| key.module.as_ref() != id);
            r.entry_mut(id).and_then(|e| e.live.take())
        });
        if let Some(live) = live {
            Self::in_module(this, cx, |cx| live.instance.borrow_mut().on_unload(cx));
        }
        this.update(cx, |r, cx| {
            if persist {
                r.persist(cx);
            }
            cx.notify();
        });
    }

    /// Write the enabled set to the open catalog, off the UI thread. Writes run in the order
    /// they were issued: one that a newer write already overtook is skipped.
    fn persist(&mut self, cx: &mut Context<Self>) {
        self.persist_generation += 1;
        let generation = self.persist_generation;
        let csv = self.enabled_ids_in_dep_order().iter().map(|id| id.as_ref()).collect::<Vec<_>>().join(",");
        let app = self.app.clone();
        let persisted = self.persisted.clone();
        cx.background_executor()
            .spawn(async move {
                let mut newest = persisted.lock().unwrap_or_else(|e| e.into_inner());
                if *newest > generation {
                    return;
                }
                *newest = generation;
                if let Err(e) = with_catalog(&app, |c| c.set_setting(ENABLED_KEY, &csv)) {
                    eprintln!("modules: could not save the enabled modules: {e}");
                }
            })
            .detach();
    }

    /// Enable what `modules.enabled` lists, once per app run (host.ts `initHost`). Off the UI
    /// thread for the read; the enables themselves do not re-persist.
    fn restore(this: &Entity<Self>, cx: &mut App) {
        if this.read(cx).restored {
            return;
        }
        this.update(cx, |r, _| r.restored = true);
        let app = this.read(cx).app.clone();
        let read = cx.background_executor().spawn(async move { with_catalog(&app, |c| c.get_setting(ENABLED_KEY)) });
        let weak = this.downgrade();
        cx.spawn(async move |cx| {
            let csv = read.await;
            cx.update(|cx| {
                let Some(this) = weak.upgrade() else { return };
                match csv {
                    Ok(csv) => {
                        for id in csv.unwrap_or_default().split(',').filter(|id| !id.is_empty()) {
                            Self::enable_inner(&this, id, false, &mut HashSet::new(), cx);
                        }
                    }
                    Err(e) => eprintln!("modules: could not read the enabled modules: {e}"),
                }
                this.update(cx, |_, cx| cx.notify());
            });
        })
        .detach();
    }

    /// Hand `event` to every enabled module, in registration order. A module disabled by an
    /// earlier module's handler does not get it.
    fn deliver(this: &Entity<Self>, event: &CoreEvent, cx: &mut App) {
        for id in this.read(cx).enabled_ids() {
            let instance = this.read(cx).entry(&id).and_then(|e| e.live.as_ref()).map(|l| l.instance.clone());
            if let Some(instance) = instance {
                Self::in_module(this, cx, |cx| instance.borrow_mut().on_event(event, cx));
            }
        }
    }

    fn report(this: &Entity<Self>, line: String, cx: &mut App) {
        eprintln!("modules: {line}");
        let model = this.read(cx).model.clone();
        model.update(cx, |m, cx| {
            m.status = line.into();
            cx.notify();
        });
    }

    // --- views -----------------------------------------------------------------------

    /// How many contributed views are cached, over all windows.
    #[cfg(test)]
    pub(crate) fn cached_view_count(&self) -> usize {
        self.views.len()
    }

    /// The view for one contribution in `window`, built on first use and kept until its
    /// module is disabled.
    fn cached_view(
        this: &Entity<Self>,
        module: &SharedString,
        kind: &'static str,
        id: &SharedString,
        factory: &ViewFactory,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyView {
        let key = ViewKey { window: window.window_handle().window_id(), module: module.clone(), kind, id: id.clone() };
        if let Some(view) = this.read(cx).views.get(&key) {
            return view.clone();
        }
        let view = factory(window, cx);
        this.update(cx, |r, _| r.views.insert(key, view.clone()));
        view
    }

    /// The views of the enabled modules' panels at `slot`, in registration order.
    pub fn panel_views(this: &Entity<Self>, slot: PanelSlot, window: &mut Window, cx: &mut App) -> Vec<SlotView> {
        let panels = this.read(cx).panels(slot);
        panels
            .into_iter()
            .map(|(module_id, p)| {
                let view = Self::cached_view(this, &module_id, slot.name(), &p.id, &p.view, window, cx);
                SlotView { module_id, id: p.id, label: p.label, view }
            })
            .collect()
    }

    /// The view of main view `view_id`, if an enabled module contributes it.
    pub fn main_view(this: &Entity<Self>, view_id: &str, window: &mut Window, cx: &mut App) -> Option<SlotView> {
        let (module_id, v) = this.read(cx).main_views().into_iter().find(|(_, v)| v.id.as_ref() == view_id)?;
        let view = Self::cached_view(this, &module_id, "main-view", &v.id, &v.view, window, cx);
        Some(SlotView { module_id, id: v.id, label: v.label, view })
    }

    /// The views of one enabled module's settings panels.
    pub fn settings_views(this: &Entity<Self>, module_id: &SharedString, window: &mut Window, cx: &mut App) -> Vec<AnyView> {
        let panels = this.read(cx).settings_panels(module_id);
        panels.iter().map(|p| Self::cached_view(this, module_id, "settings", &p.id, &p.view, window, cx)).collect()
    }

    /// Fresh views of every enabled publish target, for a Publish dialog that is opening.
    pub fn publish_target_views(this: &Entity<Self>, window: &mut Window, cx: &mut App) -> Vec<SlotView> {
        let targets = this.read(cx).publish_targets();
        targets
            .into_iter()
            .map(|(module_id, t)| SlotView { module_id, id: t.id, label: t.label, view: (t.view)(window, cx) })
            .collect()
    }

    /// Run module `module_id`'s action `action_id` (host.ts `activateToolbarAction`): call it,
    /// or open its dialog. A no-op when the module is not enabled.
    pub fn activate(this: &Entity<Self>, module_id: &str, action_id: &str, window: &mut Window, cx: &mut App) {
        let action = this
            .read(cx)
            .action_groups()
            .into_iter()
            .filter(|g| g.module_id.as_ref() == module_id)
            .flat_map(|g| g.actions)
            .find(|a| a.id.as_ref() == action_id);
        let Some(action) = action else { return };
        match action.kind {
            ActionKind::Run(run) => run(window, cx),
            ActionKind::Modal(factory) => {
                let view = factory(window, cx);
                let label = action.label.clone();
                window.open_dialog(cx, move |dialog, _, _| dialog.title(label.clone()).child(view.clone()));
            }
        }
    }
}
