//! Headless tests of the Instagram module through its real contribution, over a fake
//! [`InstagramDriver`] (no browser is ever launched): the caption prefill, the three outcomes,
//! the "awaiting review" confirmation (recorded only on "Yes", for what was composed, in the
//! catalog it came from), Cancel before Chrome, and the catalog-identity rule.

use super::panel::{InstagramPanel, Stage, AWAITING_REVIEW, NEEDS_LOGIN, NOT_RECORDED, POSTED};
use super::InstagramModule;
use crate::modules::publishing::tests::{present, publications, select, step, with_files, work};
use crate::modules::{Module, ModuleHost, RestoredCatalog};
use crate::tests::{colliding_catalog, core_switch, deliver_switch, start, App, TempDir};
use chairphoto_core::app::instagram::{InstagramDriver, PostOutcome};
use chairphoto_core::app::uploads::UPLOAD_CANCELLED;
use chairphoto_core::app::CATALOG_CHANGED;
use gpui_kit::component::WindowExt as _;
use gpui_kit::prelude::*;
use gpui_kit::{AnyView, Entity, TestAppContext};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A Chrome that answers `outcome` and remembers what it was handed (and whether the render
/// was on disk then).
struct FakeChrome {
    outcome: Mutex<PostOutcome>,
    /// (render path, caption, publish, render existed)
    posts: Mutex<Vec<(PathBuf, String, bool, bool)>>,
}

impl FakeChrome {
    fn answering(outcome: PostOutcome) -> Arc<Self> {
        Arc::new(FakeChrome { outcome: Mutex::new(outcome), posts: Mutex::default() })
    }
}

impl InstagramDriver for FakeChrome {
    fn post(&self, image: &Path, caption: &str, publish: bool) -> Result<PostOutcome, String> {
        self.posts.lock().unwrap().push((image.to_path_buf(), caption.into(), publish, image.exists()));
        Ok(*self.outcome.lock().unwrap())
    }
}

fn panel(app: &App, driver: Arc<FakeChrome>, cx: &mut TestAppContext) -> Entity<InstagramPanel> {
    let module = InstagramModule { driver };
    let restored = RestoredCatalog::default();
    restored.set(Some(chairphoto_core::app::catalog_identity(&app.state).unwrap()));
    let (model, shell) = (app.wired.model.clone(), app.wired.shell.clone());
    let host = cx.update(|_| ModuleHost::new(module.meta(), app.state.clone(), restored, model, shell));
    assert_eq!(host.meta().marker(), "instagram");
    let instance = cx.update(|cx| module.load(host, cx)).ok().unwrap();
    let c = instance.contributions();
    assert_eq!(c.publish_targets[0].label.as_ref(), "Instagram");
    let factory = c.publish_targets[0].view.clone();
    let view: AnyView = cx
        .update_window(app.window(), |_, window, cx| {
            let view = factory(window, cx);
            let shown = view.clone();
            window.open_dialog(cx, move |d, _, _| d.child(shown.clone()));
            view
        })
        .unwrap();
    cx.run_until_parked();
    work(cx);
    view.downcast::<InstagramPanel>().ok().unwrap()
}

/// A photo with a title and a version, selected.
fn setup(dir: &TempDir, cx: &mut TestAppContext) -> (App, Vec<i64>, i64) {
    let app = start(cx);
    let ids = with_files(&app, dir, 1, cx);
    work(cx);
    let version = {
        let guard = app.state.catalog.lock().unwrap();
        let c = guard.as_ref().unwrap();
        let fields = chairphoto_core::catalog::IptcFields { title: "Aurora over the fjord".into(), ..Default::default() };
        c.set_iptc(ids[0], &fields).unwrap();
        c.create_version(ids[0], "Punchy").unwrap()
    };
    select(&app, ids[0], cx);
    (app, ids, version)
}

fn caption(app: &App, view: &Entity<InstagramPanel>, cx: &mut TestAppContext) -> String {
    cx.update_window(app.window(), |_, _, cx| view.read(cx).caption.read(cx).value().to_string()).unwrap()
}

/// Supervised (the default): the post is composed with the render still on disk, the panel
/// asks whether Share was clicked, and nothing is recorded until "Yes" — which records the
/// photo and the version that were composed, even if the picker moved since.
#[gpui_kit::test]
fn a_supervised_post_is_recorded_only_when_the_user_confirms(cx: &mut TestAppContext) {
    let dir = TempDir::new("instagram-review");
    let (app, ids, version) = setup(&dir, cx);
    let chrome = FakeChrome::answering(PostOutcome::AwaitingReview);
    let view = panel(&app, chrome.clone(), cx);
    assert_eq!(caption(&app, &view, cx), "Aurora over the fjord", "the caption is prefilled");
    view.update(cx, |p, cx| {
        assert!(!p.auto_publish, "supervised by default");
        p.versions.chosen = Some(version);
        p.post(cx);
    });
    work(cx);
    let (path, text, publish, existed) = chrome.posts.lock().unwrap()[0].clone();
    assert_eq!((text.as_str(), publish, existed), ("Aurora over the fjord", false, true));
    assert!(path.exists(), "the composer's render was deleted while it awaits Share");
    view.read_with(cx, |p, _| {
        assert_eq!(p.status, AWAITING_REVIEW);
        assert!(p.pending.is_some() && !p.busy);
    });
    assert!(present(&app, "instagram-review", cx));
    assert!(publications(&app, ids[0]).is_empty(), "recorded before the user confirmed");
    view.update(cx, |p, cx| {
        p.versions.chosen = None; // the picker moves; the confirmation records what was posted
        p.post(cx); // refused while the review is open
    });
    work(cx);
    assert_eq!(chrome.posts.lock().unwrap().len(), 1, "a second post while awaiting review");
    view.update(cx, |p, cx| p.confirm_posted(cx));
    work(cx);
    view.read_with(cx, |p, _| assert_eq!((p.status.as_str(), p.pending), (POSTED, None)));
    let pubs = publications(&app, ids[0]);
    assert_eq!((pubs.len(), pubs[0].platform.as_str(), pubs[0].version_id), (1, "instagram", Some(version)));
    assert_eq!(crate::tests::status(&app, cx), "Posted to Instagram.");
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap(); // the sweep's job in production
}

/// "No, skip" records nothing.
#[gpui_kit::test]
fn no_skip_records_nothing(cx: &mut TestAppContext) {
    let dir = TempDir::new("instagram-skip");
    let (app, ids, _) = setup(&dir, cx);
    let chrome = FakeChrome::answering(PostOutcome::AwaitingReview);
    let view = panel(&app, chrome.clone(), cx);
    view.update(cx, |p, cx| p.post(cx));
    work(cx);
    view.update(cx, |p, cx| p.dismiss_review(cx));
    view.read_with(cx, |p, _| assert_eq!((p.status.as_str(), p.pending), (NOT_RECORDED, None)));
    assert!(!present(&app, "instagram-review", cx));
    assert!(publications(&app, ids[0]).is_empty());
    let path = chrome.posts.lock().unwrap()[0].0.clone();
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

/// "Publish automatically" clicks Share: a confirmed post is recorded at once and its render
/// removed; "needs login" records nothing and says what to do. An edited caption is kept.
#[gpui_kit::test]
fn a_confirmed_post_records_at_once_and_needs_login_records_nothing(cx: &mut TestAppContext) {
    let dir = TempDir::new("instagram-posted");
    let (app, ids, _) = setup(&dir, cx);
    let chrome = FakeChrome::answering(PostOutcome::NeedsLogin);
    let view = panel(&app, chrome.clone(), cx);
    cx.update_window(app.window(), |_, window, cx| view.update(cx, |p, cx| p.caption.update(cx, |t, cx| t.set_value("Mine #only", window, cx)))).unwrap();
    view.update(cx, |p, cx| p.post(cx));
    work(cx);
    view.read_with(cx, |p, _| assert_eq!(p.status, NEEDS_LOGIN));
    assert!(publications(&app, ids[0]).is_empty());
    assert!(!chrome.posts.lock().unwrap()[0].0.exists(), "a needs-login render was kept");

    *chrome.outcome.lock().unwrap() = PostOutcome::Posted;
    view.update(cx, |p, cx| {
        p.auto_publish = true;
        p.post(cx);
    });
    work(cx);
    let (path, text, publish, _) = chrome.posts.lock().unwrap()[1].clone();
    assert_eq!((text.as_str(), publish), ("Mine #only", true));
    assert!(!path.exists(), "a posted render was kept");
    view.read_with(cx, |p, _| assert_eq!(p.status, POSTED));
    let pubs = publications(&app, ids[0]);
    assert_eq!((pubs.len(), pubs[0].platform.as_str(), pubs[0].version_id), (1, "instagram", None));
}

/// Cancel while rendering: Chrome is never handed anything. No Cancel once it has the render.
#[gpui_kit::test]
fn cancel_stops_a_post_before_chrome(cx: &mut TestAppContext) {
    let dir = TempDir::new("instagram-cancel");
    let (app, ids, _) = setup(&dir, cx);
    let chrome = FakeChrome::answering(PostOutcome::Posted);
    let view = panel(&app, chrome.clone(), cx);
    view.update(cx, |p, cx| p.post(cx));
    step(cx);
    view.read_with(cx, |p, _| assert_eq!(p.stage, Some(Stage::Rendering)));
    assert!(present(&app, "instagram-cancel", cx));
    view.update(cx, |p, cx| p.cancel(cx));
    work(cx);
    view.read_with(cx, |p, _| assert_eq!((p.status.as_str(), p.busy), (UPLOAD_CANCELLED, false)));
    assert!(chrome.posts.lock().unwrap().is_empty(), "a cancelled post reached Chrome");
    assert!(publications(&app, ids[0]).is_empty());

    view.update(cx, |p, cx| p.post(cx));
    step(cx);
    step(cx);
    view.read_with(cx, |p, _| assert_eq!(p.stage, Some(Stage::Posting)));
    assert!(!present(&app, "instagram-cancel", cx), "Cancel offered once Chrome has the render");
    work(cx);
    assert_eq!(chrome.posts.lock().unwrap().len(), 1);
}

/// A switch to a catalog whose ids collide, while the review is open: "Yes" records nothing in
/// it (the confirmation is bound to the catalog the photo came from) and says so;
/// `catalog:switched` closes the dialog.
#[gpui_kit::test]
fn a_confirmation_after_a_switch_records_nothing(cx: &mut TestAppContext) {
    let dir = TempDir::new("instagram-switch");
    let (app, ids, _) = setup(&dir, cx);
    let chrome = FakeChrome::answering(PostOutcome::AwaitingReview);
    let view = panel(&app, chrome.clone(), cx);
    view.update(cx, |p, cx| p.post(cx));
    work(cx);
    let (b, b_ids) = colliding_catalog(&dir, "b", 1);
    assert_eq!(b_ids, ids);
    core_switch(&app, b);
    view.update(cx, |p, cx| p.confirm_posted(cx));
    work(cx);
    view.read_with(cx, |p, _| assert!(p.status.contains(CATALOG_CHANGED), "{}", p.status));
    assert!(publications(&app, ids[0]).is_empty(), "recorded into the catalog that opened since");
    deliver_switch(&app, cx);
    let open = cx.update_window(app.window(), |_, window, cx| window.has_active_dialog(cx)).unwrap();
    assert!(!open, "closed on catalog:switched");
    let path = chrome.posts.lock().unwrap()[0].0.clone();
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}
