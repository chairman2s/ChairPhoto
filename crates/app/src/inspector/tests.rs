//! Headless tests of the Photo inspector (#108) through the real wiring (`start` → `wire`
//! → the main window): what each tab shows, its writes, and who owns each result. Blocking
//! work (IPTC save, storage, editors) runs on `Runner::manual`, so a test decides when it
//! runs; editor jobs are never run here (that would launch a real editor) — their ownership
//! is driven through the same entry points the worker's result and the routed events use.

use super::*;
use crate::shell::state::InspectorTab;
use crate::tests::{open_catalog_with_photos, start, App, TempDir};
use chairphoto_core::app::EventSink as _;
use chairphoto_core::catalog::PromotedMetadata;
use chairphoto_core::image_pool::ImageKind;
use chairphoto_core::photo_signals::{BurstSignal, StackSignal};
use chairphoto_model::library::session::SelectMods;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{SharedString, TestAppContext};

fn inspector(app: &App, cx: &mut TestAppContext) -> Entity<PhotoInspector> {
    app.wired.root.as_ref().expect("the main window opened").read_with(cx, |r, _| r.inspector.clone())
}

fn select(app: &App, id: i64, mods: SelectMods, cx: &mut TestAppContext) {
    app.wired.shell.update(cx, |s, cx| s.select_with(cx, |l| l.select(id, mods)));
    cx.run_until_parked();
}

fn tab(app: &App, tab: InspectorTab, cx: &mut TestAppContext) {
    app.wired.shell.update(cx, |s, cx| s.set_inspector_tab(tab, cx));
    cx.run_until_parked();
}

fn render(app: &App, cx: &mut TestAppContext) {
    cx.update_window(app.window(), |_, window, cx| window.render_frame(cx)).unwrap();
    cx.run_until_parked();
}

fn click(app: &App, id: impl Into<SharedString>, cx: &mut TestAppContext) {
    let id: SharedString = id.into();
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.click(id, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn present(app: &App, id: impl Into<SharedString>, cx: &mut TestAppContext) -> bool {
    let id: SharedString = id.into();
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.try_find(id).is_some()
    })
    .unwrap()
}

fn aria(app: &App, id: &'static str, cx: &mut TestAppContext) -> Option<String> {
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.find(id).label().map(str::to_string)
    })
    .unwrap()
}

/// Run every queued storage-runner job, then let the UI thread take the results.
fn work(cx: &mut TestAppContext) {
    loop {
        let ran = cx.update(|cx| Runner::get(cx).run_pending());
        cx.run_until_parked();
        if ran == 0 {
            return;
        }
    }
}

fn catalog<T>(app: &App, f: impl FnOnce(&chairphoto_core::catalog::Catalog) -> T) -> T {
    f(app.state.catalog.lock().unwrap().as_ref().unwrap())
}

fn set_input(app: &App, input: &Entity<InputState>, text: &str, cx: &mut TestAppContext) {
    let (input, text) = (input.clone(), text.to_string());
    cx.update_window(app.window(), |_, window, cx| input.update(cx, |i, cx| i.set_value(text, window, cx))).unwrap();
    cx.run_until_parked();
}

// --- pure helpers ----------------------------------------------------------------------

#[test]
fn stars_and_swatches_toggle_off_on_the_current_value() {
    assert_eq!(next_rating(3, 3), 0, "the current star clears");
    assert_eq!(next_rating(3, 5), 5);
    assert_eq!(next_label("Red", "Red").as_deref(), Some(""), "the current label clears");
    assert_eq!(next_label("Red", "Blue").as_deref(), Some("Blue"));
    assert_eq!(next_label("", ""), None, "Clear on an unlabelled photo writes nothing");
    assert_eq!(next_label("Red", "").as_deref(), Some(""));
}

#[test]
fn the_exif_line_joins_what_the_file_carried() {
    let mut p = test_photo();
    p.camera_model = Some("X-T5".into());
    p.aperture = Some(2.8);
    p.shutter_speed = Some("1/250".into());
    p.iso = Some(400);
    assert_eq!(exif_line(&p), "X-T5 · ƒ/2.8 · 1/250 · ISO 400");
    p.aperture = Some(8.0);
    p.camera_model = None;
    assert_eq!(exif_line(&p), "ƒ/8 · 1/250 · ISO 400");
}

fn test_photo() -> chairphoto_core::catalog::Photo {
    let dir = TempDir::new("inspector-photo");
    let c = chairphoto_core::catalog::Catalog::open(&dir.0.join("c.chairphoto"), &dir.0.join("photos")).unwrap();
    let id = c.upsert_photo(&dir.0.join("photos/a.ARW"), None, 0, 1).unwrap().id;
    c.get_photo(id).unwrap()
}

#[test]
fn publication_dates_and_metadata_groups() {
    assert_eq!(publication_date(0), "1970-01-01");
    assert_eq!(publication_date(1_709_164_800), "2024-02-29");
    let e = |g: &str, k: &str| MetadataEntry { key: k.into(), group_name: g.into(), value: "v".into() };
    let entries = [e("EXIF", "Make"), e("MakerNotes", "X"), e("EXIF", "Model")];
    let groups = metadata_groups(&entries);
    assert_eq!(groups.iter().map(|(g, i)| (g.as_str(), i.len())).collect::<Vec<_>>(), [("EXIF", 2), ("MakerNotes", 1)]);
    assert_eq!(render::titlecase("flickr"), "Flickr");
}

/// A stale stored flag and a truncated burst are said, never smoothed over.
#[test]
fn signals_say_a_stale_flag_and_a_truncated_burst() {
    let s = PhotoSignals {
        photo_id: 1,
        sharpness: None,
        burst: Some(BurstSignal {
            cluster_size: 3,
            time_group_size: 5,
            scored: 3,
            rank: Some(2),
            median: Some(100.0),
            cutoff: Some(60.0),
            soft_fraction: 0.6,
            verdict: None,
            stored_flag: Some("soft-in-burst".into()),
            stale: true,
            truncated: true,
            time_gap_secs: 15,
            hamming_threshold: 10,
            best: None,
            frames: vec![],
        }),
        stack: StackSignal { child_count: 1, parent_id: None },
        version_count: 2,
    };
    let lines = signals::lines(&s);
    let text = |l: &signals::Line| format!("{l:?}");
    let all = lines.iter().map(text).collect::<Vec<_>>().join("\n");
    assert!(all.contains("Frame 2 of 3 by sharpness, in a burst of 3+ (split from 5 frames shot together)."), "{all}");
    assert!(all.contains("Cluster median 100 · soft below 60.0 (60% of the median)"), "{all}");
    assert!(all.contains("says Soft in burst, but this burst now reads no flag"), "{all}");
    assert!(all.contains("lower bounds"), "{all}");
    assert!(all.contains("1 file stacked under this one"), "{all}");
    assert!(all.contains("2 edit versions"), "{all}");
}

// --- details -------------------------------------------------------------------------

/// "Select a photo" with none; with one, the details tab and its signals.
#[gpui_kit::test]
fn the_details_tab_follows_the_active_photo(cx: &mut TestAppContext) {
    let dir = TempDir::new("insp-details");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 2, cx);
    render(&app, cx);
    assert!(!present(&app, "star-1", cx), "no photo, no controls");
    select(&app, ids[0], SelectMods::default(), cx);
    assert!(present(&app, "star-1", cx));
    let insp = inspector(&app, cx);
    insp.read_with(cx, |i, _| {
        assert_eq!(i.photo_id, Some(ids[0]));
        assert_eq!(i.data.signals.load.ready().map(|s| s.photo_id), Some(ids[0]), "the signals were read");
    });
    select(&app, ids[1], SelectMods::default(), cx);
    insp.read_with(cx, |i, _| assert_eq!(i.data.signals.load.ready().map(|s| s.photo_id), Some(ids[1])));
}

/// A read asked for the photo shown before lands after the selection moved on: dropped, so
/// one photo's signals never show on another. The interleaving is forced: the read is issued
/// under the previous photo's generation after the current photo's own read has landed.
/// (Mutation-checked: without the generation guard in `read`, photo 0's answer replaces
/// photo 1's and this fails.)
#[gpui_kit::test]
fn a_late_read_for_the_previous_photo_is_dropped(cx: &mut TestAppContext) {
    let dir = TempDir::new("insp-late");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 2, cx);
    select(&app, ids[0], SelectMods::default(), cx);
    select(&app, ids[1], SelectMods::default(), cx);
    let insp = inspector(&app, cx);
    insp.read_with(cx, |i, _| assert_eq!(i.data.signals.load.ready().map(|s| s.photo_id), Some(ids[1])));
    let stale = ids[0];
    insp.update(cx, |i, cx| {
        let current = i.generation;
        i.generation = current - 1; // as it was while photo 0 was shown
        i.read(|t| &mut t.data.signals, move |c| chairphoto_core::photo_signals::explain_photo_signals(c, stale), cx);
        i.generation = current;
    });
    cx.run_until_parked();
    insp.read_with(cx, |i, _| {
        assert_eq!(i.photo_id, Some(ids[1]));
        assert_eq!(i.data.signals.load.ready().map(|s| s.photo_id), Some(ids[1]), "photo 0's late read was dropped");
    });
}

/// Of two reads of one slot for the same photo, only the newer lands, whichever finishes
/// last. (Mutation-checked: without the per-slot sequence guard in `read`, the earlier read
/// lands last on the test scheduler and this fails.)
#[gpui_kit::test]
fn of_two_reads_of_a_slot_only_the_newer_lands(cx: &mut TestAppContext) {
    let dir = TempDir::new("insp-seq");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 2, cx);
    select(&app, ids[0], SelectMods::default(), cx);
    let insp = inspector(&app, cx);
    let (a, b) = (ids[0], ids[1]);
    insp.update(cx, |i, cx| {
        i.read(|t| &mut t.data.signals, move |c| chairphoto_core::photo_signals::explain_photo_signals(c, b), cx);
        i.read(|t| &mut t.data.signals, move |c| chairphoto_core::photo_signals::explain_photo_signals(c, a), cx);
    });
    cx.run_until_parked();
    insp.read_with(cx, |i, _| assert_eq!(i.data.signals.load.ready().map(|s| s.photo_id), Some(a)));
}

/// Stars, pick and label write the photo shown — not the rest of the selection — through
/// the Library's write path; the current star and label toggle off. (Mutation-checked:
/// routing `rate` through `apply_mark`, the selection's path, marks both photos and fails.)
#[gpui_kit::test]
fn marks_write_the_photo_shown_only(cx: &mut TestAppContext) {
    let dir = TempDir::new("insp-marks");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 2, cx);
    select(&app, ids[1], SelectMods::default(), cx);
    select(&app, ids[0], SelectMods { ctrl: true, shift: false }, cx);
    app.wired.shell.read_with(cx, |s, _| {
        assert_eq!(s.library.selection().ids.len(), 2);
        assert_eq!(s.library.selection().active_id, Some(ids[0]));
    });
    click(&app, "star-4", cx);
    click(&app, "pick-pick", cx);
    click(&app, "swatch-Red", cx);
    let (rating, pick, label) = catalog(&app, |c| {
        let p = c.get_photo(ids[0]).unwrap();
        (p.rating, p.pick_state, p.label)
    });
    assert_eq!((rating, pick, label.as_str()), (4, PickState::Pick, "Red"));
    assert_eq!(catalog(&app, |c| c.get_photo(ids[1]).unwrap().rating), 0, "the other selected photo is untouched");
    click(&app, "star-4", cx);
    click(&app, "swatch-Red", cx);
    let p = catalog(&app, |c| c.get_photo(ids[0]).unwrap());
    assert_eq!((p.rating, p.label.as_str()), (0, ""), "clicking the current value clears it");
}

/// Orientation turns the stored override and drops the photo's cached images.
#[gpui_kit::test]
fn rotating_turns_the_photo_and_invalidates_its_images(cx: &mut TestAppContext) {
    let dir = TempDir::new("insp-rotate");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 1, cx);
    select(&app, ids[0], SelectMods::default(), cx);
    click(&app, "section-orientation", cx);
    click(&app, "rotate-right", cx);
    assert_eq!(catalog(&app, |c| c.photo_rotation(ids[0]).unwrap()), 90);
    click(&app, "rotate-180", cx);
    assert_eq!(catalog(&app, |c| c.photo_rotation(ids[0]).unwrap()), 270);
    let version = app.wired.images.read_with(cx, |s, _| s.key(ids[0], ImageKind::Thumb).version);
    assert_eq!(version, 2, "each turn bumps the photo's image version");
}

/// The Stack section lists the master and its children; Unstack returns a child to the grid.
#[gpui_kit::test]
fn the_stack_section_lists_and_unstacks(cx: &mut TestAppContext) {
    let dir = TempDir::new("insp-stack");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 2, cx);
    catalog(&app, |c| c.set_stack_parent(ids[1], ids[0]).unwrap());
    app.wired.model.update(cx, |m, cx| m.refresh(cx));
    cx.run_until_parked();
    assert_eq!(app.wired.shell.read_with(cx, |s, _| s.library.photo_ids()), vec![ids[0]], "the child is stacked away");
    select(&app, ids[0], SelectMods::default(), cx);
    assert!(present(&app, "section-stack", cx), "a stacked photo has a Stack section");
    click(&app, "section-stack", cx);
    assert!(present(&app, format!("stack-row-{}", ids[1]), cx));
    click(&app, format!("stack-unstack-{}", ids[1]), cx);
    assert_eq!(catalog(&app, |c| c.get_photo(ids[1]).unwrap().stack_parent_id), None);
    let mut rows = app.wired.shell.read_with(cx, |s, _| s.library.photo_ids());
    rows.sort();
    assert_eq!(rows, ids, "the child is back in the grid");
    assert!(!present(&app, "section-stack", cx), "no stack left");
}

/// IPTC: the form loads the photo's fields; Save is enabled only when dirty; saving writes
/// the catalog and the sidecar next to the original, on a worker.
#[gpui_kit::test]
fn iptc_saves_to_the_catalog_and_the_sidecar(cx: &mut TestAppContext) {
    let dir = TempDir::new("insp-iptc");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 1, cx);
    let original = catalog(&app, |c| c.require_photo_path(ids[0]).ok());
    assert!(original.is_none(), "no file yet");
    let path = dir.0.join("photos/2026/p0.ARW");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"raw").unwrap();
    select(&app, ids[0], SelectMods::default(), cx);
    click(&app, "section-iptc", cx);
    let insp = inspector(&app, cx);
    render(&app, cx);
    assert!(!insp.read_with(cx, |i, cx| i.iptc.dirty(cx)), "a freshly loaded form is clean");
    let headline = insp.read_with(cx, |i, _| i.iptc.fields[0].clone());
    set_input(&app, &headline, "Fjord at dawn", cx);
    assert!(insp.read_with(cx, |i, cx| i.iptc.dirty(cx)));
    // The button sits below the fold of the test window's inspector column; Save IPTC's
    // own handler is this call.
    insp.update(cx, |i, cx| i.save_iptc(cx));
    assert_eq!(aria(&app, "iptc-status", cx).as_deref(), Some("Saving…"));
    work(cx);
    assert_eq!(aria(&app, "iptc-status", cx).as_deref(), Some("Saved to sidecar"));
    assert_eq!(catalog(&app, |c| c.get_iptc(ids[0]).unwrap().headline), "Fjord at dawn");
    let sidecar = std::fs::read_to_string(dir.0.join("photos/2026/p0.ARW.xmp")).expect("the sidecar was written");
    assert!(sidecar.contains("Fjord at dawn"));
    assert!(!insp.read_with(cx, |i, cx| i.iptc.dirty(cx)), "saved values are the new baseline");
}

/// A save that a catalog switch overtook does not write its status into the new catalog's
/// inspector. Two guards drop it: the epoch in `run_blocking` and the save's own generation
/// check. (Mutation-checked: removing both fails this; removing either alone passes.)
#[gpui_kit::test]
fn a_save_overtaken_by_a_catalog_switch_is_dropped(cx: &mut TestAppContext) {
    let dir = TempDir::new("insp-iptc-switch");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 1, cx);
    select(&app, ids[0], SelectMods::default(), cx);
    click(&app, "section-iptc", cx);
    let insp = inspector(&app, cx);
    insp.update(cx, |i, cx| i.save_iptc(cx));
    app.state.send(CoreEvent::CatalogSwitched("other".into()));
    cx.run_until_parked();
    insp.update(cx, |i, _| i.iptc.status = "after switch".into());
    work(cx);
    insp.read_with(cx, |i, _| assert_eq!(i.iptc.status, "after switch", "the old save's result was dropped"));
}

/// Metadata: grouped, the default groups open, "No metadata" when there is none.
#[gpui_kit::test]
fn metadata_groups_render_and_toggle(cx: &mut TestAppContext) {
    let dir = TempDir::new("insp-meta");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 2, cx);
    let e = |g: &str, k: &str| MetadataEntry { key: k.into(), group_name: g.into(), value: "v".into() };
    catalog(&app, |c| {
        c.set_photo_metadata(ids[0], &PromotedMetadata::default(), &[e("EXIF", "Make"), e("MakerNotes", "Secret")]).unwrap()
    });
    select(&app, ids[0], SelectMods::default(), cx);
    click(&app, "section-metadata", cx);
    assert!(present(&app, "meta-group-EXIF", cx));
    let insp = inspector(&app, cx);
    insp.read_with(cx, |i, _| {
        assert!(i.meta_open.contains("EXIF"));
        assert!(!i.meta_open.contains("MakerNotes"), "the long tail starts collapsed");
    });
    click(&app, "meta-group-MakerNotes", cx);
    insp.read_with(cx, |i, _| assert!(i.meta_open.contains("MakerNotes")));
    select(&app, ids[1], SelectMods::default(), cx);
    assert!(!present(&app, "meta-group-EXIF", cx), "the other photo has none");
}

/// Storage: with no NAS configured, Back up queues the backup (React's fallback).
#[gpui_kit::test]
fn back_up_queues_when_the_nas_is_unavailable(cx: &mut TestAppContext) {
    let dir = TempDir::new("insp-storage");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 1, cx);
    select(&app, ids[0], SelectMods::default(), cx);
    let insp = inspector(&app, cx);
    insp.update(cx, |i, cx| i.back_up(cx));
    insp.read_with(cx, |i, _| assert_eq!(i.storage_msg.as_deref(), Some("Backing up…")));
    work(cx);
    insp.read_with(cx, |i, _| assert_eq!(i.storage_msg.as_deref(), Some("Queued (NAS offline)")));
    let queued = catalog(&app, |c| c.list_pending_operations().unwrap());
    assert!(queued.iter().any(|o| o.photo_id == ids[0] && o.kind == "backup"), "{queued:?}");
}

// --- versions ------------------------------------------------------------------------

/// Add (typed name, else "Version N"), choose, rename, duplicate, delete; the chosen version
/// belongs to its photo and falls back to Original when deleted or when the photo changes.
#[gpui_kit::test]
fn versions_are_listed_chosen_and_managed(cx: &mut TestAppContext) {
    let dir = TempDir::new("insp-versions");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 2, cx);
    select(&app, ids[0], SelectMods::default(), cx);
    tab(&app, InspectorTab::Versions, cx);
    click(&app, "version-add", cx);
    let insp = inspector(&app, cx);
    let name_input = insp.read_with(cx, |i, _| i.version_name.clone());
    set_input(&app, &name_input, "Square", cx);
    click(&app, "version-add", cx);
    let versions = catalog(&app, |c| c.list_versions(ids[0]).unwrap());
    assert_eq!(versions.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(), ["Version 1", "Square"]);
    let square = versions[1].clone();

    click(&app, format!("version-name-{}", square.id), cx);
    assert_eq!(app.wired.shell.read_with(cx, |s, _| s.active_version().map(|v| v.id)), Some(square.id));

    insp.update_in_window(&app, cx, |i, window, cx| i.start_rename(&square, window, cx));
    let rename = insp.read_with(cx, |i, _| i.renaming.as_ref().map(|r| r.1.clone())).expect("renaming");
    set_input(&app, &rename, "Insta square", cx);
    insp.update(cx, |i, cx| i.commit_rename(square.id, cx));
    cx.run_until_parked();
    assert_eq!(catalog(&app, |c| c.list_versions(ids[0]).unwrap()[1].name.clone()), "Insta square");

    click(&app, format!("version-dup-{}", square.id), cx);
    assert_eq!(catalog(&app, |c| c.list_versions(ids[0]).unwrap().len()), 3);

    click(&app, format!("version-del-{}", square.id), cx);
    assert_eq!(catalog(&app, |c| c.list_versions(ids[0]).unwrap().len()), 2);
    assert_eq!(
        app.wired.shell.read_with(cx, |s, _| s.active_version().map(|v| v.id)),
        None,
        "deleting the chosen version falls back to Original"
    );

    let first = catalog(&app, |c| c.list_versions(ids[0]).unwrap()[0].clone());
    click(&app, format!("version-name-{}", first.id), cx);
    select(&app, ids[1], SelectMods::default(), cx);
    select(&app, ids[0], SelectMods::default(), cx);
    assert_eq!(
        app.wired.shell.read_with(cx, |s, _| s.active_version().map(|v| v.id)),
        None,
        "changing the active photo resets the version (App.tsx)"
    );
}

// --- publish -------------------------------------------------------------------------

/// "Not published yet"; "+ Mark" records the platform and the chosen version; ✕ deletes;
/// "Publish…" opens the Publish dialog.
#[gpui_kit::test]
fn publications_are_marked_listed_and_deleted(cx: &mut TestAppContext) {
    let dir = TempDir::new("insp-publish");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 1, cx);
    let version = catalog(&app, |c| c.create_version(ids[0], "Square").unwrap());
    select(&app, ids[0], SelectMods::default(), cx);
    tab(&app, InspectorTab::Publish, cx);
    assert!(present(&app, "publications-empty", cx));
    let insp = inspector(&app, cx);
    insp.update(cx, |i, cx| i.set_publish_version(Some(version), cx));
    click(&app, "platform-flickr", cx);
    click(&app, "publish-mark", cx);
    let pubs = catalog(&app, |c| c.list_publications(ids[0]).unwrap());
    assert_eq!(pubs.len(), 1);
    assert_eq!((pubs[0].platform.as_str(), pubs[0].version_id), ("flickr", Some(version)));
    assert!(present(&app, format!("publication-{}", pubs[0].id), cx));
    click(&app, format!("publication-del-{}", pubs[0].id), cx);
    assert!(catalog(&app, |c| c.list_publications(ids[0]).unwrap()).is_empty());
    assert!(present(&app, "publications-empty", cx));

    click(&app, "publish-open", cx);
    let open = cx
        .update_window(app.window(), |_, window, cx| {
            use gpui_kit::component::WindowExt as _;
            window.has_active_dialog(cx)
        })
        .unwrap();
    assert!(open, "Publish… opens the Publish dialog");
}

/// "Mark as published" defaults to the version the loupe shows (PublishedPanel).
#[gpui_kit::test]
fn mark_as_published_follows_the_active_version(cx: &mut TestAppContext) {
    let dir = TempDir::new("insp-publish-default");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 1, cx);
    let version = catalog(&app, |c| c.create_version(ids[0], "Square").unwrap());
    select(&app, ids[0], SelectMods::default(), cx);
    let v = catalog(&app, |c| c.list_versions(ids[0]).unwrap()[0].clone());
    inspector(&app, cx).update(cx, |i, cx| i.select_version(Some(v), cx));
    tab(&app, InspectorTab::Publish, cx);
    assert_eq!(inspector(&app, cx).read_with(cx, |i, _| i.publish_version), Some(version));
}

// --- tags tab --------------------------------------------------------------------------

/// The tags tab shows the Tag panel's tagging block (#107, `tags::photo_tags::PhotoTags`) for
/// the photo shown.
#[gpui_kit::test]
fn the_tags_tab_is_the_tag_panels_block(cx: &mut TestAppContext) {
    let dir = TempDir::new("insp-tags");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 1, cx);
    tab(&app, InspectorTab::Tags, cx);
    assert!(!present(&app, "photo-tags-chips", cx), "no photo, no tagging block");
    select(&app, ids[0], SelectMods::default(), cx);
    assert!(present(&app, "photo-tags-chips", cx));
    assert!(!present(&app, "star-1", cx), "the details body is not drawn on the tags tab");
}

// --- external editors ------------------------------------------------------------------

/// RapidRAW: the entry exists from the click (under the job id chosen up front); only that
/// job's routed `rapidraw:progress` events move it; its terminal event ends it; a result of
/// another job is ignored; a catalog switch drops the entry and a straggler event cannot
/// bring it back. (Mutation-checked: without the job-id comparison in
/// `on_rapidraw_progress`, the foreign "importing" event moves the phase and this fails.)
#[gpui_kit::test]
fn rapidraw_progress_belongs_to_its_job(cx: &mut TestAppContext) {
    let dir = TempDir::new("insp-rapidraw");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 1, cx);
    let id = ids[0];
    select(&app, id, SelectMods::default(), cx);
    let insp = inspector(&app, cx);
    // Queued on the manual runner and never run: nothing is launched.
    insp.update(cx, |i, cx| i.edit_in_rapidraw(cx));
    let job = insp.read_with(cx, |i, _| i.rapid.get(&id).copied()).expect("the entry shows at once").job;
    let progress = |job: u64, phase: &str| {
        CoreEvent::RapidRawProgress(RapidRawProgress { photo_id: id, job_id: job, phase: phase.into(), message: String::new() })
    };
    app.state.send(progress(job + 1_000_000, "importing"));
    cx.run_until_parked();
    insp.read_with(cx, |i, _| assert_eq!(i.rapid[&id].phase, RapidPhase::Editing, "another job's event is ignored"));
    app.state.send(progress(job, "waiting"));
    cx.run_until_parked();
    insp.read_with(cx, |i, _| assert_eq!(i.rapid[&id].phase, RapidPhase::Waiting));

    insp.update(cx, |i, cx| i.on_rapidraw_result(id, job + 1_000_000, Ok(None), cx));
    insp.read_with(cx, |i, _| {
        assert!(i.rapid.contains_key(&id), "another job's result is ignored");
        assert!(!i.notes.contains_key(&id));
    });
    app.state.send(progress(job, "cancelled"));
    cx.run_until_parked();
    insp.update(cx, |i, cx| i.on_rapidraw_result(id, job, Ok(None), cx));
    insp.read_with(cx, |i, _| {
        assert!(!i.rapid.contains_key(&id), "the terminal event ended the entry");
        assert_eq!(i.notes.get(&id).map(String::as_str), Some("Cancelled — nothing was imported."));
    });

    insp.update(cx, |i, cx| i.edit_in_rapidraw(cx));
    let job2 = insp.read_with(cx, |i, _| i.rapid[&id].job);
    assert_ne!(job2, job);
    app.state.send(CoreEvent::CatalogSwitched("other".into()));
    cx.run_until_parked();
    app.state.send(progress(job2, "importing"));
    cx.run_until_parked();
    insp.read_with(cx, |i, _| assert!(i.rapid.is_empty() && i.notes.is_empty(), "the switch dropped every entry"));
}

/// Sidecar editors: a run's result writes its note only while it still owns the photo's
/// entry (a newer run for the same photo supersedes it).
#[gpui_kit::test]
fn a_superseded_sidecar_run_does_not_write_its_note(cx: &mut TestAppContext) {
    let dir = TempDir::new("insp-sidecar");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 1, cx);
    let id = ids[0];
    select(&app, id, SelectMods::default(), cx);
    let insp = inspector(&app, cx);
    insp.update(cx, |i, cx| i.develop("darktable", "darktable", cx));
    let first = insp.read_with(cx, |i, _| i.sidecar[&id].seq);
    insp.read_with(cx, |i, _| {
        assert!(i.editing(id));
        assert_eq!(i.notes[&id], "Editing in darktable… the result imports when you close it.");
    });
    insp.update(cx, |i, cx| i.import_result("rawtherapee", cx));
    let second = insp.read_with(cx, |i, _| i.sidecar[&id].seq);
    insp.update(cx, |i, cx| i.end_sidecar(id, first, Some("stale".into()), cx));
    insp.read_with(cx, |i, _| {
        assert!(i.editing(id), "the older run's end does not end the newer");
        assert_ne!(i.notes.get(&id).map(String::as_str), Some("stale"));
    });
    insp.update(cx, |i, cx| i.end_sidecar(id, second, Some("no rawtherapee sidecar".into()), cx));
    insp.read_with(cx, |i, _| {
        assert!(!i.editing(id));
        assert_eq!(i.notes[&id], "no rawtherapee sidecar");
    });
}

trait UpdateInWindow {
    fn update_in_window(
        &self,
        app: &App,
        cx: &mut TestAppContext,
        f: impl FnOnce(&mut PhotoInspector, &mut Window, &mut Context<PhotoInspector>),
    );
}

impl UpdateInWindow for Entity<PhotoInspector> {
    fn update_in_window(
        &self,
        app: &App,
        cx: &mut TestAppContext,
        f: impl FnOnce(&mut PhotoInspector, &mut Window, &mut Context<PhotoInspector>),
    ) {
        let this = self.clone();
        cx.update_window(app.window(), |_, window, cx| this.update(cx, |i, cx| f(i, window, cx))).unwrap();
        cx.run_until_parked();
    }
}
