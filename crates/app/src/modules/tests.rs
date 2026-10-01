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
    let b = bench_unrestored(modules, features, dir, cx);
    b.model.update(cx, |m, cx| m.refresh(cx));
    cx.run_until_parked();
    b
}

/// [`bench`] before the model's first catalog read: the saved set is not restored yet.
fn bench_unrestored(modules: Vec<Rc<dyn Module>>, features: &[&str], dir: &TempDir, cx: &mut TestAppContext) -> Bench {
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
    for bad in ["", "a.b", "a,b", "a b", "modules", "indexing", "sharpness", "editor", "develop", "basic-editor"] {
        assert!(validate_id(bad).is_err(), "{bad:?}");
    }
    for shared in super::registry::BACKEND_NAMESPACES {
        assert!(validate_id(shared).is_ok(), "{shared} is its module's, shared with its backend");
    }
    for m in bundled() {
        assert_eq!(validate_id(&m.meta().id), Ok(()), "a bundled module's id");
    }
}

/// Every `"<prefix>.<key>"` settings key the host uses — constants and literal keys passed to
/// `get_setting`/`set_setting` in the core, the model, the Tauri shell and this crate — is in a
/// reserved namespace or a module's backend namespace, so no module id can reach it.
#[test]
fn every_host_settings_prefix_is_reserved() {
    use super::registry::{BACKEND_NAMESPACES, RESERVED_NAMESPACES};
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    fn walk(dir: &std::path::Path, files: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                if !path.ends_with("vendor") {
                    walk(&path, files);
                }
            } else if path.extension().is_some_and(|e| e == "rs") {
                files.push(path);
            }
        }
    }
    for dir in ["crates/core/src", "crates/model/src", "crates/app/src", "src-tauri/src"] {
        walk(&root.join(dir), &mut files);
    }
    // `const X_KEY: &str = "p.k"` / `X_SETTING` / `SETTING_X`, and `get_setting("p.k"`.
    let mut prefixes = std::collections::BTreeSet::new();
    for file in &files {
        if file.ends_with("modules/tests.rs") {
            continue;
        }
        let text = std::fs::read_to_string(file).unwrap();
        for line in text.lines() {
            let line = line.trim();
            let named = line.starts_with("pub const ") || line.starts_with("const ");
            let is_setting_const = named && (line.contains("KEY") || line.contains("SETTING")) && line.contains(": &str = \"");
            let literal = ["get_setting(\"", "set_setting(\"", "get_setting(&format!(\""]
                .iter()
                .find_map(|pat| line.find(pat).map(|i| i + pat.len()));
            let start = if is_setting_const { line.find(": &str = \"").map(|i| i + ": &str = \"".len()) } else { literal };
            let Some(start) = start else { continue };
            let key: String = line[start..].chars().take_while(|c| *c != '"').collect();
            if let Some((prefix, _)) = key.split_once('.') {
                prefixes.insert((prefix.to_string(), key.clone(), file.display().to_string()));
            }
        }
    }
    assert!(prefixes.iter().any(|(p, ..)| p == "indexing"), "the scan finds indexing.speed: {prefixes:?}");
    for (prefix, key, file) in &prefixes {
        assert!(
            RESERVED_NAMESPACES.contains(&prefix.as_str()) || BACKEND_NAMESPACES.contains(&prefix.as_str()),
            "{key} ({file}): namespace {prefix:?} is neither reserved nor a backend's"
        );
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
            probe(ModuleMeta::new("indexing", "Indexing"), &log),
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
    let b = bench_unrestored(
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

/// A toggle made before the saved set is restored — before the first catalog read, or while
/// the restore's read runs — waits for it and lands on top of it: the saved set survives.
#[gpui_kit::test]
fn a_toggle_during_startup_composes_with_the_saved_set(cx: &mut TestAppContext) {
    for when in ["before the catalog read", "during the restore's read"] {
        let dir = TempDir::new("startup");
        let log = Log::default();
        let b = bench_unrestored(
            vec![probe(ModuleMeta::new("a", "A"), &log), probe(ModuleMeta::new("c", "C"), &log)],
            &[],
            &dir,
            cx,
        );
        b.state.catalog.lock().unwrap().as_ref().unwrap().set_setting(ENABLED_KEY, "a").unwrap();
        if when == "before the catalog read" {
            cx.update(|cx| ModuleRegistry::enable(&b.registry, "c", cx));
            assert_eq!(b.enabled(cx), Vec::<String>::new(), "{when}: queued");
            assert!(b.status(cx).contains("C will be enabled"), "{when}: said so: {}", b.status(cx));
            b.model.update(cx, |m, cx| m.refresh(cx));
        } else {
            // The model's read lands and starts the restore's read; enable before that runs.
            b.model.update(cx, |m, cx| m.refresh(cx));
            let restore_started = |cx: &mut TestAppContext| b.model.read_with(cx, |m, _| m.catalog.is_some());
            while !restore_started(cx) {
                cx.executor().tick();
            }
            assert!(b.registry.read_with(cx, |r, _| r.restore_reading()), "{when}: the read is running");
            cx.update(|cx| ModuleRegistry::enable(&b.registry, "c", cx));
            assert_eq!(b.enabled(cx), Vec::<String>::new(), "{when}: queued");
        }
        cx.run_until_parked();
        assert_eq!(b.enabled(cx), ["a", "c"], "{when}");
        assert_eq!(b.setting(ENABLED_KEY).as_deref(), Some("a,c"), "{when}: the saved set survived");
    }
}

/// After a catalog switch the new catalog's saved set applies — what it lists on, the rest
/// off — without writing the old set into it; a catalog with no saved set keeps what is on.
/// A write made for the old catalog never lands in the new one.
#[gpui_kit::test]
fn a_catalog_switch_restores_the_new_catalogs_modules(cx: &mut TestAppContext) {
    let dir = TempDir::new("switch");
    let log = Log::default();
    let b = bench(
        vec![probe(ModuleMeta::new("a", "A"), &log), probe(ModuleMeta::new("b", "B"), &log), probe(ModuleMeta::new("c", "C"), &log)],
        &[],
        &dir,
        cx,
    );
    b.enable("a", cx);
    b.enable("c", cx);
    let open = |name: &str, saved: Option<&str>| {
        let db = dir.0.join(name);
        let c = Catalog::open(&db, &dir.0.join("photos")).unwrap();
        if let Some(saved) = saved {
            c.set_setting(ENABLED_KEY, saved).unwrap();
        }
        (c, db)
    };
    let switch_to = |catalog: Catalog, db: &std::path::Path, cx: &mut TestAppContext| {
        let old = b.state.catalog.lock().unwrap().replace(catalog);
        b.event(CoreEvent::CatalogSwitched(db.to_string_lossy().to_string()), cx);
        old.unwrap()
    };

    let (cat_b, db_b) = open("b.chairphoto", Some("b"));
    let cat_a = switch_to(cat_b, &db_b, cx);
    assert_eq!(b.enabled(cx), ["b"], "B's saved set, not A's carried over");
    assert_eq!(b.setting(ENABLED_KEY).as_deref(), Some("b"), "nothing was written into B");
    assert_eq!(cat_a.get_setting(ENABLED_KEY).unwrap().as_deref(), Some("a,c"), "A keeps its own");
    b.enable("c", cx);
    assert_eq!(b.setting(ENABLED_KEY).as_deref(), Some("b,c"));

    let (cat_c, db_c) = open("c.chairphoto", None);
    switch_to(cat_c, &db_c, cx);
    assert_eq!(b.enabled(cx), ["b", "c"], "a catalog with no saved set keeps what is on");
    assert_eq!(b.setting(ENABLED_KEY), None);

    // A toggle's write, then a catalog swap before the write runs: it must not land.
    cx.update(|cx| ModuleRegistry::enable(&b.registry, "a", cx));
    let (cat_d, _) = open("d.chairphoto", Some("b"));
    b.state.catalog.lock().unwrap().replace(cat_d);
    cx.run_until_parked();
    assert_eq!(b.setting(ENABLED_KEY).as_deref(), Some("b"), "the write for C did not land in D");
}

/// A restore read that a catalog switch overtook is dropped: the old catalog's set is not
/// applied, and the new catalog's restore still runs.
#[gpui_kit::test]
fn a_restore_read_overtaken_by_a_switch_is_dropped(cx: &mut TestAppContext) {
    let dir = TempDir::new("overtaken");
    let log = Log::default();
    let b = bench_unrestored(vec![probe(ModuleMeta::new("a", "A"), &log), probe(ModuleMeta::new("b", "B"), &log)], &[], &dir, cx);
    b.state.catalog.lock().unwrap().as_ref().unwrap().set_setting(ENABLED_KEY, "a").unwrap();
    b.model.update(cx, |m, cx| m.refresh(cx));
    while !b.registry.read_with(cx, |r, _| r.restore_reading()) {
        cx.executor().tick();
    }
    cx.executor().tick(); // the read runs against A; its result is not applied yet
    let db = dir.0.join("b.chairphoto");
    let catalog = Catalog::open(&db, &dir.0.join("photos")).unwrap();
    catalog.set_setting(ENABLED_KEY, "b").unwrap();
    *b.state.catalog.lock().unwrap() = Some(catalog);
    b.event(CoreEvent::CatalogSwitched(db.to_string_lossy().to_string()), cx);
    assert_eq!(b.enabled(cx), ["b"]);
    assert_eq!(*log.borrow(), ["load:b"], "A's set was never applied");
}

/// With no catalog open, a toggle waits (it has nowhere to be saved) and lands once one is.
#[gpui_kit::test]
fn a_toggle_with_no_catalog_waits_for_one(cx: &mut TestAppContext) {
    let dir = TempDir::new("nocat");
    let log = Log::default();
    let b = bench_unrestored(vec![probe(ModuleMeta::new("c", "C"), &log)], &[], &dir, cx);
    let catalog = b.state.catalog.lock().unwrap().take();
    b.model.update(cx, |m, cx| m.refresh(cx)); // fails: no catalog, no CatalogRead
    cx.run_until_parked();
    b.enable("c", cx);
    assert!(log.borrow().is_empty(), "nothing loads with nowhere to save it");
    assert_eq!(b.status(cx), "Modules: no catalog is open yet; C will be enabled once it is done");
    *b.state.catalog.lock().unwrap() = catalog;
    b.model.update(cx, |m, cx| m.refresh(cx));
    cx.run_until_parked();
    assert_eq!(b.enabled(cx), ["c"]);
    assert_eq!(b.setting(ENABLED_KEY).as_deref(), Some("c"));
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

/// Disables `target` from its `on_event`, through a registry handle set after install.
struct Disabler {
    meta: ModuleMeta,
    target: &'static str,
    registry: Rc<RefCell<Option<Entity<ModuleRegistry>>>>,
    log: Log,
}

struct DisablerInstance {
    target: &'static str,
    registry: Rc<RefCell<Option<Entity<ModuleRegistry>>>>,
    log: Log,
}

impl Module for Disabler {
    fn meta(&self) -> ModuleMeta {
        self.meta.clone()
    }

    fn load(&self, _: ModuleHost, _: &mut App) -> Result<Box<dyn ModuleInstance>, String> {
        Ok(Box::new(DisablerInstance { target: self.target, registry: self.registry.clone(), log: self.log.clone() }))
    }
}

impl ModuleInstance for DisablerInstance {
    fn contributions(&self) -> Contributions {
        Contributions::default()
    }

    fn on_event(&mut self, _: &CoreEvent, cx: &mut App) {
        let registry = self.registry.borrow().clone().unwrap();
        ModuleRegistry::disable(&registry, self.target, cx);
    }

    fn on_unload(&mut self, _: &mut App) {
        self.log.borrow_mut().push("unload:self".into());
    }
}

/// A module that disables itself, or a module it requires, from `on_event` does not crash
/// the dispatch ("already borrowed"): the disable runs after the callback returns.
#[gpui_kit::test]
fn a_module_disabling_itself_from_on_event_is_deferred_not_a_crash(cx: &mut TestAppContext) {
    for target in ["self", "base"] {
        let dir = TempDir::new("reentry");
        let log = Log::default();
        let slot: Rc<RefCell<Option<Entity<ModuleRegistry>>>> = Rc::default();
        let b = bench(
            vec![
                probe(ModuleMeta::new("base", "Base"), &log),
                Rc::new(Disabler { meta: ModuleMeta::new("self", "Self").requires("base"), target, registry: slot.clone(), log: log.clone() }),
            ],
            &[],
            &dir,
            cx,
        );
        *slot.borrow_mut() = Some(b.registry.clone());
        b.enable("self", cx);
        b.event(scan_done(), cx);
        assert!(!b.registry.read_with(cx, |r, _| r.is_enabled("self")), "{target}: the module ended up disabled");
        assert!(log.borrow().contains(&"unload:self".to_string()), "{target}: and unloaded: {:?}", log.borrow());
        let base_on = b.registry.read_with(cx, |r, _| r.is_enabled("base"));
        assert_eq!(base_on, target == "self", "{target}: only what was asked for went");
    }
}

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
        ModuleRegistry::install_with(
            vec![probe(ModuleMeta::new("b", "B"), &log), probe(ModuleMeta::new("a", "A"), &log), probe(ModuleMeta::new("off", "Off"), &log)],
            Vec::new(),
            &model,
            &shell,
            cx,
        )
    });
    model.update(cx, |m, cx| m.refresh(cx));
    cx.run_until_parked(); // the (empty) saved set is restored
    // The router starts only now and is not polled before the worker's send: a send to a
    // router already parked would wake it from a foreign thread, which the test scheduler
    // rejects (as in `tests::a_core_event_from_a_worker_thread_reaches_the_model`).
    cx.update(|cx| {
        crate::events::spawn_router(rx, model.clone(), cx).detach();
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

/// Each window gets its own views of a module's panels; closing a window drops its views (and
/// their subscriptions) while the other window's stay cached.
#[gpui_kit::test]
fn closing_a_window_drops_its_module_views(cx: &mut TestAppContext) {
    let dir = TempDir::new("windows");
    let log = Log::default();
    let b = bench(vec![probe(ModuleMeta::new("a", "A"), &log)], &[], &dir, cx);
    b.enable("a", cx);
    let open = |cx: &mut TestAppContext| -> gpui_kit::AnyWindowHandle {
        cx.update(|cx| cx.open_window(Default::default(), |_, cx| cx.new(|_| Empty)).unwrap()).into()
    };
    let (first, second) = (open(cx), open(cx));
    let views = |w: gpui_kit::AnyWindowHandle, cx: &mut TestAppContext| {
        let registry = b.registry.clone();
        cx.update_window(w, |_, window, cx| ModuleRegistry::panel_views(&registry, PanelSlot::Inspector, window, cx))
            .unwrap()
    };
    let in_first = views(first, cx)[0].view.entity_id();
    let in_second = views(second, cx)[0].view.entity_id();
    assert_ne!(in_first, in_second, "one view per window");
    assert_eq!(b.registry.read_with(cx, |r, _| r.cached_view_count()), 2);

    cx.update_window(second, |_, window, _| window.remove_window()).unwrap();
    cx.run_until_parked();
    assert_eq!(b.registry.read_with(cx, |r, _| r.cached_view_count()), 1, "the closed window's view is gone");
    assert_eq!(views(first, cx)[0].view.entity_id(), in_first, "the open window's view is still cached");
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
            WireOptions::headless(Rc::new(|| {})),
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
