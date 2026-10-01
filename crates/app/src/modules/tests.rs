//! Headless tests of the module registry: `requires` ordering and refusals, enable/disable
//! persistence in `modules.enabled`, event delivery only to enabled modules, `on_unload`,
//! namespaced settings — against probe modules that log what the registry does to them —
//! and the shell's slots, driven through the real window with the dev module.
//!
//! The registry logic tests translate the `host.test.ts` cases that survive the Rust contract
//! (unmetRequirement, enableModule, disableModule's cascade, toolbarActionGroups); the
//! semver, host-version, permission, legacy-channel and throwing-callback cases have no
//! counterpart (#104 dropped those concepts; module callbacks are infallible Rust, a failed
//! load is `Err`).

use super::dev_module::DEV_MODULE_ID;
use super::registry::{validate_id, ModuleRegistry, ENABLED_KEY};
use super::*;
use crate::model::AppModel;
use crate::shell::actions::{OpenModules, PublishSelection};
use crate::shell::state::{InspectorTab, Surface};
use crate::shell::ShellState;
use crate::{start_core, wire, WireOptions, Wired};
use chairphoto_core::app::{AppState, CoreEvent, EventSink as _};
use chairphoto_core::appearance::SystemThemeResult;
use chairphoto_core::catalog::Catalog;
use chairphoto_core::scanner::ScanProgress;
use gpui_kit::component::WindowExt as _;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Empty, TestAppContext};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

/// A private directory under the temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("cp-mod-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

type Log = Rc<RefCell<Vec<String>>>;

/// A module that logs `load:<id>`, `event:<id>:<name>` and `unload:<id>`, and contributes
/// one inspector panel `<id>-panel` and one action `<id>-action`.
struct Probe {
    meta: ModuleMeta,
    log: Log,
    fail_load: bool,
}

struct ProbeInstance {
    id: SharedString,
    log: Log,
}

impl Module for Probe {
    fn meta(&self) -> ModuleMeta {
        self.meta.clone()
    }

    fn load(&self, host: ModuleHost, _: &mut App) -> Result<Box<dyn ModuleInstance>, String> {
        if self.fail_load {
            return Err("no model file".into());
        }
        self.log.borrow_mut().push(format!("load:{}", host.meta().id));
        Ok(Box::new(ProbeInstance { id: host.meta().id.clone(), log: self.log.clone() }))
    }
}

impl ModuleInstance for ProbeInstance {
    fn contributions(&self) -> Contributions {
        Contributions {
            panels: vec![Panel {
                id: format!("{}-panel", self.id).into(),
                label: "Probe".into(),
                slot: PanelSlot::Inspector,
                view: Rc::new(|_, cx| cx.new(|_| Empty).into()),
            }],
            actions: vec![ModuleAction {
                id: format!("{}-action", self.id).into(),
                label: "Probe action".into(),
                kind: ActionKind::Run(Rc::new(|_, _| {})),
            }],
            ..Default::default()
        }
    }

    fn on_event(&mut self, event: &CoreEvent, _: &mut App) {
        self.log.borrow_mut().push(format!("event:{}:{}", self.id, event.name()));
    }

    fn on_unload(&mut self, _: &mut App) {
        self.log.borrow_mut().push(format!("unload:{}", self.id));
    }
}

fn probe(meta: ModuleMeta, log: &Log) -> Rc<dyn Module> {
    Rc::new(Probe { meta, log: log.clone(), fail_load: false })
}

/// A registry over `modules` with `features` compiled in, its model and the core state,
/// with a fresh catalog open in `dir`.
struct Bench {
    state: AppState,
    model: Entity<AppModel>,
    registry: Entity<ModuleRegistry>,
}

fn bench(modules: Vec<Rc<dyn Module>>, features: &[&str], dir: &TempDir, cx: &mut TestAppContext) -> Bench {
    let state = AppState::default();
    let catalog = Catalog::open(&dir.0.join("m.chairphoto"), &dir.0.join("photos")).unwrap();
    *state.catalog.lock().unwrap() = Some(catalog);
    let model = cx.new(|_| AppModel::new(state.clone(), None));
    let shell = cx.new(|cx| ShellState::new(&model, cx));
    let features = features.iter().map(|f| SharedString::from(*f)).collect();
    let registry = cx.update(|cx| ModuleRegistry::install_with(modules, features, &model, &shell, cx));
    Bench { state, model, registry }
}

impl Bench {
    fn enable(&self, id: &str, cx: &mut TestAppContext) {
        cx.update(|cx| ModuleRegistry::enable(&self.registry, id, cx));
        cx.run_until_parked();
    }

    fn disable(&self, id: &str, cx: &mut TestAppContext) {
        cx.update(|cx| ModuleRegistry::disable(&self.registry, id, cx));
        cx.run_until_parked();
    }

    fn enabled(&self, cx: &mut TestAppContext) -> Vec<String> {
        self.registry.read_with(cx, |r, _| r.enabled_ids().iter().map(|s| s.to_string()).collect())
    }

    fn setting(&self, key: &str) -> Option<String> {
        self.state.catalog.lock().unwrap().as_ref().unwrap().get_setting(key).unwrap()
    }

    fn status(&self, cx: &mut TestAppContext) -> String {
        self.model.read_with(cx, |m, _| m.status.to_string())
    }

    fn event(&self, event: CoreEvent, cx: &mut TestAppContext) {
        self.model.update(cx, |m, cx| m.on_core_event(&event, cx));
        cx.run_until_parked();
    }
}

fn scan_done() -> CoreEvent {
    CoreEvent::ScanProgress(ScanProgress { phase: "indexing".into(), done: 1, total: 2 })
}

// --- ids ---------------------------------------------------------------------------------

#[test]
fn module_ids_are_settings_namespaces() {
    assert!(validate_id("faces").is_ok());
    assert!(validate_id("tag-graph").is_ok());
    for bad in ["", "a.b", "a,b", "a b", "modules"] {
        assert!(validate_id(bad).is_err(), "{bad:?}");
    }
    for m in bundled() {
        assert_eq!(validate_id(&m.meta().id), Ok(()), "a bundled module's id");
    }
}

#[gpui_kit::test]
fn invalid_and_duplicate_ids_are_not_registered(cx: &mut TestAppContext) {
    let dir = TempDir::new("ids");
    let log = Log::default();
    let b = bench(
        vec![
            probe(ModuleMeta::new("a", "A"), &log),
            probe(ModuleMeta::new("a", "Second A"), &log),
            probe(ModuleMeta::new("x.y", "Dotted"), &log),
            probe(ModuleMeta::new("modules", "Host"), &log),
        ],
        &[],
        &dir,
        cx,
    );
    let names: Vec<String> = b.registry.read_with(cx, |r, _| r.list().iter().map(|m| m.name.to_string()).collect());
    assert_eq!(names, ["A"]);
}

// --- requires ----------------------------------------------------------------------------

/// Enabling a module enables what it requires first (load order: dependencies before
/// dependents), and persists the enabled set in dependency order.
#[gpui_kit::test]
fn enabling_loads_requirements_first_and_persists_in_dependency_order(cx: &mut TestAppContext) {
    let dir = TempDir::new("order");
    let log = Log::default();
    let b = bench(
        vec![
            probe(ModuleMeta::new("c", "C").requires("b"), &log),
            probe(ModuleMeta::new("b", "B").requires("a"), &log),
            probe(ModuleMeta::new("a", "A"), &log),
        ],
        &[],
        &dir,
        cx,
    );
    b.enable("c", cx);
    assert_eq!(*log.borrow(), ["load:a", "load:b", "load:c"]);
    assert_eq!(b.enabled(cx), ["c", "b", "a"], "registration order");
    assert_eq!(b.setting(ENABLED_KEY).as_deref(), Some("a,b,c"), "persisted in dependency order");

    b.enable("c", cx);
    assert_eq!(log.borrow().len(), 3, "enabling an enabled module loads nothing");
}

/// A module whose requirement is missing, or whose own or a requirement's backend is
/// compiled out, is refused with the reason; nothing loads.
#[gpui_kit::test]
fn unmet_requirements_refuse_with_the_reason(cx: &mut TestAppContext) {
    let dir = TempDir::new("unmet");
    let log = Log::default();
    let b = bench(
        vec![
            probe(ModuleMeta::new("orphan", "Orphan").requires("ghost"), &log),
            probe(ModuleMeta::new("needs-x", "Needs X").backend_feature("x"), &log),
            probe(ModuleMeta::new("needs-y", "Needs Y").backend_feature("y"), &log),
            probe(ModuleMeta::new("snap", "Snap").requires("needs-x"), &log),
        ],
        &["y"],
        &dir,
        cx,
    );
    let list = b.registry.read_with(cx, |r, _| r.list());
    let reason = |id: &str| list.iter().find(|m| m.id.as_ref() == id).unwrap().blocked_reason.clone();
    assert_eq!(reason("orphan").as_deref(), Some("requires missing module \"ghost\""));
    assert_eq!(reason("needs-x").as_deref(), Some("backend \"x\" not included in this build"));
    assert_eq!(reason("needs-y"), None);
    assert_eq!(reason("snap").as_deref(), Some("requires \"Needs X\" whose backend \"x\" is not in this build"));
    let snap = list.iter().find(|m| m.id.as_ref() == "snap").unwrap();
    assert_eq!((snap.requires[0].name.as_ref(), snap.requires[0].met), ("Needs X", false));
    assert!(!list.iter().find(|m| m.id.as_ref() == "needs-x").unwrap().backend_available);

    for id in ["orphan", "needs-x", "snap"] {
        b.enable(id, cx);
    }
    assert!(log.borrow().is_empty(), "nothing loaded: {:?}", log.borrow());
    assert_eq!(b.enabled(cx), Vec::<String>::new());
    assert_eq!(b.status(cx), "Can't enable Snap: requires \"Needs X\" whose backend \"x\" is not in this build");
    assert_eq!(b.setting(ENABLED_KEY), None, "a refusal writes nothing");
}

/// A failed load leaves the module disabled and says why; a module requiring it is refused.
#[gpui_kit::test]
fn a_failed_load_leaves_it_and_its_dependents_disabled(cx: &mut TestAppContext) {
    let dir = TempDir::new("fail");
    let log = Log::default();
    let b = bench(
        vec![
            Rc::new(Probe { meta: ModuleMeta::new("broken", "Broken"), log: log.clone(), fail_load: true }),
            probe(ModuleMeta::new("user", "User").requires("broken"), &log),
        ],
        &[],
        &dir,
        cx,
    );
    b.enable("broken", cx);
    assert_eq!(b.status(cx), "Can't enable Broken: it failed to load: no model file");
    b.enable("user", cx);
    assert_eq!(b.status(cx), "Can't enable User: dependency broken could not be enabled");
    assert!(log.borrow().is_empty());
    assert_eq!(b.enabled(cx), Vec::<String>::new());
    assert!(b.registry.read_with(cx, |r, _| r.panels(PanelSlot::Inspector).is_empty()));
}

/// A requirement cycle ends instead of recursing forever, and enables neither.
#[gpui_kit::test]
fn a_requirement_cycle_terminates(cx: &mut TestAppContext) {
    let dir = TempDir::new("cycle");
    let log = Log::default();
    let b = bench(
        vec![probe(ModuleMeta::new("p", "P").requires("q"), &log), probe(ModuleMeta::new("q", "Q").requires("p"), &log)],
        &[],
        &dir,
        cx,
    );
    b.enable("p", cx);
    assert!(log.borrow().is_empty());
    assert_eq!(b.enabled(cx), Vec::<String>::new());
}

// --- disable, unload, persistence ----------------------------------------------------------

/// Disabling a module first disables (and unloads) every enabled module that requires it,
/// drops their contributions, and persists what is left.
#[gpui_kit::test]
fn disabling_cascades_to_dependents_and_unloads(cx: &mut TestAppContext) {
    let dir = TempDir::new("cascade");
    let log = Log::default();
    let b = bench(
        vec![
            probe(ModuleMeta::new("localsend", "LocalSend"), &log),
            probe(ModuleMeta::new("snapchat", "Snapchat").requires("localsend"), &log),
            probe(ModuleMeta::new("other", "Other"), &log),
        ],
        &[],
        &dir,
        cx,
    );
    b.enable("snapchat", cx);
    b.enable("other", cx);
    assert_eq!(b.registry.read_with(cx, |r, _| r.panels(PanelSlot::Inspector).len()), 3);
    log.borrow_mut().clear();

    b.disable("localsend", cx);
    assert_eq!(*log.borrow(), ["unload:snapchat", "unload:localsend"]);
    assert_eq!(b.enabled(cx), ["other"]);
    assert_eq!(b.status(cx), "Disabled Snapchat (it requires LocalSend)");
    b.registry.read_with(cx, |r, _| {
        let panels: Vec<String> = r.panels(PanelSlot::Inspector).iter().map(|(_, p)| p.id.to_string()).collect();
        assert_eq!(panels, ["other-panel"], "the disabled modules' panels are gone");
        let groups: Vec<String> = r.action_groups().iter().map(|g| g.module_id.to_string()).collect();
        assert_eq!(groups, ["other"]);
    });
    assert_eq!(b.setting(ENABLED_KEY).as_deref(), Some("other"));

    b.disable("other", cx);
    assert_eq!(b.setting(ENABLED_KEY).as_deref(), Some(""), "the empty set is persisted too");
}

/// The first catalog read restores `modules.enabled` (without rewriting it), once: a later
/// read does not re-enable a module the user turned off since.
#[gpui_kit::test]
fn the_first_catalog_read_restores_the_enabled_set_once(cx: &mut TestAppContext) {
    let dir = TempDir::new("restore");
    let log = Log::default();
    let b = bench(
        vec![
            probe(ModuleMeta::new("b", "B").requires("a"), &log),
            probe(ModuleMeta::new("a", "A"), &log),
            probe(ModuleMeta::new("c", "C"), &log),
        ],
        &[],
        &dir,
        cx,
    );
    b.state.catalog.lock().unwrap().as_ref().unwrap().set_setting(ENABLED_KEY, "a,b,gone").unwrap();
    b.model.update(cx, |m, cx| m.refresh(cx));
    cx.run_until_parked();
    assert_eq!(*log.borrow(), ["load:a", "load:b"], "unknown ids are skipped");
    assert_eq!(b.setting(ENABLED_KEY).as_deref(), Some("a,b,gone"), "restoring does not rewrite the set");

    b.disable("b", cx);
    assert_eq!(b.setting(ENABLED_KEY).as_deref(), Some("a"));
    // Whatever the catalog says by then, a later read does not apply it.
    b.state.catalog.lock().unwrap().as_ref().unwrap().set_setting(ENABLED_KEY, "a,b").unwrap();
    b.model.update(cx, |m, cx| m.refresh(cx));
    cx.run_until_parked();
    assert_eq!(b.enabled(cx), ["a"], "a second catalog read does not restore again");
}

/// Toggles in quick succession land in the order they were made.
#[gpui_kit::test]
fn the_newest_toggle_is_what_is_persisted(cx: &mut TestAppContext) {
    let dir = TempDir::new("race");
    let log = Log::default();
    let b = bench(vec![probe(ModuleMeta::new("a", "A"), &log), probe(ModuleMeta::new("c", "C"), &log)], &[], &dir, cx);
    cx.update(|cx| {
        ModuleRegistry::enable(&b.registry, "a", cx);
        ModuleRegistry::enable(&b.registry, "c", cx);
        ModuleRegistry::disable(&b.registry, "a", cx);
    });
    cx.run_until_parked();
    assert_eq!(b.setting(ENABLED_KEY).as_deref(), Some("c"));
}

/// A module's settings are `<id>.<key>` in the catalog.
#[gpui_kit::test]
fn module_settings_are_namespaced_by_id(cx: &mut TestAppContext) {
    let dir = TempDir::new("settings");
    let b = bench(Vec::new(), &[], &dir, cx);
    let host = cx.update(|cx| {
        let shell = cx.new(|cx| ShellState::new(&b.model, cx));
        ModuleHost::new(ModuleMeta::new("ai", "AI"), b.state.clone(), b.model.clone(), shell)
    });
    host.settings().set("provider", "ollama").unwrap();
    assert_eq!(b.setting("ai.provider").as_deref(), Some("ollama"));
    assert_eq!(host.settings().get("provider").unwrap().as_deref(), Some("ollama"));
    assert_eq!(host.settings().get(&ENABLED_KEY["modules.".len()..]).unwrap(), None, "the host's own keys are out of reach");
    assert_eq!(ModuleMeta::new("ai", "AI").marker(), "ai");
    assert_eq!(ModuleMeta::new("snap", "Snap").publication_marker("snapchat").marker(), "snapchat");
}

// --- events ------------------------------------------------------------------------------

/// Core events reach enabled modules only, in registration order, through the real event
/// router (a worker thread's `send` → the model → the registry).
#[gpui_kit::test]
fn core_events_reach_only_enabled_modules(cx: &mut TestAppContext) {
    let dir = TempDir::new("events");
    let log = Log::default();
    let (state, rx, ()) = start_core(|_| ());
    *state.catalog.lock().unwrap() =
        Some(Catalog::open(&dir.0.join("e.chairphoto"), &dir.0.join("photos")).unwrap());
    let model = cx.new(|_| AppModel::new(state.clone(), None));
    let shell = cx.new(|cx| ShellState::new(&model, cx));
    let registry = cx.update(|cx| {
        crate::events::spawn_router(rx, model.clone(), cx).detach();
        ModuleRegistry::install_with(
            vec![probe(ModuleMeta::new("b", "B"), &log), probe(ModuleMeta::new("a", "A"), &log), probe(ModuleMeta::new("off", "Off"), &log)],
            Vec::new(),
            &model,
            &shell,
            cx,
        )
    });
    cx.update(|cx| {
        ModuleRegistry::enable(&registry, "a", cx);
        ModuleRegistry::enable(&registry, "b", cx);
    });
    log.borrow_mut().clear();

    let sender = state.clone();
    std::thread::spawn(move || sender.send(scan_done())).join().unwrap();
    cx.run_until_parked();
    assert_eq!(*log.borrow(), ["event:b:scan:progress", "event:a:scan:progress"]);

    log.borrow_mut().clear();
    cx.update(|cx| ModuleRegistry::disable(&registry, "b", cx));
    state.send(CoreEvent::CatalogSwitched("x".into()));
    cx.run_until_parked();
    assert_eq!(*log.borrow(), ["unload:b", "event:a:catalog:switched"]);
}

/// The bench's own helper path (model → registry) delivers too; used by the slot tests.
#[gpui_kit::test]
fn a_disabled_module_gets_no_events(cx: &mut TestAppContext) {
    let dir = TempDir::new("noevents");
    let log = Log::default();
    let b = bench(vec![probe(ModuleMeta::new("a", "A"), &log)], &[], &dir, cx);
    b.event(scan_done(), cx);
    assert!(log.borrow().is_empty());
    b.enable("a", cx);
    b.event(scan_done(), cx);
    b.disable("a", cx);
    b.event(scan_done(), cx);
    assert_eq!(*log.borrow(), ["load:a", "event:a:scan:progress", "unload:a"]);
}

// --- the shell's slots, with the dev module -------------------------------------------------

struct Shell {
    state: AppState,
    wired: Wired,
}

impl Shell {
    fn window(&self) -> AnyWindowHandle {
        *self.wired.main_window.as_ref().unwrap()
    }

    fn present(&self, id: &'static str, cx: &mut TestAppContext) -> bool {
        cx.update_window(self.window(), |_, window, cx| {
            window.render_frame(cx);
            window.try_find(id).is_some()
        })
        .unwrap()
    }

    fn click(&self, id: &'static str, cx: &mut TestAppContext) {
        cx.update_window(self.window(), |_, window, cx| {
            window.render_frame(cx);
            window.click(id, cx);
        })
        .unwrap();
        cx.run_until_parked();
    }

    fn dispatch(&self, action: Box<dyn gpui_kit::Action>, cx: &mut TestAppContext) {
        cx.update_window(self.window(), |_, window, cx| window.dispatch_action(action, cx)).unwrap();
        cx.run_until_parked();
    }

    fn surface(&self, cx: &mut TestAppContext) -> Surface {
        self.wired.shell.read_with(cx, |s, _| s.surface.clone())
    }
}

/// `run`'s wiring with a fresh catalog holding one photo, opened as `catalog:switched` does.
fn shell(dir: &TempDir, cx: &mut TestAppContext) -> (Shell, i64) {
    let (state, events_rx, ()) = start_core(|_| ());
    let wired = cx.update(|cx| {
        wire(
            cx,
            state.clone(),
            events_rx,
            None,
            &SystemThemeResult::unavailable(),
            WireOptions { on_exit: Rc::new(|| {}), open_default_catalog: false },
        )
    });
    let db = dir.0.join("s.chairphoto");
    let root = dir.0.join("photos");
    let catalog = Catalog::open(&db, &root).unwrap();
    let id = catalog.upsert_photo(&root.join("2026/p.ARW"), None, 0, 1).unwrap().id;
    *state.catalog.lock().unwrap() = Some(catalog);
    state.send(CoreEvent::CatalogSwitched(db.to_string_lossy().to_string()));
    cx.run_until_parked();
    (Shell { state, wired }, id)
}

/// The Modules panel lists the dev module; ticking it puts the module's contributions in
/// every slot the shell has (rail, stage, sidebar, inspector, settings), and unticking it takes
/// them all away again — the stage falling back to the Library.
#[gpui_kit::test]
fn the_modules_panel_toggles_every_slot(cx: &mut TestAppContext) {
    let dir = TempDir::new("slots");
    let (app, photo_id) = shell(&dir, cx);
    let photo = app.state.catalog.lock().unwrap().as_ref().unwrap().get_photo(photo_id).unwrap();
    app.wired.shell.update(cx, |s, cx| {
        s.library.view_photo(photo);
        s.set_inspector_tab(InspectorTab::Tags, cx);
    });
    for id in ["rail-view-dev-view", "dev-sidebar", "dev-inspector"] {
        assert!(!app.present(id, cx), "{id} before the module is enabled");
    }

    app.dispatch(Box::new(OpenModules), cx);
    settle_dialog(cx);
    assert!(app.present("module-row-dev", cx), "the Modules panel lists the dev module");
    assert!(!app.present("dev-settings", cx), "no settings panel while disabled");
    app.click("module-toggle-dev", cx);
    assert!(app.wired.modules.read_with(cx, |r, _| r.is_enabled(DEV_MODULE_ID)));
    assert_eq!(
        app.state.catalog.lock().unwrap().as_ref().unwrap().get_setting(ENABLED_KEY).unwrap().as_deref(),
        Some(DEV_MODULE_ID),
        "the toggle persisted"
    );
    for id in ["dev-settings", "rail-view-dev-view", "dev-sidebar", "module-panel-sidebar-dev-sidebar", "dev-inspector"] {
        assert!(app.present(id, cx), "{id} once the module is enabled");
    }

    // The modal dialog covers the window: close it to reach the rail.
    cx.update_window(app.window(), |_, window, cx| window.close_dialog(cx)).unwrap();
    cx.run_until_parked();
    app.click("rail-view-dev-view", cx);
    assert_eq!(app.surface(cx), Surface::Module("dev-view".into()));
    assert!(app.present("dev-main", cx), "the main view fills the stage");

    app.dispatch(Box::new(OpenModules), cx);
    settle_dialog(cx);
    app.click("module-toggle-dev", cx);
    assert!(!app.wired.modules.read_with(cx, |r, _| r.is_enabled(DEV_MODULE_ID)));
    assert_eq!(app.surface(cx), Surface::Library, "the stage fell back to the Library");
    for id in ["dev-settings", "rail-view-dev-view", "dev-sidebar", "dev-inspector", "dev-main"] {
        assert!(!app.present(id, cx), "{id} after the module is disabled");
    }
}

/// Inspector panels show on the tags tab only, with a photo active (`PhotoInspector.tsx`).
#[gpui_kit::test]
fn inspector_panels_live_on_the_tags_tab(cx: &mut TestAppContext) {
    let dir = TempDir::new("inspector");
    let (app, photo_id) = shell(&dir, cx);
    cx.update(|cx| ModuleRegistry::enable(&app.wired.modules, DEV_MODULE_ID, cx));
    app.wired.shell.update(cx, |s, cx| s.set_inspector_tab(InspectorTab::Tags, cx));
    assert!(!app.present("dev-inspector", cx), "no photo, no inspector panels");
    let photo = app.state.catalog.lock().unwrap().as_ref().unwrap().get_photo(photo_id).unwrap();
    app.wired.shell.update(cx, |s, _| s.library.view_photo(photo));
    assert!(app.present("dev-inspector", cx));
    app.wired.shell.update(cx, |s, cx| s.set_inspector_tab(InspectorTab::Details, cx));
    assert!(!app.present("dev-inspector", cx), "details tab");
}

/// Actions: a run action runs; a modal action opens its view in a dialog; the More ⋯ menu
/// gains a "Modules" submenu only while some module has actions. Publish targets fill the
/// Publish dialog.
#[gpui_kit::test]
fn actions_and_publish_targets_open_where_they_belong(cx: &mut TestAppContext) {
    let dir = TempDir::new("actions");
    let (app, _) = shell(&dir, cx);
    let more_row = |app: &Shell, index: usize, cx: &mut TestAppContext| -> Option<String> {
        app.click("more-menu", cx);
        let label = cx
            .update_window(app.window(), |_, window, cx| {
                window.render_frame(cx);
                window.within("popup-menu").find(index).label().map(str::to_string)
            })
            .unwrap();
        app.press_escape(cx);
        label
    };
    assert_ne!(more_row(&app, 8, cx).as_deref(), Some("Modules"));

    cx.update(|cx| ModuleRegistry::enable(&app.wired.modules, DEV_MODULE_ID, cx));
    cx.run_until_parked();
    // Row 7 is the separator under "Start cull session".
    assert_eq!(more_row(&app, 8, cx).as_deref(), Some("Modules"), "after Start cull session's separator");
    let groups = app.wired.modules.read_with(cx, |r, _| r.action_groups());
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].module_name.as_ref(), "Dev module");

    cx.update_window(app.window(), |_, window, cx| {
        ModuleRegistry::activate(&app.wired.modules, DEV_MODULE_ID, "dev-run", window, cx)
    })
    .unwrap();
    assert_eq!(app.wired.model.read_with(cx, |m, _| m.status.to_string()), "Dev module: action ran");

    assert!(!app.present("dev-modal", cx));
    cx.update_window(app.window(), |_, window, cx| {
        ModuleRegistry::activate(&app.wired.modules, DEV_MODULE_ID, "dev-modal", window, cx)
    })
    .unwrap();
    cx.run_until_parked();
    assert!(app.present("dev-modal", cx), "the modal action's view is in a dialog");
    cx.update_window(app.window(), |_, window, cx| window.close_dialog(cx)).unwrap();
    cx.run_until_parked();

    app.dispatch(Box::new(PublishSelection), cx);
    settle_dialog(cx);
    assert!(app.present("publish-target-dev-target", cx));
    assert!(app.present("dev-publish", cx), "the target's form");
}

/// The loupe and tag-editor slots, which their views (#109, #107) mount: one cached view per
/// window while the module stays enabled, none after.
#[gpui_kit::test]
fn loupe_and_tag_editor_panels_are_built_once_per_window(cx: &mut TestAppContext) {
    let dir = TempDir::new("loupe");
    let (app, _) = shell(&dir, cx);
    let registry = app.wired.modules.clone();
    cx.update(|cx| ModuleRegistry::enable(&registry, DEV_MODULE_ID, cx));
    let views = |slot: PanelSlot, cx: &mut TestAppContext| {
        cx.update_window(app.window(), |_, window, cx| ModuleRegistry::panel_views(&registry, slot, window, cx))
            .unwrap()
    };
    for (slot, id) in [(PanelSlot::Loupe, "dev-loupe"), (PanelSlot::TagEditor, "dev-tag-editor")] {
        let first = views(slot, cx);
        assert_eq!(first.iter().map(|v| v.id.to_string()).collect::<Vec<_>>(), [id]);
        let again = views(slot, cx);
        assert_eq!(first[0].view.entity_id(), again[0].view.entity_id(), "{id}: cached");
    }
    cx.update(|cx| ModuleRegistry::disable(&registry, DEV_MODULE_ID, cx));
    assert!(views(PanelSlot::Loupe, cx).is_empty());
}

/// Let a dialog's entrance animation finish, so clicks land on its content where it rests:
/// gpui-component animates dialogs on the wall clock for `dialog::ANIMATION_DURATION` (250 ms
/// in 0.7.0), which the test scheduler does not advance; until then a click on the panel's
/// checkbox reached nothing.
fn settle_dialog(cx: &mut TestAppContext) {
    std::thread::sleep(std::time::Duration::from_millis(450));
    cx.run_until_parked();
}

impl Shell {
    fn press_escape(&self, cx: &mut TestAppContext) {
        cx.update_window(self.window(), |_, window, cx| window.press("escape", cx)).unwrap();
        cx.run_until_parked();
    }
}
