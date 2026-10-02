//! Headless tests of a module's card in the pop-out loupe (#110), through the real wiring.

use crate::image_tests::FakePool;
use crate::loupe::card::{CardScope, LoupeCard, PAGE};
use crate::loupe::window;
use crate::modules::dev_module::DEV_MODULE_ID;
use crate::modules::ModuleRegistry;
use crate::tests::{
    colliding_catalog, core_switch, deliver_switch, open_catalog_with_photos, start_with_pool, App, TempDir,
};
use chairphoto_core::image_pool::{ImageKind, JobKey};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, SharedString, TestAppContext};
use std::sync::Arc;

fn app_with(n: usize, tag: &str, cx: &mut TestAppContext) -> (App, Arc<FakePool>, TempDir, Vec<i64>) {
    let dir = TempDir::new(tag);
    let pool = Arc::new(FakePool::default());
    let app = start_with_pool(cx, pool.clone());
    let ids = open_catalog_with_photos(&app, &dir, n, cx);
    (app, pool, dir, ids)
}

fn open(cx: &mut TestAppContext) -> AnyWindowHandle {
    cx.update(window::open);
    cx.run_until_parked();
    cx.update(|cx| window::handle(cx)).expect("the pop-out opened")
}

/// Tag `ids` with a new tag in the open catalog; returns the tag's id.
fn tag_all(app: &App, ids: &[i64]) -> i64 {
    let guard = app.state.catalog.lock().unwrap();
    let c = guard.as_ref().unwrap();
    let tag = c.create_tag("Animals/Bird").unwrap();
    for &id in ids {
        c.assign_tag(id, tag).unwrap();
    }
    tag
}

fn card(title: &str, tag: i64) -> LoupeCard {
    LoupeCard {
        title: title.to_string().into(),
        subtitle: Some("Animals/Bird".into()),
        color: None,
        chips: vec!["Tag".into()],
        stats: vec![("Photos".into(), "60".into())],
        related: Vec::new(),
        photos: Some(CardScope::Tag(tag)),
    }
}

/// Module `module` puts `card` up (or takes its own down), under the catalog the UI has read.
fn show(app: &App, module: &str, card: Option<LoupeCard>, cx: &mut TestAppContext) {
    let from = app.wired.model.read_with(cx, |m, _| m.catalog_identity());
    app.wired.shell.update(cx, |s, cx| s.show_loupe_card(module.to_string().into(), card, from, cx));
    cx.run_until_parked();
}

fn present_in(h: AnyWindowHandle, id: impl Into<SharedString>, cx: &mut TestAppContext) -> bool {
    let id = id.into();
    let mut out = false;
    cx.update_window(h, |_, window, cx| {
        window.render_frame(cx);
        out = window.try_find(id).is_some();
    })
    .unwrap();
    out
}

fn label_in(h: AnyWindowHandle, id: &'static str, cx: &mut TestAppContext) -> Option<String> {
    let mut out = None;
    cx.update_window(h, |_, window, cx| {
        window.render_frame(cx);
        out = window.try_find(id).and_then(|e| e.label().map(str::to_string));
    })
    .unwrap();
    out
}

fn click_in(h: AnyWindowHandle, id: impl Into<SharedString>, cx: &mut TestAppContext) {
    let id = id.into();
    cx.update_window(h, |_, window, cx| {
        window.render_frame(cx);
        window.click(id, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn press_in(h: AnyWindowHandle, key: &str, cx: &mut TestAppContext) {
    cx.update_window(h, |_, window, cx| {
        window.render_frame(cx);
        window.press(key, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn wall(cx: &mut TestAppContext) -> (Vec<i64>, usize) {
    let view = cx.update(|cx| window::view(cx)).expect("open");
    view.read_with(cx, |v, cx| v.card().read(cx).wall())
}

fn pending(app: &App, id: i64, cx: &mut TestAppContext) -> bool {
    app.wired.images.read_with(cx, |s, _| s.is_pending(id, ImageKind::Preview))
}

/// A card takes over the pop-out while its module is enabled: its wall pages the scope 48 at
/// a time, a tile opens the photo full-size and Esc returns to the wall; only its owner takes
/// it down, and disabling the owner does too.
#[gpui_kit::test]
fn a_module_card_takes_over_the_pop_out_until_its_owner_takes_it_down(cx: &mut TestAppContext) {
    let (app, pool, _dir, ids) = app_with(60, "pop-card", cx);
    let tag = tag_all(&app, &ids);
    cx.update(|cx| ModuleRegistry::enable(&app.wired.modules, DEV_MODULE_ID, cx));
    // No selection: the pop-out's loupe (behind the card) claims nothing, so what the card's
    // photo gives back is visible here.
    let h = open(cx);
    show(&app, DEV_MODULE_ID, Some(card("Bird", tag)), cx);
    assert_eq!(label_in(h, "loupe-card-title", cx).as_deref(), Some("Bird"));
    assert!(!present_in(h, "loupe", cx), "the card replaces the photo");
    let (shown, total) = wall(cx);
    assert_eq!((shown.len(), total), (PAGE, 60));
    assert_eq!(label_in(h, "loupe-card-wall-head", cx).as_deref(), Some("60 photos"));
    // "Show more" is below the fold of the test window: drive its handler.
    assert!(present_in(h, "loupe-card-more", cx));
    let view = cx.update(|cx| window::view(cx)).unwrap();
    view.update(cx, |v, cx| v.card().update(cx, |c, cx| c.load_more(cx)));
    cx.run_until_parked();
    assert_eq!(wall(cx).0.len(), 60, "Show more loads the rest");
    assert!(!present_in(h, "loupe-card-more", cx), "nothing left");

    // A tile: the photo full-size; Esc back to the wall, giving its preview back.
    let first = wall(cx).0[0];
    click_in(h, format!("loupe-card-tile-{first}"), cx);
    assert!(present_in(h, "loupe-card-back", cx));
    assert!(pending(&app, first, cx), "its preview is asked for");
    press_in(h, "escape", cx);
    assert!(present_in(h, "loupe-card-wall-head", cx), "back to the wall");
    let preview = JobKey::photo(first, ImageKind::Preview);
    assert!(pool.cancelled.lock().unwrap().contains(&preview), "the full-size photo's preview is given back");

    // Another module's `None` leaves it; its owner's takes it down.
    show(&app, "someone-else", None, cx);
    assert!(present_in(h, "loupe-card-title", cx));
    show(&app, DEV_MODULE_ID, None, cx);
    assert!(present_in(h, "loupe", cx), "the loupe again");

    // Disabling the owner takes it down.
    show(&app, DEV_MODULE_ID, Some(card("Bird", tag)), cx);
    assert!(present_in(h, "loupe-card-title", cx));
    cx.update(|cx| ModuleRegistry::disable(&app.wired.modules, DEV_MODULE_ID, cx));
    cx.run_until_parked();
    assert!(app.wired.shell.read_with(cx, |s, _| s.loupe_card().is_none()));
    assert!(present_in(h, "loupe", cx));
}

/// The wall reads the catalog the card was shown under: a card shown while another catalog is
/// already open (before `catalog:switched` arrives) fails closed instead of listing the new
/// catalog's photos under a colliding tag id; the switch then takes the card down.
#[gpui_kit::test]
fn a_cards_wall_is_bound_to_its_catalog_and_a_switch_takes_it_down(cx: &mut TestAppContext) {
    let (app, _pool, dir, ids) = app_with(3, "pop-card-switch", cx);
    let tag = tag_all(&app, &ids);
    cx.update(|cx| ModuleRegistry::enable(&app.wired.modules, DEV_MODULE_ID, cx));
    let h = open(cx);
    show(&app, DEV_MODULE_ID, Some(card("Bird", tag)), cx);
    assert_eq!(wall(cx), (ids.clone(), 3));

    // The new catalog's second tag has the id a card can name, on one of its photos.
    let (other, new_ids) = colliding_catalog(&dir, "other", 3);
    let first = other.create_tag("Other/A").unwrap();
    let second = other.create_tag("Other/B").unwrap();
    assert_eq!(first, tag, "ids collide");
    other.assign_tag(new_ids[2], second).unwrap();
    core_switch(&app, other);
    show(&app, DEV_MODULE_ID, Some(card("B", second)), cx);
    assert_eq!(wall(cx), (Vec::new(), 0), "the read bound to the old catalog failed closed");

    deliver_switch(&app, cx);
    assert!(app.wired.shell.read_with(cx, |s, _| s.loupe_card().is_none()));
    assert!(present_in(h, "loupe", cx));
}

/// A card photo shown full-size takes key focus; once its owner takes the card down, the
/// pop-out's loupe has the keys again — the arrows step the selection there.
#[gpui_kit::test]
fn the_loupe_has_the_keys_again_after_a_card_comes_down(cx: &mut TestAppContext) {
    let (app, _pool, _dir, ids) = app_with(4, "pop-card-focus", cx);
    let tag = tag_all(&app, &ids);
    cx.update(|cx| ModuleRegistry::enable(&app.wired.modules, DEV_MODULE_ID, cx));
    app.wired.shell.update(cx, |s, cx| s.select_with(cx, |l| l.select_single(ids[0])));
    cx.run_until_parked();
    let h = open(cx);
    show(&app, DEV_MODULE_ID, Some(card("Bird", tag)), cx);
    click_in(h, format!("loupe-card-tile-{}", ids[2]), cx);
    assert!(present_in(h, "loupe-card-back", cx), "the card's photo is up and has focus");

    show(&app, DEV_MODULE_ID, None, cx);
    assert!(present_in(h, "loupe", cx), "the loupe again");
    press_in(h, "right", cx);
    let active = app.wired.shell.read_with(cx, |s, _| s.library.selection().active_id);
    assert_eq!(active, Some(ids[1]), "the arrow reached the loupe");
}

/// Closing the pop-out on a full-size card photo gives its tiers back.
#[gpui_kit::test]
fn closing_the_pop_out_releases_the_cards_photo(cx: &mut TestAppContext) {
    let (app, pool, _dir, ids) = app_with(3, "pop-card-close", cx);
    let tag = tag_all(&app, &ids);
    cx.update(|cx| ModuleRegistry::enable(&app.wired.modules, DEV_MODULE_ID, cx));
    let h = open(cx);
    show(&app, DEV_MODULE_ID, Some(card("Bird", tag)), cx);
    click_in(h, format!("loupe-card-tile-{}", ids[1]), cx);
    assert!(pending(&app, ids[1], cx));
    cx.update(window::close);
    cx.run_until_parked();
    assert!(pool.cancelled.lock().unwrap().contains(&JobKey::photo(ids[1], ImageKind::Preview)));
}
