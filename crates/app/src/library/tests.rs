//! Headless tests of the Library view through the real wiring (`start` → `wire` → the main
//! window): the grid's rows, clicks and keys, the culling write path, row generations, deep
//! links, thumbnails per window, and the "Stack bursts" dialog.

use crate::library::grid::LibraryView;
use crate::model::not_yet_ported_line;
use crate::shell::state::Mark;
use crate::tests::{click, open_catalog_with_photos, press, start, status, App, TempDir};
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

/// Double-click selects and asks for the loupe, which is #109's.
#[gpui_kit::test]
fn a_double_click_opens_the_loupe(cx: &mut TestAppContext) {
    let dir = TempDir::new("grid-open");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 3, cx);
    cx.update_window(app.window(), |_, window, cx| window.double_click(tile(ids[2]), cx)).unwrap();
    cx.run_until_parked();
    assert_eq!(selection(&app, cx).0, Some(ids[2]));
    assert_eq!(status(&app, cx), not_yet_ported_line("Loupe", 109));
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
        s.on_page(&stale, Ok(PhotoPage { photos: all_rows, offset: 0, total: ids.len() }), cx)
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
