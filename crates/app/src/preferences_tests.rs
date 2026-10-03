//! Headless tests of Preferences (#113): opened from the rail's gear through the real window,
//! each tab's sections driven through their buttons and fields, and the catalog-switch
//! interleaving forced. Reads and writes run on `Runner::manual` ([`work`]).

use super::storage_tests::{photo_count, set_input, settle, work, work_once};
use super::*;
use crate::preferences::editors::{DarkroomSection, EditorsSection};
use crate::preferences::storage::{MaintenanceSection, Removal, SafetySection, TieringSection};
use crate::preferences::tags::TagMaintenance;
use crate::preferences::{Content, LastPreferences, Preferences, Tab};
use crate::storage::Runner;
use chairphoto_core::catalog::{StorageTier, VolumeKind};
use chairphoto_model::darkroom::kelvin::WbPrefer;

fn prefs(cx: &mut TestAppContext) -> Entity<Preferences> {
    cx.update(|cx| cx.try_global::<LastPreferences>().and_then(|p| p.0.upgrade())).expect("Preferences is open")
}

/// Open Preferences with the rail's gear and let its first reads land.
fn open(app: &App, cx: &mut TestAppContext) -> Entity<Preferences> {
    click(app, "rail-preferences", cx);
    settle(app, cx);
    work(cx);
    prefs(cx)
}

fn tab(app: &App, id: &'static str, cx: &mut TestAppContext) {
    click(app, id, cx);
    work(cx);
}

fn setting(app: &App, key: &str) -> Option<String> {
    app.state.catalog.lock().unwrap().as_ref().unwrap().get_setting(key).unwrap()
}

fn put_setting(app: &App, key: &str, value: &str) {
    app.state.catalog.lock().unwrap().as_ref().unwrap().set_setting(key, value).unwrap();
}

fn present(app: &App, id: &'static str, cx: &mut TestAppContext) -> bool {
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.try_find(id).is_some()
    })
    .unwrap()
}

/// Run `f` in the main window — for controls the dialog's scroll area has below the fold,
/// which the test platform will not click.
fn in_window<R>(app: &App, cx: &mut TestAppContext, f: impl FnOnce(&mut Window, &mut gpui_kit::App) -> R) -> R {
    let r = cx.update_window(app.window(), |_, window, cx| f(window, cx)).unwrap();
    cx.run_until_parked();
    r
}

fn close_dialog(app: &App, cx: &mut TestAppContext) {
    use gpui_kit::component::WindowExt as _;
    cx.update_window(app.window(), |_, window, cx| window.close_dialog(cx)).unwrap();
    cx.run_until_parked();
}

fn library(p: &Entity<Preferences>, cx: &mut TestAppContext) -> Entity<crate::preferences::storage::LibrarySection> {
    p.read_with(cx, |p, _| match &p.content {
        Content::Storage(s) => s.library.clone(),
        _ => panic!("not on Storage"),
    })
}

fn safety(p: &Entity<Preferences>, cx: &mut TestAppContext) -> Entity<SafetySection> {
    p.read_with(cx, |p, _| match &p.content {
        Content::Storage(s) => s.safety.clone(),
        _ => panic!("not on Storage"),
    })
}

fn tiering(p: &Entity<Preferences>, cx: &mut TestAppContext) -> Entity<TieringSection> {
    p.read_with(cx, |p, _| match &p.content {
        Content::Storage(s) => s.tiering.clone(),
        _ => panic!("not on Storage"),
    })
}

fn maintenance(p: &Entity<Preferences>, cx: &mut TestAppContext) -> Entity<MaintenanceSection> {
    p.read_with(cx, |p, _| match &p.content {
        Content::Storage(s) => s.maintenance.clone(),
        _ => panic!("not on Storage"),
    })
}

fn tags(p: &Entity<Preferences>, cx: &mut TestAppContext) -> Entity<TagMaintenance> {
    p.read_with(cx, |p, _| match &p.content {
        Content::Tags(t) => t.clone(),
        _ => panic!("not on Tags"),
    })
}

fn editors(p: &Entity<Preferences>, cx: &mut TestAppContext) -> (Entity<EditorsSection>, Entity<DarkroomSection>) {
    p.read_with(cx, |p, _| match &p.content {
        Content::Editors(e, d) => (e.clone(), d.clone()),
        _ => panic!("not on Editors"),
    })
}

// --- the dialog and its tabs ------------------------------------------------------------------

/// The gear opens Preferences on Storage (More ⋯ → Preferences… too); the tabs switch what is
/// built; an enabled module with settings gets a tab named after it, which goes away — back to
/// Storage — when the module is disabled.
#[gpui_kit::test]
fn the_gear_opens_preferences_and_module_tabs_follow_the_registry(cx: &mut TestAppContext) {
    let dir = TempDir::new("prefs-tabs");
    let app = start(cx);
    open_catalog(&app, &dir, cx);
    let p = open(&app, cx);
    assert_eq!(p.read_with(cx, |p, _| p.tab.clone()), Tab::Storage);
    let root = dir.0.join("photos").to_string_lossy().to_string();
    let lib = library(&p, cx);
    assert_eq!(lib.read_with(cx, |l, cx| l.root.read(cx).value().to_string()), root, "the library folder was read");

    for (id, want) in [
        ("prefs-tab-tags", Tab::Tags),
        ("prefs-tab-editors", Tab::Editors),
        ("prefs-tab-modules", Tab::Modules),
        ("prefs-tab-appearance", Tab::Appearance),
    ] {
        tab(&app, id, cx);
        assert_eq!(p.read_with(cx, |p, _| p.tab.clone()), want);
    }
    tab(&app, "prefs-tab-modules", cx);
    assert!(!present(&app, "prefs-tab-module-dev", cx));
    click(&app, "module-toggle-dev", cx);
    assert!(present(&app, "prefs-tab-module-dev", cx), "an enabled module with settings has a tab");
    tab(&app, "prefs-tab-module-dev", cx);
    assert!(present(&app, "dev-settings", cx), "the tab shows the module's settings panel");
    cx.update(|cx| crate::modules::ModuleRegistry::disable(&app.wired.modules, "dev", cx));
    cx.run_until_parked();
    assert_eq!(p.read_with(cx, |p, _| p.tab.clone()), Tab::Storage, "its tab vanished: back to Storage");
    assert!(!present(&app, "prefs-tab-module-dev", cx));

    close_dialog(&app, cx);
    click(&app, "more-menu", cx);
    let row = cx
        .update_window(app.window(), |_, window, cx| {
            window.render_frame(cx);
            let menu = window.within("popup-menu");
            (0..30).find(|i| menu.find(*i).label() == Some("Preferences…"))
        })
        .unwrap()
        .expect("More ⋯ has Preferences…");
    press(&app, "escape", cx);
    click_menu_row(&app, "more-menu", row, "Preferences…", cx);
    settle(&app, cx);
    assert_ne!(prefs(cx).entity_id(), p.entity_id(), "More ⋯ → Preferences… opened it again");
}

// --- storage --------------------------------------------------------------------------------

/// Set re-roots the open catalog (the same file, the new root, the folder created) and says
/// to rescan, on the section and on the status line.
#[gpui_kit::test]
fn set_reroots_the_open_catalog(cx: &mut TestAppContext) {
    let dir = TempDir::new("prefs-root");
    let app = start(cx);
    open_catalog(&app, &dir, cx);
    let db = app.state.catalog.lock().unwrap().as_ref().unwrap().db_path().to_path_buf();
    let p = open(&app, cx);
    let lib = library(&p, cx);
    let new_root = dir.0.join("library");
    let input = lib.read_with(cx, |l, _| l.root.clone());
    set_input(&app, &input, &new_root.to_string_lossy(), cx);
    click(&app, "library-set", cx);
    work(cx);
    {
        let guard = app.state.catalog.lock().unwrap();
        let c = guard.as_ref().unwrap();
        assert_eq!((c.db_path(), c.root()), (db.as_path(), new_root.as_path()));
    }
    assert!(new_root.is_dir());
    assert_eq!(lib.read_with(cx, |l, _| l.status.clone()).as_deref(), Some("Library folder set — re-scan to index it."));
    assert_eq!(status(&app, cx), "Library folder changed — click Rescan library to index it.");
}

/// The day count takes digits only and loads what is stored; Save writes React's key and
/// line; blank turns the policy off.
#[gpui_kit::test]
fn tiering_saves_the_offload_age(cx: &mut TestAppContext) {
    let dir = TempDir::new("prefs-tiering");
    let app = start(cx);
    open_catalog(&app, &dir, cx);
    put_setting(&app, "offload_age_days", "30");
    let p = open(&app, cx);
    let t = tiering(&p, cx);
    let days = t.read_with(cx, |t, _| t.days.clone());
    assert_eq!(days.read_with(cx, |i, _| i.value().to_string()), "30");
    set_input(&app, &days, "", cx);
    in_window(&app, cx, |window, cx| {
        days.update(cx, |i, cx| i.focus(window, cx));
        window.input("9x0", cx);
    });
    assert_eq!(days.read_with(cx, |i, _| i.value().to_string()), "90", "digits only");
    t.update(cx, |t, cx| t.save(cx));
    work(cx);
    assert_eq!(setting(&app, "offload_age_days").as_deref(), Some("90"));
    assert_eq!(
        t.read_with(cx, |t, _| t.status.clone()).as_deref(),
        Some("Saved — photos older than 90 day(s) will be offloaded to the NAS.")
    );
    set_input(&app, &days, "", cx);
    t.update(cx, |t, cx| t.save(cx));
    work(cx);
    assert_eq!(setting(&app, "offload_age_days").as_deref(), Some("0"));
    assert_eq!(
        t.read_with(cx, |t, _| t.status.clone()).as_deref(),
        Some("Saved — automatic offload is off (photos stay on local disk).")
    );
    // Offload older now with no NAS: nothing to do, said so.
    t.update(cx, |t, cx| t.offload_now(cx));
    work(cx);
    let line = t.read_with(cx, |t, _| t.status.clone()).unwrap();
    assert!(line.starts_with("Nothing to offload") || line.contains("backup"), "{line}");
}

/// Index existing NAS photos: an empty field asks for a folder; Enter in the field indexes a
/// folder on a registered NAS volume in place, and the photos are in the catalog.
#[gpui_kit::test]
fn the_nas_index_runs_in_place(cx: &mut TestAppContext) {
    let dir = TempDir::new("prefs-nas");
    let app = start(cx);
    open_catalog(&app, &dir, cx);
    let nas = dir.0.join("nas");
    std::fs::create_dir_all(nas.join("2020")).unwrap();
    std::fs::write(nas.join("2020/IMG_0001.jpg"), "not really a jpeg").unwrap();
    app.state
        .catalog
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .add_volume("NAS", &nas, VolumeKind::Backup)
        .unwrap();
    let p = open(&app, cx);
    let t = tiering(&p, cx);
    t.update(cx, |t, cx| t.index_nas(cx));
    assert_eq!(t.read_with(cx, |t, _| t.status.clone()).as_deref(), Some("Choose the NAS folder to index."));
    let field = t.read_with(cx, |t, _| t.nas.clone());
    set_input(&app, &field, &nas.to_string_lossy(), cx);
    in_window(&app, cx, |window, cx| {
        field.update(cx, |i, cx| i.focus(window, cx));
        window.press("enter", cx);
    });
    assert!(t.read_with(cx, |t, _| t.busy), "Enter started the index");
    work(cx);
    let line = t.read_with(cx, |t, _| t.status.clone()).unwrap();
    assert_eq!(line, "Indexed 1 new photo(s) from the NAS. Find them under the \"On NAS\" filter.");
    assert_eq!(photo_count(&app), 1);
}

/// Remove unavailable photos: the confirm lists them; Cancel keeps them ("Cancelled."), OK
/// removes the catalog rows only. Remove empty finds none. Compact reports the sizes.
#[gpui_kit::test]
fn maintenance_removes_behind_a_confirm_and_compacts(cx: &mut TestAppContext) {
    let dir = TempDir::new("prefs-maint");
    let app = start(cx);
    std::fs::create_dir_all(dir.0.join("photos")).unwrap(); // the library folder is mounted
    open_catalog_with_photos(&app, &dir, 2, cx); // rows whose files do not exist
    let p = open(&app, cx);
    let m = maintenance(&p, cx);
    in_window(&app, cx, |w, cx| m.update(cx, |m, cx| m.remove(Removal::Unavailable, w, cx)));
    work(cx);
    settle(&app, cx);
    click(&app, "cancel", cx);
    work(cx);
    assert_eq!(m.read_with(cx, |m, _| m.status.clone()).as_deref(), Some("Cancelled."));
    assert_eq!(photo_count(&app), 2);
    in_window(&app, cx, |w, cx| m.update(cx, |m, cx| m.remove(Removal::Unavailable, w, cx)));
    work(cx);
    settle(&app, cx);
    click(&app, "ok", cx);
    work(cx);
    assert_eq!(m.read_with(cx, |m, _| m.status.clone()).as_deref(), Some("Removed 2 unavailable entries (no files deleted)."));
    assert_eq!(photo_count(&app), 0);
    app.wired.model.read_with(cx, |m, _| assert_eq!(m.catalog.as_ref().unwrap().photo_count, 0, "the model re-read"));

    in_window(&app, cx, |w, cx| m.update(cx, |m, cx| m.remove(Removal::Empty, w, cx)));
    work(cx);
    assert_eq!(m.read_with(cx, |m, _| m.status.clone()).as_deref(), Some("No empty (0-byte) photos found."));

    m.update(cx, |m, cx| m.compact(cx));
    assert!(m.read_with(cx, |m, _| m.busy));
    work(cx);
    let line = m.read_with(cx, |m, _| m.status.clone()).unwrap();
    assert!(line.starts_with("Compacted: ") || line.starts_with("Already compact ("), "{line}");
}

/// **Forced interleaving.** Compact's worker finishes, the catalog switches, then its result
/// reaches the UI thread: Preferences rebuilt the tab for the new catalog, and the old
/// section's result is dropped — it never says the new catalog was compacted.
#[gpui_kit::test]
fn a_result_from_before_a_catalog_switch_is_dropped(cx: &mut TestAppContext) {
    let dir = TempDir::new("prefs-switch");
    let app = start(cx);
    open_catalog(&app, &dir, cx);
    let p = open(&app, cx);
    let old = maintenance(&p, cx);
    old.update(cx, |m, cx| m.compact(cx));
    assert_eq!(work_once(cx), 1, "the worker ran; its result waits for the UI thread");
    // A real switch, and the model hears `catalog:switched` before anything else runs.
    chairphoto_core::app::catalogs::switch_catalog_in(
        &app.state,
        Some(&dir.0.join("registry")),
        &dir.0.join("other/other.chairphoto"),
        &dir.0.join("other"),
        true,
        None,
    )
    .unwrap();
    app.wired.model.update(cx, |m, cx| m.on_core_event(&CoreEvent::CatalogSwitched("other".into()), cx));
    work(cx);
    let new = maintenance(&p, cx);
    assert_ne!(new.entity_id(), old.entity_id(), "the tab was rebuilt for the new catalog");
    assert_eq!(new.read_with(cx, |m, _| m.status.clone()), None);
    assert_eq!(
        old.read_with(cx, |m, _| m.status.clone()).as_deref(),
        Some("Compacting the catalog… this can take a moment."),
        "the old catalog's result landed"
    );
}

/// Safety counts the library; "Show me" filters the grid to the bucket and closes
/// Preferences.
#[gpui_kit::test]
fn safety_show_me_filters_to_the_bucket(cx: &mut TestAppContext) {
    let dir = TempDir::new("prefs-safety");
    let app = start(cx);
    open_catalog_with_photos(&app, &dir, 3, cx);
    let p = open(&app, cx);
    let s = safety(&p, cx).read_with(cx, |s, _| s.summary.clone()).expect("counted");
    assert_eq!(s.at_risk + s.missing + s.unverified + s.stale + s.safe, 3, "{s:?}");
    if s.at_risk == 0 {
        panic!("photos with only a local copy are at risk: {s:?}");
    }
    click(&app, "safety-show-at-risk", cx);
    assert_eq!(app.wired.shell.read_with(cx, |s, _| s.library.scope().storage_tier), StorageTier::AtRisk);
    use gpui_kit::component::WindowExt as _;
    assert!(!cx.update_window(app.window(), |_, window, cx| window.has_active_dialog(cx)).unwrap(), "Preferences closed");
}

// --- tags -------------------------------------------------------------------------------------

/// Tidy drops an implied ancestor; duplicates list a look-alike pair and its merge says where
/// the merge lives; unused tags list, a leaf deletes at once and a branch asks first.
#[gpui_kit::test]
fn tag_maintenance_tidies_finds_and_deletes(cx: &mut TestAppContext) {
    let dir = TempDir::new("prefs-tags");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 1, cx);
    {
        let guard = app.state.catalog.lock().unwrap();
        let c = guard.as_ref().unwrap();
        let harbor = c.create_tag("Harbor").unwrap();
        let marina = c.create_tag("Harbor/Marina").unwrap();
        c.conn()
            .execute("INSERT INTO photo_tags(photo_id, tag_id, created_at) VALUES(?1, ?2, 0), (?1, ?3, 0)", (ids[0], harbor, marina))
            .unwrap();
        c.create_tag("Sunset").unwrap();
        c.create_tag("Sunsets").unwrap();
        c.create_tag("Empty/Leaf").unwrap();
    }
    let p = open(&app, cx);
    tab(&app, "prefs-tab-tags", cx);
    let t = tags(&p, cx);

    click(&app, "tags-tidy", cx);
    work(cx);
    assert_eq!(t.read_with(cx, |t, _| t.status.clone()).as_deref(), Some("Removed 1 redundant tag(s)."));

    click(&app, "tags-find-duplicates", cx);
    assert!(t.read_with(cx, |t, _| t.busy.is_some()), "Looking…");
    work(cx);
    let pairs = t.read_with(cx, |t, _| t.duplicates.clone()).expect("found");
    assert!(pairs.iter().any(|p| [&p.a_path, &p.b_path].iter().all(|x| x.starts_with("Sunset"))), "Sunset/Sunsets");

    click(&app, "tags-find-unused", cx);
    work(cx);
    let orphans = t.read_with(cx, |t, _| t.orphans.clone()).expect("found");
    let leaf = orphans.iter().find(|o| o.path == "Sunsets").expect("an unused leaf").id;
    let branch = orphans.iter().find(|o| o.path == "Empty").expect("an empty branch");
    assert!(branch.has_children);
    let branch = branch.id;
    in_window(&app, cx, |w, cx| t.update(cx, |t, cx| t.delete(leaf, w, cx)));
    work(cx);
    assert_eq!(t.read_with(cx, |t, _| t.status.clone()).as_deref(), Some("Deleted Sunsets."));
    in_window(&app, cx, |w, cx| t.update(cx, |t, cx| t.delete(branch, w, cx)));
    settle(&app, cx);
    click(&app, "cancel", cx);
    work(cx);
    assert!(t.read_with(cx, |t, _| t.orphans.as_ref().unwrap().iter().any(|o| o.id == branch)), "Cancel kept the branch");
    in_window(&app, cx, |w, cx| t.update(cx, |t, cx| t.delete(branch, w, cx)));
    settle(&app, cx);
    click(&app, "ok", cx);
    work(cx);
    assert_eq!(t.read_with(cx, |t, _| t.status.clone()).as_deref(), Some("Deleted Empty."));
    let left: i64 = app
        .state
        .catalog
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .conn()
        .query_row("SELECT COUNT(*) FROM tags WHERE full_path LIKE 'Empty%' OR full_path = 'Sunsets'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, 0, "the branch went with its sub-tag");
}

/// Catalog A with the look-alike tags Sunset (1 photo) and Sunsets (1 photo); Preferences open
/// on Tags with the duplicates found. Returns (Preferences, the section, Sunset, Sunsets, the
/// button that merges Sunsets away).
fn duplicates_found(
    app: &App,
    dir: &TempDir,
    cx: &mut TestAppContext,
) -> (Entity<Preferences>, Entity<TagMaintenance>, i64, i64, &'static str) {
    let ids = open_catalog_with_photos(app, dir, 2, cx);
    let (sunset, sunsets) = {
        let guard = app.state.catalog.lock().unwrap();
        let c = guard.as_ref().unwrap();
        let sunset = c.create_tag("Sunset").unwrap();
        let sunsets = c.create_tag("Sunsets").unwrap();
        c.conn()
            .execute("INSERT INTO photo_tags(photo_id, tag_id, created_at) VALUES(?1, ?2, 0), (?3, ?4, 0)", (ids[0], sunset, ids[1], sunsets))
            .unwrap();
        (sunset, sunsets)
    };
    // The tag tree re-reads on the model's next read, as after any catalog change.
    app.wired.model.update(cx, |m, cx| m.refresh(cx));
    cx.run_until_parked();
    let p = open(app, cx);
    tab(app, "prefs-tab-tags", cx);
    let t = tags(&p, cx);
    click(app, "tags-find-duplicates", cx);
    work(cx);
    let pairs = t.read_with(cx, |t, _| t.duplicates.clone()).expect("found");
    let (i, pair) = pairs.iter().enumerate().find(|(_, p)| [p.a_id, p.b_id].contains(&sunsets)).expect("Sunset/Sunsets");
    let side = if pair.a_id == sunsets { "a" } else { "b" };
    let button: &'static str = Box::leak(format!("tags-merge-{side}-{i}").into_boxed_str());
    (p, t, sunset, sunsets, button)
}

fn merge_dialog(app: &App, cx: &mut TestAppContext) -> Option<Entity<crate::tags::merge::TagMerge>> {
    match app.wired.tags.read_with(cx, |t, _| t.last_dialog.clone()) {
        Some(crate::tags::state::TagDialog::Merge(m)) => m.upgrade(),
        _ => None,
    }
}

/// Tags → "Merge Sunsets away…" opens the Tag panel's merge preview over Preferences; the
/// merge commits, the preview closes, Preferences stays, and the section reports the merge
/// and drops its stale lists. (#161)
#[gpui_kit::test]
fn merge_x_away_opens_the_merge_preview_over_preferences(cx: &mut TestAppContext) {
    let dir = TempDir::new("prefs-tags-merge");
    let app = start(cx);
    let (_p, t, sunset, sunsets, button) = duplicates_found(&app, &dir, cx);

    click(&app, button, cx);
    settle(&app, cx);
    let merge = merge_dialog(&app, cx).expect("the merge preview opened");
    assert_eq!(merge.read_with(cx, |m, _| m.source.tag.id), sunsets);
    assert!(present(&app, "tag-merge-list", cx) && present(&app, "prefs-tab-tags", cx), "the preview is over Preferences");
    let target: &'static str = Box::leak(format!("tag-merge-into-{sunset}").into_boxed_str());
    click(&app, target, cx);
    merge.read_with(cx, |m, _| assert_eq!(m.preview.as_ref().expect("previewed").photos_retagged, 1));
    click(&app, "tag-merge-commit", cx);
    drop(merge);
    cx.run_until_parked();

    let left: i64 = app
        .state
        .catalog
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .conn()
        .query_row("SELECT COUNT(*) FROM tags WHERE full_path = 'Sunsets'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, 0, "merged away");
    t.read_with(cx, |t, _| {
        assert_eq!(t.status.as_deref(), Some("Merged Sunsets into Sunset — 1 photo moved."));
        assert!(t.duplicates.is_none() && t.orphans.is_none(), "the lists the merge made stale are gone");
    });
    assert!(!present(&app, "tag-merge-back", cx), "the preview closed");
    assert!(present(&app, "prefs-tab-tags", cx), "Preferences stayed open");
}

/// **Catalog identity.** The pairs' ids are the section's catalog's: "Merge X away…" opens
/// nothing while the tag tree is not that catalog's (superseded, not yet re-read), nor from a
/// section a delivered switch has replaced. With the switch undelivered, the preview opens
/// under the old tree and its dry run fails closed — the twin's tag with the same id stays.
#[gpui_kit::test]
fn merge_x_away_never_opens_over_another_catalogs_tags(cx: &mut TestAppContext) {
    let dir = TempDir::new("prefs-tags-merge-switch");
    let app = start(cx);
    let (_p, t, sunset, sunsets, button) = duplicates_found(&app, &dir, cx);

    // The tree superseded (as at a switch, before its re-read): the button is dead and the
    // call refuses.
    app.wired.tags.update(cx, |s, cx| s.on_catalog_switched(cx));
    cx.run_until_parked();
    t.update(cx, |t, cx| t.merge(sunsets, cx));
    assert_eq!(t.read_with(cx, |t, _| t.status.clone()).as_deref(), Some(crate::preferences::tags::TREE_NOT_READY));
    assert!(merge_dialog(&app, cx).is_none());
    app.wired.tags.update(cx, |s, cx| s.refresh(cx));
    cx.run_until_parked();

    // The core switches to a twin whose tags have the same ids; the event is withheld.
    let root = dir.0.join("photos-b");
    let b = Catalog::open(&dir.0.join("b.chairphoto"), &root).unwrap();
    assert_eq!(b.create_tag("Sunset").unwrap(), sunset);
    assert_eq!(b.create_tag("Sunsets").unwrap(), sunsets);
    core_switch(&app, b);
    click(&app, button, cx);
    settle(&app, cx);
    let merge = merge_dialog(&app, cx).expect("opened under the old tree");
    let target = app.wired.tags.read_with(cx, |s, _| s.tag(sunset).unwrap().clone());
    merge.update(cx, |m, cx| m.pick(target, cx));
    cx.run_until_parked();
    assert_eq!(merge.read_with(cx, |m, _| m.error.clone()), Some(chairphoto_core::app::CATALOG_CHANGED.to_string()));
    drop(merge);
    // The tree re-read (a model read after a scan would do it) is now the twin's, while the
    // section — and the model, the event still withheld — are A's: the ids would name B's tags.
    app.wired.tags.update(cx, |s, _| s.last_dialog = None);
    app.wired.tags.update(cx, |s, cx| s.refresh(cx));
    cx.run_until_parked();
    t.update(cx, |t, _| t.status = None);
    t.update(cx, |t, cx| t.merge(sunsets, cx));
    assert_eq!(t.read_with(cx, |t, _| t.status.clone()).as_deref(), Some(crate::preferences::tags::TREE_NOT_READY));
    assert!(merge_dialog(&app, cx).is_none(), "no preview over the twin's tags");

    // Delivered: the preview closed itself, although the section that asked for it was
    // rebuilt (Preferences owns its subscriptions); Preferences stays. The old section refuses.
    deliver_switch(&app, cx);
    settle(&app, cx);
    assert!(!present(&app, "tag-merge-back", cx), "the switch closed the preview");
    assert!(present(&app, "prefs-tab-tags", cx), "and only the preview");
    app.wired.tags.update(cx, |s, _| s.last_dialog = None);
    t.update(cx, |t, _| t.status = None);
    t.update(cx, |t, cx| t.merge(sunsets, cx));
    assert_eq!(t.read_with(cx, |t, _| t.status.clone()).as_deref(), Some(crate::preferences::tags::TREE_NOT_READY));
    assert!(merge_dialog(&app, cx).is_none());
    let twin_kept = app.state.catalog.lock().unwrap().as_ref().unwrap().get_tag(sunsets).is_ok();
    assert!(twin_kept, "the twin's Sunsets is untouched");
}

/// A re-root reopens the catalog (a new identity, no `catalog:switched`): Preferences rebuilds
/// the Tags tab first, and only then does the tag tree's re-read supersede the open preview.
/// The preview still closes — Preferences, not the dropped section, owns its subscriptions —
/// and Preferences stays.
#[gpui_kit::test]
fn the_merge_preview_closes_when_a_reroot_supersedes_its_tree(cx: &mut TestAppContext) {
    let dir = TempDir::new("prefs-tags-merge-reroot");
    let app = start(cx);
    let (p, t, _, _, button) = duplicates_found(&app, &dir, cx);
    click(&app, button, cx);
    settle(&app, cx);
    assert!(merge_dialog(&app, cx).is_some() && present(&app, "tag-merge-list", cx));

    let identity = app.wired.model.read_with(cx, |m, _| m.catalog_identity()).expect("a catalog is open");
    chairphoto_core::app::catalogs::reroot_open_catalog_as(&app.state, identity, dir.0.join("library")).unwrap();
    app.wired.model.update(cx, |m, cx| m.refresh(cx));
    cx.run_until_parked();
    assert_ne!(tags(&p, cx).entity_id(), t.entity_id(), "the tab was rebuilt for the reopened catalog");
    settle(&app, cx);
    assert!(!present(&app, "tag-merge-list", cx), "the preview closed");
    assert!(present(&app, "prefs-tab-tags", cx), "Preferences stayed open");
}

// --- editors ----------------------------------------------------------------------------------

/// The inspector's "Edit in" list follows a save in Preferences → Editors, with no catalog
/// switch: RapidRAW pointed at a missing binary is not offered; pointing it at one that exists
/// offers it once the save lands. (#161)
#[gpui_kit::test]
fn the_edit_in_list_follows_an_editor_saved_in_preferences(cx: &mut TestAppContext) {
    let dir = TempDir::new("prefs-editors-inspector");
    let app = start(cx);
    open_catalog_with_photos(&app, &dir, 1, cx);
    put_setting(&app, crate::preferences::editors::RAPIDRAW_BIN_KEY, "/nonexistent/rapidraw");
    work(cx);
    let insp = app.wired.root.as_ref().unwrap().read_with(cx, |r, _| r.inspector.clone());
    assert_eq!(insp.read_with(cx, |i, _| i.editors.as_ref().map(|e| e.rapidraw)), Some(false), "read once, not offered");

    let bin = dir.0.join("rapidraw");
    std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
    let p = open(&app, cx);
    tab(&app, "prefs-tab-editors", cx);
    let (e, _) = editors(&p, cx);
    e.update(cx, |e, cx| e.save_rapidraw(crate::preferences::editors::RAPIDRAW_BIN_KEY, bin.to_string_lossy().into(), cx));
    work(cx);
    assert_eq!(e.read_with(cx, |e, _| e.status.clone()).as_deref(), Some("Saved."));
    assert_eq!(insp.read_with(cx, |i, _| i.editors.as_ref().map(|e| e.rapidraw)), Some(true), "re-checked after the save");

    // A darktable/RawTherapee/ART path says so too. (Which of them the list then offers
    // depends on this machine's PATH, so the event is what is checked.)
    let seen = std::rc::Rc::new(std::cell::Cell::new(0));
    let _sub = cx.update(|cx| {
        let seen = seen.clone();
        cx.subscribe(&app.wired.model, move |_, _: &crate::model::EditorsChanged, _| seen.set(seen.get() + 1))
    });
    e.update(cx, |e, cx| e.save("darktable", "gui", "/nonexistent/darktable".into(), cx));
    work(cx);
    assert_eq!(seen.get(), 1, "the path save announced the change");
}

/// Review L3 (#161): a save fires on blur, so the Editors section is often gone before its
/// worker returns — another tab clicked, or Preferences closed. The setting is stored and the
/// inspector re-reads anyway.
#[gpui_kit::test]
fn the_edit_in_list_follows_a_save_that_lands_after_the_section_is_gone(cx: &mut TestAppContext) {
    for leave_by_closing in [false, true] {
        let dir = TempDir::new(if leave_by_closing { "prefs-editors-close" } else { "prefs-editors-tab" });
        let app = start(cx);
        open_catalog_with_photos(&app, &dir, 1, cx);
        put_setting(&app, crate::preferences::editors::RAPIDRAW_BIN_KEY, "/nonexistent/rapidraw");
        work(cx);
        let insp = app.wired.root.as_ref().unwrap().read_with(cx, |r, _| r.inspector.clone());
        assert_eq!(insp.read_with(cx, |i, _| i.editors.as_ref().map(|e| e.rapidraw)), Some(false));
        let bin = dir.0.join("rapidraw");
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        let p = open(&app, cx);
        tab(&app, "prefs-tab-editors", cx);
        let (e, _) = editors(&p, cx);
        e.update(cx, |e, cx| e.save_rapidraw(crate::preferences::editors::RAPIDRAW_BIN_KEY, bin.to_string_lossy().into(), cx));
        let section = e.downgrade();
        drop((e, p));
        if leave_by_closing {
            close_dialog(&app, cx);
        } else {
            click(&app, "prefs-tab-storage", cx);
        }
        assert!(section.upgrade().is_none(), "the section is gone before the save lands");
        work(cx);
        assert_eq!(setting(&app, crate::preferences::editors::RAPIDRAW_BIN_KEY).as_deref(), bin.to_str(), "stored");
        assert_eq!(
            insp.read_with(cx, |i, _| i.editors.as_ref().map(|e| e.rapidraw)),
            Some(true),
            "re-checked although the section was gone (closing Preferences: {leave_by_closing})"
        );
    }
}

/// Editors: a path override saves under React's key and re-checks availability; RapidRAW's
/// format saves. Darkroom: the stored values load; the cache size normalises and saves; preload,
/// white balance and render timing save React's values.
#[gpui_kit::test]
fn editors_and_darkroom_save_reacts_keys(cx: &mut TestAppContext) {
    let dir = TempDir::new("prefs-editors");
    let app = start(cx);
    open_catalog(&app, &dir, cx);
    put_setting(&app, "develop.preloadNeighbours", "0");
    put_setting(&app, "metrics.exportParity", "{\"checked\":2,\"differing\":0}");
    put_setting(&app, "editor.renderTiming", "1");
    put_setting(&app, "editor.renderTiming.lastSummary", "p50 12 ms");
    let p = open(&app, cx);
    tab(&app, "prefs-tab-editors", cx);
    let (e, d) = editors(&p, cx);

    let keys: Vec<String> = e.read_with(cx, |e, _| e.paths.iter().map(|p| p.key.clone()).collect());
    assert!(keys.iter().any(|k| k == "darktable"), "{keys:?}");
    e.update(cx, |e, cx| e.save("darktable", "gui", " /nonexistent/darktable-gui ".into(), cx));
    work(cx);
    assert_eq!(setting(&app, "editor.darktable.gui").as_deref(), Some("/nonexistent/darktable-gui"));
    e.read_with(cx, |e, _| {
        assert_eq!(e.status.as_deref(), Some("Saved."));
        assert!(e.editors.iter().find(|x| x.key == "darktable").unwrap().gui, "re-checked: an override counts as configured");
    });
    e.update(cx, |e, cx| e.set_rapidraw_format("png", cx));
    work(cx);
    assert_eq!(setting(&app, "editor.rapidraw.format").as_deref(), Some("png"));

    d.read_with(cx, |d, cx| {
        assert_eq!(d.gb.read(cx).value(), "20", "the default size");
        assert_eq!(d.preload, Some(false));
        assert_eq!(d.wb, Some(WbPrefer::Kelvin));
        assert_eq!(d.timing, Some(true));
        assert_eq!(d.last_summary, "p50 12 ms");
        assert_eq!(d.usage.is_some(), true);
        assert!(d.parity.as_deref().unwrap().contains("2 RAW exports checked, none differed"));
    });
    assert!(d.read_with(cx, |d, _| d.timing == Some(true) && !d.last_summary.is_empty()), "the summary line shows");
    let gb = d.read_with(cx, |d, _| d.gb.clone());
    set_input(&app, &gb, "abc", cx);
    cx.update_window(app.window(), |_, window, cx| d.update(cx, |d, cx| d.save_gb(window, cx))).unwrap();
    work(cx);
    assert_eq!(gb.read_with(cx, |i, _| i.value().to_string()), "20", "an invalid size is the default");
    assert_eq!(setting(&app, "develop.decodeCacheGb").as_deref(), Some("20"));
    set_input(&app, &gb, "2.5", cx);
    cx.update_window(app.window(), |_, window, cx| d.update(cx, |d, cx| d.save_gb(window, cx))).unwrap();
    work(cx);
    assert_eq!(setting(&app, "develop.decodeCacheGb").as_deref(), Some("2.5"));

    d.update(cx, |d, cx| d.set_preload(true, cx));
    work(cx);
    assert_eq!(setting(&app, "develop.preloadNeighbours").as_deref(), Some("1"));
    d.update(cx, |d, cx| d.set_wb(WbPrefer::Relative, cx));
    work(cx);
    assert_eq!(setting(&app, "develop.wbSlider").as_deref(), Some("relative"));
    d.update(cx, |d, cx| d.set_timing(false, cx));
    work(cx);
    assert_eq!(setting(&app, "editor.renderTiming").as_deref(), Some("0"));
    assert_eq!(d.read_with(cx, |d, _| d.timing), Some(false));
}

/// **Forced reversal** (#113 Codex gate, finding 3). Every setting is changed twice; the
/// Runner then runs the queued writes newest first. Each setting persists the newer value,
/// and each control shows it.
#[gpui_kit::test]
fn setting_writes_persist_in_the_order_made(cx: &mut TestAppContext) {
    let dir = TempDir::new("prefs-write-order");
    let app = start(cx);
    open_catalog(&app, &dir, cx);
    let p = open(&app, cx);
    let tier = tiering(&p, cx);
    let days = tier.read_with(cx, |t, _| t.days.clone());
    for n in ["7", "30"] {
        set_input(&app, &days, n, cx);
        tier.update(cx, |t, cx| t.save(cx));
    }
    in_window(&app, cx, |w, cx| p.update(cx, |p, cx| p.select(Tab::Editors, w, cx)));
    work(cx);
    let (e, d) = editors(&p, cx);
    let gb = d.read_with(cx, |d, _| d.gb.clone());
    for n in ["5", "9"] {
        set_input(&app, &gb, n, cx);
        in_window(&app, cx, |w, cx| d.update(cx, |d, cx| d.save_gb(w, cx)));
    }
    d.update(cx, |d, cx| {
        d.set_preload(false, cx);
        d.set_preload(true, cx);
        d.set_timing(true, cx);
        d.set_timing(false, cx);
        d.set_wb(WbPrefer::Relative, cx);
        d.set_wb(WbPrefer::Kelvin, cx);
    });
    e.update(cx, |e, cx| {
        e.set_rapidraw_format("png", cx);
        e.set_rapidraw_format("jpg", cx);
        e.save("darktable", "gui", "/old/darktable".into(), cx);
        e.save("darktable", "gui", "/new/darktable".into(), cx);
    });
    let ran = cx.update(|cx| Runner::get(cx).run_pending_reversed());
    assert!(ran >= 12, "every write was queued: {ran}");
    work(cx);
    for (key, want) in [
        ("offload_age_days", "30"),
        ("develop.decodeCacheGb", "9"),
        ("develop.preloadNeighbours", "1"),
        ("editor.renderTiming", "0"),
        ("develop.wbSlider", "kelvin"),
        ("editor.rapidraw.format", "jpg"),
        ("editor.darktable.gui", "/new/darktable"),
    ] {
        assert_eq!(setting(&app, key).as_deref(), Some(want), "{key}: an older write overwrote a newer one");
    }
    d.read_with(cx, |d, _| {
        assert_eq!(d.preload, Some(true));
        assert_eq!(d.timing, Some(false));
        assert_eq!(d.wb, Some(WbPrefer::Kelvin));
    });
    assert_eq!(e.read_with(cx, |e, _| e.rapidraw_format.clone()), "jpg");
}

/// An older write's completion, arriving after a newer change was made but before its write
/// ran, does not put the control back to the older value.
#[gpui_kit::test]
fn a_stale_write_completion_does_not_undo_a_newer_change(cx: &mut TestAppContext) {
    let dir = TempDir::new("prefs-write-stale");
    let app = start(cx);
    open_catalog(&app, &dir, cx);
    let p = open(&app, cx);
    in_window(&app, cx, |w, cx| p.update(cx, |p, cx| p.select(Tab::Editors, w, cx)));
    work(cx);
    let (_, d) = editors(&p, cx);
    d.update(cx, |d, cx| d.set_preload(false, cx));
    // The off write runs; its completion is not delivered yet.
    assert_eq!(cx.update(|cx| Runner::get(cx).run_pending()), 1);
    d.update(cx, |d, cx| d.set_preload(true, cx));
    cx.run_until_parked();
    assert_eq!(d.read_with(cx, |d, _| d.preload), Some(true), "the off write's completion undid the on");
    work(cx);
    assert_eq!(setting(&app, "develop.preloadNeighbours").as_deref(), Some("1"));
    assert_eq!(d.read_with(cx, |d, _| d.preload), Some(true));
}

// --- appearance -------------------------------------------------------------------------------

/// Appearance: Standard paints Standard and hides the status line; Follow shows what is
/// followed (no Omarchy here). The mode is the per-machine preference, not a catalog setting.
#[gpui_kit::test]
fn appearance_switches_the_mode_per_machine(cx: &mut TestAppContext) {
    use crate::machine_prefs::MachinePrefs;
    use crate::theme::{AppearanceMode, MODE_PREF};
    let dir = TempDir::new("prefs-appearance");
    let app = start(cx);
    open_catalog(&app, &dir, cx);
    open(&app, cx);
    tab(&app, "prefs-tab-appearance", cx);
    assert!(present(&app, "appearance-status", cx), "following: the status line");
    click(&app, "appearance-standard", cx);
    assert_eq!(cx.update(|cx| crate::theme::mode(cx)), AppearanceMode::Standard);
    assert_eq!(cx.update(|cx| MachinePrefs::read(cx, MODE_PREF)).as_deref(), Some("standard"));
    assert_eq!(setting(&app, MODE_PREF), None, "not a catalog setting");
    assert!(!present(&app, "appearance-status", cx));
    click(&app, "appearance-follow", cx);
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| crate::theme::mode(cx)), AppearanceMode::FollowOmarchy);
    assert!(present(&app, "appearance-status", cx));
}

// --- catalog identity (#113 review) -----------------------------------------------------------

/// **Forced interleaving** (#113 review, finding 1). Work queued by Preferences in one
/// catalog — a tag delete keyed by the orphan list's id, a tiering Save, Compact — runs after
/// the core switched to a catalog whose tag carries the same id; `catalog:switched` withheld
/// or delivered. Every one fails closed: the new catalog's tag and settings are untouched.
fn queued_work_across_a_switch(delivered: bool, cx: &mut TestAppContext) {
    let dir = TempDir::new("prefs-identity");
    let app = start(cx);
    open_catalog(&app, &dir, cx);
    let orphan = app.state.catalog.lock().unwrap().as_ref().unwrap().create_tag("Unused").unwrap();
    let p = open(&app, cx);
    tab(&app, "prefs-tab-tags", cx);
    let t = tags(&p, cx);
    click(&app, "tags-find-unused", cx);
    work(cx);
    assert!(t.read_with(cx, |t, _| t.orphans.as_ref().unwrap().iter().any(|o| o.id == orphan)));
    in_window(&app, cx, |w, cx| t.update(cx, |t, cx| t.delete(orphan, w, cx)));
    // Storage, without running anything queued.
    in_window(&app, cx, |w, cx| p.update(cx, |p, cx| p.select(Tab::Storage, w, cx)));
    let tier = tiering(&p, cx);
    let days = tier.read_with(cx, |t, _| t.days.clone());
    set_input(&app, &days, "7", cx);
    tier.update(cx, |t, cx| t.save(cx));
    let m = maintenance(&p, cx);
    m.update(cx, |m, cx| m.compact(cx));

    let (b, _) = colliding_catalog(&dir, "b", 0);
    let kept = b.create_tag("Kept").unwrap();
    assert_eq!(kept, orphan, "the tag ids collide, as real catalogs' do");
    core_switch(&app, b);
    if delivered {
        deliver_switch(&app, cx);
    }
    work(cx);
    assert!(
        app.state.catalog.lock().unwrap().as_ref().unwrap().get_tag(kept).is_ok(),
        "the old catalog's delete removed the new catalog's tag"
    );
    assert_eq!(setting(&app, "offload_age_days"), None, "the old catalog's Save wrote into the new one");
    if !delivered {
        let changed = Some(chairphoto_core::app::CATALOG_CHANGED.to_string());
        assert_eq!(t.read_with(cx, |t, _| t.status.clone()), changed);
        assert_eq!(tier.read_with(cx, |t, _| t.status.clone()), changed);
        assert_eq!(m.read_with(cx, |m, _| m.status.clone()), changed);
    }
}

#[gpui_kit::test]
fn queued_work_fails_closed_before_the_switch_event(cx: &mut TestAppContext) {
    queued_work_across_a_switch(false, cx);
}

#[gpui_kit::test]
fn queued_work_fails_closed_after_the_switch_event(cx: &mut TestAppContext) {
    queued_work_across_a_switch(true, cx);
}

/// Once `catalog:switched` and the model's read of the new catalog land, the open tab is
/// rebuilt bound to the new catalog, and its work runs there.
#[gpui_kit::test]
fn after_a_switch_the_rebuilt_tab_works_in_the_new_catalog(cx: &mut TestAppContext) {
    let dir = TempDir::new("prefs-identity-new");
    let app = start(cx);
    open_catalog(&app, &dir, cx);
    let p = open(&app, cx);
    let (b, _) = colliding_catalog(&dir, "b", 0);
    core_switch(&app, b);
    deliver_switch(&app, cx);
    work(cx);
    let now = chairphoto_core::app::catalog_identity(&app.state).unwrap();
    let tier = tiering(&p, cx);
    assert_eq!(tier.read_with(cx, |t, _| t.ctx_identity()), Some(now));
    let days = tier.read_with(cx, |t, _| t.days.clone());
    set_input(&app, &days, "7", cx);
    tier.update(cx, |t, cx| t.save(cx));
    work(cx);
    assert_eq!(setting(&app, "offload_age_days").as_deref(), Some("7"));
}

/// Finding 2: a blur (or Enter) before the stored values have loaded saves nothing — the
/// RapidRAW binary and the decode-cache size keep what is stored.
#[gpui_kit::test]
fn editors_save_nothing_before_their_values_load(cx: &mut TestAppContext) {
    let dir = TempDir::new("prefs-editors-early");
    let app = start(cx);
    open_catalog(&app, &dir, cx);
    put_setting(&app, "editor.rapidraw.bin", "/opt/rapidraw/bin");
    put_setting(&app, "develop.decodeCacheGb", "7");
    let p = open(&app, cx);
    // The Editors tab, its reads queued and not run.
    in_window(&app, cx, |w, cx| p.update(cx, |p, cx| p.select(Tab::Editors, w, cx)));
    let (e, d) = editors(&p, cx);
    let bin = e.read_with(cx, |e, _| e.rapidraw_bin.clone());
    // Focus changes reach their listeners (the input's Blur) in an active window, at a frame.
    in_window(&app, cx, |w, _| w.activate_window());
    let blur = |cx: &mut TestAppContext| {
        in_window(&app, cx, |w, cx| {
            bin.update(cx, |i, cx| i.focus(w, cx));
            w.render_frame(cx);
        });
        in_window(&app, cx, |w, cx| {
            w.blur(cx);
            w.render_frame(cx);
        });
    };
    // Into the field and out again: a blur.
    blur(cx);
    in_window(&app, cx, |w, cx| d.update(cx, |d, cx| d.save_gb(w, cx)));
    work(cx);
    assert_eq!(setting(&app, "editor.rapidraw.bin").as_deref(), Some("/opt/rapidraw/bin"), "the early blur wiped the binary");
    assert_eq!(setting(&app, "develop.decodeCacheGb").as_deref(), Some("7"), "the early save stored the default");
    assert_eq!(bin.read_with(cx, |i, _| i.value().to_string()), "/opt/rapidraw/bin", "then it loaded");
    // Loaded: a blur saves (and so the early one was a real blur).
    blur(cx);
    work(cx);
    assert_eq!(e.read_with(cx, |e, _| e.status.clone()).as_deref(), Some("Saved."));
}
