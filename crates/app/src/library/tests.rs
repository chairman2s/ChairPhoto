//! Headless tests of the Library view through the real wiring (`start` → `wire` → the main
//! window): the grid's rows, clicks and keys, the culling write path, row generations, deep
//! links, thumbnails per window, and the "Stack bursts" dialog.

use crate::library::grid::LibraryView;
use crate::shell::state::Mark;
use crate::tests::{
    click, click_menu_row, colliding_catalog, core_switch, deliver_switch, open_catalog_with_photos, press, start, status,
    App, TempDir,
};
use crate::image_store::{ImageState, StoreStats};
use crate::image_tests::{pixels, FakePool};
use chairphoto_core::app::CoreEvent;
use chairphoto_core::image_pool::{ImageKind, JobKey};
use chairphoto_model::darkroom::filmstrip::CoverLook;
use std::sync::Arc;
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
pub(super) fn make_burst(c: &chairphoto_core::catalog::Catalog, ids: &[i64]) {
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

pub(super) fn stack_dialog(app: &App, cx: &mut TestAppContext) -> Entity<crate::library::stacks::StackDialog> {
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

// --- the column count follows the width (#187) ------------------------------------------------

/// After each window resize the grid's column count is the one its measured width gives,
/// with no frame forced: only what GPUI redraws by itself, as in the running app. The width
/// is measured during the list's layout, so a change asks for another frame then — a request
/// GPUI dropped mid-draw, which left the columns one width behind (4 at 700 px wide, where
/// the 624 px list fits 3).
#[gpui_kit::test]
fn the_column_count_follows_the_width_after_each_resize(cx: &mut TestAppContext) {
    let dir = TempDir::new("grid-resize");
    let app = start(cx);
    open_catalog_with_photos(&app, &dir, 60, cx);
    render(&app, cx);
    let view = library_view(&app, cx);
    let tile_min = app.wired.shell.read_with(cx, |s, _| s.layout.thumb_size);
    let mut seen = Vec::new();
    for (w, h) in [(700., 800.), (1500., 900.), (1100., 800.), (700., 800.)] {
        cx.simulate_window_resize(app.window(), gpui_kit::size(px(w), px(h)));
        cx.run_until_parked();
        let (cols, measured) = view.read_with(cx, |v, _| (v.columns(), v.measured()));
        let measured = measured.expect("laid out").0;
        let want = crate::library::layout::columns(measured, tile_min);
        assert_eq!(cols, want, "{w}x{h}: the list is {measured} px wide");
        seen.push(want);
    }
    seen.dedup();
    assert!(seen.len() >= 3, "the widths give different column counts: {seen:?}");
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

// --- the bench's selection pile (#171) ---------------------------------------------------------

/// #171: the bench's pile shows the first three selected photos (row order) as thumbnails,
/// the one the bench marks highlighted (Bench.tsx's `selectionThumbs`), asked for through the
/// image layer under the bench's own claim — which a new selection replaces and a cleared
/// one empties, releasing what only the pile wanted.
#[gpui_kit::test]
fn the_bench_pile_shows_the_first_three_selected_thumbnails(cx: &mut TestAppContext) {
    let dir = TempDir::new("bench-pile");
    let pool = Arc::new(FakePool::default());
    let app = crate::tests::start_with_pool(cx, pool.clone());
    let ids = open_catalog_with_photos(&app, &dir, 6, cx);
    let claim = app.wired.root.as_ref().unwrap().read_with(cx, |r, _| r.bench_claim);
    let claimed = |cx: &mut TestAppContext| {
        let mut held: Vec<i64> = app
            .wired
            .images
            .read_with(cx, |s, _| s.claim(claim))
            .into_iter()
            .map(|(id, kind)| {
                assert_eq!(kind, ImageKind::Thumb);
                id
            })
            .collect();
        held.sort();
        held
    };
    let thumbs = |cx: &mut TestAppContext| {
        let mut out = Vec::new();
        cx.update_window(app.window(), |_, window, cx| {
            window.render_frame(cx);
            for &id in &ids {
                if let Some(e) = window.try_find(("bench-thumb", id as u64)) {
                    out.push(e.label().unwrap_or_default().to_string());
                }
            }
        })
        .unwrap();
        out
    };

    // Selected in the order 4, 2, 1, 3 (the active one, last clicked): the pile is the first
    // three in row order, and the active photo among them is ringed.
    click_tile(&app, ids[4], Modifiers::default(), cx);
    for i in [2, 1, 3] {
        click_tile(&app, ids[i], Modifiers::control(), cx);
    }
    render(&app, cx);
    assert_eq!(thumbs(cx), ["p1.ARW", "p2.ARW", "p3.ARW (marking)"]);
    assert_eq!(claimed(cx), vec![ids[1], ids[2], ids[3]]);
    assert!(submitted_thumb(&pool, ids[1]), "asked of the decode pool, not decoded here");

    // Its pixels land: the store holds them for the pile.
    pool.finish(&JobKey::photo(ids[1], ImageKind::Thumb), Ok(pixels(60, 40)));
    render(&app, cx);
    assert!(matches!(app.wired.images.read_with(cx, |s, _| s.peek(ids[1], ImageKind::Thumb)), ImageState::Ready(_)));

    // A new selection replaces the claim; a cleared one empties it.
    click_tile(&app, ids[5], Modifiers::default(), cx);
    assert_eq!(thumbs(cx), ["p5.ARW (marking)"]);
    assert_eq!(claimed(cx), vec![ids[5]]);
    cx.update_window(app.window(), |_, window, cx| {
        window.dispatch_action(Box::new(crate::shell::actions::ClearSelection), cx)
    })
    .unwrap();
    render(&app, cx);
    assert!(thumbs(cx).is_empty());
    assert!(claimed(cx).is_empty(), "no selection, no pile: the claim is released");
}

/// #171: the pile shares its tiers with the grid, which holds no claim. While the pile shows
/// a thumbnail the grid's scrolling cannot release it; when the pile lets go, it releases
/// only what the grid no longer asks for — never a visible tile's render.
#[gpui_kit::test]
fn the_bench_pile_releases_only_what_the_grid_no_longer_wants(cx: &mut TestAppContext) {
    let dir = TempDir::new("bench-pile-release");
    let pool = Arc::new(FakePool::default());
    let app = crate::tests::start_with_pool(cx, pool.clone());
    open_catalog_with_photos(&app, &dir, 600, cx);
    render(&app, cx);
    let rows = rows(&app, cx);
    let thumb = |id: i64| JobKey::photo(id, ImageKind::Thumb);
    let cancelled = |id: i64| pool.cancelled.lock().unwrap().contains(&thumb(id));
    let grid_wants = |id: i64, cx: &mut TestAppContext| library_view(&app, cx).read_with(cx, |v, _| v.requested_thumbs().contains(&id));

    // The grid opened at its newest rows; the last three photos are on the table, and the
    // grid wants them too: clearing the selection releases nothing of theirs.
    let pile: Vec<i64> = rows[rows.len() - 3..].to_vec();
    app.wired.shell.update(cx, |s, cx| s.select_with(cx, |l| l.select_single(pile[2])));
    click_tile(&app, pile[0], Modifiers::control(), cx);
    click_tile(&app, pile[1], Modifiers::control(), cx);
    render(&app, cx);
    for &id in &pile {
        assert!(grid_wants(id, cx) && submitted_thumb(&pool, id), "photo {id} on screen and asked for");
    }
    cx.update_window(app.window(), |_, window, cx| window.dispatch_action(Box::new(crate::shell::actions::ClearSelection), cx))
        .unwrap();
    render(&app, cx);
    for &id in &pile {
        assert!(!cancelled(id), "the pile let go of {id}, which the grid still shows");
    }

    // Again, then scroll the grid far away: it lets go of those tiles, the pile's claim keeps
    // their renders; clearing the selection now releases them.
    app.wired.shell.update(cx, |s, cx| s.select_with(cx, |l| l.select_single(pile[2])));
    click_tile(&app, pile[0], Modifiers::control(), cx);
    click_tile(&app, pile[1], Modifiers::control(), cx);
    render(&app, cx);
    for _ in 0..5 {
        cx.update_window(app.window(), |_, window, cx| {
            window.scroll("library", ScrollDelta::Pixels(point(px(0.), px(2000.))), cx)
        })
        .unwrap();
        cx.run_until_parked();
    }
    render(&app, cx);
    for &id in &pile {
        assert!(!grid_wants(id, cx), "photo {id} scrolled out of the grid's window");
        assert!(!cancelled(id), "the grid released {id}, which the pile holds");
        assert!(app.wired.images.read_with(cx, |s, _| s.is_pending(id, ImageKind::Thumb)));
    }
    cx.update_window(app.window(), |_, window, cx| window.dispatch_action(Box::new(crate::shell::actions::ClearSelection), cx))
        .unwrap();
    render(&app, cx);
    for &id in &pile {
        assert!(cancelled(id), "the pile let go of {id}, which nobody wants now");
    }
}

fn submitted_thumb(pool: &FakePool, id: i64) -> bool {
    let key = JobKey::photo(id, ImageKind::Thumb);
    pool.batches.lock().unwrap().iter().any(|b| b.contains(&key))
}

// --- cover looks (#151) ---------------------------------------------------------------------

/// The Library on a hand-driven decode pool: every thumbnail request is held until the test
/// answers it.
struct LookRig {
    app: App,
    dir: TempDir,
    ids: Vec<i64>,
    pool: Arc<FakePool>,
}

impl LookRig {
    fn new(tag: &str, n: usize, cx: &mut TestAppContext) -> Self {
        let dir = TempDir::new(tag);
        let pool = Arc::new(FakePool::default());
        let app = crate::tests::start_with_pool(cx, pool.clone());
        let ids = open_catalog_with_photos(&app, &dir, n, cx);
        render(&app, cx);
        Self { app, dir, ids, pool }
    }

    fn catalog<T>(&self, f: impl FnOnce(&chairphoto_core::catalog::Catalog) -> T) -> T {
        f(self.app.state.catalog.lock().unwrap().as_ref().unwrap())
    }

    /// The Thumb jobs sent for `photo` so far.
    fn jobs(&self, photo: i64) -> usize {
        let key = JobKey::photo(photo, ImageKind::Thumb);
        self.pool.batches.lock().unwrap().iter().flatten().filter(|k| **k == key).count()
    }

    fn all_jobs(&self) -> Vec<usize> {
        self.ids.iter().map(|&id| self.jobs(id)).collect()
    }

    /// Answer `photo`'s thumbnail render with a `w` pixels wide image.
    fn finish(&self, photo: i64, w: u32, cx: &mut TestAppContext) {
        self.pool.finish(&JobKey::photo(photo, ImageKind::Thumb), Ok(pixels(w, 4)));
        cx.run_until_parked();
    }

    /// The width of the thumbnail the grid draws for `photo`, or what it has instead.
    fn tile(&self, photo: i64, cx: &mut TestAppContext) -> Result<u32, &'static str> {
        match self.app.wired.images.read_with(cx, |s, _| s.peek(photo, ImageKind::Thumb)) {
            ImageState::Ready(l) => Ok(l.image.size(0).width.0 as u32),
            ImageState::Loading => Err("loading"),
            ImageState::Failed(_) => Err("failed"),
            ImageState::Absent => Err("absent"),
        }
    }

    fn stats(&self, cx: &mut TestAppContext) -> StoreStats {
        self.app.wired.images.read_with(cx, |s, _| s.stats())
    }

    /// The rows re-read (what leaving the Darkroom does), then a frame of the grid.
    fn refresh_rows(&self, cx: &mut TestAppContext) {
        self.app.wired.shell.update(cx, |s, cx| s.refresh_rows(cx));
        cx.run_until_parked();
        render(&self.app, cx);
    }

    /// Make a new version of `photo` its cover; the version's id.
    fn cover(&self, photo: i64) -> i64 {
        self.catalog(|c| {
            let v = c.create_version(photo, "Warm").unwrap();
            c.set_cover_version(photo, Some(v)).unwrap();
            v
        })
    }
}

/// A tile's thumbnail follows the cover look its row names (React's `?v=` token): a new
/// cover, a new revision of it, or the cover taken off renders that tile again — and only
/// that tile, and only its thumbnail tier — and the earlier look is never shown again; the
/// same look asks nothing.
#[gpui_kit::test]
fn a_grid_tile_follows_its_rows_cover_look(cx: &mut TestAppContext) {
    let rig = LookRig::new("grid-cover", 3, cx);
    let photo = rig.ids[1];
    for &id in &rig.ids {
        rig.finish(id, 4, cx);
    }
    assert_eq!(rig.tile(photo, cx), Ok(4));
    let mut want = rig.all_jobs();
    assert_eq!(want, [1, 1, 1], "one render per tile");
    let images = rig.app.wired.images.clone();
    images.update(cx, |s, _| s.request(photo, ImageKind::Preview));
    rig.pool.finish(&JobKey::photo(photo, ImageKind::Preview), Ok(pixels(16, 16)));
    cx.run_until_parked();

    let version = rig.cover(photo);
    rig.refresh_rows(cx);
    want[1] += 1;
    assert_eq!(rig.all_jobs(), want, "a new cover renders that tile again, no other");
    assert_eq!(rig.tile(photo, cx), Err("loading"), "the earlier look is not shown");
    rig.finish(photo, 8, cx);
    assert_eq!(rig.tile(photo, cx), Ok(8), "the cover's look lands");
    let preview = images.read_with(cx, |s, _| matches!(s.peek(photo, ImageKind::Preview), ImageState::Ready(_)));
    assert!(preview, "only the thumbnail tier follows the look: the preview stays cached");

    render(&rig.app, cx);
    rig.refresh_rows(cx);
    assert_eq!(rig.all_jobs(), want, "the same look: nothing asked");

    // The cover version's settings change: its revision moves.
    rig.catalog(|c| c.set_version_edit(version, r#"{"tone":{"ev":1}}"#).unwrap());
    rig.refresh_rows(cx);
    want[1] += 1;
    assert_eq!(rig.all_jobs(), want, "a new revision renders it again");
    rig.finish(photo, 12, cx);
    assert_eq!(rig.tile(photo, cx), Ok(12));

    // The cover taken off: the plain thumbnail again.
    rig.catalog(|c| c.set_cover_version(photo, None).unwrap());
    rig.refresh_rows(cx);
    want[1] += 1;
    assert_eq!(rig.all_jobs(), want, "no cover: rendered again");
    rig.finish(photo, 4, cx);
    assert_eq!(rig.tile(photo, cx), Ok(4));
}

/// A render for the earlier look, already on a worker when the row's look changed, answers
/// late: it is dropped — never shown, never cached — and the new look goes out after it.
#[gpui_kit::test]
fn a_late_answer_for_a_tiles_earlier_look_is_dropped(cx: &mut TestAppContext) {
    let rig = LookRig::new("grid-cover-late", 2, cx);
    let photo = rig.ids[0];
    rig.pool.start(JobKey::photo(photo, ImageKind::Thumb)); // cannot be cancelled
    let jobs = rig.jobs(photo);

    rig.cover(photo);
    rig.refresh_rows(cx);
    assert_eq!(rig.jobs(photo), jobs, "the new look waits for the running render");
    let dropped = rig.stats(cx).stale_dropped;
    rig.finish(photo, 4, cx);
    assert_eq!(rig.tile(photo, cx), Err("loading"), "the earlier look is not shown");
    assert_eq!(rig.stats(cx).stale_dropped, dropped + 1);
    assert_eq!(rig.jobs(photo), jobs + 1, "then the new look is asked for");
    rig.finish(photo, 8, cx);
    assert_eq!(rig.tile(photo, cx), Ok(8));
}

/// A rotation still invalidates every tier of the photo: the grid asks for its thumbnail
/// again under the same look, and the turned render lands (the look did not change, so it is
/// not refused or held back).
#[gpui_kit::test]
fn a_rotation_still_renders_a_tile_again(cx: &mut TestAppContext) {
    let rig = LookRig::new("grid-cover-rotate", 1, cx);
    let photo = rig.ids[0];
    rig.cover(photo);
    rig.refresh_rows(cx);
    rig.finish(photo, 8, cx);
    assert_eq!(rig.tile(photo, cx), Ok(8));
    let jobs = rig.jobs(photo);

    // The inspector's rotate button: the core turns the photo, the store is invalidated.
    rig.app.wired.shell.update(cx, |s, cx| s.select_with(cx, |l| l.select_single(photo)));
    cx.run_until_parked();
    click(&rig.app, "section-orientation", cx);
    click(&rig.app, "rotate-right", cx);
    assert_eq!(rig.catalog(|c| c.photo_rotation(photo).unwrap()), 90);
    render(&rig.app, cx);
    assert_eq!(rig.jobs(photo), jobs + 1, "the turned thumbnail is asked for");
    assert_eq!(rig.tile(photo, cx), Err("loading"), "the unturned one is not shown");
    rig.finish(photo, 6, cx);
    assert_eq!(rig.tile(photo, cx), Ok(6));
}

/// The Darkroom's strip and the grid name the same looks: entering and leaving the Darkroom
/// renders no grid thumbnail again (the strip letting go does not make the grid's cached
/// tiles unknown — review rv134 M1's lesson that views must not break each other).
#[cfg(feature = "edit")]
#[gpui_kit::test]
fn a_darkroom_visit_renders_no_grid_tile_again(cx: &mut TestAppContext) {
    let rig = LookRig::new("grid-cover-visit", 3, cx);
    for &id in &rig.ids {
        rig.finish(id, 4, cx);
    }
    let before = rig.all_jobs();
    rig.app.wired.shell.update(cx, |s, cx| s.select_with(cx, |l| l.select_single(rig.ids[1])));
    cx.run_until_parked();
    rig.app.wired.shell.update(cx, |s, cx| s.open_develop(cx));
    cx.run_until_parked();
    render(&rig.app, cx);
    rig.app.wired.shell.update(cx, |s, cx| s.show_library(cx));
    cx.run_until_parked();
    rig.refresh_rows(cx);
    render(&rig.app, cx);
    assert_eq!(rig.all_jobs(), before, "nothing rendered again");
    for &id in &rig.ids {
        assert_eq!(rig.tile(id, cx), Ok(4));
    }
}

/// **Catalog identity** (map #92). Catalog B's photo carries the same id and the same cover
/// token as the tile's in A. The core switches while the tile's render is on a worker:
/// - `catalog:switched` withheld: the render answers in B — refused on the worker, never
///   shown under A's row, not a failure, and not asked again under A's row; the event then
///   empties the store and B's rows ask for B's look, which lands.
/// - delivered first: the store forgets A; A's late answer is dropped; B's look lands.
fn grid_cover_across_a_switch(delivered: bool, cx: &mut TestAppContext) {
    let rig = LookRig::new(if delivered { "grid-cover-switch-ev" } else { "grid-cover-switch" }, 2, cx);
    let photo = rig.ids[1];
    let a = rig.app.wired.shell.read_with(cx, |s, _| s.rows_from()).unwrap();
    let version = rig.cover(photo);
    rig.refresh_rows(cx);
    let key = JobKey::photo(photo, ImageKind::Thumb);
    rig.pool.start(key.clone());
    let jobs = rig.jobs(photo);

    let (b, b_ids) = colliding_catalog(&rig.dir, "b", 2);
    let b_version = b.create_version(b_ids[1], "B's").unwrap();
    let token = b.set_cover_version(b_ids[1], Some(b_version)).unwrap();
    assert_eq!((b_ids[1], token), (photo, Some(format!("{version}:0"))), "the ids and the token collide");
    core_switch(&rig.app, b);

    if !delivered {
        let refused = rig.stats(cx).refused;
        rig.finish(photo, 4, cx);
        assert_eq!(rig.stats(cx).refused, refused + 1, "refused on the worker");
        assert_eq!(rig.tile(photo, cx), Err("absent"), "not shown under A's row, and not a failure");
        render(&rig.app, cx);
        assert_eq!(rig.jobs(photo), jobs, "not asked again while the row is A's");
        deliver_switch(&rig.app, cx);
        render(&rig.app, cx);
    } else {
        deliver_switch(&rig.app, cx);
        let dropped = rig.stats(cx).stale_dropped;
        render(&rig.app, cx);
        rig.finish(photo, 4, cx);
        assert_eq!(rig.stats(cx).stale_dropped, dropped + 1, "A's answer dropped");
    }
    let b = rig.app.wired.shell.read_with(cx, |s, _| s.rows_from()).unwrap();
    assert_ne!(a, b, "the rows are B's");
    assert_eq!(rig.jobs(photo), jobs + 1, "B's row asks for B's look");
    assert_eq!(rig.tile(photo, cx), Err("loading"));
    rig.finish(photo, 8, cx);
    assert_eq!(rig.tile(photo, cx), Ok(8), "B's look lands");
}

/// Only a look's own submissions are bound to its catalog (review rv134 M1): a view asking
/// without a look — the inspector's stack, a card, the collage — for a photo whose tier has a
/// look from a catalog no longer open is not refused. A look's own refused ask is not asked
/// again until rows come from another catalog — even rows that do not name the photo. A
/// store of its own, with the core's identity probe, so no grid joins in.
#[gpui_kit::test]
fn a_plain_request_is_never_bound_to_a_look(cx: &mut TestAppContext) {
    let dir = TempDir::new("grid-cover-plain");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 1, cx);
    let photo = ids[0];
    let a = app.wired.shell.read_with(cx, |s, _| s.rows_from()).unwrap();
    let pool = Arc::new(FakePool::default());
    let state = app.state.clone();
    let images = cx.update(|cx| {
        let submit: Arc<dyn crate::image_store::Submit> = pool.clone();
        cx.new(|cx| {
            let mut store = crate::image_store::ImageStore::new(submit, crate::image_store::DEFAULT_BUDGET_BYTES, cx);
            store.set_identity_probe(Arc::new(move || chairphoto_core::app::catalog_identity(&state).ok()));
            store
        })
    });
    let key = JobKey::photo(photo, ImageKind::Thumb);
    let ready = |cx: &mut TestAppContext| images.read_with(cx, |s, _| matches!(s.peek(photo, ImageKind::Thumb), ImageState::Ready(_)));
    images.update(cx, |s, cx| s.request_look_batch(a, &[(photo, None)], cx));
    pool.finish(&key, Ok(pixels(4, 4)));
    cx.run_until_parked();
    assert!(ready(cx));

    chairphoto_core::app::catalogs::reroot_open_catalog_as(&app.state, a, dir.0.join("newroot")).unwrap();
    images.update(cx, |s, cx| s.invalidate(photo, cx));
    images.update(cx, |s, _| s.request(photo, ImageKind::Thumb));
    let refused = images.read_with(cx, |s, _| s.stats().refused);
    pool.finish(&key, Ok(pixels(4, 4)));
    cx.run_until_parked();
    assert_eq!(images.read_with(cx, |s, _| s.stats().refused), refused, "not checked against the look's catalog");
    assert!(ready(cx), "the plain request lands");
    // The look's own ask, still from the earlier rows, is checked: refused.
    images.update(cx, |s, cx| s.invalidate(photo, cx));
    images.update(cx, |s, cx| s.request_look_batch(a, &[(photo, None)], cx));
    pool.finish(&key, Ok(pixels(4, 4)));
    cx.run_until_parked();
    assert_eq!(images.read_with(cx, |s, _| s.stats().refused), refused + 1);
    assert!(!ready(cx));
    let jobs = pool.submitted();
    images.update(cx, |s, _| s.request(photo, ImageKind::Thumb));
    assert_eq!(pool.submitted(), jobs, "refused: not asked again while its look is the old rows'");

    // Rows read from the reopened catalog that do not name the photo (it scrolled away):
    // the earlier rows' refusal goes with their looks, and a plain request asks again.
    let b = chairphoto_core::app::catalog_identity(&app.state).unwrap();
    assert_ne!(a, b);
    images.update(cx, |s, cx| s.request_look_batch(b, &[], cx));
    images.update(cx, |s, _| s.request(photo, ImageKind::Thumb));
    assert_eq!(pool.submitted(), jobs + 1, "the refusal did not stick");
    pool.finish(&key, Ok(pixels(4, 4)));
    cx.run_until_parked();
    assert!(ready(cx));
}

#[gpui_kit::test]
fn a_grid_tile_never_shows_the_new_catalog_before_the_switch_event(cx: &mut TestAppContext) {
    grid_cover_across_a_switch(false, cx);
}

#[gpui_kit::test]
fn a_grid_tile_never_shows_the_old_catalog_after_the_switch_event(cx: &mut TestAppContext) {
    grid_cover_across_a_switch(true, cx);
}

/// A re-root reopens the catalog under a new identity and sends no `catalog:switched`. A
/// tile refused across it is asked again once the rows are re-read from the reopened
/// catalog: the refusal does not stick (review rv134 M1).
#[gpui_kit::test]
fn a_tile_refused_across_a_re_root_is_asked_again_for_the_new_rows(cx: &mut TestAppContext) {
    let rig = LookRig::new("grid-cover-reroot", 2, cx);
    let photo = rig.ids[1];
    let a = rig.app.wired.shell.read_with(cx, |s, _| s.rows_from()).unwrap();
    rig.pool.start(JobKey::photo(photo, ImageKind::Thumb));
    chairphoto_core::app::catalogs::reroot_open_catalog_as(&rig.app.state, a, rig.dir.0.join("newroot")).unwrap();
    let jobs = rig.jobs(photo);

    rig.finish(photo, 4, cx);
    assert_eq!(rig.tile(photo, cx), Err("absent"), "refused: empty, not failed");
    render(&rig.app, cx);
    assert_eq!(rig.jobs(photo), jobs, "not asked again under the old rows");

    rig.refresh_rows(cx);
    assert_ne!(rig.app.wired.shell.read_with(cx, |s, _| s.rows_from()), Some(a), "rows from the reopened catalog");
    assert_eq!(rig.jobs(photo), jobs + 1, "asked again for the new rows");
    rig.finish(photo, 8, cx);
    assert_eq!(rig.tile(photo, cx), Ok(8), "it lands");
}

/// The video badge's tooltip says what a double-click does here — open the loupe, whose button
/// plays the video — not React's "double-click to play" (#161).
#[test]
fn the_video_tooltip_names_the_loupes_play_button() {
    let tip = crate::library::grid::video_tip();
    assert_eq!(tip, "Video — double-click to open, then Play in system player");
    assert!(crate::loupe::view::PLAY_LABEL.ends_with("Play in system player"), "the loupe's button is the one named");
}

/// rv151 L5. A cover changes while the grid is not drawn (the loupe is on the stage here;
/// in the app also the People view, the collage, the map…). When the rows land, the tile's
/// thumbnail is invalidated for the row's new look — not when the grid next draws — so a
/// view that asks without the token gets the new look.
#[gpui_kit::test]
fn a_cover_change_reaches_the_thumbnail_when_the_rows_land(cx: &mut TestAppContext) {
    let rig = LookRig::new("grid-cover-rows-land", 3, cx);
    for &id in &rig.ids {
        rig.finish(id, 4, cx);
    }
    let photo = rig.ids[2];
    let images = rig.app.wired.images.clone();
    assert_eq!(images.read_with(cx, |s, _| s.look(photo)).map(|l| l.cover), Some(None), "the grid's look");
    // Off the grid: the loupe on another photo.
    let other = rig.ids[0];
    rig.app.wired.shell.update(cx, |s, cx| {
        s.select_with(cx, |l| l.select_single(other));
        s.toggle_loupe(cx);
    });
    cx.run_until_parked();
    render(&rig.app, cx);

    let version = rig.cover(photo);
    rig.app.wired.shell.update(cx, |s, cx| s.refresh_rows(cx));
    cx.run_until_parked();
    assert_eq!(
        images.read_with(cx, |s, _| s.look(photo)).and_then(|l| l.cover).map(|c| c.version),
        Some(version),
        "the row's new look"
    );
    assert_eq!(rig.tile(photo, cx), Err("absent"), "the earlier look's thumbnail is gone");
    assert_eq!(rig.tile(other, cx), Ok(4), "no other tile touched");
    // A plain view (a card, the collage) asks: the new look is rendered.
    images.update(cx, |s, _| s.request(photo, ImageKind::Thumb));
    rig.finish(photo, 8, cx);
    assert_eq!(rig.tile(photo, cx), Ok(8));
}

/// rv151 L3. A refusal that arrives after rows from another catalog were read, for a photo
/// those rows do not name, is not kept: the look it was refused for is gone, so a plain view
/// asks again (else the tile would stay empty for every view until the grid reached it).
#[gpui_kit::test]
fn a_late_refusal_for_a_superseded_look_is_not_kept(cx: &mut TestAppContext) {
    let dir = TempDir::new("grid-late-refusal");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 1, cx);
    let photo = ids[0];
    let a = app.wired.shell.read_with(cx, |s, _| s.rows_from()).unwrap();
    let pool = Arc::new(FakePool::default());
    let state = app.state.clone();
    let images = cx.update(|cx| {
        let submit: Arc<dyn crate::image_store::Submit> = pool.clone();
        cx.new(|cx| {
            let mut store = crate::image_store::ImageStore::new(submit, crate::image_store::DEFAULT_BUDGET_BYTES, cx);
            store.set_identity_probe(Arc::new(move || chairphoto_core::app::catalog_identity(&state).ok()));
            store
        })
    });
    let key = JobKey::photo(photo, ImageKind::Thumb);
    images.update(cx, |s, cx| s.request_look_batch(a, &[(photo, None)], cx));
    pool.start(key.clone()); // on a worker
    chairphoto_core::app::catalogs::reroot_open_catalog_as(&app.state, a, dir.0.join("newroot")).unwrap();
    let b = chairphoto_core::app::catalog_identity(&app.state).unwrap();
    // Rows from the reopened catalog that do not name the photo (it scrolled away).
    images.update(cx, |s, cx| s.request_look_batch(b, &[], cx));
    pool.finish(&key, Ok(pixels(4, 4)));
    cx.run_until_parked();
    assert_eq!(images.read_with(cx, |s, _| s.stats().refused), 1, "refused on the worker");
    let jobs = pool.submitted();
    images.update(cx, |s, _| s.request(photo, ImageKind::Thumb));
    assert_eq!(pool.submitted(), jobs + 1, "a plain view asks again: the refusal was not kept");
}

/// rv151 L1. In a switch's window (the core on B, `catalog:switched` withheld; B's photo has
/// the tile's id and cover token), the tile was evicted and a plain view (the inspector's
/// stack, a card) asks for it first: it renders B's photo, unrefused (#134 M1). The grid,
/// still on A's rows, does not draw those pixels: they were rendered in another catalog, so
/// it asks again, bound to A — refused — and the tile stays empty.
#[gpui_kit::test]
fn a_plain_render_in_a_switch_window_is_not_drawn_under_the_old_row(cx: &mut TestAppContext) {
    let rig = LookRig::new("grid-plain-switch", 2, cx);
    let photo = rig.ids[1];
    let version = rig.cover(photo);
    rig.refresh_rows(cx);
    for &id in &rig.ids {
        rig.finish(id, 4, cx);
    }
    assert_eq!(rig.tile(photo, cx), Ok(4));
    let a = rig.app.wired.shell.read_with(cx, |s, _| s.rows_from()).unwrap();
    let (b, b_ids) = colliding_catalog(&rig.dir, "b", 2);
    let bv = b.create_version(b_ids[1], "B's").unwrap();
    assert_eq!(b.set_cover_version(b_ids[1], Some(bv)).unwrap(), Some(format!("{version}:0")), "the token collides");
    core_switch(&rig.app, b);

    // The eviction and the plain request in one update: the plain view asks first.
    let images = rig.app.wired.images.clone();
    images.update(cx, |s, cx| {
        s.evict(|k| k.photo == photo, cx);
        s.request(photo, ImageKind::Thumb);
    });
    let refused = rig.stats(cx).refused;
    rig.finish(photo, 16, cx);
    assert_eq!(rig.stats(cx).refused, refused, "a plain request is not refused");
    render(&rig.app, cx);
    assert_eq!(rig.app.wired.shell.read_with(cx, |s, _| s.rows_from()), Some(a), "the grid's rows are still A's");
    assert_ne!(rig.tile(photo, cx), Ok(16), "B's render is not drawn under A's row");
    let jobs = rig.jobs(photo);
    rig.finish(photo, 8, cx);
    assert_eq!(rig.stats(cx).refused, refused + 1, "the grid's own ask, bound to A, is refused");
    assert_eq!(rig.tile(photo, cx), Err("absent"));
    render(&rig.app, cx);
    assert_eq!(rig.jobs(photo), jobs, "and not asked again under A's row");
}

/// #187's audit, on rv151 L1's setup: the frame in which the grid finds a tile's cached
/// pixels foreign (rendered in another catalog) has already built the tile with them; the
/// grid drops them from its list's prepaint, and the redraw that takes them off screen must
/// still happen — GPUI drops one asked for mid-draw. No frame is forced: only what GPUI
/// redraws by itself, as in the running app.
#[gpui_kit::test]
fn a_tile_dropped_while_the_grid_draws_leaves_the_screen(cx: &mut TestAppContext) {
    let rig = LookRig::new("grid-drop-redraw", 2, cx);
    let photo = rig.ids[1];
    let version = rig.cover(photo);
    rig.refresh_rows(cx);
    for &id in &rig.ids {
        rig.finish(id, 4, cx);
    }
    let (b, b_ids) = colliding_catalog(&rig.dir, "b", 2);
    let bv = b.create_version(b_ids[1], "B's").unwrap();
    assert_eq!(b.set_cover_version(b_ids[1], Some(bv)).unwrap(), Some(format!("{version}:0")), "the token collides");
    core_switch(&rig.app, b);
    let images = rig.app.wired.images.clone();
    images.update(cx, |s, cx| {
        s.evict(|k| k.photo == photo, cx);
        s.request(photo, ImageKind::Thumb);
    });
    // B's render lands; GPUI redraws the grid, which finds it foreign and drops it.
    rig.finish(photo, 16, cx);
    assert_ne!(rig.tile(photo, cx), Ok(16), "dropped from the store");
    let drawn = cx
        .update_window(rig.app.window(), |_, window, _| window.try_find(("tile-picture", photo as u64)).is_some())
        .unwrap();
    assert!(!drawn, "B's pixels are still on screen under A's row");
}

/// rv151 L2. A tile a plain view asked for first — after a switch emptied the store, before
/// the grid's frame — is the grid's too when its row names no cover: the grid adopts the
/// render in flight (it is not thrown away and sent again) and keeps it once it lands.
#[gpui_kit::test]
fn a_plain_render_of_a_row_without_a_cover_is_not_rendered_twice(cx: &mut TestAppContext) {
    let rig = LookRig::new("grid-plain-adopt", 1, cx);
    let photo = rig.ids[0];
    rig.finish(photo, 4, cx);
    let images = rig.app.wired.images.clone();
    // The store emptied (as a switch does), and a plain view asks before the grid's frame.
    images.update(cx, |s, cx| {
        s.clear(cx);
        s.request(photo, ImageKind::Thumb);
    });
    let jobs = rig.jobs(photo);
    let dropped = rig.stats(cx).stale_dropped;
    render(&rig.app, cx);
    assert_eq!(rig.jobs(photo), jobs, "the grid adopts the plain render in flight");
    rig.finish(photo, 6, cx);
    assert_eq!(rig.stats(cx).stale_dropped, dropped, "nothing thrown away");
    render(&rig.app, cx);
    assert_eq!(rig.jobs(photo), jobs, "and keeps it once it lands");
    assert_eq!(rig.tile(photo, cx), Ok(6));

    // Cached by a plain view: kept as well.
    images.update(cx, |s, cx| {
        s.clear(cx);
        s.request(photo, ImageKind::Thumb);
    });
    rig.finish(photo, 7, cx);
    let jobs = rig.jobs(photo);
    render(&rig.app, cx);
    assert_eq!(rig.jobs(photo), jobs, "a cached plain thumbnail of a row with no cover is current");
    assert_eq!(rig.tile(photo, cx), Ok(7));
}

/// rv151 L2, which cached thumbnails a look takes over from a plain view: the plain
/// thumbnail rendered in the row's catalog, for a row with no cover. Not a cover render, not
/// one for a row that names a cover, not one rendered in another catalog. A store of its own
/// (no grid), with the core's identity probe.
#[gpui_kit::test]
fn a_look_takes_over_only_the_plain_thumbnail_of_its_own_catalog(cx: &mut TestAppContext) {
    let dir = TempDir::new("grid-plain-takeover");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 1, cx);
    let photo = ids[0];
    let a = app.wired.shell.read_with(cx, |s, _| s.rows_from()).unwrap();
    let pool = Arc::new(FakePool::default());
    let state = app.state.clone();
    let images = cx.update(|cx| {
        let submit: Arc<dyn crate::image_store::Submit> = pool.clone();
        cx.new(|cx| {
            let mut store = crate::image_store::ImageStore::new(submit, crate::image_store::DEFAULT_BUDGET_BYTES, cx);
            store.set_identity_probe(Arc::new(move || chairphoto_core::app::catalog_identity(&state).ok()));
            store
        })
    });
    let key = JobKey::photo(photo, ImageKind::Thumb);
    // A plain view's render (landing as `loaded`) and then a look for `cover`: whether the
    // look asked for the thumbnail again.
    let asks_again = |loaded: crate::image_store::Loaded, cover: Option<CoverLook>, cx: &mut TestAppContext| {
        images.update(cx, |s, cx| s.clear(cx));
        images.update(cx, |s, _| s.request(photo, ImageKind::Thumb));
        pool.finish(&key, Ok(loaded));
        cx.run_until_parked();
        let before = pool.submitted();
        images.update(cx, |s, cx| s.request_look_batch(a, &[(photo, cover)], cx));
        let asked = pool.submitted() > before;
        // Answer what the look sent, so nothing is left in flight for the next case.
        pool.finish(&key, Ok(pixels(1, 1)));
        cx.run_until_parked();
        asked
    };
    let covered = Some(CoverLook { version: 1, rev: 0 });
    assert!(!asks_again(pixels(4, 4), None, cx), "the plain thumbnail of a row with no cover");
    assert!(asks_again(crate::image_store::Loaded { cover: true, ..pixels(4, 4) }, None, cx), "a cover render");
    assert!(asks_again(pixels(4, 4), covered, cx), "a row that names a cover");
    // Rendered once another catalog was open: not this row's.
    let (b, _) = colliding_catalog(&dir, "b", 1);
    core_switch(&app, b);
    assert!(asks_again(pixels(4, 4), None, cx), "rendered in another catalog");
}
