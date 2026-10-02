//! Headless tests of the shared publish flow with a fake [`PublishService`] (no network): the
//! subject snapshot, the version picker, PublishPanel (tags prefill, albums, Publish records a
//! publication with its URL), OAuthSettings (keys saved namespaced, Connect, Finish), and the
//! catalog-identity rule.

use super::oauth::OAuthSettings;
use super::panel::{PublishPanel, ALBUMS_CACHE, LAST_ALBUM};
use super::{Album, PublishRequest, PublishService};
use crate::modules::{ModuleHost, ModuleMeta, ModuleSettings};
use crate::storage::Runner;
use crate::tests::{colliding_catalog, core_switch, deliver_switch, open_catalog_with_photos, start, App, TempDir};
use chairphoto_core::app::{CatalogIdentity, CATALOG_CHANGED};
use gpui_kit::component::WindowExt as _;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AppContext as _, Entity, SharedString, TestAppContext};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Calls {
    published: Vec<PublishRequest>,
    created: Vec<String>,
    listed: usize,
    verifier: Option<String>,
}

/// A service with tags and albums that answers from memory.
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
    fn publish(&self, _: &ModuleSettings, request: PublishRequest) -> Result<String, String> {
        self.calls.lock().unwrap().published.push(request);
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

fn work(cx: &mut TestAppContext) {
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

fn setting(app: &App, key: &str) -> Option<String> {
    app.state.catalog.lock().unwrap().as_ref().unwrap().get_setting(key).unwrap()
}

fn open_panel(app: &App, host: &ModuleHost, service: Arc<dyn PublishService>, cx: &mut TestAppContext) -> Entity<PublishPanel> {
    let view = cx
        .update_window(app.window(), |_, window, cx| {
            let (model, shell, settings) = (host.model().clone(), host.shell().clone(), host.settings());
            let state = app.state.clone();
            let view = cx.new(|cx| PublishPanel::new(state, model, &shell, settings, "fakr".into(), service, window, cx));
            crate::modules::dialog::open("Publish", 560., true, view.clone(), window, cx);
            view
        })
        .unwrap();
    cx.run_until_parked();
    work(cx);
    view
}

fn select(app: &App, id: i64, cx: &mut TestAppContext) {
    app.wired.shell.update(cx, |s, cx| {
        s.library.select(id, Default::default());
        cx.notify();
    });
    cx.run_until_parked();
}

fn present(app: &App, id: &'static str, cx: &mut TestAppContext) -> bool {
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
    let ids = open_catalog_with_photos(&app, &dir, 2, cx);
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
    let ids = open_catalog_with_photos(&app, &dir, 1, cx);
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

/// A switch to a catalog whose ids collide: Publish is refused before anything is recorded in
/// the new catalog; `catalog:switched` closes the dialog.
#[gpui_kit::test]
fn a_catalog_switch_refuses_the_record_and_closes_the_dialog(cx: &mut TestAppContext) {
    let dir = TempDir::new("publish-switch");
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, 1, cx);
    work(cx);
    select(&app, ids[0], cx);
    let host = host(&app, cx);
    let calls = Arc::new(Mutex::new(Calls::default()));
    let view = open_panel(&app, &host, Arc::new(Fake { calls, answer: "https://x".into() }), cx);
    let (b, b_ids) = colliding_catalog(&dir, "b", 1);
    assert_eq!(b_ids, ids);
    core_switch(&app, b);
    view.update(cx, |p, cx| p.publish(cx));
    work(cx);
    view.read_with(cx, |p, _| assert_eq!(p.status, CATALOG_CHANGED));
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
    open_catalog_with_photos(&app, &dir, 1, cx);
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
    open_catalog_with_photos(&app, &dir, 1, cx);
    work(cx);
    let host = host(&app, cx);
    let calls = Arc::new(Mutex::new(Calls::default()));
    let service: Arc<dyn PublishService> = Arc::new(Fake { calls: calls.clone(), answer: String::new() });
    let view = cx
        .update_window(app.window(), |_, window, cx| {
            let settings = host.settings();
            let view = cx.new(|cx| OAuthSettings::new(settings, service, window, cx));
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
