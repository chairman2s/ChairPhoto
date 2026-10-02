//! Headless tests of the Library view through the real wiring (`start` → `wire` → the main
//! window): the grid's rows, clicks and keys, the culling write path, row generations, deep
//! links, thumbnails per window, and the "Stack bursts" dialog.

use crate::library::grid::LibraryView;
use crate::shell::state::Mark;
use crate::tests::{
    click, click_menu_row, colliding_catalog, core_switch, deliver_switch, open_catalog_with_photos, press, start, status,
    App, TempDir,
};
use chairphoto_core::app::CoreEvent;
use chairphoto_core::catalog::{CullingFilter, PhotoPage, PickState};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    point, px, AppContext as _, ElementId, Entity, InputEvent as _, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, ScrollDelta,
    TestAppContext,
};

fn library_view(app: &App, cx: &mut TestAppContext) -> Entity<LibraryView> {
    app.wired.root.as_ref().expect("the main window opened").read_with(cx, |root, _| root.library.clone())
}

fn rows(app: &App, cx: &mut TestAppContext) -> Vec<i64> {
    app.wired.shell.read_with(cx, |s, _| s.library.photo_ids())
}

fn selection(app: &App, cx: &mut TestAppContext) -> (Option<i64>, Vec<i64>) {
    app.wired.shell.read_with(cx, |s, _| {
        let sel = s.library.selection();
        (sel.active_id, sel.ids.to_vec())
    })
}

fn tile(id: i64) -> ElementId {
    ("tile", id as u64).into()
}

/// Click a tile with modifier keys held (the kit's `click` sends none).
fn click_tile(app: &App, id: i64, modifiers: Modifiers, cx: &mut TestAppContext) {
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        let position = window.find(tile(id)).bounds().center();
        for event in [
            MouseDownEvent { button: MouseButton::Left, position, modifiers, click_count: 1, first_mouse: false }
                .to_platform_input(),
            MouseUpEvent { button: MouseButton::Left, position, modifiers, click_count: 1 }.to_platform_input(),
        ] {
            window.dispatch_event(event, cx);
        }
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn rating_of(app: &App, id: i64) -> (i64, PickState, String) {
    let guard = app.state.catalog.lock().unwrap();
    let p = guard.as_ref().unwrap().get_photo(id).unwrap();
    (p.rating, p.pick_state, p.label)
}

fn render(app: &App, cx: &mut TestAppContext) {
    cx.update_window(app.window(), |_, window, cx| window.render_frame(cx)).unwrap();
    cx.run_until_parked();
}

/// The grid lists the catalog's rows once it is open, and draws a tile per visible photo.
#[gpui_kit::test]
fn the_grid_lists_the_catalog_and_draws_its_tiles(cx: &mut TestAppContext) {
    let dir = TempDir::new("grid-rows");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 5, cx);
    assert_eq!(rows(&app, cx), ids);
    render(&app, cx);
    cx.update_window(app.window(), |_, window, _| {
        for &id in &ids {
            assert!(window.try_find(tile(id)).is_some(), "tile {id} drawn");
        }
        assert!(window.try_find("grid-empty").is_none());
    })
    .unwrap();
    assert!(library_view(&app, cx).read_with(cx, |v, _| v.columns()) > 1, "the width fits several columns");
}

/// With no photo matching, the grid says why — and only once the rows have landed.
#[gpui_kit::test]
fn an_empty_view_says_what_is_filtering_it(cx: &mut TestAppContext) {
    let dir = TempDir::new("grid-empty");
    let app = start(cx);
    open_catalog_with_photos(&app, &dir, 2, cx);
    click(&app, "filter-Picks", cx);
    render(&app, cx);
    cx.update_window(app.window(), |_, window, _| {
        assert_eq!(window.find("grid-empty").label(), Some("No photos match the current filters."));
    })
    .unwrap();
}

/// A plain click selects one photo, Ctrl toggles, Shift selects the range from the anchor
/// (React's CatalogGrid `onSelect` with `{ctrl, shift}`).
#[gpui_kit::test]
fn clicks_select_toggle_and_range(cx: &mut TestAppContext) {
    let dir = TempDir::new("grid-click");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 6, cx);
    click_tile(&app, ids[1], Modifiers::default(), cx);
    assert_eq!(selection(&app, cx), (Some(ids[1]), vec![ids[1]]));
    click_tile(&app, ids[3], Modifiers::control(), cx);
    assert_eq!(selection(&app, cx), (Some(ids[3]), vec![ids[1], ids[3]]));
    click_tile(&app, ids[5], Modifiers::shift(), cx);
    assert_eq!(selection(&app, cx), (Some(ids[5]), vec![ids[3], ids[4], ids[5]]), "range from the Ctrl anchor");
}

/// Double-click selects and opens the loupe on the photo.
#[gpui_kit::test]
fn a_double_click_opens_the_loupe(cx: &mut TestAppContext) {
    let dir = TempDir::new("grid-open");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 3, cx);
    cx.update_window(app.window(), |_, window, cx| window.double_click(tile(ids[2]), cx)).unwrap();
    cx.run_until_parked();
    assert_eq!(selection(&app, cx).0, Some(ids[2]));
    assert_eq!(
        app.wired.shell.read_with(cx, |s, _| s.stage_view()),
        crate::shell::state::StageView::Loupe
    );
}

/// The arrows step the active photo (Shift extends), Home/End jump, Ctrl+A selects all —
/// through the real keymap in the Library context.
#[gpui_kit::test]
fn keys_move_and_extend_the_selection(cx: &mut TestAppContext) {
    let dir = TempDir::new("grid-keys");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 6, cx);

    press(&app, "right", cx);
    assert_eq!(selection(&app, cx).0, None, "nothing active: the arrows do nothing (React)");
    click_tile(&app, ids[2], Modifiers::default(), cx);
    press(&app, "right", cx);
    assert_eq!(selection(&app, cx), (Some(ids[3]), vec![ids[3]]));
    press(&app, "down", cx);
    assert_eq!(selection(&app, cx).0, Some(ids[4]), "↓ is the next photo, as in React");
    press(&app, "shift-left", cx);
    press(&app, "shift-up", cx);
    assert_eq!(selection(&app, cx), (Some(ids[2]), vec![ids[2], ids[3], ids[4]]));
    press(&app, "home", cx);
    assert_eq!(selection(&app, cx), (Some(ids[0]), vec![ids[0]]));
    press(&app, "left", cx);
    assert_eq!(selection(&app, cx).0, Some(ids[0]), "stops at the first photo");
    press(&app, "end", cx);
    assert_eq!(selection(&app, cx).0, Some(ids[5]));
    press(&app, "pageup", cx);
    assert_eq!(selection(&app, cx).0, Some(ids[0]), "a screenful back clamps to the first photo");
    press(&app, "ctrl-a", cx);
    assert_eq!(selection(&app, cx).1, ids);
}

/// A culling key writes the mark to the catalog and, with one photo targeted, advances to
/// the next — over the rows as they were when the key was pressed, even when the refresh
/// drops the marked photo from the view.
#[gpui_kit::test]
fn culling_keys_write_and_advance_over_the_rows_they_started_from(cx: &mut TestAppContext) {
    let dir = TempDir::new("grid-cull");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 4, cx);
    click(&app, "filter-Unrated", cx);
    click_tile(&app, ids[1], Modifiers::default(), cx);

    press(&app, "3", cx);
    assert_eq!(rating_of(&app, ids[1]).0, 3);
    assert_eq!(rows(&app, cx), vec![ids[0], ids[2], ids[3]], "rated: no longer unrated");
    assert_eq!(selection(&app, cx).0, Some(ids[2]), "advanced to the next of the old rows");

    press(&app, "p", cx);
    press(&app, "r", cx);
    assert_eq!(rating_of(&app, ids[2]).1, PickState::Pick);
    assert_eq!(rating_of(&app, ids[3]).2, "Red", "the second key marked the photo the first advanced to");
}

/// With several photos selected a key marks them all and does not advance; the bench's
/// controls write through the same path without advancing, and the active star clears.
#[gpui_kit::test]
fn batch_marks_and_bench_marks_do_not_advance(cx: &mut TestAppContext) {
    let dir = TempDir::new("grid-batch");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 4, cx);
    click_tile(&app, ids[0], Modifiers::default(), cx);
    click_tile(&app, ids[2], Modifiers::control(), cx);
    press(&app, "x", cx);
    assert_eq!(rating_of(&app, ids[0]).1, PickState::Reject);
    assert_eq!(rating_of(&app, ids[2]).1, PickState::Reject);
    assert_eq!(rating_of(&app, ids[1]).1, PickState::None);
    assert_eq!(selection(&app, cx), (Some(ids[2]), vec![ids[0], ids[2]]), "no advance for a batch");

    click_tile(&app, ids[3], Modifiers::default(), cx);
    click(&app, "bench-star-4", cx);
    assert_eq!(rating_of(&app, ids[3]).0, 4);
    assert_eq!(selection(&app, cx).0, Some(ids[3]), "a bench click does not advance");
    click(&app, "bench-star-4", cx);
    assert_eq!(rating_of(&app, ids[3]).0, 0, "clicking the active star clears");
    click(&app, "bench-label-Green", cx);
    assert_eq!(rating_of(&app, ids[3]).2, "Green");
}

/// A culling key whose write fails does not advance: the selection stays on the photo the
/// user tried to mark, and the status line says why (React advanced only after
/// `applyToSelection` resolved). The failure is injected with a temporary trigger on the
/// catalog's own connection that aborts every update of `photos`.
#[gpui_kit::test]
fn a_failed_culling_write_does_not_advance(cx: &mut TestAppContext) {
    let dir = TempDir::new("grid-cull-fail");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 3, cx);
    click_tile(&app, ids[1], Modifiers::default(), cx);
    app.state
        .catalog
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .conn()
        .execute_batch("CREATE TEMP TRIGGER refuse_marks BEFORE UPDATE ON photos BEGIN SELECT RAISE(ABORT, 'injected'); END;")
        .unwrap();

    press(&app, "3", cx);
    assert_eq!(rating_of(&app, ids[1]).0, 0, "the injected failure refused the write");
    assert!(status(&app, cx).starts_with("Could not mark:"), "status: {}", status(&app, cx));
    assert_eq!(selection(&app, cx), (Some(ids[1]), vec![ids[1]]), "a failed mark advanced the selection");
}

/// A mark queued when the catalog switches is not written: its ids name the closed
/// catalog's photos (AGENTS.md: a catalog switch makes older work unreachable).
#[gpui_kit::test]
fn a_mark_queued_across_a_catalog_switch_is_dropped(cx: &mut TestAppContext) {
    let dir = TempDir::new("grid-switch");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 2, cx);
    click_tile(&app, ids[0], Modifiers::default(), cx);
    app.wired.shell.update(cx, |s, cx| {
        s.apply_mark(Mark::Rating(5), false, cx);
        s.on_core_event(&CoreEvent::CatalogSwitched("another.chairphoto".into()), cx);
    });
    cx.run_until_parked();
    assert_eq!(rating_of(&app, ids[0]).0, 0, "written into whatever catalog was open after the switch");
}

/// **Forced interleaving** (#106 review, finding 1). A mark is queued on the open catalog's
/// photo; then the core switches to a catalog whose photo carries the same id, before the
/// write runs — with `catalog:switched` delivered or still on its way. The new catalog's photo
/// keeps its rating either way. Both the keys' path (`apply_mark`) and the inspector's
/// (`apply_mark_to`) are covered.
fn mark_across_a_switch(delivered: bool, cx: &mut TestAppContext) {
    let dir = TempDir::new("grid-switch-ids");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 2, cx);
    click_tile(&app, ids[0], Modifiers::default(), cx);
    let from = app.wired.shell.read_with(cx, |s, _| s.rows_from().unwrap());
    app.wired.shell.update(cx, |s, cx| {
        s.apply_mark(Mark::Rating(5), false, cx);
        s.apply_mark_to(Mark::Pick(PickState::Pick), vec![ids[1]], from, cx);
    });
    let (b, b_ids) = colliding_catalog(&dir, "b", 2);
    assert_eq!(b_ids, ids, "the ids collide, as real catalogs' do");
    core_switch(&app, b);
    if delivered {
        deliver_switch(&app, cx);
    }
    cx.run_until_parked();
    assert_eq!(rating_of(&app, b_ids[0]).0, 0, "the old catalog's mark landed on the new catalog's photo");
    assert_eq!(rating_of(&app, b_ids[1]).1, PickState::None, "the inspector's mark landed on the new catalog");
    if !delivered {
        assert_eq!(status(&app, cx), format!("Could not mark: {}", chairphoto_core::app::CATALOG_CHANGED));
        // The rows the refused write re-read are the new catalog's: the old selection is gone.
        assert_eq!(selection(&app, cx), (None, vec![]));
    }
}

#[gpui_kit::test]
fn a_mark_never_reaches_the_new_catalog_before_the_switch_event(cx: &mut TestAppContext) {
    mark_across_a_switch(false, cx);
}

#[gpui_kit::test]
fn a_mark_never_reaches_the_new_catalog_after_the_switch_event(cx: &mut TestAppContext) {
    mark_across_a_switch(true, cx);
}

/// **Forced interleaving** (finding 2). Burst analysis started on the open catalog runs
/// after the core switched to a catalog with the same ids: the new catalog's burst flags are
/// untouched (an analysis of unscored photos would clear them), event delivered or not.
fn burst_across_a_switch(delivered: bool, cx: &mut TestAppContext) {
    let dir = TempDir::new("grid-burst-switch");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 3, cx);
    let (b, b_ids) = colliding_catalog(&dir, "b", 3);
    assert_eq!(b_ids, ids);
    b.set_burst_flags(b_ids.iter().map(|&id| (id, "soft-in-burst".to_string())).collect()).unwrap();
    cx.update_window(app.window(), |_, window, cx| {
        window.dispatch_action(Box::new(crate::shell::actions::AnalyseBurst), cx)
    })
    .unwrap();
    core_switch(&app, b);
    if delivered {
        deliver_switch(&app, cx);
    }
    cx.run_until_parked();
    let flags: Vec<Option<String>> = {
        let guard = app.state.catalog.lock().unwrap();
        b_ids.iter().map(|&id| guard.as_ref().unwrap().get_photo(id).unwrap().burst_flag).collect()
    };
    assert!(flags.iter().all(|f| f.as_deref() == Some("soft-in-burst")), "flags rewritten in the new catalog: {flags:?}");
    if !delivered {
        assert_eq!(status(&app, cx), format!("Burst analysis failed: {}", chairphoto_core::app::CATALOG_CHANGED));
    }
}

#[gpui_kit::test]
fn burst_analysis_never_reaches_the_new_catalog_before_the_switch_event(cx: &mut TestAppContext) {
    burst_across_a_switch(false, cx);
}

#[gpui_kit::test]
fn burst_analysis_never_reaches_the_new_catalog_after_the_switch_event(cx: &mut TestAppContext) {
    burst_across_a_switch(true, cx);
}

/// The first three photos a second apart with one hash, the rest an hour or more later: one
/// burst.
fn make_burst(c: &chairphoto_core::catalog::Catalog, ids: &[i64]) {
    for (i, id) in ids.iter().enumerate() {
        let time = if i < 3 { format!("2026-01-01T10:00:0{i}") } else { format!("2026-01-01T{}:00:00", 11 + i) };
        c.conn().execute("UPDATE photos SET capture_time = ?1, phash = 7 WHERE id = ?2", (time.as_str(), *id)).unwrap();
    }
}

/// Two burst analyses (gpui #106 gate): A over the whole view (a four-frame burst with a
/// soft third frame), then B over that soft frame alone, which clears its flag. Whichever
/// worker runs first, B's verdict stands and B's result is the status line; A's late result
/// — delivered after B's — is dropped (the core's own half, an older run never writing after
/// a newer one, is forced in `burst_analysis::ownership_tests`).
#[gpui_kit::test]
fn a_superseded_burst_analysis_is_neither_persisted_nor_shown(cx: &mut TestAppContext) {
    let dir = TempDir::new("grid-burst-runs");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 4, cx);
    {
        let guard = app.state.catalog.lock().unwrap();
        let c = guard.as_ref().unwrap();
        make_burst(c, &ids);
        for (&id, sharpness) in ids.iter().zip([100.0, 100.0, 10.0, 90.0]) {
            c.conn().execute("UPDATE photos SET sharpness = ?1 WHERE id = ?2", (sharpness, id)).unwrap();
        }
    }
    let analyse = |cx: &mut TestAppContext| {
        cx.update_window(app.window(), |_, window, cx| {
            window.dispatch_action(Box::new(crate::shell::actions::AnalyseBurst), cx)
        })
        .unwrap();
    };
    analyse(cx); // A: nothing selected, so the whole view
    let (job_a, generation) = app.wired.shell.read_with(cx, |s, _| s.burst_owner());
    app.wired.shell.update(cx, |s, _| s.library.select(ids[2], Default::default()));
    analyse(cx); // B: the soft frame alone
    cx.run_until_parked();
    let b_line = "Burst analysis done — 1 cluster(s), 0 best frame(s), 0 soft-in-burst.";
    assert_eq!(status(&app, cx), b_line);
    let soft_flag = app.state.catalog.lock().unwrap().as_ref().unwrap().get_photo(ids[2]).unwrap().burst_flag;
    assert_eq!(soft_flag, None, "A's soft-in-burst flag overwrote B's verdict");

    // A's result arriving after B's, as a slower run's would.
    let late = chairphoto_core::burst_analysis::BurstAnalysisResult {
        total: 4,
        clusters: 1,
        flagged_soft: 1,
        flagged_best: 1,
        cleared: 0,
    };
    app.wired.shell.update(cx, |s, cx| s.finish_burst(job_a, generation, Ok(late), cx));
    cx.run_until_parked();
    assert_eq!(status(&app, cx), b_line, "the superseded run's result was shown");
}

fn stack_dialog(app: &App, cx: &mut TestAppContext) -> Entity<crate::library::stacks::StackDialog> {
    let root = app.wired.root.clone().unwrap();
    root.read_with(cx, |root, _| root.stacks.as_ref().map(|(d, _)| d.clone())).expect("the Stack dialog is open")
}

/// **Forced interleaving** (finding 2). The Stack dialog proposed a burst in the open
/// catalog; the core switches to a catalog whose photos carry the same ids and form the same
/// burst; then Stack is accepted (before the event closes the dialog), or accepted and the
/// event delivered before the write runs. Nothing is stacked in the new catalog.
fn stack_accept_across_a_switch(delivered: bool, cx: &mut TestAppContext) {
    let dir = TempDir::new("grid-stack-switch");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 4, cx);
    make_burst(app.state.catalog.lock().unwrap().as_ref().unwrap(), &ids);
    cx.update_window(app.window(), |_, window, cx| {
        window.dispatch_action(Box::new(crate::shell::actions::ProposeStacks), cx)
    })
    .unwrap();
    cx.run_until_parked();
    let dialog = stack_dialog(&app, cx);
    let group = dialog.read_with(cx, |d, _| {
        let r = d.result.as_ref().expect("proposed");
        assert_eq!(r.proposals.len(), 1);
        r.proposals[0].keeper_id
    });

    let (b, b_ids) = colliding_catalog(&dir, "b", 4);
    assert_eq!(b_ids, ids);
    make_burst(&b, &b_ids);
    core_switch(&app, b);
    dialog.update(cx, |d, cx| d.accept(group, cx));
    if delivered {
        deliver_switch(&app, cx);
    }
    cx.run_until_parked();
    let stacked = {
        let guard = app.state.catalog.lock().unwrap();
        b_ids.iter().filter(|&&id| guard.as_ref().unwrap().get_photo(id).unwrap().stack_parent_id.is_some()).count()
    };
    assert_eq!(stacked, 0, "the old catalog's proposal stacked the new catalog's photos");
    if !delivered {
        dialog.read_with(cx, |d, _| assert_eq!(d.error.as_deref(), Some(chairphoto_core::app::CATALOG_CHANGED)));
    }
}

#[gpui_kit::test]
fn a_stack_accept_never_reaches_the_new_catalog_before_the_switch_event(cx: &mut TestAppContext) {
    stack_accept_across_a_switch(false, cx);
}

#[gpui_kit::test]
fn a_stack_accept_never_reaches_the_new_catalog_after_the_switch_event(cx: &mut TestAppContext) {
    stack_accept_across_a_switch(true, cx);
}

/// Whether the grid holds the focus (its keys work).
fn grid_focused(app: &App, cx: &mut TestAppContext) -> bool {
    let grid = library_view(app, cx);
    cx.update_window(app.window(), |_, window, cx| grid.read(cx).focus_handle().is_focused(window)).unwrap()
}

/// Make the main window the active one, as on screen: GPUI reports focus changes to its
/// listeners only in an active window.
fn activate(app: &App, cx: &mut TestAppContext) {
    cx.update_window(app.window(), |_, window, _| window.activate_window()).unwrap();
    cx.run_until_parked();
}

/// More ⋯ → Preferences…, its row in the menu.
const PREFERENCES_ROW: usize = 15;

/// Finding 4: a title-bar menu action leaves the grid's keys working. gpui-component's
/// popup focuses the menu's action context (the root) to dispatch, and leaves focus there;
/// the root hands it back to the grid.
#[gpui_kit::test]
fn grid_keys_work_after_a_title_bar_menu_action(cx: &mut TestAppContext) {
    let dir = TempDir::new("grid-menu-focus");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 3, cx);
    activate(&app, cx);
    click_tile(&app, ids[1], Modifiers::default(), cx);
    click_menu_row(&app, "import-menu", 4, "Cache previews on import", cx);
    render(&app, cx);
    assert!(grid_focused(&app, cx), "focus stayed on the root after the menu action");
    press(&app, "4", cx);
    assert_eq!(rating_of(&app, ids[1]).0, 4, "the grid's key did nothing after the menu");
}

/// Finding 4, a dialog opened from a menu: when it closes, focus returns to the root, and
/// the root hands it to the grid.
#[gpui_kit::test]
fn grid_keys_work_after_a_dialog_opened_from_a_menu_closes(cx: &mut TestAppContext) {
    use gpui_kit::component::WindowExt as _;
    let dir = TempDir::new("grid-dialog-focus");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 3, cx);
    activate(&app, cx);
    click_tile(&app, ids[1], Modifiers::default(), cx);
    click_menu_row(&app, "more-menu", PREFERENCES_ROW, "Preferences…", cx);
    render(&app, cx);
    assert!(!grid_focused(&app, cx), "the dialog took the focus");
    cx.update_window(app.window(), |_, window, cx| window.close_dialog(cx)).unwrap();
    cx.run_until_parked();
    render(&app, cx);
    assert!(grid_focused(&app, cx), "the closed dialog left the focus on the root");
    press(&app, "3", cx);
    assert_eq!(rating_of(&app, ids[1]).0, 3);
}

/// Finding 5: a catalog switch that closes the Stack dialog gives the grid its focus back.
#[gpui_kit::test]
fn a_switch_closing_the_stack_dialog_refocuses_the_grid(cx: &mut TestAppContext) {
    let dir = TempDir::new("grid-stack-refocus");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 4, cx);
    make_burst(app.state.catalog.lock().unwrap().as_ref().unwrap(), &ids);
    cx.update_window(app.window(), |_, window, cx| {
        window.dispatch_action(Box::new(crate::shell::actions::ProposeStacks), cx)
    })
    .unwrap();
    cx.run_until_parked();
    render(&app, cx);
    stack_dialog(&app, cx);
    assert!(!grid_focused(&app, cx));
    let (b, _) = colliding_catalog(&dir, "b", 2);
    core_switch(&app, b);
    deliver_switch(&app, cx);
    render(&app, cx);
    let root = app.wired.root.clone().unwrap();
    assert!(root.read_with(cx, |root, _| root.stacks.is_none()), "the switch closed the dialog");
    assert!(grid_focused(&app, cx), "the grid lost its keys with the dialog");
}

/// Finding 6: the Stack dialog asks for the thumbnails of the groups on screen (plus an
/// overscan), not all of them; what scrolls away is released, and closing releases the rest.
/// Its image store here has a pool that answers nothing, so a request stays pending until it
/// is released (cancelled in the pool).
#[gpui_kit::test]
fn the_stack_dialog_windows_and_releases_its_thumbnails(cx: &mut TestAppContext) {
    use crate::image_tests::FakePool;
    use crate::library::stacks::StackDialog;
    use chairphoto_core::image_pool::{ImageKind, JobKey};
    let dir = TempDir::new("grid-stack-thumbs");
    let app = start(cx);
    let groups = 40;
    let ids = open_catalog_with_photos(&app, &dir, groups * 2, cx);
    {
        let guard = app.state.catalog.lock().unwrap();
        let c = guard.as_ref().unwrap();
        // Pairs a second apart, pairs an hour apart, each pair its own hash.
        for (i, id) in ids.iter().enumerate() {
            let (g, k) = (i / 2, i % 2);
            let time = format!("2026-01-{:02}T{:02}:00:0{k}", 1 + g / 20, g % 20);
            c.conn()
                .execute(
                    "UPDATE photos SET capture_time = ?1, phash = ?2 WHERE id = ?3",
                    (time.as_str(), (g as i64) * 1_000_003, *id),
                )
                .unwrap();
        }
    }
    let pool = std::sync::Arc::new(FakePool::default());
    let images = cx.update(|cx| {
        let submit: std::sync::Arc<dyn crate::image_store::Submit> = pool.clone();
        cx.new(|cx| crate::image_store::ImageStore::new(submit, crate::image_store::DEFAULT_BUDGET_BYTES, cx))
    });
    let (model, shell) = (app.wired.model.clone(), app.wired.shell.clone());
    let from = shell.read_with(cx, |s, _| s.rows_from().unwrap());
    let dialog_window = cx.update(|cx| {
        let (images, ids) = (images.clone(), ids.clone());
        cx.open_window(Default::default(), |window, cx| {
            cx.new(|cx| StackDialog::new(&model, shell, images, ids, from, window, cx))
        })
        .unwrap()
    });
    let dialog = dialog_window.root(cx).unwrap();
    let window: gpui_kit::AnyWindowHandle = dialog_window.into();
    cx.run_until_parked();
    for _ in 0..3 {
        cx.update_window(window, |_, window, cx| window.render_frame(cx)).unwrap();
        cx.run_until_parked();
    }
    dialog.read_with(cx, |d, _| assert_eq!(d.result.as_ref().map(|r| r.proposals.len()), Some(groups)));
    let first = pool.submitted();
    assert!(first > 0, "the visible groups asked for their frames");
    assert!(first < groups, "{first} of {} frames requested at once", groups * 2);
    let first_window = dialog.read_with(cx, |d, _| d.requested.clone());

    for _ in 0..6 {
        cx.update_window(window, |_, window, cx| {
            window.scroll("stack-body", ScrollDelta::Pixels(point(px(0.), px(-600.))), cx)
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| window.render_frame(cx)).unwrap();
        cx.run_until_parked();
    }
    let later = dialog.read_with(cx, |d, _| d.requested.clone());
    assert!(pool.submitted() > first, "scrolling asked for the revealed groups");
    let cancelled: Vec<JobKey> = pool.cancelled.lock().unwrap().clone();
    let gone: Vec<i64> = first_window.difference(&later).copied().collect();
    assert!(!gone.is_empty(), "the window did not move");
    for id in &gone {
        assert!(cancelled.contains(&JobKey::photo(*id, ImageKind::Thumb)), "frame {id} scrolled away but was not released");
    }

    drop(dialog);
    cx.update_window(window, |_, window, _| window.remove_window()).unwrap();
    cx.run_until_parked();
    let cancelled: Vec<JobKey> = pool.cancelled.lock().unwrap().clone();
    for id in &later {
        assert!(cancelled.contains(&JobKey::photo(*id, ImageKind::Thumb)), "frame {id} still wanted after the dialog closed");
    }
    images.read_with(cx, |s, _| assert!(later.iter().all(|&id| !s.is_pending(id, ImageKind::Thumb))));
}

/// A page from a superseded read does not replace the newer rows (the session's
/// generation), and the newest read is the one that marks the rows loaded.
#[gpui_kit::test]
fn a_stale_page_does_not_replace_newer_rows(cx: &mut TestAppContext) {
    let dir = TempDir::new("grid-stale");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 3, cx);
    let all_rows = app.wired.shell.read_with(cx, |s, _| s.library.photos().to_vec());
    let stale = app.wired.shell.update(cx, |s, _| s.library.refresh());
    click(&app, "filter-Picks", cx);
    assert!(rows(&app, cx).is_empty());
    app.wired.shell.update(cx, |s, cx| {
        let from = s.rows_from().unwrap();
        s.on_page(&stale, Ok((from, PhotoPage { photos: all_rows, offset: 0, total: ids.len() })), cx)
    });
    assert!(rows(&app, cx).is_empty(), "the old unfiltered page landed over the Picks view");
}

/// A photo link widens a filtered view to the whole library and selects the photo; a tag
/// link becomes the scope.
#[gpui_kit::test]
fn deep_links_select_the_photo_and_filter_by_the_tag(cx: &mut TestAppContext) {
    let dir = TempDir::new("grid-links");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 3, cx);
    let (uuid, tag_uuid, tag_id) = {
        let guard = app.state.catalog.lock().unwrap();
        let c = guard.as_ref().unwrap();
        let tag_id = c.create_tag("Places/Oslo").unwrap();
        c.assign_tag(ids[0], tag_id).unwrap();
        (c.get_photo(ids[2]).unwrap().uuid, c.get_tag(tag_id).unwrap().uuid, tag_id)
    };
    click(&app, "filter-Picks", cx);
    assert!(rows(&app, cx).is_empty());

    app.wired.model.update(cx, |m, cx| m.open_url(&format!("chairphoto://{uuid}"), cx));
    cx.run_until_parked();
    app.wired.shell.read_with(cx, |s, _| assert_eq!(s.library.scope().filter, CullingFilter::All, "widened"));
    assert_eq!(rows(&app, cx), ids);
    assert_eq!(selection(&app, cx), (Some(ids[2]), vec![ids[2]]));

    app.wired.model.update(cx, |m, cx| m.open_url(&format!("chairphoto://tag/{tag_uuid}"), cx));
    cx.run_until_parked();
    app.wired.shell.read_with(cx, |s, _| assert_eq!(s.library.scope().tag_id, Some(tag_id)));
    assert_eq!(rows(&app, cx), vec![ids[0]]);
}

/// Thumbnails are requested for the visible rows plus the overscan only — not for every
/// photo — and scrolling asks for the rows it reveals.
#[gpui_kit::test]
fn thumbnails_are_requested_per_window(cx: &mut TestAppContext) {
    let dir = TempDir::new("grid-thumbs");
    let app = start(cx);
    let n = 2000;
    open_catalog_with_photos(&app, &dir, n, cx);
    render(&app, cx);
    let first = app.wired.images.read_with(cx, |s, _| s.stats().submitted);
    assert!(first > 0, "the visible tiles asked for thumbnails");
    assert!(first < 400, "{first} of {n} thumbnails requested at once");

    // The grid opened at the bottom (newest); scroll up a long way.
    for _ in 0..5 {
        cx.update_window(app.window(), |_, window, cx| {
            window.scroll("library", ScrollDelta::Pixels(point(px(0.), px(2000.))), cx)
        })
        .unwrap();
        cx.run_until_parked();
    }
    let after = app.wired.images.read_with(cx, |s, _| s.stats().submitted);
    assert!(after > first, "scrolling requested the revealed rows ({first} → {after})");
    assert!(after < 1200, "{after} requests after scrolling past a fraction of {n}");
}

/// Codex gate (Low): a filter that empties the grid releases the thumbnails the last window
/// asked for — an empty grid draws no list, so the per-frame window hook never runs. The
/// grid here is a second `LibraryView` over the app's shell with a pool that answers
/// nothing, so each request stays pending until it is released (cancelled in the pool).
#[gpui_kit::test]
fn a_filter_that_empties_the_grid_releases_its_thumbnails(cx: &mut TestAppContext) {
    use crate::image_tests::FakePool;
    use chairphoto_core::image_pool::{ImageKind, JobKey};
    let dir = TempDir::new("grid-empty-release");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 12, cx);
    let pool = std::sync::Arc::new(FakePool::default());
    let images = cx.update(|cx| {
        let submit: std::sync::Arc<dyn crate::image_store::Submit> = pool.clone();
        cx.new(|cx| crate::image_store::ImageStore::new(submit, crate::image_store::DEFAULT_BUDGET_BYTES, cx))
    });
    let shell = app.wired.shell.clone();
    let grid_window = cx.update(|cx| {
        let images = images.clone();
        cx.open_window(Default::default(), |_, cx| cx.new(|cx| LibraryView::new(shell, images, cx))).unwrap()
    });
    let window: gpui_kit::AnyWindowHandle = grid_window.into();
    let frame = |cx: &mut TestAppContext| {
        cx.update_window(window, |_, window, cx| window.render_frame(cx)).unwrap();
        cx.run_until_parked();
    };
    frame(cx);
    frame(cx);
    let pending: Vec<i64> =
        images.read_with(cx, |s, _| ids.iter().copied().filter(|&id| s.is_pending(id, ImageKind::Thumb)).collect());
    assert!(!pending.is_empty(), "the grid asked for its tiles' thumbnails");

    click(&app, "filter-Picks", cx);
    assert!(rows(&app, cx).is_empty());
    frame(cx);
    let cancelled: Vec<JobKey> = pool.cancelled.lock().unwrap().clone();
    for id in &pending {
        assert!(cancelled.contains(&JobKey::photo(*id, ImageKind::Thumb)), "thumbnail {id} still queued for an empty grid");
    }
    images.read_with(cx, |s, _| assert!(pending.iter().all(|&id| !s.is_pending(id, ImageKind::Thumb))));
}

/// The "Stack bursts" dialog proposes the burst, and accepting stacks it under the keeper:
/// the other frames leave the grid.
#[gpui_kit::test]
fn the_stack_dialog_proposes_and_stacks_a_burst(cx: &mut TestAppContext) {
    let dir = TempDir::new("grid-stacks");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 4, cx);
    {
        // Three frames a second apart with the same hash; the fourth an hour later.
        let guard = app.state.catalog.lock().unwrap();
        let c = guard.as_ref().unwrap();
        for (i, id) in ids.iter().enumerate() {
            let time = if i < 3 { format!("2026-01-01T10:00:0{i}") } else { "2026-01-01T11:00:00".into() };
            c.conn()
                .execute("UPDATE photos SET capture_time = ?1, phash = 7 WHERE id = ?2", (time.as_str(), *id))
                .unwrap();
        }
    }
    cx.update_window(app.window(), |_, window, cx| {
        window.dispatch_action(Box::new(crate::shell::actions::ProposeStacks), cx)
    })
    .unwrap();
    cx.run_until_parked();
    render(&app, cx);
    let keeper = ids[0];
    cx.update_window(app.window(), |_, window, _| {
        let summary = window.find("stack-summary").label().map(str::to_string).unwrap_or_default();
        assert!(summary.starts_with("1 group in 4 photos."), "{summary}");
    })
    .unwrap();
    cx.update_window(app.window(), |_, window, cx| window.click(format!("stack-{keeper}"), cx)).unwrap();
    cx.run_until_parked();
    assert_eq!(rows(&app, cx), vec![ids[0], ids[3]], "the burst collapsed under its keeper");
    render(&app, cx);
    cx.update_window(app.window(), |_, window, _| {
        assert_eq!(window.find("stack-done").label(), Some("Stacked 1 group this session."));
    })
    .unwrap();
    press(&app, "escape", cx);
    let root = app.wired.root.clone().unwrap();
    assert!(root.read_with(cx, |root, _| root.stacks.is_none()), "Escape closed it");
}

/// With nothing in view the whole-view tools say so instead of opening.
#[gpui_kit::test]
fn whole_view_tools_need_photos(cx: &mut TestAppContext) {
    let dir = TempDir::new("grid-tools");
    let app = start(cx);
    open_catalog_with_photos(&app, &dir, 0, cx);
    for (action, line) in [
        (
            Box::new(crate::shell::actions::ProposeStacks) as Box<dyn gpui_kit::Action>,
            "No photos to group — scan or select some first.",
        ),
        (Box::new(crate::shell::actions::AnalyseBurst), "No photos to analyse — scan or select some first."),
    ] {
        cx.update_window(app.window(), |_, window, cx| window.dispatch_action(action, cx)).unwrap();
        cx.run_until_parked();
        assert_eq!(status(&app, cx), line);
    }
}

/// Two culling keys pressed on one photo before either write has run both step from that
/// photo, over the rows they started from — not from wherever the first one's step and
/// refresh left the grid (the session's `step_active_over`, React's closures).
#[gpui_kit::test]
fn overlapping_culls_both_step_from_the_photo_they_started_on(cx: &mut TestAppContext) {
    let dir = TempDir::new("grid-overlap");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 4, cx);
    click(&app, "filter-Unrated", cx);
    click_tile(&app, ids[1], Modifiers::default(), cx);
    cx.update_window(app.window(), |_, window, cx| {
        window.dispatch_action(Box::new(crate::library::Rate3), cx);
        window.dispatch_action(Box::new(crate::library::Rate2), cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(rating_of(&app, ids[1]).0, 2, "both marked the photo they were pressed on, in order");
    assert_eq!(rating_of(&app, ids[2]).0, 0);
    assert_eq!(rows(&app, cx), vec![ids[0], ids[2], ids[3]]);
    assert_eq!(selection(&app, cx).0, Some(ids[2]), "not skipped past the unjudged ids[2]");
}

/// Measurement, not a check (run it with `--ignored --nocapture`): the grid's UI-thread
/// cost per frame — render, layout, prepaint and paint into the scene, no GPU — while it
/// scrolls a 20,000-photo synthetic catalog 24 px per frame, as the frame bench
/// (`examples/grid_bench.rs`) does on screen. The test platform has no decode pool, so
/// every tile ends as a failed-thumbnail tile (text, no image).
#[gpui_kit::test]
#[ignore = "measurement: the grid's CPU frame cost on a synthetic catalog"]
fn measure_grid_frame_cost(cx: &mut TestAppContext) {
    use chairphoto_core::app::EventSink as _;
    use std::time::Instant;
    let dir = TempDir::new("grid-frames");
    let app = start(cx);
    let n = 20_000;
    {
        let root = dir.0.join("photos");
        let catalog = chairphoto_core::catalog::Catalog::open(&dir.0.join("frames.chairphoto"), &root).unwrap();
        let tx = catalog.conn().unchecked_transaction().unwrap();
        for i in 0..n {
            catalog.upsert_photo(&root.join(format!("bench/{i:06}.jpg")), None, 0, 1).unwrap();
        }
        tx.commit().unwrap();
        *app.state.catalog.lock().unwrap() = Some(catalog);
    }
    app.state.send(CoreEvent::CatalogSwitched("frames.chairphoto".into()));
    cx.run_until_parked();
    assert_eq!(rows(&app, cx).len(), n);
    for _ in 0..3 {
        render(&app, cx);
    }
    let handle = library_view(&app, cx).read_with(cx, |v, _| v.scroll_handle().clone());
    let mut frames = Vec::new();
    for _ in 0..300 {
        {
            let base = &handle.0.borrow().base_handle;
            let mut offset = base.offset();
            offset.y = (offset.y + px(24.)).min(px(0.));
            base.set_offset(offset);
        }
        let took = cx
            .update_window(app.window(), |_, window, cx| {
                let start = Instant::now();
                window.render_frame(cx);
                start.elapsed()
            })
            .unwrap();
        frames.push(took.as_secs_f64() * 1e3);
        cx.run_until_parked(); // the image answers land between frames
    }
    frames.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| frames[((frames.len() - 1) as f64 * p) as usize];
    let submitted = app.wired.images.read_with(cx, |s, _| s.stats().submitted);
    let cols = library_view(&app, cx).read_with(cx, |v, _| v.columns());
    eprintln!(
        "MEASURE grid frame cost, {n} photos, {cols} columns, 300 frames at 24 px: p50 {:.2} ms, p95 {:.2} ms, \
         max {:.2} ms; thumbnails requested {submitted}",
        pct(0.5),
        pct(0.95),
        frames.last().unwrap()
    );
}

/// The video badge's tooltip says what a double-click does here — open the loupe, whose button
/// plays the video — not React's "double-click to play" (#161).
#[test]
fn the_video_tooltip_names_the_loupes_play_button() {
    let tip = crate::library::grid::video_tip();
    assert_eq!(tip, "Video — double-click to open, then Play in system player");
    assert!(crate::loupe::view::PLAY_LABEL.ends_with("Play in system player"), "the loupe's button is the one named");
}
