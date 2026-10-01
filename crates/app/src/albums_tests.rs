//! Headless tests of Albums and export (#115): the albums and smart-albums sections and their
//! dialogs driven through the real window, the rule editor, and the Export and bundle-export
//! jobs with Cancel and catalog switches forced between start and finish. Exports write only
//! under the test's temp dir. Export work runs on `Runner::manual` (see `storage_tests`).

use super::storage_tests::{dispatch, has_dialog, set_input, settle, work};
use super::*;
use crate::albums::prompt::NamePrompt;
use crate::albums::rule::Op;
use crate::albums::smart_editor::{SmartAlbumEditor, COUNT_DEBOUNCE};
use crate::albums::state::AlbumDialog;
use crate::export::bundle::BundleExport;
use crate::export::open::ExportDialog;
use crate::export::panel::ExportPanel;
use chairphoto_core::app::CATALOG_CHANGED;
use chairphoto_core::export::ExportPreset;

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// A catalog whose photos are real files (with sidecars) under `dir/photos`, all in one
/// import batch; returns the ids.
fn open_catalog_with_files(app: &App, dir: &TempDir, n: usize, cx: &mut TestAppContext) -> Vec<i64> {
    let db = dir.0.join("files.chairphoto");
    let root = dir.0.join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let catalog = Catalog::open(&db, &root).unwrap();
    let batch = catalog.create_import_batch("/media/card/DCIM/100TRIP").unwrap();
    let ids: Vec<i64> = (0..n)
        .map(|i| {
            let path = root.join(format!("IMG_{i}.CR3"));
            std::fs::write(&path, format!("raw {i}")).unwrap();
            std::fs::write(root.join(format!("IMG_{i}.CR3.xmp")), "<x:xmpmeta xmlns:x='adobe:ns:meta/'/>").unwrap();
            catalog.upsert_photo(&path, None, 0, 5).unwrap().id
        })
        .collect();
    catalog.assign_photos_to_batch(batch, &ids).unwrap();
    *app.state.catalog.lock().unwrap() = Some(catalog);
    app.state.send(CoreEvent::CatalogSwitched(db.to_string_lossy().to_string()));
    cx.run_until_parked();
    ids
}

/// Type into a field as a user would: unlike `set_input`, this emits `InputEvent::Change`.
fn type_into(app: &App, input: &Entity<gpui_kit::component::input::InputState>, text: &str, cx: &mut TestAppContext) {
    let (input, text) = (input.clone(), text.to_string());
    cx.update_window(app.window(), |_, window, cx| input.update(cx, |i, cx| i.replace_all(text, window, cx))).unwrap();
    cx.run_until_parked();
}

fn select_all(app: &App, cx: &mut TestAppContext) {
    app.wired.shell.update(cx, |s, cx| s.select_with(cx, |l| l.select_all()));
    cx.run_until_parked();
}

fn with_catalog<T>(app: &App, f: impl FnOnce(&Catalog) -> T) -> T {
    f(app.state.catalog.lock().unwrap().as_ref().unwrap())
}

fn album_dialog(app: &App, cx: &mut TestAppContext) -> AlbumDialog {
    settle(app, cx);
    app.wired.albums.read_with(cx, |a, _| a.last_dialog.clone()).expect("an albums dialog opened")
}

fn prompt(app: &App, cx: &mut TestAppContext) -> Entity<NamePrompt> {
    match album_dialog(app, cx) {
        AlbumDialog::Prompt(p) => p.upgrade().expect("the prompt is open"),
        _ => panic!("the name prompt"),
    }
}

fn editor(app: &App, cx: &mut TestAppContext) -> Entity<SmartAlbumEditor> {
    match album_dialog(app, cx) {
        AlbumDialog::SmartEditor(e) => e.upgrade().expect("the editor is open"),
        _ => panic!("the smart-album editor"),
    }
}

fn export_dialog(app: &App, cx: &mut TestAppContext) -> ExportDialog {
    settle(app, cx);
    app.wired.exports.read_with(cx, |e, _| e.last_dialog.clone()).expect("an export dialog opened")
}

fn albums(app: &App, cx: &mut TestAppContext) -> Vec<(String, i64)> {
    app.wired.shell.read_with(cx, |s, _| s.lists.albums.iter().map(|a| (a.name.clone(), a.photo_count)).collect())
}

fn name_prompt(app: &App, trigger: &'static str, name: &str, cx: &mut TestAppContext) {
    click(app, trigger, cx);
    let p = prompt(app, cx);
    let input = p.read_with(cx, |p, _| p.input.clone());
    set_input(app, &input, name, cx);
    click(app, "album-prompt-ok", cx);
}

// --- albums -----------------------------------------------------------------------------

/// ＋ New album, ⚙ Rename (prefilled), "+N" adds the selection, the name toggles the filter,
/// ✕ Delete asks first: Cancel keeps it, OK deletes it and clears the active filter.
#[gpui_kit::test]
fn albums_are_created_renamed_filled_filtered_and_deleted_from_the_section(cx: &mut TestAppContext) {
    let dir = TempDir::new("albums-crud");
    let app = start(cx);
    open_catalog_with_photos(&app, &dir, 3, cx);
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("albums-empty").is_some(), "\"No albums yet\"");
        assert!(window.try_find("smart-albums-empty").is_some(), "\"No smart albums yet\"");
    })
    .unwrap();

    name_prompt(&app, "album-new", "  Trip  ", cx);
    assert!(!has_dialog(&app, cx), "OK closes the prompt");
    assert_eq!(albums(&app, cx), [("Trip".to_string(), 0)]);
    let id = app.wired.shell.read_with(cx, |s, _| s.lists.albums[0].id);

    click(&app, leak(format!("album-rename-{id}")), cx);
    let p = prompt(&app, cx);
    assert_eq!(p.read_with(cx, |p, cx| p.input.read(cx).value().to_string()), "Trip", "prefilled");
    set_input(&app, &p.read_with(cx, |p, _| p.input.clone()), "Trip 2026", cx);
    click(&app, "album-prompt-ok", cx);
    assert_eq!(albums(&app, cx), [("Trip 2026".to_string(), 0)]);

    // "+N" shows only with a selection.
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find(leak(format!("album-add-{id}"))).is_none());
    })
    .unwrap();
    select_all(&app, cx);
    click(&app, leak(format!("album-add-{id}")), cx);
    assert_eq!(albums(&app, cx), [("Trip 2026".to_string(), 3)]);
    assert_eq!(status(&app, cx), "Added 3 photo(s) to the album.");

    click(&app, leak(format!("album-name-{id}")), cx);
    assert_eq!(app.wired.shell.read_with(cx, |s, _| s.library.scope().album_id), Some(id));

    click(&app, leak(format!("album-delete-{id}")), cx);
    settle(&app, cx);
    click(&app, "cancel", cx);
    assert_eq!(albums(&app, cx).len(), 1, "Cancel kept the album");
    click(&app, leak(format!("album-delete-{id}")), cx);
    settle(&app, cx);
    click(&app, "ok", cx);
    assert!(albums(&app, cx).is_empty());
    assert_eq!(app.wired.shell.read_with(cx, |s, _| s.library.scope().album_id), None, "the filter cleared");
    assert_eq!(with_catalog(&app, |c| c.count_photos(&Default::default()).unwrap()), 3, "photos are not deleted");
}

/// An unchanged or empty name submits nothing.
#[gpui_kit::test]
fn an_empty_or_unchanged_name_writes_nothing(cx: &mut TestAppContext) {
    let dir = TempDir::new("albums-noop");
    let app = start(cx);
    open_catalog_with_photos(&app, &dir, 1, cx);
    name_prompt(&app, "album-new", "   ", cx);
    assert!(albums(&app, cx).is_empty());
    name_prompt(&app, "album-new", "Keep", cx);
    let id = app.wired.shell.read_with(cx, |s, _| s.lists.albums[0].id);
    let before = with_catalog(&app, |c| c.list_albums().unwrap()[0].name.clone());
    name_prompt(&app, leak(format!("album-rename-{id}")), "Keep", cx);
    assert_eq!(with_catalog(&app, |c| c.list_albums().unwrap()[0].name.clone()), before);
}

/// **Catalog identity.** The albums were listed from catalog A; the core switches to B, whose
/// album and photo ids collide. Rename, add-the-selection and delete — asked with the old
/// list's ids — fail closed (`CATALOG_CHANGED`) and B is untouched, with the switch event
/// undelivered and (for the prompt) delivered: the open prompt closes on it.
#[gpui_kit::test]
fn album_writes_keyed_by_the_old_lists_ids_never_reach_another_catalog(cx: &mut TestAppContext) {
    let dir = TempDir::new("albums-identity");
    let app = start(cx);
    open_catalog_with_photos(&app, &dir, 2, cx);
    name_prompt(&app, "album-new", "A's", cx);
    let id = app.wired.shell.read_with(cx, |s, _| s.lists.albums[0].id);
    select_all(&app, cx);

    let (b, _) = colliding_catalog(&dir, "b", 2);
    let b_album = b.create_album("B's").unwrap();
    assert_eq!(b_album, id, "the ids collide, as two catalogs' do");
    core_switch(&app, b);

    app.wired.albums.update(cx, |a, cx| a.rename_album(id, "renamed".into(), cx));
    cx.run_until_parked();
    assert_eq!(status(&app, cx), format!("Renaming the album failed: {CATALOG_CHANGED}"));
    app.wired.albums.update(cx, |a, cx| a.add_selection(id, cx));
    cx.run_until_parked();
    app.wired.albums.update(cx, |a, cx| a.delete_album(id, cx));
    cx.run_until_parked();
    with_catalog(&app, |c| {
        let list = c.list_albums().unwrap();
        assert_eq!((list[0].name.as_str(), list[0].photo_count), ("B's", 0), "B's album untouched");
    });

    // The prompt opened over A's list closes once the switch arrives.
    click(&app, leak(format!("album-rename-{id}")), cx);
    prompt(&app, cx);
    assert!(has_dialog(&app, cx));
    deliver_switch(&app, cx);
    assert!(!has_dialog(&app, cx), "the switch closed the prompt");
    assert_eq!(with_catalog(&app, |c| c.list_albums().unwrap()[0].name.clone()), "B's");
}

// --- smart albums -----------------------------------------------------------------------

/// The rule editor: a new album's rows — field, operator and typed value — make the rule; the
/// live count follows the rows after the debounce; Create saves it and makes it the scope;
/// ⚙ Edit rule round-trips the rule into rows, and Save rule rewrites it and renames.
#[gpui_kit::test]
fn the_smart_album_editor_builds_counts_saves_and_round_trips_a_rule(cx: &mut TestAppContext) {
    let dir = TempDir::new("smart-editor");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 3, cx);
    with_catalog(&app, |c| {
        c.set_culling(ids[0], Some(4), None, None).unwrap();
        c.set_culling(ids[1], Some(2), None, None).unwrap();
    });

    click(&app, "smart-album-new", cx);
    let ed = editor(&app, cx);
    cx.executor().advance_clock(COUNT_DEBOUNCE);
    cx.run_until_parked();
    assert_eq!(ed.read_with(cx, |e, _| e.count), Some(3), "no conditions match every photo");

    click(&app, "sa-add", cx);
    cx.update_window(app.window(), |_, window, cx| {
        ed.update(cx, |e, cx| {
            e.set_field(0, "rating", window, cx);
            e.set_op(0, Op::Gte, window, cx);
        })
    })
    .unwrap();
    let input = ed.read_with(cx, |e, _| e.rows[0].inputs[0].clone());
    type_into(&app, &input, "3", cx);
    ed.read_with(cx, |e, _| assert!(e.counting, "a change starts the debounce"));
    cx.executor().advance_clock(COUNT_DEBOUNCE);
    cx.run_until_parked();
    ed.read_with(cx, |e, cx| {
        assert_eq!(e.count, Some(1));
        let rule: serde_json::Value = serde_json::from_str(&e.rule_json(cx)).unwrap();
        assert_eq!(rule, serde_json::json!({ "match": "all", "conditions": [{ "field": "rating", "op": "gte", "value": 3 }] }));
    });

    // A name is required.
    click(&app, "sa-save", cx);
    assert_eq!(ed.read_with(cx, |e, _| e.error.clone()).as_deref(), Some("Give the smart album a name."));
    set_input(&app, &ed.read_with(cx, |e, _| e.name.clone()), "Keepers", cx);
    click(&app, "sa-save", cx);
    assert!(!has_dialog(&app, cx), "Create closes the editor");
    let (id, rule) = with_catalog(&app, |c| {
        let a = &c.list_smart_albums().unwrap()[0];
        (a.id, a.rule_json.clone())
    });
    assert!(rule.contains(r#""field":"rating""#) && rule.contains(r#""op":"gte""#), "{rule}");
    app.wired.shell.read_with(cx, |s, _| {
        assert_eq!(s.library.scope().smart_album_id, Some(id), "the new album is the scope");
        assert_eq!(s.lists.smart_albums[0].photo_count, 1);
    });

    click(&app, leak(format!("smart-album-edit-{id}")), cx);
    let ed = editor(&app, cx);
    ed.read_with(cx, |e, cx| {
        assert_eq!(e.rows.len(), 1);
        assert_eq!((e.rows[0].cond.field, e.rows[0].cond.op), ("rating", Op::Gte));
        assert_eq!(e.rows[0].inputs[0].read(cx).value().to_string(), "3", "the value round-tripped into its field");
        assert_eq!(e.name.read(cx).value().to_string(), "Keepers");
    });
    set_input(&app, &ed.read_with(cx, |e, _| e.rows[0].inputs[0].clone()), "1", cx);
    set_input(&app, &ed.read_with(cx, |e, _| e.name.clone()), "Rated", cx);
    click(&app, "sa-save", cx);
    let (name, count) = with_catalog(&app, |c| {
        let a = &c.list_smart_albums().unwrap()[0];
        (a.name.clone(), a.photo_count)
    });
    assert_eq!((name.as_str(), count), ("Rated", 2));
}

/// ✎ Rename and ✕ Delete (behind its confirm) on a smart album; deleting the active one
/// clears the filter.
#[gpui_kit::test]
fn smart_albums_rename_and_delete_from_the_section(cx: &mut TestAppContext) {
    let dir = TempDir::new("smart-crud");
    let app = start(cx);
    open_catalog_with_photos(&app, &dir, 1, cx);
    with_catalog(&app, |c| c.create_smart_album("All", r#"{"match":"all","conditions":[]}"#).unwrap());
    app.wired.model.update(cx, |m, cx| m.refresh(cx));
    cx.run_until_parked();
    let id = app.wired.shell.read_with(cx, |s, _| s.lists.smart_albums[0].id);
    name_prompt(&app, leak(format!("smart-album-rename-{id}")), "Everything", cx);
    assert_eq!(with_catalog(&app, |c| c.list_smart_albums().unwrap()[0].name.clone()), "Everything");
    click(&app, leak(format!("smart-album-name-{id}")), cx);
    assert_eq!(app.wired.shell.read_with(cx, |s, _| s.library.scope().smart_album_id), Some(id));
    click(&app, leak(format!("smart-album-delete-{id}")), cx);
    settle(&app, cx);
    click(&app, "ok", cx);
    assert!(with_catalog(&app, |c| c.list_smart_albums().unwrap().is_empty()));
    assert_eq!(app.wired.shell.read_with(cx, |s, _| s.library.scope().smart_album_id), None);
}

/// **Catalog identity.** The editor opened over catalog A (its tag and batch ids are A's);
/// the core switches to B. Save fails closed and creates nothing in B; the event's arrival
/// closes the editor.
#[gpui_kit::test]
fn the_smart_album_editor_is_bound_to_the_catalog_it_opened_over(cx: &mut TestAppContext) {
    let dir = TempDir::new("smart-identity");
    let app = start(cx);
    open_catalog_with_photos(&app, &dir, 1, cx);
    click(&app, "smart-album-new", cx);
    let ed = editor(&app, cx);
    set_input(&app, &ed.read_with(cx, |e, _| e.name.clone()), "Mine", cx);
    let (b, _) = colliding_catalog(&dir, "b", 1);
    core_switch(&app, b);
    click(&app, "sa-save", cx);
    assert_eq!(ed.read_with(cx, |e, _| e.error.clone()).as_deref(), Some(CATALOG_CHANGED));
    assert!(with_catalog(&app, |c| c.list_smart_albums().unwrap().is_empty()), "nothing created in B");
    deliver_switch(&app, cx);
    assert!(!has_dialog(&app, cx), "the switch closed the editor");
}

// --- export -----------------------------------------------------------------------------

fn open_export(app: &App, cx: &mut TestAppContext) -> Entity<ExportPanel> {
    dispatch(app, crate::shell::actions::ExportSelection, cx);
    match export_dialog(app, cx) {
        ExportDialog::Photos(p) => p.upgrade().expect("the Export dialog is open"),
        _ => panic!("the Export dialog"),
    }
}

fn files(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// Export with no selection says so instead of opening.
#[gpui_kit::test]
fn export_needs_a_selection(cx: &mut TestAppContext) {
    let dir = TempDir::new("export-none");
    let app = start(cx);
    open_catalog_with_files(&app, &dir, 1, cx);
    dispatch(&app, crate::shell::actions::ExportSelection, cx);
    assert!(!has_dialog(&app, cx));
    assert_eq!(status(&app, cx), "Select photos to export.");
}

/// The Export dialog's options reach the job: Hand-off copies the originals and their
/// sidecars into the destination, the chosen tag group becomes `hashtags.txt` (and its
/// preview, capped by the limit, is what Copy puts on the clipboard); the bench follows the
/// job; the result shows in the dialog and on the status line; the originals are unchanged.
#[gpui_kit::test]
fn export_options_become_a_job_that_writes_copies_into_the_destination(cx: &mut TestAppContext) {
    let dir = TempDir::new("export-run");
    let app = start(cx);
    open_catalog_with_files(&app, &dir, 2, cx);
    let group = with_catalog(&app, |c| {
        let g = c.create_tag_group("Reach").unwrap();
        for t in ["Landscape", "Norway", "Fjord"] {
            let id = c.create_tag(t).unwrap();
            c.add_tag_to_group(g, id).unwrap();
        }
        g
    });
    select_all(&app, cx);
    let panel = open_export(&app, cx);
    cx.run_until_parked();
    panel.read_with(cx, |p, _| {
        assert_eq!(p.photo_ids.len(), 2);
        assert_eq!(p.preset, ExportPreset::HandOff);
        assert_eq!(p.groups.len(), 1, "the tag groups were read");
    });
    click(&app, "export-showoff", cx);
    assert_eq!(panel.read_with(cx, |p, _| p.preset), ExportPreset::ShowOff);
    click(&app, "export-handoff", cx);

    panel.update(cx, |p, cx| p.set_group(Some(group), cx));
    cx.run_until_parked();
    type_into(&app, &panel.read_with(cx, |p, _| p.limit.clone()), "2", cx);
    cx.run_until_parked();
    let tags = panel.read_with(cx, |p, _| p.hashtags.clone());
    assert_eq!(tags.len(), 2, "the limit caps the preview: {tags:?}");
    click(&app, "export-copy", cx);
    assert_eq!(cx.read_from_clipboard().and_then(|c| c.text()), Some(tags.join(" ")));
    assert!(panel.read_with(cx, |p, _| p.copied));

    let out = dir.0.join("out");
    set_input(&app, &panel.read_with(cx, |p, _| p.dest.clone()), &out.to_string_lossy(), cx);
    click(&app, "export-run", cx);
    assert!(panel.read_with(cx, |p, _| p.busy));
    app.wired.shell.read_with(cx, |s, _| assert!(s.jobs.export_photos.is_some(), "the bench follows the export"));
    assert_eq!(status(&app, cx), "Exporting 2 photo(s)…");
    work(cx);
    panel.read_with(cx, |p, _| {
        assert!(!p.busy);
        assert_eq!(p.result.as_ref().map(|r| (r.exported, r.skipped_offline, r.errors)), Some((2, 0, 0)));
    });
    assert_eq!(status(&app, cx), "Exported 2.");
    app.wired.shell.read_with(cx, |s, _| assert!(s.jobs.export_photos.is_none(), "the bench cleared"));
    assert_eq!(files(&out), ["IMG_0.CR3", "IMG_0.CR3.xmp", "IMG_1.CR3", "IMG_1.CR3.xmp", "hashtags.txt"]);
    assert_eq!(std::fs::read_to_string(out.join("hashtags.txt")).unwrap(), tags.join(" "));
    assert_eq!(std::fs::read(dir.0.join("photos/IMG_0.CR3")).unwrap(), b"raw 0", "the original is untouched");
}

/// An empty destination is refused before any job starts.
#[gpui_kit::test]
fn export_refuses_an_empty_destination(cx: &mut TestAppContext) {
    let dir = TempDir::new("export-nodest");
    let app = start(cx);
    open_catalog_with_files(&app, &dir, 1, cx);
    select_all(&app, cx);
    let panel = open_export(&app, cx);
    set_input(&app, &panel.read_with(cx, |p, _| p.dest.clone()), "  ", cx);
    click(&app, "export-run", cx);
    assert_eq!(panel.read_with(cx, |p, _| p.error.clone()).as_deref(), Some("Choose a destination folder."));
    assert!(app.wired.exports.read_with(cx, |e, _| e.photos.is_none()));
}

/// The bench's Cancel export, pressed before the worker reaches its first photo: nothing is
/// written, and the dialog and status line say it was cancelled.
#[gpui_kit::test]
fn cancelling_an_export_stops_it_before_the_next_photo(cx: &mut TestAppContext) {
    let dir = TempDir::new("export-cancel");
    let app = start(cx);
    open_catalog_with_files(&app, &dir, 3, cx);
    select_all(&app, cx);
    let panel = open_export(&app, cx);
    let out = dir.0.join("out");
    set_input(&app, &panel.read_with(cx, |p, _| p.dest.clone()), &out.to_string_lossy(), cx);
    click(&app, "export-run", cx);
    click(&app, "export-cancel", cx);
    work(cx);
    assert!(files(&out).is_empty(), "nothing exported: {:?}", files(&out));
    assert_eq!(status(&app, cx), "Export cancelled before any photo was exported.");
    assert_eq!(panel.read_with(cx, |p, _| p.error.clone()).as_deref(), Some("Export cancelled before any photo was exported."));
    app.wired.shell.read_with(cx, |s, _| assert!(s.jobs.export_photos.is_none()));
}

/// **Forced interleaving.** Export pressed in catalog A; the core switches to B (colliding
/// ids) before the worker runs. With the event undelivered the worker is already tripped and
/// writes nothing — neither A's photos nor B's same-numbered ones. With it delivered, the
/// dialog has closed, the bench has cleared and the stopped job's report never lands.
#[gpui_kit::test]
fn a_catalog_switch_makes_a_queued_export_unreachable(cx: &mut TestAppContext) {
    for deliver in [false, true] {
        let dir = TempDir::new(&format!("export-switch-{deliver}"));
        let app = start(cx);
        open_catalog_with_files(&app, &dir, 2, cx);
        work(cx);
        select_all(&app, cx);
        let panel = open_export(&app, cx);
        let out = dir.0.join("out");
        set_input(&app, &panel.read_with(cx, |p, _| p.dest.clone()), &out.to_string_lossy(), cx);
        click(&app, "export-run", cx);
        let (b, _) = colliding_catalog(&dir, "b", 2);
        core_switch(&app, b);
        if deliver {
            deliver_switch(&app, cx);
            assert!(!has_dialog(&app, cx), "the switch closed the dialog");
            app.wired.shell.read_with(cx, |s, _| assert!(s.jobs.export_photos.is_none()));
            app.wired.model.update(cx, |m, cx| m.set_status("after the switch", cx));
        }
        work(cx);
        assert!(files(&out).is_empty(), "deliver={deliver}: nothing exported: {:?}", files(&out));
        if deliver {
            assert_eq!(status(&app, cx), "after the switch", "the old catalog's export report landed");
        } else {
            assert!(status(&app, cx).starts_with("Export cancelled"), "{}", status(&app, cx));
        }
    }
}

/// A newer export trips the older one: the older one's report never lands, the newer one's
/// does.
#[gpui_kit::test]
fn a_newer_export_supersedes_the_older(cx: &mut TestAppContext) {
    let dir = TempDir::new("export-newer");
    let app = start(cx);
    open_catalog_with_files(&app, &dir, 1, cx);
    select_all(&app, cx);
    let (ids, from) = app.wired.shell.read_with(cx, |s, _| (s.library.selection().targets.clone(), s.rows_from().unwrap()));
    let request = |dest: &str| chairphoto_core::app::exports::ExportRequest {
        photo_ids: ids.clone(),
        preset: ExportPreset::HandOff,
        dest_dir: dir.0.join(dest),
        hashtag_group_id: None,
        hashtag_limit: None,
        version_id: None,
    };
    app.wired.exports.update(cx, |e, cx| e.start_photos(request("one"), from, cx));
    app.wired.exports.update(cx, |e, cx| e.start_photos(request("two"), from, cx));
    work(cx);
    assert!(files(&dir.0.join("one")).is_empty(), "the superseded export wrote nothing");
    assert_eq!(files(&dir.0.join("two")), ["IMG_0.CR3", "IMG_0.CR3.xmp"]);
    assert_eq!(status(&app, cx), "Exported 1.");
}

// --- bundle export ----------------------------------------------------------------------

/// The import-batches section's ⬇ opens the bundle export for that batch: the filename
/// defaults from the label; Export bundle runs the job (the bench follows it) and writes the
/// `.chairphoto` zip; the result shows in the dialog. A cancelled one leaves nothing behind.
#[gpui_kit::test]
fn a_batch_exports_as_a_bundle_and_a_cancelled_bundle_leaves_nothing(cx: &mut TestAppContext) {
    let dir = TempDir::new("bundle-export");
    let app = start(cx);
    open_catalog_with_files(&app, &dir, 2, cx);
    let batch = app.wired.shell.read_with(cx, |s, _| s.lists.batches[0].id);
    click(&app, leak(format!("batch-export-{batch}")), cx);
    let ExportDialog::Bundle(view) = export_dialog(&app, cx) else { panic!("the bundle export") };
    let view: Entity<BundleExport> = view.upgrade().unwrap();
    assert_eq!(view.read_with(cx, |v, cx| v.filename.read(cx).value().to_string()), "100TRIP.chairphoto");
    click(&app, "bundle-export-run", cx);
    assert_eq!(view.read_with(cx, |v, _| v.error.clone()).as_deref(), Some("Choose a destination folder."));

    let out = dir.0.join("bundles");
    set_input(&app, &view.read_with(cx, |v, _| v.dest.clone()), &out.to_string_lossy(), cx);
    set_input(&app, &view.read_with(cx, |v, _| v.filename.clone()), "trip", cx);
    click(&app, "bundle-export-run", cx);
    app.wired.shell.read_with(cx, |s, _| assert!(s.jobs.export_bundle.is_some(), "the bench follows the bundle"));
    work(cx);
    assert!(out.join("trip.chairphoto").is_file());
    view.read_with(cx, |v, _| {
        assert!(!v.busy);
        assert_eq!(v.result.as_ref().map(|r| (r.exported, r.skipped_offline, r.errors)), Some((2, 0, 0)));
    });
    assert_eq!(status(&app, cx), "Bundle written. 2 originals exported.");

    set_input(&app, &view.read_with(cx, |v, _| v.filename.clone()), "cancelled", cx);
    click(&app, "bundle-export-run", cx);
    click(&app, "bundle-export-cancel", cx);
    work(cx);
    assert!(!out.join("cancelled.chairphoto").exists());
    assert_eq!(files(&out), ["trip.chairphoto"], "no temp file left behind");
    assert!(view.read_with(cx, |v, _| v.error.clone()).unwrap().starts_with("Bundle export cancelled"));
}

/// The Export dialog's "Export as bundle…" appears with a batch as the scope and hands over to
/// the bundle export for that batch.
#[gpui_kit::test]
fn the_export_dialog_hands_a_batch_scope_to_the_bundle_export(cx: &mut TestAppContext) {
    let dir = TempDir::new("export-to-bundle");
    let app = start(cx);
    open_catalog_with_files(&app, &dir, 1, cx);
    let batch = app.wired.shell.read_with(cx, |s, _| s.lists.batches[0].id);
    click(&app, leak(format!("batch-{batch}")), cx);
    select_all(&app, cx);
    open_export(&app, cx);
    click(&app, "export-as-bundle", cx);
    let ExportDialog::Bundle(view) = export_dialog(&app, cx) else { panic!("the bundle export opened") };
    assert_eq!(view.upgrade().unwrap().read_with(cx, |v, _| v.batch.id), batch);
    assert!(has_dialog(&app, cx));
}
