//! Headless tests of Tags (#107): the tag panel, its dialogs and the inspector's tagging block,
//! driven through the real window and wiring; the catalog-switch interleavings forced.
//!
//! Tag work runs on GPUI's background executor (`tags::state::run`), which the deterministic
//! test scheduler runs at `run_until_parked` — so a test can put a catalog switch between a
//! write's start and its worker.

use super::*;
use crate::tags::panel::TagPanel;
use crate::tags::photo_tags::{PhotoTags, RECENT_GROUP};
use crate::tags::state::TagDialog;
use chairphoto_core::catalog::Catalog;
use chairphoto_model::library::session::SelectMods;
use gpui_kit::component::input::InputState;
use gpui_kit::component::WindowExt as _;
use gpui_kit::{SharedString, WeakEntity};

/// The vocabulary every test starts from, with their ids.
struct Seed {
    photos: Vec<i64>,
    place: i64,
    norway: i64,
    bergen: i64,
    people: i64,
}

/// A catalog with three photos and `Place/Norway/Bergen`, `People/Anna`, `Bergensbanen`;
/// photo 0 carries Bergen.
fn seed(c: &Catalog, root: &std::path::Path) -> Seed {
    let photos: Vec<i64> =
        (0..3).map(|i| c.upsert_photo(&root.join(format!("2026/p{i}.ARW")), None, 0, 1).unwrap().id).collect();
    let bergen = c.create_tag("Place/Norway/Bergen").unwrap();
    let norway = c.find_tag_id_by_path("Place/Norway").unwrap().unwrap();
    let place = c.find_tag_id_by_path("Place").unwrap().unwrap();
    let people = c.create_tag("People").unwrap();
    c.create_tag("People/Anna").unwrap();
    c.create_tag("Bergensbanen").unwrap();
    c.assign_tag(photos[0], bergen).unwrap();
    Seed { photos, place, norway, bergen, people }
}

fn open_tagged(app: &App, dir: &TempDir, name: &str, cx: &mut TestAppContext) -> Seed {
    let db = dir.0.join(format!("{name}.chairphoto"));
    let root = dir.0.join("photos");
    let catalog = Catalog::open(&db, &root).unwrap();
    let seed = seed(&catalog, &root);
    *app.state.catalog.lock().unwrap() = Some(catalog);
    app.state.send(CoreEvent::CatalogSwitched(db.to_string_lossy().to_string()));
    cx.run_until_parked();
    seed
}

fn with_catalog<R>(app: &App, f: impl FnOnce(&Catalog) -> R) -> R {
    f(app.state.catalog.lock().unwrap().as_ref().unwrap())
}

fn parent_of(app: &App, id: i64) -> Option<i64> {
    with_catalog(app, |c| c.get_tag(id).unwrap().parent_id)
}

fn panel(app: &App, cx: &mut TestAppContext) -> Entity<TagPanel> {
    app.wired.root.as_ref().unwrap().read_with(cx, |r, _| r.tag_panel.clone())
}

fn photo_tags(app: &App, cx: &mut TestAppContext) -> Entity<PhotoTags> {
    app.wired.root.as_ref().unwrap().read_with(cx, |r, _| r.photo_tags.clone())
}

fn scope_tag(app: &App, cx: &mut TestAppContext) -> Option<i64> {
    app.wired.shell.read_with(cx, |s, _| s.library.scope().tag_id)
}

fn render(app: &App, cx: &mut TestAppContext) {
    cx.update_window(app.window(), |_, window, cx| window.render_frame(cx)).unwrap();
    cx.run_until_parked();
}

fn set_input(app: &App, input: &Entity<InputState>, text: &str, cx: &mut TestAppContext) {
    let (input, text) = (input.clone(), text.to_string());
    cx.update_window(app.window(), |_, window, cx| input.update(cx, |i, cx| i.set_value(text, window, cx))).unwrap();
    cx.run_until_parked();
}

/// Focus `input`, then press `key` through the real keymap.
fn press_in(app: &App, input: &Entity<InputState>, key: &str, cx: &mut TestAppContext) {
    let input = input.clone();
    cx.update_window(app.window(), |_, window, cx| {
        let focus = gpui_kit::Focusable::focus_handle(input.read(cx), cx);
        window.focus(&focus, cx);
        window.render_frame(cx);
        window.press(key, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

/// Let a dialog's opening animation finish (see `storage_tests::settle`).
fn settle(app: &App, cx: &mut TestAppContext) {
    std::thread::sleep(std::time::Duration::from_millis(300));
    render(app, cx);
}

fn last_dialog(app: &App, cx: &mut TestAppContext) -> TagDialog {
    settle(app, cx);
    app.wired.tags.read_with(cx, |t, _| t.last_dialog.clone()).expect("a tag dialog opened")
}

fn up<T: 'static>(weak: WeakEntity<T>) -> Entity<T> {
    weak.upgrade().expect("the dialog's view is alive")
}

fn has_dialog(app: &App, cx: &mut TestAppContext) -> bool {
    cx.update_window(app.window(), |_, window, cx| window.has_active_dialog(cx)).unwrap()
}

fn right_click_menu_row(app: &App, row: &'static str, label: &str, cx: &mut TestAppContext) {
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.right_click(row, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        let mut menu = window.within("popup-menu");
        let index = (0..20)
            .find(|&i| menu.try_find(i).and_then(|e| e.label().map(|l| l == label)).unwrap_or(false))
            .unwrap_or_else(|| panic!("no menu row {label:?}"));
        menu.click(index, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn select_photo(app: &App, id: i64, cx: &mut TestAppContext) {
    app.wired.shell.update(cx, |s, cx| s.select_with(cx, |l| l.select(id, SelectMods::default())));
    cx.run_until_parked();
}

// --- the panel ----------------------------------------------------------------------------

/// The tree loads with the catalog, in path order with counts; a row filters the Library to
/// the tag (React's `selectTag`), All photos clears it.
#[gpui_kit::test]
fn the_panel_lists_the_tree_and_a_row_filters_the_library(cx: &mut TestAppContext) {
    let dir = TempDir::new("tags-panel");
    let app = start(cx);
    let s = open_tagged(&app, &dir, "a", cx);
    app.wired.tags.read_with(cx, |t, _| {
        assert!(t.loaded);
        let paths: Vec<&str> = t.tags.iter().map(|t| t.tag.full_path.as_str()).collect();
        assert_eq!(paths, ["Bergensbanen", "People", "People/Anna", "Place", "Place/Norway", "Place/Norway/Bergen"]);
        assert_eq!(t.tag(s.place).unwrap().photo_count, 1, "a parent counts its descendants' photos");
    });
    let id: &'static str = Box::leak(format!("tag-filter-{}", s.bergen).into_boxed_str());
    click(&app, id, cx);
    assert_eq!(scope_tag(&app, cx), Some(s.bergen));
    app.wired.shell.read_with(cx, |sh, _| assert_eq!(sh.library.photo_ids(), vec![s.photos[0]], "the grid shows the tagged photo"));
    click(&app, "tag-all", cx);
    assert_eq!(scope_tag(&app, cx), None);
}

/// **Drag-reorder.** Dragging a row onto another reparents it (GPUI `on_drag`/`on_drop`);
/// onto All photos moves it to the top level; onto its own descendant is refused with the
/// backend's reason and moves nothing.
#[gpui_kit::test]
fn dragging_a_row_reparents_the_tag(cx: &mut TestAppContext) {
    let dir = TempDir::new("tags-drag");
    let app = start(cx);
    let s = open_tagged(&app, &dir, "a", cx);
    let drag = |from: i64, to: &str, cx: &mut TestAppContext| {
        let from = format!("tag-row-{from}");
        let to = to.to_string();
        cx.update_window(app.window(), |_, window, cx| {
            window.render_frame(cx);
            window.drag_to(SharedString::from(from), SharedString::from(to), cx);
        })
        .unwrap();
        cx.run_until_parked();
    };
    drag(s.bergen, &format!("tag-row-{}", s.people), cx);
    assert_eq!(parent_of(&app, s.bergen), Some(s.people), "Bergen moved under People");
    app.wired.tags.read_with(cx, |t, _| {
        assert!(t.tags.iter().any(|t| t.tag.full_path == "People/Bergen"), "the tree was re-read");
    });

    drag(s.place, &format!("tag-row-{}", s.norway), cx);
    assert_eq!(parent_of(&app, s.place), None, "a tag cannot move under its own child");
    assert_eq!(status(&app, cx), "Move failed: cannot move a tag into itself or one of its descendants");

    drag(s.norway, "tag-all", cx);
    assert_eq!(parent_of(&app, s.norway), None, "onto All photos = the top level");
}

/// **Search.** Typing ranks the deepest match first; ↓ moves the highlight; Enter picks it:
/// the Library filters to it, its collapsed ancestors expand and the box clears. Escape
/// clears a query.
#[gpui_kit::test]
fn search_picks_the_highlighted_tag_and_expands_its_ancestors(cx: &mut TestAppContext) {
    let dir = TempDir::new("tags-search");
    let app = start(cx);
    let s = open_tagged(&app, &dir, "a", cx);
    let panel = panel(&app, cx);
    click(&app, "tag-collapse-all", cx);
    panel.read_with(cx, |p, _| assert!(p.collapsed.contains(&s.place) && p.collapsed.contains(&s.norway)));
    render(&app, cx);
    cx.update_window(app.window(), |_, window, _| {
        assert!(window.try_find(format!("tag-row-{}", s.bergen)).is_none(), "hidden under a collapsed parent");
    })
    .unwrap();

    let search = panel.read_with(cx, |p, _| p.search.clone());
    set_input(&app, &search, "berg", cx);
    let hits: Vec<String> =
        cx.update(|cx| panel.read(cx).matches(cx)).iter().map(|t| t.tag.full_path.clone()).collect();
    assert_eq!(hits, ["Place/Norway/Bergen", "Bergensbanen"]);
    press_in(&app, &search, "down", cx);
    panel.read_with(cx, |p, _| assert_eq!(p.highlight, 1));
    press_in(&app, &search, "up", cx);
    press_in(&app, &search, "enter", cx);
    render(&app, cx);
    assert_eq!(scope_tag(&app, cx), Some(s.bergen));
    panel.read_with(cx, |p, cx| {
        assert!(!p.collapsed.contains(&s.place) && !p.collapsed.contains(&s.norway), "ancestors expanded");
        assert_eq!(p.search.read(cx).value(), "", "the box cleared");
    });
    cx.update_window(app.window(), |_, window, _| {
        assert!(window.find(format!("tag-row-{}", s.bergen)).visible(), "the picked row is on screen");
    })
    .unwrap();

    set_input(&app, &search, "zzz", cx);
    render(&app, cx);
    cx.update_window(app.window(), |_, window, _| assert!(window.try_find("tag-search-none").is_some())).unwrap();
    press_in(&app, &search, "escape", cx);
    panel.read_with(cx, |p, cx| assert_eq!(p.search.read(cx).value(), "", "Escape cleared the query"));
}

/// The context menu's privacy verbs: the tag (and with "incl. sub-tags" its subtree) turns
/// private, and the status line counts what changed.
#[gpui_kit::test]
fn the_context_menu_makes_a_subtree_private(cx: &mut TestAppContext) {
    let dir = TempDir::new("tags-menu");
    let app = start(cx);
    let s = open_tagged(&app, &dir, "a", cx);
    let row: &'static str = Box::leak(format!("tag-row-{}", s.place).into_boxed_str());
    right_click_menu_row(&app, row, "Make private incl. sub-tags", cx);
    assert_eq!(status(&app, cx), "3 tags marked private.");
    assert!(with_catalog(&app, |c| c.tag_private(s.bergen).unwrap()));
    right_click_menu_row(&app, row, "Move to top level", cx); // disabled: no-op
    assert_eq!(parent_of(&app, s.place), None);
}

// --- dialogs ------------------------------------------------------------------------------

/// The tag editor: rename (Enter), a refused rename reverts the field and says why,
/// synonyms and translations are added and show in the export preview, the organizational
/// toggle empties it, and Delete asks once more before deleting and closing.
#[gpui_kit::test]
fn the_tag_editor_renames_adds_terms_and_deletes(cx: &mut TestAppContext) {
    let dir = TempDir::new("tags-editor");
    let app = start(cx);
    let s = open_tagged(&app, &dir, "a", cx);
    let edit: &'static str = Box::leak(format!("tag-edit-{}", s.bergen).into_boxed_str());
    click(&app, edit, cx);
    let TagDialog::Editor(editor) = last_dialog(&app, cx) else { panic!("the editor") };
    let editor = up(editor);
    assert_eq!(app.wired.shell.read_with(cx, |sh, _| sh.editing_tag), Some(s.bergen), "module panels see the tag");

    let name = editor.read_with(cx, |e, _| e.name.clone());
    set_input(&app, &name, "Bjørgvin", cx);
    press_in(&app, &name, "enter", cx);
    assert_eq!(with_catalog(&app, |c| c.get_tag(s.bergen).unwrap().full_path), "Place/Norway/Bjørgvin");

    // A sibling collision: two Place/Norway/Oslo.
    with_catalog(&app, |c| c.create_tag("Place/Norway/Oslo").unwrap());
    set_input(&app, &name, "Oslo", cx);
    press_in(&app, &name, "enter", cx);
    editor.read_with(cx, |e, cx| {
        assert!(e.error.is_some(), "the refusal is shown");
        assert_eq!(e.name.read(cx).value(), "Bjørgvin", "the field reverted to the committed name");
    });

    let (syn, tr_lang, tr_text) = editor.read_with(cx, |e, _| (e.syn_text.clone(), e.tr_lang.clone(), e.tr_text.clone()));
    set_input(&app, &syn, "Bergen city", cx);
    press_in(&app, &syn, "enter", cx);
    set_input(&app, &tr_lang, "nb", cx);
    set_input(&app, &tr_text, "Bergen by", cx);
    click(&app, "tag-editor-tr-add", cx);
    editor.read_with(cx, |e, _| {
        assert_eq!(e.terms.len(), 2);
        assert!(e.preview.contains(&"Bergen city".to_string()), "{:?}", e.preview);
    });
    click(&app, "tag-editor-preview-nb", cx);
    editor.read_with(cx, |e, _| assert!(e.preview.contains(&"Bergen by".to_string()), "{:?}", e.preview));

    click(&app, "tag-editor-organizational", cx);
    assert!(!with_catalog(&app, |c| c.tag_exportable(s.bergen).unwrap()));
    editor.read_with(cx, |e, _| assert!(e.preview.is_empty(), "an organizational tag exports nothing"));

    click(&app, "tag-editor-delete", cx);
    assert!(with_catalog(&app, |c| c.get_tag(s.bergen).is_ok()), "the first press only asks");
    click(&app, "tag-editor-delete", cx);
    assert!(with_catalog(&app, |c| c.get_tag(s.bergen).is_err()));
    assert!(!has_dialog(&app, cx), "deleting closes the editor");
    drop(editor);
    settle(&app, cx);
    assert_eq!(app.wired.shell.read_with(cx, |sh, _| sh.editing_tag), None, "a closed editor names no tag");
}

/// New tags: an indented paste under a parent previews and creates every path.
#[gpui_kit::test]
fn new_child_tags_create_a_pasted_hierarchy(cx: &mut TestAppContext) {
    let dir = TempDir::new("tags-create");
    let app = start(cx);
    let s = open_tagged(&app, &dir, "a", cx);
    let row: &'static str = Box::leak(format!("tag-row-{}", s.people).into_boxed_str());
    right_click_menu_row(&app, row, "New child tags…", cx);
    let TagDialog::Create(create) = last_dialog(&app, cx) else { panic!("the create dialog") };
    let create = up(create);
    let text = create.read_with(cx, |c, _| c.text.clone());
    cx.update_window(app.window(), |_, window, cx| text.update(cx, |t, cx| t.set_value("Family\n  Ola\n  Kari", window, cx))).unwrap();
    render(&app, cx);
    assert_eq!(cx.update(|cx| create.read(cx).paths(cx)), ["People/Family", "People/Family/Ola", "People/Family/Kari"]);
    click(&app, "tag-create-run", cx);
    assert!(with_catalog(&app, |c| c.find_tag_id_by_path("People/Family/Kari").unwrap().is_some()));
    assert!(!has_dialog(&app, cx));
}

/// Merge: the dry run changes nothing; Merge commits it and reports on the status line.
#[gpui_kit::test]
fn merge_previews_then_commits(cx: &mut TestAppContext) {
    let dir = TempDir::new("tags-merge");
    let app = start(cx);
    let s = open_tagged(&app, &dir, "a", cx);
    let row: &'static str = Box::leak(format!("tag-row-{}", s.bergen).into_boxed_str());
    right_click_menu_row(&app, row, "Merge into…", cx);
    let TagDialog::Merge(merge) = last_dialog(&app, cx) else { panic!("the merge dialog") };
    let merge = up(merge);
    let target: &'static str = Box::leak(format!("tag-merge-into-{}", s.people).into_boxed_str());
    click(&app, target, cx);
    merge.read_with(cx, |m, _| assert_eq!(m.preview.as_ref().unwrap().photos_retagged, 1));
    assert!(with_catalog(&app, |c| c.get_tag(s.bergen).is_ok()), "a preview writes nothing");
    click(&app, "tag-merge-commit", cx);
    assert!(with_catalog(&app, |c| c.get_tag(s.bergen).is_err()));
    assert_eq!(status(&app, cx), "Merged Place/Norway/Bergen into People — 1 photo moved.");
    assert!(!has_dialog(&app, cx));
}

/// Split off the selection: the menu offers it only with a selection; preview, then split.
#[gpui_kit::test]
fn split_moves_the_selected_photos_to_a_new_tag(cx: &mut TestAppContext) {
    let dir = TempDir::new("tags-split");
    let app = start(cx);
    let s = open_tagged(&app, &dir, "a", cx);
    select_photo(&app, s.photos[0], cx);
    let row: &'static str = Box::leak(format!("tag-row-{}", s.bergen).into_boxed_str());
    right_click_menu_row(&app, row, "Split off 1 selected…", cx);
    let TagDialog::Split(split) = last_dialog(&app, cx) else { panic!("the split dialog") };
    let split = up(split);
    let path = split.read_with(cx, |s, _| s.path.clone());
    set_input(&app, &path, "Venue/Grieghallen", cx);
    press_in(&app, &path, "enter", cx);
    split.read_with(cx, |s, _| assert_eq!(s.preview.as_ref().unwrap().photos_moved, 1));
    click(&app, "tag-split-run", cx);
    let venue = with_catalog(&app, |c| c.find_tag_id_by_path("Venue/Grieghallen").unwrap()).expect("created");
    let on_photo: Vec<i64> = with_catalog(&app, |c| c.get_photo_tags(s.photos[0]).unwrap().iter().map(|t| t.id).collect());
    assert_eq!(on_photo, [venue]);
    assert_eq!(status(&app, cx), "1 photo moved to Venue/Grieghallen.");
}

// --- the inspector's tagging block ------------------------------------------------------------

/// Add-tag creates a typed path on every target; × removes from every target; copy and
/// paste carry tags to another photo; Recently used lists what was applied.
#[gpui_kit::test]
fn the_tagging_block_adds_removes_copies_and_pastes(cx: &mut TestAppContext) {
    let dir = TempDir::new("tags-block");
    let app = start(cx);
    let s = open_tagged(&app, &dir, "a", cx);
    select_photo(&app, s.photos[0], cx);
    click(&app, "inspector-tab-tags", cx);
    let block = photo_tags(&app, cx);
    block.read_with(cx, |b, _| {
        assert_eq!(b.target.active, Some(s.photos[0]));
        assert_eq!(b.assigned.iter().map(|t| t.id).collect::<Vec<_>>(), [s.bergen]);
    });

    let input = block.read_with(cx, |b, _| b.input.clone());
    set_input(&app, &input, "Events/Festival", cx);
    press_in(&app, &input, "enter", cx);
    let festival = with_catalog(&app, |c| c.find_tag_id_by_path("Events/Festival").unwrap()).expect("created");
    block.read_with(cx, |b, _| assert!(b.assigned.iter().any(|t| t.id == festival), "the chip shows"));

    // Suggestions skip assigned tags; Enter assigns the highlighted one.
    set_input(&app, &input, "anna", cx);
    assert_eq!(cx.update(|cx| block.read(cx).suggestions(cx)).len(), 1);
    press_in(&app, &input, "enter", cx);
    let anna = with_catalog(&app, |c| c.find_tag_id_by_path("People/Anna").unwrap().unwrap());
    assert!(with_catalog(&app, |c| c.get_photo_tags(s.photos[0]).unwrap().iter().any(|t| t.id == anna)));

    click(&app, "photo-tags-copy", cx);
    assert_eq!(status(&app, cx), "Copied 3 tag(s)");
    select_photo(&app, s.photos[1], cx);
    click(&app, "photo-tags-paste", cx);
    assert_eq!(status(&app, cx), "Pasted 3 tag(s) onto 1 photo(s)");
    assert_eq!(with_catalog(&app, |c| c.get_photo_tags(s.photos[1]).unwrap().len()), 3);

    let remove: &'static str = Box::leak(format!("photo-tag-remove-{anna}").into_boxed_str());
    click(&app, remove, cx);
    assert!(!with_catalog(&app, |c| c.get_photo_tags(s.photos[1]).unwrap().iter().any(|t| t.id == anna)));

    block.read_with(cx, |b, _| {
        assert_eq!(b.group, RECENT_GROUP);
        assert!(!b.members.is_empty(), "Recently used lists the tags applied");
    });
}

/// The groups manager: a new group, a member added by path (created if new), and the
/// quick-tag button assigns it to the selection.
#[gpui_kit::test]
fn quick_tag_groups_are_managed_and_assign(cx: &mut TestAppContext) {
    let dir = TempDir::new("tags-groups");
    let app = start(cx);
    let s = open_tagged(&app, &dir, "a", cx);
    select_photo(&app, s.photos[2], cx);
    click(&app, "inspector-tab-tags", cx);
    click(&app, "quick-groups-manage", cx);
    let TagDialog::Groups(manager) = last_dialog(&app, cx) else { panic!("the groups manager") };
    let manager = up(manager);
    let (new_group, new_member) = manager.read_with(cx, |m, _| (m.new_group.clone(), m.new_member.clone()));
    set_input(&app, &new_group, "Street", cx);
    press_in(&app, &new_group, "enter", cx);
    set_input(&app, &new_member, "Street/Candid", cx);
    press_in(&app, &new_member, "enter", cx);
    let group = manager.read_with(cx, |m, _| {
        assert_eq!(m.members.iter().map(|t| t.full_path.as_str()).collect::<Vec<_>>(), ["Street/Candid"]);
        m.active.unwrap()
    });
    cx.update_window(app.window(), |_, window, cx| window.close_dialog(cx)).unwrap();
    cx.run_until_parked();

    let chip: &'static str = Box::leak(format!("quick-group-{group}").into_boxed_str());
    click(&app, chip, cx);
    let candid = with_catalog(&app, |c| c.find_tag_id_by_path("Street/Candid").unwrap().unwrap());
    let button: &'static str = Box::leak(format!("quick-tag-{candid}").into_boxed_str());
    click(&app, button, cx);
    assert!(with_catalog(&app, |c| c.get_photo_tags(s.photos[2]).unwrap().iter().any(|t| t.id == candid)));
}

// --- catalog switches ---------------------------------------------------------------------

/// **Forced interleaving.** A tag write is started, then the catalog switches to another
/// whose ids collide (same seed), *then* the worker runs: it writes nothing — in either
/// catalog — because the open catalog is no longer the one the view showed. The clipboard and
/// tree of the old catalog are gone.
#[gpui_kit::test]
fn a_write_started_before_a_catalog_switch_writes_nothing(cx: &mut TestAppContext) {
    let dir = TempDir::new("tags-switch");
    let app = start(cx);
    let s = open_tagged(&app, &dir, "a", cx);
    app.wired.tags.update(cx, |t, cx| t.copy(vec![s.people], cx));
    // Start the write; nothing has run yet.
    app.wired.tags.update(cx, |t, cx| t.assign(vec![s.photos[1]], s.people, cx));
    // Swap in another catalog with the same ids before the worker gets the lock.
    let other = dir.0.join("b.chairphoto");
    let root = dir.0.join("photos-b");
    let catalog = Catalog::open(&other, &root).unwrap();
    let s2 = seed(&catalog, &root);
    assert_eq!((s2.photos[1], s2.people), (s.photos[1], s.people), "the ids collide");
    let old = app.state.catalog.lock().unwrap().replace(catalog).unwrap();
    app.state.send(CoreEvent::CatalogSwitched(other.to_string_lossy().to_string()));
    cx.run_until_parked();
    assert!(with_catalog(&app, |c| c.get_photo_tags(s.photos[1]).unwrap().is_empty()), "nothing written in the new catalog");
    assert!(old.get_photo_tags(s.photos[1]).unwrap().is_empty(), "nor in the old one");
    app.wired.tags.read_with(cx, |t, _| {
        assert!(t.clipboard.is_empty(), "the old catalog's copied ids are gone");
        assert!(t.loaded && t.tags.len() == 6, "the new catalog's tree loaded");
    });
}

/// A dialog's read that lands after a catalog switch is dropped: the merge preview of the old
/// catalog never shows.
#[gpui_kit::test]
fn a_dialog_read_landing_after_a_switch_is_dropped(cx: &mut TestAppContext) {
    let dir = TempDir::new("tags-switch-read");
    let app = start(cx);
    let s = open_tagged(&app, &dir, "a", cx);
    let row: &'static str = Box::leak(format!("tag-row-{}", s.bergen).into_boxed_str());
    right_click_menu_row(&app, row, "Merge into…", cx);
    let TagDialog::Merge(merge) = last_dialog(&app, cx) else { panic!("the merge dialog") };
    let merge = up(merge);
    let target = app.wired.tags.read_with(cx, |t, _| t.tag(s.people).unwrap().clone());
    merge.update(cx, |m, cx| m.pick(target, cx));
    // The switch reaches the tag state before the preview's result does.
    app.wired.tags.update(cx, |t, cx| t.on_catalog_switched(cx));
    cx.run_until_parked();
    merge.read_with(cx, |m, _| assert!(m.preview.is_none() && m.error.is_none(), "the old catalog's preview landed"));
}

// --- dialogs bound to the catalog they opened on (#107 review) ----------------------------

/// The core switches to a second catalog seeded the same way — every tag, photo and group id
/// collides — with `catalog:switched` delivered (and the new tree read) or still on its way.
fn switch_to_twin(app: &App, dir: &TempDir, delivered: bool, cx: &mut TestAppContext) -> Seed {
    let root = dir.0.join("photos-b");
    let b = Catalog::open(&dir.0.join("b.chairphoto"), &root).unwrap();
    let s = seed(&b, &root);
    b.create_tag_group("Street").unwrap();
    core_switch(app, b);
    if delivered {
        deliver_switch(app, cx);
    }
    s
}

/// What a refused dialog write shows, when the dialog is still there to show it.
fn changed() -> Option<String> {
    Some(chairphoto_core::app::CATALOG_CHANGED.to_string())
}

/// **Forced interleaving** (#107 review, finding 1). Merge opened (and previewed) in one
/// catalog; the core switches to its twin; Merge is confirmed. Nothing merges in the new
/// catalog. With the event delivered, the dialog has closed itself; the confirm that raced
/// it is refused too.
fn merge_across_a_switch(delivered: bool, cx: &mut TestAppContext) {
    let dir = TempDir::new("tags-merge-switch");
    let app = start(cx);
    let s = open_tagged(&app, &dir, "a", cx);
    let row: &'static str = Box::leak(format!("tag-row-{}", s.bergen).into_boxed_str());
    right_click_menu_row(&app, row, "Merge into…", cx);
    let TagDialog::Merge(merge) = last_dialog(&app, cx) else { panic!("the merge dialog") };
    let merge = up(merge);
    let target = app.wired.tags.read_with(cx, |t, _| t.tag(s.people).unwrap().clone());
    merge.update(cx, |m, cx| m.pick(target, cx));
    cx.run_until_parked();
    assert!(merge.read_with(cx, |m, _| m.preview.is_some()));
    let b = switch_to_twin(&app, &dir, delivered, cx);
    assert_eq!(b.bergen, s.bergen);
    if delivered {
        settle(&app, cx);
        assert!(!has_dialog(&app, cx), "the switch closed the dialog");
    }
    merge.update(cx, |m, cx| m.commit(cx));
    cx.run_until_parked();
    assert!(with_catalog(&app, |c| c.get_tag(b.bergen).is_ok()), "the old catalog's merge removed the new catalog's tag");
    if !delivered {
        assert_eq!(merge.read_with(cx, |m, _| m.error.clone()), changed());
    }
}

#[gpui_kit::test]
fn merge_never_reaches_the_new_catalog_before_the_switch_event(cx: &mut TestAppContext) {
    merge_across_a_switch(false, cx);
}

#[gpui_kit::test]
fn merge_never_reaches_the_new_catalog_after_the_switch_event(cx: &mut TestAppContext) {
    merge_across_a_switch(true, cx);
}

/// Split: opened over the selection in one catalog, run after the core switched.
fn split_across_a_switch(delivered: bool, cx: &mut TestAppContext) {
    let dir = TempDir::new("tags-split-switch");
    let app = start(cx);
    let s = open_tagged(&app, &dir, "a", cx);
    select_photo(&app, s.photos[0], cx);
    let row: &'static str = Box::leak(format!("tag-row-{}", s.bergen).into_boxed_str());
    right_click_menu_row(&app, row, "Split off 1 selected…", cx);
    let TagDialog::Split(split) = last_dialog(&app, cx) else { panic!("the split dialog") };
    let split = up(split);
    let path = split.read_with(cx, |s, _| s.path.clone());
    set_input(&app, &path, "Venue/Grieghallen", cx);
    split.update(cx, |s, cx| s.run(true, cx));
    cx.run_until_parked();
    assert!(split.read_with(cx, |s, _| s.preview.is_some()), "previewed: the split may run");
    let b = switch_to_twin(&app, &dir, delivered, cx);
    if delivered {
        settle(&app, cx);
        assert!(!has_dialog(&app, cx), "the switch closed the dialog");
    }
    split.update(cx, |s, cx| s.run(false, cx));
    cx.run_until_parked();
    assert!(with_catalog(&app, |c| c.find_tag_id_by_path("Venue/Grieghallen").unwrap().is_none()), "split in the new catalog");
    let on_photo: Vec<i64> = with_catalog(&app, |c| c.get_photo_tags(b.photos[0]).unwrap().iter().map(|t| t.id).collect());
    assert_eq!(on_photo, [b.bergen]);
    if !delivered {
        assert_eq!(split.read_with(cx, |s, _| s.error.clone()), changed());
    }
}

#[gpui_kit::test]
fn split_never_reaches_the_new_catalog_before_the_switch_event(cx: &mut TestAppContext) {
    split_across_a_switch(false, cx);
}

#[gpui_kit::test]
fn split_never_reaches_the_new_catalog_after_the_switch_event(cx: &mut TestAppContext) {
    split_across_a_switch(true, cx);
}

/// The tag editor: Delete asked in one catalog, confirmed after the core switched.
fn editor_delete_across_a_switch(delivered: bool, cx: &mut TestAppContext) {
    let dir = TempDir::new("tags-editor-switch");
    let app = start(cx);
    let s = open_tagged(&app, &dir, "a", cx);
    let edit: &'static str = Box::leak(format!("tag-edit-{}", s.bergen).into_boxed_str());
    click(&app, edit, cx);
    let TagDialog::Editor(editor) = last_dialog(&app, cx) else { panic!("the editor") };
    let editor = up(editor);
    editor.update(cx, |e, cx| e.delete(cx));
    assert!(editor.read_with(cx, |e, _| e.confirm_delete), "the first press asks");
    let b = switch_to_twin(&app, &dir, delivered, cx);
    if delivered {
        settle(&app, cx);
        assert!(!has_dialog(&app, cx), "the switch closed the editor");
    }
    editor.update(cx, |e, cx| e.delete(cx));
    cx.run_until_parked();
    assert!(with_catalog(&app, |c| c.get_tag(b.bergen).is_ok()), "the old catalog's delete removed the new catalog's tag");
    if !delivered {
        assert_eq!(editor.read_with(cx, |e, _| e.error.clone()), changed());
    }
}

#[gpui_kit::test]
fn editor_delete_never_reaches_the_new_catalog_before_the_switch_event(cx: &mut TestAppContext) {
    editor_delete_across_a_switch(false, cx);
}

#[gpui_kit::test]
fn editor_delete_never_reaches_the_new_catalog_after_the_switch_event(cx: &mut TestAppContext) {
    editor_delete_across_a_switch(true, cx);
}

/// The groups manager: a group chosen in one catalog, deleted after the core switched.
fn groups_delete_across_a_switch(delivered: bool, cx: &mut TestAppContext) {
    let dir = TempDir::new("tags-groups-switch");
    let app = start(cx);
    let s = open_tagged(&app, &dir, "a", cx);
    let group = with_catalog(&app, |c| c.create_tag_group("Street").unwrap());
    select_photo(&app, s.photos[2], cx);
    click(&app, "inspector-tab-tags", cx);
    click(&app, "quick-groups-manage", cx);
    let TagDialog::Groups(manager) = last_dialog(&app, cx) else { panic!("the groups manager") };
    let manager = up(manager);
    manager.update(cx, |m, cx| m.select(group, cx));
    cx.run_until_parked();
    switch_to_twin(&app, &dir, delivered, cx);
    if delivered {
        settle(&app, cx);
        assert!(!has_dialog(&app, cx), "the switch closed the manager");
    }
    manager.update(cx, |m, cx| m.delete_active(cx));
    cx.run_until_parked();
    let groups = with_catalog(&app, |c| c.list_tag_groups().unwrap());
    assert!(groups.iter().any(|g| g.id == group), "the old catalog's delete removed the new catalog's group");
}

#[gpui_kit::test]
fn groups_delete_never_reaches_the_new_catalog_before_the_switch_event(cx: &mut TestAppContext) {
    groups_delete_across_a_switch(false, cx);
}

#[gpui_kit::test]
fn groups_delete_never_reaches_the_new_catalog_after_the_switch_event(cx: &mut TestAppContext) {
    groups_delete_across_a_switch(true, cx);
}

/// Finding 3: the tagging block's nearby window is remembered in the catalog its tree came
/// from — after the core switched (the event not yet delivered) it is not written into the
/// new catalog.
#[gpui_kit::test]
fn the_nearby_window_is_not_written_into_the_new_catalog(cx: &mut TestAppContext) {
    let dir = TempDir::new("tags-window-switch");
    let app = start(cx);
    let s = open_tagged(&app, &dir, "a", cx);
    select_photo(&app, s.photos[0], cx);
    switch_to_twin(&app, &dir, false, cx);
    photo_tags(&app, cx).update(cx, |p, cx| p.set_window(600, cx));
    cx.run_until_parked();
    assert_eq!(with_catalog(&app, |c| c.get_setting(crate::tags::photo_tags::WINDOW_SETTING).unwrap()), None);
}
