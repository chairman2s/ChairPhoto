//! Headless tests of Storage and import (#114): each dialog driven through the real window,
//! and the job lifecycles with their interleavings forced. Storage work runs on
//! `Runner::manual` (installed by `start`), so nothing happens off the test thread until a
//! test calls [`work`] — which is what lets a test put a catalog switch *between* a job's
//! worker finishing and its result landing.

use super::*;
use crate::storage::open::StorageDialog;
use crate::storage::{RecentRegistry, Runner};
use chairphoto_core::catalog::{SidecarIdentity, VolumeKind};
use gpui_kit::component::input::InputState;

/// Run every queued storage job, then let the UI thread take the results.
fn work(cx: &mut TestAppContext) -> usize {
    let mut total = 0;
    loop {
        let ran = cx.update(|cx| Runner::get(cx).run_pending());
        cx.run_until_parked();
        if ran == 0 {
            return total;
        }
        total += ran;
    }
}

/// Run only what is queued now — nothing those jobs queue in turn lands yet.
fn work_once(cx: &mut TestAppContext) -> usize {
    cx.update(|cx| Runner::get(cx).run_pending())
}

fn dialog(app: &App, cx: &mut TestAppContext) -> StorageDialog {
    settle(app, cx);
    app.wired.storage.read_with(cx, |s, _| s.last_dialog.clone()).expect("a storage dialog opened")
}

/// Let a dialog's opening animation (gpui-component's 250 ms slide, on the wall clock) finish:
/// a click while the dialog still moves can land its mouse-up away from its mouse-down.
fn settle(app: &App, cx: &mut TestAppContext) {
    std::thread::sleep(std::time::Duration::from_millis(300));
    cx.update_window(app.window(), |_, window, cx| window.render_frame(cx)).unwrap();
    cx.run_until_parked();
}

fn set_input(app: &App, input: &Entity<InputState>, text: &str, cx: &mut TestAppContext) {
    let input = input.clone();
    let text = text.to_string();
    cx.update_window(app.window(), |_, window, cx| input.update(cx, |i, cx| i.set_value(text, window, cx))).unwrap();
}

fn dispatch(app: &App, action: impl gpui_kit::Action, cx: &mut TestAppContext) {
    cx.update_window(app.window(), |_, window, cx| window.dispatch_action(Box::new(action), cx)).unwrap();
    cx.run_until_parked();
}

fn has_dialog(app: &App, cx: &mut TestAppContext) -> bool {
    use gpui_kit::component::WindowExt as _;
    cx.update_window(app.window(), |_, window, cx| window.has_active_dialog(cx)).unwrap()
}

fn photo_count(app: &App) -> usize {
    app.state.catalog.lock().unwrap().as_ref().unwrap().count_photos(&Default::default()).unwrap()
}

/// A card folder with `n` small files the scanner takes for JPEGs.
fn card(dir: &TempDir, n: usize) -> PathBuf {
    let card = dir.0.join("card");
    std::fs::create_dir_all(&card).unwrap();
    for i in 0..n {
        std::fs::write(card.join(format!("IMG_{i:04}.jpg")), format!("not really a jpeg {i}")).unwrap();
    }
    card
}

/// Switch the open catalog for real (the core's two-phase switch, which trips every job), to
/// a fresh catalog; the event goes out through the app's sink but is not delivered yet.
fn switch_catalog_now(app: &App, dir: &TempDir, cx: &mut TestAppContext) {
    let path = dir.0.join("other/other.chairphoto");
    chairphoto_core::app::catalogs::switch_catalog_in(
        &app.state,
        Some(&dir.0.join("registry")),
        &path,
        &dir.0.join("other"),
        true,
        Some("other".into()),
    )
    .unwrap();
    // The router delivers `catalog:switched` now — before any queued worker runs.
    cx.run_until_parked();
}

/// Hand `catalog:switched` to the storage entity directly, before anything else runs.
fn storage_sees_switch(app: &App, cx: &mut TestAppContext) {
    app.wired.storage.update(cx, |s, cx| s.on_core_event(&CoreEvent::CatalogSwitched("other".into()), cx));
}

// --- import from card ---------------------------------------------------------------------

/// Import ▾ → Import from card…: the listing flags what the library already holds, the new
/// photos start selected, Import hands off to the background job (the bench shows it), and
/// its end sets React's status line, clears the bench and re-reads the catalog.
#[gpui_kit::test]
fn a_card_import_runs_from_the_panel_to_the_status_line(cx: &mut TestAppContext) {
    let dir = TempDir::new("import");
    let app = start(cx);
    open_catalog(&app, &dir, cx);
    let source = card(&dir, 2);
    click_menu_row(&app, "import-menu", 0, "Import from card…", cx);
    let StorageDialog::ImportCard(panel) = dialog(&app, cx) else { panic!("the import panel") };
    work(cx); // last source + library root
    assert!(has_dialog(&app, cx));

    let src = source.to_string_lossy().to_string();
    panel.update(cx, |p, cx| p.scan(Some(src.clone()), cx));
    set_input(&app, &panel.read_with(cx, |p, _| p.source.clone()), &src, cx);
    work(cx);
    panel.read_with(cx, |p, _| {
        let cards = p.cards.as_ref().expect("listed");
        assert_eq!(cards.len(), 2);
        assert!(cards.iter().all(|c| !c.is_duplicate));
        assert_eq!(p.selected.len(), 2, "new photos start selected");
    });

    click(&app, "import-run", cx);
    assert!(!has_dialog(&app, cx), "the panel closes on hand-off");
    app.wired.shell.read_with(cx, |s, _| assert_eq!(s.jobs.import, Some((0, 0)), "the bench shows the import"));
    assert_eq!(status(&app, cx), "Importing from card…");
    work(cx);
    assert_eq!(status(&app, cx), "Imported 2 new of 2 on card");
    app.wired.shell.read_with(cx, |s, _| {
        assert_eq!(s.jobs.import, None, "the bench cleared");
        assert_eq!(s.lists.batches.len(), 1, "the new batch was re-read");
    });
    assert_eq!(photo_count(&app), 2);
    app.wired.model.read_with(cx, |m, _| assert_eq!(m.catalog.as_ref().unwrap().photo_count, 2));

    // The batch shows in the browser; a click filters to it and a second click clears it.
    let batch = app.wired.shell.read_with(cx, |s, _| s.lists.batches[0].id);
    let id: &'static str = Box::leak(format!("batch-{batch}").into_boxed_str());
    click(&app, id, cx);
    assert_eq!(app.wired.shell.read_with(cx, |s, _| s.library.scope().batch_id), Some(batch));
    click(&app, id, cx);
    assert_eq!(app.wired.shell.read_with(cx, |s, _| s.library.scope().batch_id), None);

    // The same card again: both are now duplicates and start unselected.
    panel.update(cx, |p, cx| p.scan(Some(src.clone()), cx));
    work(cx);
    panel.read_with(cx, |p, _| {
        assert!(p.cards.as_ref().unwrap().iter().all(|c| c.is_duplicate));
        assert!(p.selected.is_empty());
    });
}

/// The bench's Cancel import, pressed before the worker reaches its first file: the copy
/// stops at once, nothing is indexed, and the status line says it was cancelled.
#[gpui_kit::test]
fn cancelling_an_import_stops_it_before_the_next_file(cx: &mut TestAppContext) {
    let dir = TempDir::new("import-cancel");
    let app = start(cx);
    open_catalog(&app, &dir, cx);
    card(&dir, 3);
    // Every file selected, as the panel sends them.
    let all: Vec<String> = std::fs::read_dir(dir.0.join("card")).unwrap().map(|e| e.unwrap().path().to_string_lossy().to_string()).collect();
    app.wired.storage.update(cx, |s, cx| s.start_card_import(dir.0.join("card"), String::new(), all, cx));
    cx.run_until_parked();
    click(&app, "bench-cancel-import", cx);
    work(cx);
    let line = status(&app, cx);
    assert_eq!(line, "Import cancelled before any file was copied.");
    assert_eq!(photo_count(&app), 0);
    app.wired.shell.read_with(cx, |s, _| assert_eq!(s.jobs.import, None));
}

/// **Forced interleaving.** An import's worker finishes, then the catalog switches, *then*
/// its result reaches the UI thread: the result is dropped — no status line, no bench
/// change — because it belongs to the catalog that was left.
#[gpui_kit::test]
fn an_import_result_that_lands_after_a_catalog_switch_is_dropped(cx: &mut TestAppContext) {
    let dir = TempDir::new("import-switch");
    let app = start(cx);
    open_catalog(&app, &dir, cx);
    work(cx); // the launch reconcile check
    let source = card(&dir, 1);
    let all = vec![source.join("IMG_0000.jpg").to_string_lossy().to_string()];
    app.wired.storage.update(cx, |s, cx| s.start_card_import(source, String::new(), all, cx));
    assert_eq!(work_once(cx), 1, "the worker ran; its result waits for the UI thread");
    storage_sees_switch(&app, cx);
    app.wired.model.update(cx, |m, cx| {
        m.status = "after the switch".into();
        cx.notify();
    });
    cx.run_until_parked();
    assert_eq!(status(&app, cx), "after the switch", "the old catalog's import result landed");
    app.wired.storage.read_with(cx, |s, _| assert!(s.import.is_none()));
}

/// A catalog switch after Import was pressed but before its worker started trips the import
/// generation claimed at the press (core), so the worker copies and indexes nothing — in
/// either catalog — and its report, the left catalog's, is dropped.
#[gpui_kit::test]
fn a_catalog_switch_stops_a_queued_import(cx: &mut TestAppContext) {
    let dir = TempDir::new("import-switch-core");
    let app = start(cx);
    open_catalog(&app, &dir, cx);
    let source = card(&dir, 2);
    let all: Vec<String> = (0..2).map(|i| source.join(format!("IMG_{i:04}.jpg")).to_string_lossy().to_string()).collect();
    app.wired.storage.update(cx, |s, cx| s.start_card_import(source, String::new(), all, cx));
    switch_catalog_now(&app, &dir, cx);
    work(cx);
    assert_eq!(photo_count(&app), 0, "nothing reached the new catalog");
    let copied = std::fs::read_dir(dir.0.join("photos")).map(|d| d.count()).unwrap_or(0);
    assert_eq!(copied, 0, "nothing was copied into the old library folder");
    assert_eq!(status(&app, cx), "Importing from card…", "the left catalog's report landed");
    app.wired.shell.read_with(cx, |s, _| assert_eq!(s.jobs.import, None, "the switch cleared the bench"));
}

// --- rescan -------------------------------------------------------------------------------

/// Rescan library: Phase A's result sets the status line; a result that lands after a
/// catalog switch is dropped.
#[gpui_kit::test]
fn a_rescan_reports_and_a_stale_rescan_is_dropped(cx: &mut TestAppContext) {
    let dir = TempDir::new("rescan");
    let app = start(cx);
    open_catalog(&app, &dir, cx);
    std::fs::create_dir_all(dir.0.join("photos")).unwrap();
    std::fs::write(dir.0.join("photos/a.jpg"), b"jpeg").unwrap();
    dispatch(&app, crate::shell::actions::RescanLibrary, cx);
    assert_eq!(status(&app, cx), "Scanning library…");
    work(cx);
    assert_eq!(status(&app, cx), "Scanned 1, imported 1 (1 new)");

    dispatch(&app, crate::shell::actions::RescanLibrary, cx);
    assert_eq!(work_once(cx), 1);
    storage_sees_switch(&app, cx);
    cx.run_until_parked();
    assert_eq!(status(&app, cx), "Scanning library…", "the left catalog's rescan result landed");
}

// --- catalog switcher ---------------------------------------------------------------------

/// The catalog pill opens the switcher; creating a catalog switches to it (the model, the
/// shell and storage all see `catalog:switched`), records it in the registry and closes the
/// dialog. Creating it again is refused with the core's message.
#[gpui_kit::test]
fn the_catalog_switcher_creates_and_switches(cx: &mut TestAppContext) {
    let dir = TempDir::new("switcher");
    let app = start(cx);
    cx.update(|cx| cx.set_global(RecentRegistry(Some(dir.0.join("registry")))));
    open_catalog(&app, &dir, cx);
    click(&app, "catalog-pill", cx);
    let StorageDialog::Catalogs(sw) = dialog(&app, cx) else { panic!("the switcher") };
    work(cx);
    sw.read_with(cx, |s, _| assert_eq!(s.recents.as_ref().map(Vec::len), Some(0)));
    click(&app, "toggle-new", cx);
    sw.read_with(cx, |s, _| assert!(s.show_new, "Create new… opened the form"));
    let (name, folder) = sw.read_with(cx, |s, _| (s.name.clone(), s.folder.clone()));
    set_input(&app, &name, "Trip", cx);
    set_input(&app, &folder, &dir.0.join("trip").to_string_lossy(), cx);
    click(&app, "create-catalog", cx);
    sw.read_with(cx, |s, _| assert!(s.busy));
    work(cx);
    app.wired.model.read_with(cx, |m, _| assert_eq!(m.catalog.as_ref().map(|c| c.name.as_str()), Some("Trip.chairphoto")));
    assert_eq!(app.wired.storage.read_with(cx, |s, _| s.epoch()), 2, "storage saw both switches");
    assert!(!has_dialog(&app, cx), "the switcher closed");
    let recent = chairphoto_core::app::load_recent_catalogs_in(&dir.0.join("registry")).unwrap();
    assert_eq!(recent[0].name, "Trip");

    // Again: the file exists now.
    click(&app, "catalog-pill", cx);
    let StorageDialog::Catalogs(sw) = dialog(&app, cx) else { panic!() };
    work(cx);
    sw.read_with(cx, |s, _| assert_eq!(s.recents.as_ref().map(Vec::len), Some(1)));
    sw.update(cx, |s, cx| {
        s.show_new = true;
        cx.notify();
    });
    let (name, folder) = sw.read_with(cx, |s, _| (s.name.clone(), s.folder.clone()));
    set_input(&app, &name, "Trip", cx);
    set_input(&app, &folder, &dir.0.join("trip").to_string_lossy(), cx);
    sw.update(cx, |s, cx| s.create(cx));
    work(cx);
    sw.read_with(cx, |s, _| assert!(s.error.as_deref().unwrap_or("").starts_with("Catalog file already exists"), "{:?}", s.error));
    assert!(has_dialog(&app, cx), "a failed switch keeps the dialog open");
    sw.update(cx, |s, cx| {
        s.name.update(cx, |_, _| {});
        cx.notify();
    });
}

// --- reconcile ----------------------------------------------------------------------------

/// A catalog with a reachable backup volume and one photo whose original exists; returns the
/// photo id.
fn catalog_with_backup(app: &App, dir: &TempDir, cx: &mut TestAppContext) -> i64 {
    let ids = open_catalog_with_photos(app, dir, 0, cx);
    assert!(ids.is_empty());
    let root = dir.0.join("photos");
    std::fs::create_dir_all(root.join("2026")).unwrap();
    std::fs::write(root.join("2026/a.jpg"), b"jpeg bytes").unwrap();
    std::fs::create_dir_all(dir.0.join("nas")).unwrap();
    let guard = app.state.catalog.lock().unwrap();
    let c = guard.as_ref().unwrap();
    c.add_volume("NAS", &dir.0.join("nas"), VolumeKind::Backup).unwrap();
    c.upsert_photo(&root.join("2026/a.jpg"), None, 10, 1).unwrap().id
}

/// The auto-reconcile React ran on window focus: a backup waiting while the NAS is reachable
/// is drained when the window comes back, without anyone asking.
#[gpui_kit::test]
fn window_focus_backs_up_what_waits_for_a_reachable_nas(cx: &mut TestAppContext) {
    let dir = TempDir::new("reconcile");
    let app = start(cx);
    let id = catalog_with_backup(&app, &dir, cx);
    work(cx); // the launch check: nothing waits
    app.state.catalog.lock().unwrap().as_ref().unwrap().enqueue_operation("backup", id).unwrap();
    refocus_main_window(&app, cx);
    work(cx);
    assert_eq!(status(&app, cx), "Backed up 1");
    assert!(dir.0.join("nas/2026/a.jpg").exists(), "the copy is on the NAS");
    app.wired.shell.read_with(cx, |s, _| assert_eq!(s.counts.pending, 0, "the queue count was re-read"));
}

/// With the NAS offline, focus drains nothing (React: only when a backup volume is reachable).
#[gpui_kit::test]
fn window_focus_leaves_the_queue_while_the_nas_is_away(cx: &mut TestAppContext) {
    let dir = TempDir::new("reconcile-offline");
    let app = start(cx);
    let id = catalog_with_backup(&app, &dir, cx);
    work(cx);
    std::fs::remove_dir_all(dir.0.join("nas")).unwrap();
    app.state.volume_health.invalidate();
    app.state.catalog.lock().unwrap().as_ref().unwrap().enqueue_operation("backup", id).unwrap();
    refocus_main_window(&app, cx);
    work(cx);
    assert_ne!(status(&app, cx), "Backed up 1");
    let pending = app.state.catalog.lock().unwrap().as_ref().unwrap().list_pending_operations().unwrap();
    assert_eq!(pending.len(), 1);
}

/// **Forced interleaving.** A drain is in flight (queued, its result not landed) when the
/// catalog switches: the new catalog's drain is not skipped because of it.
#[gpui_kit::test]
fn a_drain_from_before_a_switch_does_not_block_the_new_catalogs_drain(cx: &mut TestAppContext) {
    let dir = TempDir::new("reconcile-switch");
    let app = start(cx);
    let id = catalog_with_backup(&app, &dir, cx);
    work(cx); // the launch check: nothing waits
    app.state.catalog.lock().unwrap().as_ref().unwrap().enqueue_operation("backup", id).unwrap();
    dispatch(&app, crate::shell::actions::Reconcile, cx);
    let runner = cx.update(|cx| Runner::get(cx));
    assert_eq!(runner.pending(), 1, "the old catalog's drain is in flight");

    switch_catalog_now(&app, &dir, cx);
    // The new catalog's drain (its launch check, the chip or the menu all end here).
    let before = runner.pending();
    app.wired.storage.update(cx, |s, cx| s.run_reconcile(cx));
    assert_eq!(runner.pending(), before + 1, "the new catalog's drain was skipped for the old one");
    let epoch = app.wired.storage.read_with(cx, |s, _| s.epoch());
    assert_eq!(app.wired.storage.read_with(cx, |s, _| s.reconciling), Some(epoch));

    work(cx);
    app.wired.storage.read_with(cx, |s, _| assert_eq!(s.reconciling, None, "both drains ended"));
}

// --- trash --------------------------------------------------------------------------------

/// Two trashed photos whose files exist; returns their ids.
fn catalog_with_trash(app: &App, dir: &TempDir, cx: &mut TestAppContext) -> Vec<i64> {
    let ids = open_catalog_with_photos(app, dir, 2, cx);
    for i in 0..2 {
        let p = dir.0.join(format!("photos/2026/p{i}.ARW"));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"raw").unwrap();
    }
    // The files appear behind the app's back, after the Library grid's storage badges have
    // already read (and cached) the library folder as missing.
    app.state.volume_health.invalidate();
    app.state.catalog.lock().unwrap().as_ref().unwrap().trash_photos(&ids).unwrap();
    ids
}

/// Restore needs no confirmation; Delete needs `delete` typed — a near miss deletes nothing,
/// Escape cancels the confirmation without closing the dialog, and Enter on the word deletes
/// and reports.
#[gpui_kit::test]
fn the_trash_restores_freely_and_deletes_only_on_the_typed_word(cx: &mut TestAppContext) {
    let dir = TempDir::new("trash");
    let app = start(cx);
    let ids = catalog_with_trash(&app, &dir, cx);
    click(&app, "browser-trash", cx);
    let StorageDialog::Trash(trash) = dialog(&app, cx) else { panic!("the trash") };
    work(cx);
    trash.read_with(cx, |t, _| assert_eq!(t.photos.as_ref().map(Vec::len), Some(2)));

    let tile: &'static str = Box::leak(format!("trash-{}", ids[0]).into_boxed_str());
    click(&app, tile, cx);
    click(&app, "trash-restore", cx);
    work(cx);
    trash.read_with(cx, |t, _| assert_eq!(t.photos.as_ref().map(|p| p.iter().map(|p| p.id).collect::<Vec<_>>()), Some(vec![ids[1]])));
    app.wired.shell.read_with(cx, |s, _| assert_eq!(s.counts.trash, Some(1), "the trash count was re-read"));

    click(&app, "trash-delete", cx);
    trash.read_with(cx, |t, _| assert!(t.confirming));
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.input("delet", cx);
        window.press("enter", cx);
    })
    .unwrap();
    work(cx);
    trash.read_with(cx, |t, cx| assert_eq!(t.typed.read(cx).value(), "delet", "typed into the confirm field"));
    trash.read_with(cx, |t, _| assert!(t.report.is_none(), "a near miss deleted"));
    // The Delete button does nothing either until the word is right.
    trash.update(cx, |t, cx| t.destroy(cx));
    work(cx);
    trash.read_with(cx, |t, _| assert!(t.report.is_none()));
    assert!(dir.0.join("photos/2026/p1.ARW").exists());

    cx.update_window(app.window(), |_, window, cx| {
        let focused = window.focused(cx);
        let typed = gpui_kit::Focusable::focus_handle(trash.read(cx).typed.read(cx), cx);
        assert_eq!(focused.as_ref(), Some(&typed), "the confirm field has focus");
        window.press("escape", cx)
    })
    .unwrap();
    cx.run_until_parked();
    trash.read_with(cx, |t, _| assert!(!t.confirming, "Escape cancelled the confirmation"));
    assert!(has_dialog(&app, cx), "… and not the dialog");

    click(&app, "trash-delete", cx);
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.input("delete", cx);
        window.press("enter", cx);
    })
    .unwrap();
    work(cx);
    trash.read_with(cx, |t, _| {
        let r = t.report.as_ref().expect("deleted");
        assert_eq!((r.deleted, r.files_deleted), (1, 1));
        assert_eq!(t.photos.as_ref().map(Vec::len), Some(0));
    });
    assert!(!dir.0.join("photos/2026/p1.ARW").exists(), "the original is gone");
    assert!(dir.0.join("photos/2026/p0.ARW").exists(), "the restored one is untouched");
}

/// **Forced interleaving.** A delete whose worker ran before a catalog switch, but whose
/// report lands after it: the report is dropped (its ids name the left catalog's photos).
#[gpui_kit::test]
fn a_trash_report_that_lands_after_a_switch_is_dropped(cx: &mut TestAppContext) {
    let dir = TempDir::new("trash-switch");
    let app = start(cx);
    catalog_with_trash(&app, &dir, cx);
    dispatch(&app, crate::shell::actions::OpenTrash, cx);
    let StorageDialog::Trash(trash) = dialog(&app, cx) else { panic!() };
    work(cx);
    click(&app, "trash-delete", cx);
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.input("delete", cx);
    })
    .unwrap();
    trash.update(cx, |t, cx| t.destroy(cx));
    assert_eq!(work_once(cx), 1);
    storage_sees_switch(&app, cx);
    cx.run_until_parked();
    trash.read_with(cx, |t, _| assert!(t.report.is_none(), "the left catalog's report landed"));
}

// --- identity debt ------------------------------------------------------------------------

/// A catalog whose `n` photos each owe their identity, with the files reachable.
fn catalog_with_debt(app: &App, dir: &TempDir, n: usize, cx: &mut TestAppContext) {
    let ids = open_catalog_with_photos(app, dir, n, cx);
    let guard = app.state.catalog.lock().unwrap();
    let c = guard.as_ref().unwrap();
    for (i, id) in ids.iter().enumerate() {
        let p = dir.0.join(format!("photos/2026/p{i}.ARW"));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"raw").unwrap();
        c.record_sidecar_identity(*id, &p, &SidecarIdentity::Unreachable).unwrap();
    }
    drop(guard);
    // Recorded behind the shell's back: re-read, as an import's end would.
    app.wired.model.update(cx, |m, cx| m.refresh(cx));
    work(cx);
}

/// The identity-debt chip opens the panel; a repair pass runs as a job — progress and its
/// terminal event follow the job id — and its end reloads the queue (now empty).
#[gpui_kit::test]
fn a_repair_pass_runs_from_the_panel_and_reloads_the_queue(cx: &mut TestAppContext) {
    let dir = TempDir::new("debt");
    let app = start(cx);
    catalog_with_debt(&app, &dir, 3, cx);
    app.wired.shell.read_with(cx, |s, _| assert_eq!(s.counts.identity_debt, Some(3)));
    click(&app, "attn-identity", cx);
    let StorageDialog::IdentityDebt(panel) = dialog(&app, cx) else { panic!("the debt panel") };
    work(cx);
    panel.read_with(cx, |p, _| {
        assert_eq!(p.summary.map(|s| s.total), Some(3));
        assert_eq!(p.rows.as_ref().map(Vec::len), Some(3));
    });
    app.wired.storage.read_with(cx, |s, _| assert!(!s.repair.running, "nothing ran, so nothing re-attached"));

    click(&app, "repair-start", cx);
    app.wired.storage.read_with(cx, |s, _| assert!(s.repair.running && s.repair.job.is_none()));
    work(cx);
    app.wired.storage.read_with(cx, |s, _| {
        assert!(!s.repair.running, "the terminal event ended the pass");
        assert_eq!(s.repair.result.map(|r| r.bound), Some(3));
    });
    panel.read_with(cx, |p, _| {
        assert_eq!(p.summary.map(|s| s.total), Some(0), "the queue was re-read");
        assert_eq!(p.rows.as_ref().map(Vec::len), Some(0));
    });
    app.wired.shell.read_with(cx, |s, _| assert_eq!(s.counts.identity_debt, Some(0), "the chip count too"));
}

/// The panel re-attaches to a pass already running when it opens (claimed elsewhere — by an
/// earlier panel), follows its job id, and ends with its terminal event.
#[gpui_kit::test]
fn the_debt_panel_reattaches_to_a_running_pass(cx: &mut TestAppContext) {
    let dir = TempDir::new("debt-reattach");
    let app = start(cx);
    catalog_with_debt(&app, &dir, 2, cx);
    let pass = chairphoto_core::app::identity::claim_identity_repair(&app.state).unwrap();
    let job = pass.job;
    dispatch(&app, crate::shell::actions::OpenIdentityDebt, cx);
    work(cx);
    app.wired.storage.read_with(cx, |s, _| {
        assert!(s.repair.running, "re-attached");
        assert_eq!(s.repair.job, Some(job));
    });
    pass.run();
    cx.run_until_parked();
    work(cx);
    app.wired.storage.read_with(cx, |s, _| {
        assert!(!s.repair.running);
        assert_eq!(s.repair.result.map(|r| r.bound), Some(2));
    });
}

/// **Forced interleaving.** The terminal event arrives before the start's job id is adopted:
/// it is buffered and replayed on adoption, so the panel does not wait forever. Another
/// pass's terminal event is ignored.
#[gpui_kit::test]
fn a_terminal_event_that_beats_the_job_id_is_replayed(cx: &mut TestAppContext) {
    use chairphoto_core::app::IdentityRepairDone;
    let dir = TempDir::new("debt-early");
    let app = start(cx);
    catalog_with_debt(&app, &dir, 1, cx);
    let done = |job| {
        CoreEvent::IdentityRepairDone(IdentityRepairDone {
            ok: true,
            job,
            summary: chairphoto_core::catalog::IdentityRepairSummary { bound: 1, total: 1, ..Default::default() },
            error: None,
        })
    };
    app.wired.storage.update(cx, |s, cx| s.start_repair(cx));
    // The claim's worker has not run: no job id yet. Two terminal events arrive.
    app.wired.storage.update(cx, |s, cx| {
        s.on_core_event(&done(41), cx);
        s.on_core_event(&done(42), cx);
    });
    let epoch = app.wired.storage.read_with(cx, |s, _| s.epoch());
    app.wired.storage.update(cx, |s, cx| s.adopt((1, epoch), Ok(Some((42, 0, 0))), cx));
    app.wired.storage.read_with(cx, |s, _| {
        assert!(!s.repair.running, "the buffered terminal event ended the adopted pass");
        assert_eq!(s.repair.result.map(|r| r.bound), Some(1));
    });
    // The queued claim still runs (its slot must be released); its events find no follower.
    work(cx);

    // While a pass is followed, another pass's progress and terminal event change nothing.
    app.wired.storage.update(cx, |s, cx| s.start_repair(cx));
    app.wired.storage.update(cx, |s, cx| s.adopt((2, epoch), Ok(Some((50, 0, 4))), cx));
    app.wired.storage.update(cx, |s, cx| {
        s.on_core_event(&CoreEvent::IdentityRepairProgress(chairphoto_core::app::IdentityRepairProgress { done: 3, total: 9, job: 49 }), cx);
        s.on_core_event(&done(49), cx);
    });
    app.wired.storage.read_with(cx, |s, _| {
        assert!(s.repair.running, "another pass's terminal event ended the followed one");
        assert_eq!(s.repair.progress, Some((0, 4)), "another pass's progress was shown");
    });
    app.wired.storage.update(cx, |s, cx| {
        s.on_core_event(&CoreEvent::IdentityRepairProgress(chairphoto_core::app::IdentityRepairProgress { done: 2, total: 4, job: 50 }), cx);
    });
    app.wired.storage.read_with(cx, |s, _| assert_eq!(s.repair.progress, Some((2, 4))));
    app.wired.storage.update(cx, |s, cx| s.on_core_event(&done(50), cx));
    app.wired.storage.read_with(cx, |s, _| assert!(!s.repair.running));
    work(cx);
}

/// A catalog switch drops the followed pass: the switch tripped it and cleared its slot, and
/// its terminal event, arriving later, is the left catalog's.
#[gpui_kit::test]
fn a_catalog_switch_drops_the_followed_repair_pass(cx: &mut TestAppContext) {
    let dir = TempDir::new("debt-switch");
    let app = start(cx);
    catalog_with_debt(&app, &dir, 2, cx);
    app.wired.storage.update(cx, |s, cx| s.start_repair(cx));
    storage_sees_switch(&app, cx);
    work(cx); // the claim and the pass run now, and their events arrive after the switch
    app.wired.storage.read_with(cx, |s, _| {
        assert!(!s.repair.running);
        assert!(s.repair.result.is_none(), "the left catalog's pass reported into the new one");
    });
}

// --- bundles ------------------------------------------------------------------------------

/// Import a bundle: preview it (new vs already present), import, and see the result line;
/// importing it again is the no-op the preview promises.
#[gpui_kit::test]
fn a_bundle_previews_and_imports(cx: &mut TestAppContext) {
    let dir = TempDir::new("bundle");
    let app = start(cx);
    // Build the bundle from a throwaway catalog that imported a card.
    let src_dir = TempDir::new("bundle-src");
    let src_state = AppState::default();
    *src_state.catalog.lock().unwrap() =
        Some(Catalog::open(&src_dir.0.join("src.chairphoto"), &src_dir.0.join("lib")).unwrap());
    let source = card(&src_dir, 2);
    chairphoto_core::app::scans::ingest_from_card(&src_state, &source, Some("Trip"), None).unwrap();
    let bundle = dir.0.join("trip.chairphoto");
    {
        let guard = src_state.catalog.lock().unwrap();
        let c = guard.as_ref().unwrap();
        let batch = c.list_import_batches().unwrap()[0].id;
        let gathered = chairphoto_core::bundle::writer::gather_bundle(c, batch).unwrap().unwrap();
        chairphoto_core::bundle::writer::write_bundle(&gathered, &bundle, |_, _| {}).unwrap();
    }

    open_catalog(&app, &dir, cx);
    click_menu_row(&app, "import-menu", 1, "Import a .chairphoto bundle…", cx);
    let StorageDialog::ImportBundle(dlg) = dialog(&app, cx) else { panic!("the bundle dialog") };
    let path = dlg.read_with(cx, |d, _| d.path.clone());
    set_input(&app, &path, &bundle.to_string_lossy(), cx);
    click(&app, "bundle-check", cx);
    work(cx);
    dlg.read_with(cx, |d, cx| {
        let p = d.preview.as_ref().unwrap_or_else(|| panic!("previewed: {:?} path {:?}", d.error, d.path.read(cx).value()));
        assert_eq!((p.total, p.new_count, p.existing), (2, 2, 0));
    });
    click(&app, "bundle-run", cx);
    dlg.read_with(cx, |d, _| assert!(d.importing));
    work(cx);
    // React's line, from the core's result: the bundle importer indexes its photos before
    // the merge runs, so the merge itself counts none as added (pre-existing reporting).
    dlg.read_with(cx, |d, _| assert_eq!(d.result.as_deref(), Some("Import complete. No new photos. 2 originals copied.")));
    assert_eq!(photo_count(&app), 2);

    click(&app, "bundle-check", cx);
    work(cx);
    dlg.read_with(cx, |d, _| assert_eq!(d.preview.as_ref().map(|p| (p.new_count, p.existing)), Some((0, 2))));
}

// --- volumes ------------------------------------------------------------------------------

/// Add a volume (Enter in the path adds); the library folder cannot be removed; removing
/// another asks first, and Cancel keeps it.
#[gpui_kit::test]
fn volumes_add_and_remove_behind_a_confirm(cx: &mut TestAppContext) {
    let dir = TempDir::new("volumes");
    let app = start(cx);
    open_catalog(&app, &dir, cx);
    std::fs::create_dir_all(dir.0.join("nas")).unwrap();
    click_menu_row(&app, "more-menu", 10, "Storage volumes…", cx);
    let StorageDialog::Volumes(panel) = dialog(&app, cx) else { panic!("the volumes panel") };
    work(cx);
    panel.read_with(cx, |p, _| assert_eq!(p.volumes.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(), ["catalog-root"]));
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("volume-remove-0").is_none(), "the library folder has no Remove");
    })
    .unwrap();

    let (name, path) = panel.read_with(cx, |p, _| (p.name.clone(), p.path.clone()));
    set_input(&app, &name, "NAS", cx);
    set_input(&app, &path, &dir.0.join("nas").to_string_lossy(), cx);
    click(&app, "volume-add", cx);
    work(cx);
    let nas = panel.read_with(cx, |p, _| {
        let i = p.volumes.iter().position(|v| v.name == "NAS").expect("added");
        assert!(p.volumes[i].reachable);
        assert_eq!(p.volumes[i].kind, VolumeKind::Backup);
        i
    });
    let remove: &'static str = Box::leak(format!("volume-remove-{nas}").into_boxed_str());
    click(&app, remove, cx);
    settle(&app, cx);
    click(&app, "cancel", cx);
    work(cx);
    assert_eq!(panel.read_with(cx, |p, _| p.volumes.len()), 2, "Cancel kept the volume");
    click(&app, remove, cx);
    settle(&app, cx);
    click(&app, "ok", cx);
    work(cx);
    assert_eq!(panel.read_with(cx, |p, _| p.volumes.len()), 1, "OK removed it");
}
