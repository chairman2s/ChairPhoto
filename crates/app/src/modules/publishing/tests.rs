//! Headless tests of the shared publish flow with a fake [`PublishService`] (no network): the
//! subject snapshot, the version picker, PublishPanel (tags prefill, albums, Publish records a
//! publication with its URL), OAuthSettings (keys saved namespaced, Connect, Finish), and the
//! catalog-identity rule.

use super::oauth::OAuthSettings;
use super::panel::{PublishPanel, Stage, ALBUMS_CACHE, LAST_ALBUM};
use super::{Album, PublishRequest, PublishService};
use crate::modules::{ModuleHost, ModuleMeta, ModuleSettings};
use crate::storage::Runner;
use crate::tests::{colliding_catalog, core_switch, deliver_switch, start, App, TempDir};
use chairphoto_core::app::{CoreEvent, EventSink as _};
use chairphoto_core::catalog::Catalog;
use chairphoto_core::app::uploads::{RenderedJob, UploadJob, UploadRenderer, UploadService, UPLOAD_CANCELLED};
use chairphoto_core::app::{CatalogIdentity, CATALOG_CHANGED};
use gpui_kit::component::WindowExt as _;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AppContext as _, Entity, SharedString, TestAppContext};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Calls {
    published: Vec<PublishRequest>,
    rendered: usize,
    created: Vec<String>,
    listed: usize,
    verifier: Option<String>,
}

/// A service with tags and albums that answers from memory. Its render writes a marker file
/// (no thumbnail cache needed); its upload checks the job is still live, as a real one must.
struct Fake {
    calls: Arc<Mutex<Calls>>,
    answer: String,
}

impl PublishService for Fake {
    fn name(&self) -> SharedString {
        "Fakr".into()
    }
    fn signup_url(&self) -> SharedString {
        "https://fakr.example/apps".into()
    }
    fn begin_auth(&self, _: &ModuleSettings) -> Result<String, String> {
        Ok("https://fakr.example/authorize?t=1".into())
    }
    fn complete_auth(&self, settings: &ModuleSettings, verifier: &str) -> Result<(), String> {
        self.calls.lock().unwrap().verifier = Some(verifier.into());
        settings.set("access_token", "tok")
    }
    fn connected(&self, settings: &ModuleSettings) -> Result<bool, String> {
        Ok(settings.get("access_token")?.is_some())
    }
    fn service(&self) -> UploadService {
        UploadService::Flickr
    }
    fn render(&self, _: &ModuleSettings, job: UploadJob) -> Result<RenderedJob, String> {
        // Counted when the render really runs (a tripped job refuses before it).
        let calls = self.calls.clone();
        let render: UploadRenderer = Arc::new(move |_, out| {
            calls.lock().unwrap().rendered += 1;
            std::fs::write(out, b"pixels").map_err(|e| e.to_string())
        });
        job.render(&render)
    }
    fn upload(&self, _: &ModuleSettings, rendered: &RenderedJob, request: &PublishRequest) -> Result<String, String> {
        rendered.ensure_live()?;
        assert_eq!(std::fs::read(rendered.path()).unwrap(), b"pixels", "uploads the job's own render");
        self.calls.lock().unwrap().published.push(request.clone());
        Ok(self.answer.clone())
    }
    fn has_tags(&self) -> bool {
        true
    }
    fn suggest_tags(&self, _: &ModuleSettings, _: CatalogIdentity, photo_id: i64) -> Result<String, String> {
        Ok(format!("photo{photo_id} \"northern lights\""))
    }
    fn has_albums(&self) -> bool {
        true
    }
    fn list_albums(&self, _: &ModuleSettings) -> Result<Vec<Album>, String> {
        self.calls.lock().unwrap().listed += 1;
        Ok(vec![Album { uri: "/a/1".into(), name: "Trips".into() }, Album { uri: "/a/2".into(), name: "Birds".into() }])
    }
    fn can_create_album(&self) -> bool {
        true
    }
    fn create_album(&self, _: &ModuleSettings, name: &str) -> Result<Album, String> {
        self.calls.lock().unwrap().created.push(name.into());
        Ok(Album { uri: "/a/new".into(), name: name.into() })
    }
}

/// A catalog in `dir` with `n` photos whose originals exist (a publish resolves them).
pub(crate) fn with_files(app: &App, dir: &TempDir, n: usize, cx: &mut TestAppContext) -> Vec<i64> {
    let db = dir.0.join("photos.chairphoto");
    let root = dir.0.join("photos");
    std::fs::create_dir_all(root.join("2026")).unwrap();
    let catalog = Catalog::open(&db, &root).unwrap();
    let ids = (0..n)
        .map(|i| {
            let p = root.join(format!("2026/p{i}.jpg"));
            // A real (tiny) JPEG: a service's own render decodes it.
            image::RgbImage::from_pixel(48 + i as u32, 32, image::Rgb([200, 120, 40])).save(&p).unwrap();
            catalog.upsert_photo(&p, None, 0, 6).unwrap().id
        })
        .collect();
    *app.state.catalog.lock().unwrap() = Some(catalog);
    app.state.send(CoreEvent::CatalogSwitched(db.to_string_lossy().to_string()));
    cx.run_until_parked();
    ids
}

/// One worker step of a publish: run what is queued, then let its answer land (which may
/// queue the next step).
pub(crate) fn step(cx: &mut TestAppContext) {
    cx.update(|cx| Runner::get(cx).run_pending());
    cx.run_until_parked();
}

/// The test module's id (and settings namespace).
const FAKR: &str = "fakr";

pub(crate) fn work(cx: &mut TestAppContext) {
    loop {
        let ran = cx.update(|cx| Runner::get(cx).run_pending());
        cx.run_until_parked();
        if ran == 0 {
            return;
        }
    }
}

/// The module host for a module `fakr`, restored against the open catalog.
fn host(app: &App, cx: &mut TestAppContext) -> ModuleHost {
    let catalog = chairphoto_core::app::catalog_identity(&app.state).unwrap();
    let restored = crate::modules::RestoredCatalog::default();
    restored.set(Some(catalog));
    let model = app.wired.model.clone();
    let shell = app.wired.shell.clone();
    cx.update(|_| ModuleHost::new(ModuleMeta::new("fakr", "Fakr"), app.state.clone(), restored, model, shell))
}

pub(crate) fn setting(app: &App, key: &str) -> Option<String> {
    app.state.catalog.lock().unwrap().as_ref().unwrap().get_setting(key).unwrap()
}

fn open_panel(app: &App, host: &ModuleHost, service: Arc<dyn PublishService>, cx: &mut TestAppContext) -> Entity<PublishPanel> {
    open_panel_as(app, host, service, "fakr", cx)
}

/// [`open_panel`] recording under `marker`.
fn open_panel_as(app: &App, host: &ModuleHost, service: Arc<dyn PublishService>, marker: &'static str, cx: &mut TestAppContext) -> Entity<PublishPanel> {
    let view = cx
        .update_window(app.window(), |_, window, cx| {
            let (model, shell, settings) = (host.model().clone(), host.shell().clone(), host.settings());
            let state = app.state.clone();
            let view = cx.new(|cx| PublishPanel::new(state, model, &shell, settings, marker.into(), service, window, cx));
            crate::modules::dialog::open("Publish", 560., true, view.clone(), window, cx);
            view
        })
        .unwrap();
    cx.run_until_parked();
    work(cx);
    view
}

pub(crate) fn select(app: &App, id: i64, cx: &mut TestAppContext) {
    app.wired.shell.update(cx, |s, cx| {
        s.library.select(id, Default::default());
        cx.notify();
    });
    cx.run_until_parked();
}

pub(crate) fn present(app: &App, id: &'static str, cx: &mut TestAppContext) -> bool {
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.try_find(id).is_some()
    })
    .unwrap()
}

/// Publish: the request carries the chosen version, the fields, the album and the tags; the
/// publication is recorded under the module's marker with the service's URL; the status
/// line says so. Tags prefill until edited; the album list is fetched once and then cached,
/// the chosen one remembered.
#[gpui_kit::test]
fn publish_records_a_publication_with_its_url(cx: &mut TestAppContext) {
    let dir = TempDir::new("publish-panel");
    let app = start(cx);
    let ids = with_files(&app, &dir, 2, cx);
    work(cx);
    let version = {
        let guard = app.state.catalog.lock().unwrap();
        guard.as_ref().unwrap().create_version(ids[0], "Punchy").unwrap()
    };
    select(&app, ids[0], cx);
    let host = host(&app, cx);
    let calls = Arc::new(Mutex::new(Calls::default()));
    let service = Arc::new(Fake { calls: calls.clone(), answer: "https://fakr.example/p/42".into() });
    let view = open_panel(&app, &host, service.clone(), cx);
    view.read_with(cx, |p, cx| {
        assert_eq!(p.versions.versions.len(), 1, "the active photo's versions");
        assert_eq!(p.versions.chosen, None, "Original by default (no active version)");
        assert_eq!(p.tags.read(cx).value().to_string(), format!("photo{} \"northern lights\"", ids[0]), "prefilled");
        assert!(!p.tags_touched, "the prefill is not an edit");
        assert_eq!(p.album, "/a/1", "the first album");
        assert_eq!(p.albums.len(), 2);
    });
    assert_eq!(calls.lock().unwrap().listed, 1);
    assert!(setting(&app, &format!("fakr.{ALBUMS_CACHE}")).is_some(), "the list is cached");

    view.update(cx, |p, cx| {
        p.versions.chosen = Some(version);
        p.select_album("/a/2".into(), cx);
    });
    cx.update_window(app.window(), |_, window, cx| {
        view.update(cx, |p, cx| p.title.update(cx, |i, cx| i.set_value("  Aurora ", window, cx)));
    })
    .unwrap();
    work(cx);
    assert_eq!(setting(&app, &format!("fakr.{LAST_ALBUM}")).as_deref(), Some("/a/2"));
    view.update(cx, |p, cx| p.publish(cx));
    view.read_with(cx, |p, _| assert!(p.busy, "queued, not run on the UI thread"));
    work(cx);
    let req = calls.lock().unwrap().published[0].clone();
    assert_eq!((req.photo_id, req.version_id, req.title.as_str(), req.album_uri.as_str()), (ids[0], Some(version), "Aurora", "/a/2"));
    assert!(req.tags.contains("northern lights"));
    let pubs = app.state.catalog.lock().unwrap().as_ref().unwrap().list_publications(ids[0]).unwrap();
    assert_eq!(pubs.len(), 1);
    assert_eq!((pubs[0].platform.as_str(), pubs[0].version_id, pubs[0].url.as_deref()), ("fakr", Some(version), Some("https://fakr.example/p/42")));
    view.read_with(cx, |p, _| assert_eq!(p.status, "Published to Fakr ✓"));
    assert_eq!(crate::tests::status(&app, cx), "Published to Fakr.");

    // A second form opens on the cached list without asking the service again.
    cx.update_window(app.window(), |_, window, cx| window.close_dialog(cx)).unwrap();
    let view = open_panel(&app, &host, service, cx);
    assert_eq!(calls.lock().unwrap().listed, 1, "served from the cache");
    view.read_with(cx, |p, _| assert_eq!(p.album, "/a/2", "the remembered album"));
    // "+ New" puts the album first and chooses it.
    cx.update_window(app.window(), |_, window, cx| {
        view.update(cx, |p, cx| p.new_album.update(cx, |i, cx| i.set_value("Owls", window, cx)));
    })
    .unwrap();
    view.update(cx, |p, cx| p.create_album(cx));
    work(cx);
    view.read_with(cx, |p, _| {
        assert_eq!(p.albums[0].name, "Owls");
        assert_eq!(p.album, "/a/new");
    });
}

/// A non-URL answer records no URL; an edited Tags field is not overwritten by the prefill.
#[gpui_kit::test]
fn a_non_url_answer_records_no_url(cx: &mut TestAppContext) {
    let dir = TempDir::new("publish-nourl");
    let app = start(cx);
    let ids = with_files(&app, &dir, 1, cx);
    work(cx);
    select(&app, ids[0], cx);
    let host = host(&app, cx);
    let calls = Arc::new(Mutex::new(Calls::default()));
    let view = open_panel(&app, &host, Arc::new(Fake { calls, answer: "/api/v2/image/abc".into() }), cx);
    view.update(cx, |p, cx| p.publish(cx));
    work(cx);
    let pubs = app.state.catalog.lock().unwrap().as_ref().unwrap().list_publications(ids[0]).unwrap();
    assert_eq!(pubs[0].url, None);
}

/// The upload succeeds but the record step fails (here: a marker the catalog rejects): the
/// panel and the status line say it was published and why it wasn't recorded, and warn
/// against publishing again — not a bare error that reads as "try again".
#[gpui_kit::test]
fn an_upload_whose_record_fails_says_it_was_published(cx: &mut TestAppContext) {
    let dir = TempDir::new("publish-unrecorded");
    let app = start(cx);
    let ids = with_files(&app, &dir, 1, cx);
    work(cx);
    select(&app, ids[0], cx);
    let host = host(&app, cx);
    let calls = Arc::new(Mutex::new(Calls::default()));
    let service = Arc::new(Fake { calls: calls.clone(), answer: "https://x".into() });
    let view = open_panel_as(&app, &host, service, " ", cx);
    view.update(cx, |p, cx| p.publish(cx));
    work(cx);
    assert_eq!(calls.lock().unwrap().published.len(), 1, "the upload went through");
    let pubs = app.state.catalog.lock().unwrap().as_ref().unwrap().list_publications(ids[0]).unwrap();
    assert!(pubs.is_empty(), "the record failed");
    view.read_with(cx, |p, _| {
        assert!(!p.busy);
        assert!(p.status.contains("publication platform is empty"), "the record's error: {}", p.status);
        assert!(p.status.starts_with("Published 1 to Fakr, but couldn't record"), "{}", p.status);
        assert!(p.status.ends_with("don't publish it again."), "{}", p.status);
    });
    let line = crate::tests::status(&app, cx);
    assert!(line.starts_with("Published 1 to Fakr, but couldn't record"), "{line}");
}

/// A switch to a catalog whose ids collide: Publish is refused before anything is recorded in
/// the new catalog; `catalog:switched` closes the dialog.
#[gpui_kit::test]
fn a_catalog_switch_refuses_the_record_and_closes_the_dialog(cx: &mut TestAppContext) {
    let dir = TempDir::new("publish-switch");
    let app = start(cx);
    let ids = with_files(&app, &dir, 1, cx);
    work(cx);
    select(&app, ids[0], cx);
    let host = host(&app, cx);
    let calls = Arc::new(Mutex::new(Calls::default()));
    let view = open_panel(&app, &host, Arc::new(Fake { calls: calls.clone(), answer: "https://x".into() }), cx);
    let (b, b_ids) = colliding_catalog(&dir, "b", 1);
    assert_eq!(b_ids, ids);
    core_switch(&app, b);
    view.update(cx, |p, cx| p.publish(cx));
    work(cx);
    view.read_with(cx, |p, _| assert_eq!(p.status, CATALOG_CHANGED));
    assert!(calls.lock().unwrap().published.is_empty(), "nothing uploaded");
    let pubs = app.state.catalog.lock().unwrap().as_ref().unwrap().list_publications(ids[0]).unwrap();
    assert!(pubs.is_empty(), "nothing recorded in B");
    deliver_switch(&app, cx);
    let open = cx.update_window(app.window(), |_, window, cx| window.has_active_dialog(cx)).unwrap();
    assert!(!open, "closed on catalog:switched");
}

/// No active photo: "Select a photo".
#[gpui_kit::test]
fn without_a_photo_the_panel_says_so(cx: &mut TestAppContext) {
    let dir = TempDir::new("publish-empty");
    let app = start(cx);
    with_files(&app, &dir, 1, cx);
    work(cx);
    let host = host(&app, cx);
    let calls = Arc::new(Mutex::new(Calls::default()));
    open_panel(&app, &host, Arc::new(Fake { calls, answer: String::new() }), cx);
    assert!(present(&app, "publish-panel-empty", cx));
}

/// OAuthSettings: the keys are saved in the module's namespace (trimmed), Connect saves and
/// shows the verifier field, Finish hands the verifier over and shows "Connected ✓".
#[gpui_kit::test]
fn oauth_settings_save_connect_and_finish(cx: &mut TestAppContext) {
    let dir = TempDir::new("publish-oauth");
    let app = start(cx);
    with_files(&app, &dir, 1, cx);
    work(cx);
    let host = host(&app, cx);
    let calls = Arc::new(Mutex::new(Calls::default()));
    let service: Arc<dyn PublishService> = Arc::new(Fake { calls: calls.clone(), answer: String::new() });
    let view = cx
        .update_window(app.window(), |_, window, cx| {
            let (settings, model) = (host.settings(), host.model().clone());
            let view = cx.new(|cx| OAuthSettings::new(settings, &model, service, window, cx));
            crate::modules::dialog::open("Settings", 560., true, view.clone(), window, cx);
            view
        })
        .unwrap();
    cx.run_until_parked();
    work(cx);
    view.read_with(cx, |v, _| assert!(!v.connected));
    cx.update_window(app.window(), |_, window, cx| {
        view.update(cx, |v, cx| {
            v.key.update(cx, |i, cx| i.set_value(" k1 ", window, cx));
            v.secret.update(cx, |i, cx| i.set_value("s1", window, cx));
            v.max_long_edge.update(cx, |i, cx| i.set_value("2048", window, cx));
        })
    })
    .unwrap();
    view.update(cx, |v, cx| v.connect(cx));
    work(cx);
    assert_eq!(setting(&app, "fakr.api_key").as_deref(), Some("k1"));
    assert_eq!(setting(&app, "fakr.api_secret").as_deref(), Some("s1"));
    assert_eq!(setting(&app, "fakr.max_long_edge").as_deref(), Some("2048"));
    view.read_with(cx, |v, _| assert_eq!(v.auth_url.as_deref(), Some("https://fakr.example/authorize?t=1")));
    assert!(present(&app, "oauth-verifier", cx));
    cx.update_window(app.window(), |_, window, cx| {
        view.update(cx, |v, cx| v.verifier.update(cx, |i, cx| i.set_value(" 123-456 ", window, cx)))
    })
    .unwrap();
    view.update(cx, |v, cx| v.finish(cx));
    work(cx);
    assert_eq!(calls.lock().unwrap().verifier.as_deref(), Some("123-456"));
    view.read_with(cx, |v, _| {
        assert!(v.connected);
        assert_eq!(v.auth_url, None);
        assert_eq!(v.status, "Connected.");
    });
}

pub(crate) fn publications(app: &App, id: i64) -> Vec<chairphoto_core::catalog::Publication> {
    app.state.catalog.lock().unwrap().as_ref().unwrap().list_publications(id).unwrap()
}

/// The steps show as they land (preparing → rendering → uploading → done); Cancel is offered
/// until the upload starts, and not after.
#[gpui_kit::test]
fn a_publish_shows_its_steps_and_offers_cancel_until_the_upload(cx: &mut TestAppContext) {
    let dir = TempDir::new("publish-steps");
    let app = start(cx);
    let ids = with_files(&app, &dir, 1, cx);
    work(cx);
    select(&app, ids[0], cx);
    let host = host(&app, cx);
    let calls = Arc::new(Mutex::new(Calls::default()));
    let view = open_panel(&app, &host, Arc::new(Fake { calls: calls.clone(), answer: "https://x/1".into() }), cx);
    view.update(cx, |p, cx| p.publish(cx));
    view.read_with(cx, |p, _| assert_eq!((p.stage, p.status.as_str()), (Some(Stage::Preparing), "Preparing…")));
    assert!(present(&app, "publish-panel-cancel", cx));
    step(cx);
    view.read_with(cx, |p, _| assert_eq!((p.stage, p.status.as_str()), (Some(Stage::Rendering), "Rendering…")));
    assert!(present(&app, "publish-panel-cancel", cx));
    step(cx);
    view.read_with(cx, |p, _| assert_eq!((p.stage, p.status.as_str()), (Some(Stage::Uploading), "Uploading to Fakr…")));
    assert!(!present(&app, "publish-panel-cancel", cx), "Cancel offered for an upload in flight");
    view.update(cx, |p, cx| p.cancel(cx));
    step(cx);
    view.read_with(cx, |p, _| assert_eq!((p.stage, p.busy, p.status.as_str()), (None, false, "Published to Fakr ✓")));
    assert_eq!(calls.lock().unwrap().published.len(), 1, "a Cancel during the upload stopped it");
    assert_eq!(publications(&app, ids[0]).len(), 1);
}

/// Cancel while rendering: the render's job is tripped, nothing is uploaded or recorded, and
/// the panel says so. Cancel before the claim lands: nothing is even rendered.
#[gpui_kit::test]
fn cancel_stops_a_publish_before_its_upload(cx: &mut TestAppContext) {
    let dir = TempDir::new("publish-cancel");
    let app = start(cx);
    let ids = with_files(&app, &dir, 1, cx);
    work(cx);
    select(&app, ids[0], cx);
    let host = host(&app, cx);
    let calls = Arc::new(Mutex::new(Calls::default()));
    let view = open_panel(&app, &host, Arc::new(Fake { calls: calls.clone(), answer: "https://x/1".into() }), cx);

    view.update(cx, |p, cx| p.publish(cx));
    step(cx); // claimed: rendering is queued
    view.update(cx, |p, cx| p.cancel(cx));
    step(cx); // the render refuses (its job is tripped)
    view.read_with(cx, |p, _| assert_eq!((p.busy, p.stage, p.status.as_str()), (false, None, UPLOAD_CANCELLED)));
    assert_eq!(calls.lock().unwrap().rendered, 0, "Cancel did not stop the render");
    assert!(calls.lock().unwrap().published.is_empty(), "a cancelled publish uploaded");
    assert!(publications(&app, ids[0]).is_empty());

    view.update(cx, |p, cx| p.publish(cx));
    view.update(cx, |p, cx| p.cancel(cx));
    let rendered = calls.lock().unwrap().rendered;
    step(cx);
    view.read_with(cx, |p, _| assert_eq!((p.busy, p.status.as_str()), (false, UPLOAD_CANCELLED)));
    assert_eq!(calls.lock().unwrap().rendered, rendered, "a publish cancelled before its claim rendered");
    work(cx);
    assert!(calls.lock().unwrap().published.is_empty());
}

/// A catalog switch while the publish renders trips its job: nothing is uploaded, and the
/// panel says the catalog changed (the dialog then closes on `catalog:switched`).
#[gpui_kit::test]
fn a_switch_during_the_render_stops_the_publish(cx: &mut TestAppContext) {
    let dir = TempDir::new("publish-switch-render");
    let app = start(cx);
    let ids = with_files(&app, &dir, 1, cx);
    work(cx);
    select(&app, ids[0], cx);
    let host = host(&app, cx);
    let calls = Arc::new(Mutex::new(Calls::default()));
    let view = open_panel(&app, &host, Arc::new(Fake { calls: calls.clone(), answer: "https://x/1".into() }), cx);
    view.update(cx, |p, cx| p.publish(cx));
    step(cx);
    let (b, _) = colliding_catalog(&dir, "b", 1);
    core_switch(&app, b);
    work(cx);
    view.read_with(cx, |p, _| assert_eq!(p.status, CATALOG_CHANGED));
    assert!(calls.lock().unwrap().published.is_empty());
    assert!(publications(&app, ids[0]).is_empty(), "nothing recorded in B");
}

/// An upload in flight when the catalog switches finishes — the photo is on the service — but
/// its publication is not recorded into the catalog that opened since: the panel says it was
/// published, not recorded, and not to publish it again.
#[gpui_kit::test]
fn a_switch_during_the_upload_records_nothing_and_says_so(cx: &mut TestAppContext) {
    struct SwitchingFake {
        inner: Fake,
        switch: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }
    impl PublishService for SwitchingFake {
        fn name(&self) -> SharedString {
            self.inner.name()
        }
        fn signup_url(&self) -> SharedString {
            self.inner.signup_url()
        }
        fn service(&self) -> UploadService {
            self.inner.service()
        }
        fn begin_auth(&self, s: &ModuleSettings) -> Result<String, String> {
            self.inner.begin_auth(s)
        }
        fn complete_auth(&self, s: &ModuleSettings, v: &str) -> Result<(), String> {
            self.inner.complete_auth(s, v)
        }
        fn connected(&self, s: &ModuleSettings) -> Result<bool, String> {
            self.inner.connected(s)
        }
        fn render(&self, s: &ModuleSettings, job: UploadJob) -> Result<RenderedJob, String> {
            self.inner.render(s, job)
        }
        fn upload(&self, s: &ModuleSettings, rendered: &RenderedJob, request: &PublishRequest) -> Result<String, String> {
            let answer = self.inner.upload(s, rendered, request)?;
            // The switch lands while the bytes are with the service.
            (self.switch.lock().unwrap().take().unwrap())();
            Ok(answer)
        }
    }
    let dir = TempDir::new("publish-switch-upload");
    let app = start(cx);
    let ids = with_files(&app, &dir, 1, cx);
    work(cx);
    select(&app, ids[0], cx);
    let host = host(&app, cx);
    let calls = Arc::new(Mutex::new(Calls::default()));
    let (b, _) = colliding_catalog(&dir, "b", 1);
    let state = app.state.clone();
    let service = SwitchingFake {
        inner: Fake { calls: calls.clone(), answer: "https://x/1".into() },
        switch: Mutex::new(Some(Box::new(move || {
            chairphoto_core::app::detach_catalog_and_trip_jobs(&state).unwrap();
            chairphoto_core::app::publish_catalog_and_reset_jobs(&state, b).unwrap();
        }))),
    };
    let view = open_panel(&app, &host, Arc::new(service), cx);
    view.update(cx, |p, cx| p.publish(cx));
    work(cx);
    assert_eq!(calls.lock().unwrap().published.len(), 1, "the upload went through");
    assert!(publications(&app, ids[0]).is_empty(), "recorded into the catalog that opened since");
    view.read_with(cx, |p, _| {
        assert!(p.status.starts_with("Published 1 to Fakr, but couldn't record"), "{}", p.status);
        assert!(p.status.contains(CATALOG_CHANGED), "{}", p.status);
    });
}

/// OAuthSettings outlives a catalog switch: `catalog:switched` clears it (a key typed for A is
/// not shown against B) and drops A's answers in flight; the next catalog read shows B's keys,
/// and Save writes them into B only.
#[gpui_kit::test]
fn oauth_settings_follow_a_catalog_switch(cx: &mut TestAppContext) {
    let dir = TempDir::new("publish-oauth-switch");
    let app = start(cx);
    with_files(&app, &dir, 1, cx);
    work(cx);
    app.state.catalog.lock().unwrap().as_ref().unwrap().set_setting(&format!("{FAKR}.api_key"), "key-a").unwrap();
    let host = host(&app, cx);
    let calls = Arc::new(Mutex::new(Calls::default()));
    let service: Arc<dyn PublishService> = Arc::new(Fake { calls, answer: String::new() });
    let view = cx
        .update_window(app.window(), |_, window, cx| {
            let (settings, model) = (host.settings(), host.model().clone());
            let view = cx.new(|cx| OAuthSettings::new(settings, &model, service, window, cx));
            crate::modules::dialog::open("Settings", 560., true, view.clone(), window, cx);
            view
        })
        .unwrap();
    work(cx);
    let key = |cx: &mut TestAppContext| {
        cx.update_window(app.window(), |_, window, cx| {
            window.render_frame(cx);
            view.read(cx).key.read(cx).value().to_string()
        })
        .unwrap()
    };
    assert_eq!(key(cx), "key-a");

    // A Save queued against A, then the switch: the answer for A is dropped, and B's keys load.
    view.update(cx, |v, cx| v.save(cx));
    let held = cx.update(|cx| Runner::get(cx).hold_pending());
    let (b, _) = colliding_catalog(&dir, "b", 1);
    b.set_setting(&format!("{FAKR}.api_key"), "key-b").unwrap();
    core_switch(&app, b);
    deliver_switch(&app, cx);
    assert_eq!(key(cx), "", "A's key shown against B");
    cx.update(|cx| Runner::get(cx).release(held));
    work(cx);
    view.read_with(cx, |v, _| assert_eq!(v.status, "", "A's answer landed after the switch"));
    assert_eq!(key(cx), "key-b");
    assert_eq!(setting(&app, "fakr.api_key").as_deref(), Some("key-b"), "A's queued save wrote into B");
    view.update(cx, |v, cx| v.save(cx));
    work(cx);
    view.read_with(cx, |v, _| assert_eq!(v.status, "Saved."));
}
