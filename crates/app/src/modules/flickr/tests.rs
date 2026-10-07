//! Headless tests of the Flickr module through its real contributions, over a fake
//! [`FlickrApi`] (no network, no account): sign-in stores the tokens in `flickr.*`, a publish
//! renders the photo for real and records a `flickr` publication with its page URL, and the
//! photostream import previews, resolves and records — bound to the catalog it matched.

use super::import::ImportPublishedPanel;
use super::FlickrModule;
use crate::modules::publishing::oauth::OAuthSettings;
use crate::modules::publishing::panel::PublishPanel;
use crate::modules::publishing::tests::{publications, select, setting, step, with_files, work};
use crate::modules::{Contributions, Module, ModuleHost, ModuleInstance, RestoredCatalog};
use crate::storage::Runner;
use crate::tests::{colliding_catalog, core_switch, deliver_switch, start, App, TempDir};
use chairphoto_core::app::flickr::FlickrApi;
use chairphoto_core::app::oauth::{AccessToken, Credentials, OAuthApi, RequestToken};
use chairphoto_core::app::CATALOG_CHANGED;
use chairphoto_core::flickr::FlickrPhoto;
use gpui_kit::component::WindowExt as _;
use gpui_kit::prelude::*;
use gpui_kit::{AnyView, Entity, TestAppContext};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// A Flickr that answers from memory.
#[derive(Default)]
struct Fake {
    /// (title, description, tags, bytes) per upload.
    uploads: Mutex<Vec<(String, String, String, usize)>>,
    stream: Mutex<Vec<FlickrPhoto>>,
    thumbs_asked: Mutex<Vec<String>>,
    /// What a thumbnail fetch answers; `None` fails it, as offline.
    thumb_bytes: Mutex<Option<Vec<u8>>>,
}

impl OAuthApi for Fake {
    fn request_token(&self, _: &str, _: &str) -> Result<RequestToken, String> {
        Ok(RequestToken { token: "req-tok".into(), secret: "req-sec".into(), authorize_url: "https://flickr.example/authorize?oauth_token=req-tok".into() })
    }
    fn access_token(&self, _: &str, _: &str, token: &str, secret: &str, verifier: &str) -> Result<AccessToken, String> {
        if (token, secret, verifier) != ("req-tok", "req-sec", "123-456-789") {
            return Err("Flickr authorization failed: oauth_problem=verifier_invalid".into());
        }
        Ok(AccessToken { token: "acc-tok".into(), secret: "acc-sec".into(), user_nsid: Some("99@N01".into()) })
    }
}

impl FlickrApi for Fake {
    fn upload(&self, c: &Credentials, image: &Path, title: &str, description: &str, tags: &str) -> Result<String, String> {
        assert_eq!((c.token.as_str(), c.token_secret.as_str()), ("acc-tok", "acc-sec"));
        let bytes = std::fs::read(image).map_err(|e| e.to_string())?;
        assert!(bytes.starts_with(&[0xFF, 0xD8]), "the upload is a rendered JPEG");
        let mut uploads = self.uploads.lock().unwrap();
        uploads.push((title.into(), description.into(), tags.into(), bytes.len()));
        Ok(format!("{}", 5000 + uploads.len()))
    }
    fn photostream(&self, _: &Credentials) -> Result<Vec<FlickrPhoto>, String> {
        Ok(self.stream.lock().unwrap().clone())
    }
    fn thumbnail(&self, url: &str) -> Result<Vec<u8>, String> {
        self.thumbs_asked.lock().unwrap().push(url.into());
        self.thumb_bytes.lock().unwrap().clone().ok_or_else(|| "offline".into())
    }
}

/// The Flickr module loaded against the open catalog, and what it contributes.
struct Loaded {
    host: ModuleHost,
    _instance: Box<dyn ModuleInstance>,
    contributions: Contributions,
}

fn load(app: &App, api: Arc<Fake>, cx: &mut TestAppContext) -> Loaded {
    let module = FlickrModule { api };
    let catalog = chairphoto_core::app::catalog_identity(&app.state).unwrap();
    let restored = RestoredCatalog::default();
    restored.set(Some(catalog));
    let (model, shell) = (app.wired.model.clone(), app.wired.shell.clone());
    let host = cx.update(|_| ModuleHost::new(module.meta(), app.state.clone(), restored, model, shell).with_images(Some(app.wired.images.clone())));
    let instance = cx.update(|cx| module.load(host.clone(), cx)).ok().unwrap();
    let contributions = instance.contributions();
    Loaded { host, _instance: instance, contributions }
}

/// Build contributed view `factory` in a dialog of the main window.
fn open<V: 'static>(app: &App, factory: &crate::modules::ViewFactory, cx: &mut TestAppContext) -> Entity<V> {
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
    view.downcast::<V>().ok().unwrap()
}

fn connect(app: &App) {
    let guard = app.state.catalog.lock().unwrap();
    let c = guard.as_ref().unwrap();
    for (k, v) in [("flickr.api_key", "k"), ("flickr.api_secret", "s"), ("flickr.access_token", "acc-tok"), ("flickr.access_secret", "acc-sec"), ("flickr.user_nsid", "99@N01")] {
        c.set_setting(k, v).unwrap();
    }
}

/// Sign-in: Connect keeps the request token in `flickr.*`; a wrong verifier is refused and
/// says why; the right one stores the access token and the NSID and forgets the request
/// token. No key, secret or token ever reaches the status line.
#[gpui_kit::test]
fn connect_and_finish_store_the_tokens_in_flickr_settings(cx: &mut TestAppContext) {
    let dir = TempDir::new("flickr-signin");
    let app = start(cx);
    with_files(&app, &dir, 1, cx);
    work(cx);
    let loaded = load(&app, Arc::new(Fake::default()), cx);
    assert_eq!(loaded.host.meta().marker(), "flickr");
    let view: Entity<OAuthSettings> = open(&app, &loaded.contributions.settings[0].view, cx);
    cx.update_window(app.window(), |_, window, cx| {
        view.update(cx, |v, cx| {
            v.key.update(cx, |i, cx| i.set_value("my-key", window, cx));
            v.secret.update(cx, |i, cx| i.set_value("my-secret", window, cx));
        })
    })
    .unwrap();
    view.update(cx, |v, cx| v.connect(cx));
    work(cx);
    assert_eq!(setting(&app, "flickr.api_key").as_deref(), Some("my-key"));
    assert_eq!(setting(&app, "flickr.request_token").as_deref(), Some("req-tok"));
    view.read_with(cx, |v, _| assert!(v.auth_url.as_deref().unwrap().starts_with("https://flickr.example/authorize")));

    let verify = |code: &'static str, cx: &mut TestAppContext| {
        cx.update_window(app.window(), |_, window, cx| view.update(cx, |v, cx| v.verifier.update(cx, |i, cx| i.set_value(code, window, cx)))).unwrap();
        view.update(cx, |v, cx| v.finish(cx));
        work(cx);
    };
    verify("000", cx);
    view.read_with(cx, |v, _| {
        assert!(!v.connected);
        assert!(v.status.contains("verifier_invalid"), "{}", v.status);
    });
    verify(" 123-456-789 ", cx);
    view.read_with(cx, |v, _| {
        assert!(v.connected);
        assert_eq!(v.status, "Connected.");
        for secret in ["my-secret", "acc-tok", "acc-sec", "req-sec"] {
            assert!(!v.status.contains(secret));
        }
    });
    assert_eq!(setting(&app, "flickr.access_token").as_deref(), Some("acc-tok"));
    assert_eq!(setting(&app, "flickr.user_nsid").as_deref(), Some("99@N01"));
    assert_eq!(setting(&app, "flickr.request_token").as_deref(), Some(""), "the request token outlived Finish");
}

/// Publish through the contributed target: the photo is rendered for real (a JPEG), uploaded
/// with the form's title and tags, and recorded as a `flickr` publication with its canonical
/// page URL. Not connected: it says so before rendering anything.
#[gpui_kit::test]
fn publish_uploads_a_render_and_records_a_flickr_publication(cx: &mut TestAppContext) {
    let dir = TempDir::new("flickr-publish");
    let app = start(cx);
    let ids = with_files(&app, &dir, 1, cx);
    work(cx);
    select(&app, ids[0], cx);
    let api = Arc::new(Fake::default());
    let loaded = load(&app, api.clone(), cx);
    assert_eq!(loaded.contributions.publish_targets[0].label.as_ref(), "Flickr");

    let view: Entity<PublishPanel> = open(&app, &loaded.contributions.publish_targets[0].view, cx);
    view.update(cx, |p, cx| p.publish(cx));
    work(cx);
    view.read_with(cx, |p, _| assert_eq!(p.status, "Enter your flickr API key and secret in the module settings first."));
    assert!(api.uploads.lock().unwrap().is_empty());

    connect(&app);
    cx.update_window(app.window(), |_, window, cx| {
        view.update(cx, |p, cx| {
            p.title.update(cx, |i, cx| i.set_value("Aurora", window, cx));
            p.tags.update(cx, |t, cx| t.set_value("sky \"northern lights\"", window, cx));
        })
    })
    .unwrap();
    view.update(cx, |p, cx| p.publish(cx));
    work(cx);
    view.read_with(cx, |p, _| assert_eq!(p.status, "Published to Flickr ✓"));
    let uploads = api.uploads.lock().unwrap().clone();
    assert_eq!((uploads[0].0.as_str(), uploads[0].2.as_str()), ("Aurora", "sky \"northern lights\""));
    let pubs = publications(&app, ids[0]);
    assert_eq!(pubs.len(), 1);
    assert_eq!((pubs[0].platform.as_str(), pubs[0].url.as_deref()), ("flickr", Some("https://www.flickr.com/photos/99@N01/5001/")));
}

fn photo(id: &str, title: &str) -> FlickrPhoto {
    FlickrPhoto {
        id: id.into(),
        owner: "99@N01".into(),
        title: title.into(),
        date_taken: String::new(),
        date_upload_unix: 1_500_000_000 + id.parse::<i64>().unwrap(),
        thumb_url: Some(format!("https://live.staticflickr.com/65535/{id}_s.jpg")),
    }
}

fn import_panel(app: &App, api: Arc<Fake>, cx: &mut TestAppContext) -> Entity<ImportPublishedPanel> {
    let loaded = load(app, api, cx);
    open(app, &loaded.contributions.settings[1].view, cx)
}

/// Preview → counts; an ambiguous photo resolved by clicking a candidate (Undo takes it back,
/// a second click re-resolves); Import records every match and resolution with Flickr's
/// upload date — into the catalog the preview matched against.
#[gpui_kit::test]
fn the_import_previews_resolves_and_records(cx: &mut TestAppContext) {
    let dir = TempDir::new("flickr-import");
    let app = start(cx);
    let ids = with_files(&app, &dir, 2, cx);
    work(cx);
    connect(&app);
    // Two raw files sharing the stem "q1" in different folders: Flickr's "q1" matches both.
    let (q1a, q1b) = {
        let guard = app.state.catalog.lock().unwrap();
        let c = guard.as_ref().unwrap();
        let root = dir.0.join("photos");
        let (a, b) = (c.upsert_photo(&root.join("2027/q1.ARW"), None, 0, 6).unwrap().id, c.upsert_photo(&root.join("2028/q1.ARW"), None, 0, 6).unwrap().id);
        // Taken at different seconds, so they are two photos, not two copies of one.
        c.conn()
            .execute_batch(&format!(
                "UPDATE photos SET capture_time = '2020-01-01T10:00:00' WHERE id = {a};
                 UPDATE photos SET capture_time = '2020-01-01T10:00:05' WHERE id = {b};"
            ))
            .unwrap();
        (a, b)
    };
    let api = Arc::new(Fake::default());
    *api.stream.lock().unwrap() = vec![photo("1", "p0"), photo("2", "q1"), photo("7", "nothing")];
    let view = import_panel(&app, api.clone(), cx);
    view.update(cx, |p, cx| p.preview(cx));
    work(cx);
    view.read_with(cx, |p, _| {
        assert_eq!(p.status, "1 matched · 1 ambiguous · 1 not in catalog");
        let (_, preview) = p.preview.as_ref().unwrap();
        assert_eq!(preview.ambiguous[0].candidates.len(), 2);
    });
    assert_eq!(api.thumbs_asked.lock().unwrap().len(), 2, "Flickr thumbnails for what the preview shows");
    view.update(cx, |p, cx| p.resolve("2", q1a, cx));
    view.update(cx, |p, cx| p.unresolve("2", cx));
    assert!(view.read_with(cx, |p, _| p.resolved.is_empty()));
    view.update(cx, |p, cx| p.resolve("2", ids[0], cx)); // not one of its candidates: ignored
    assert!(view.read_with(cx, |p, _| p.resolved.is_empty()));
    view.update(cx, |p, cx| p.resolve("2", q1b, cx));
    assert_eq!(view.read_with(cx, |p, _| p.full_plan().len()), 2);
    view.update(cx, |p, cx| p.apply(cx));
    work(cx);
    view.read_with(cx, |p, _| {
        assert_eq!(p.status, "Done — 2 publications recorded.");
        assert!(p.preview.is_none());
    });
    let p0 = publications(&app, ids[0]);
    assert_eq!((p0[0].platform.as_str(), p0[0].published_at), ("flickr", 1_500_000_001));
    assert_eq!(publications(&app, q1b)[0].published_at, 1_500_000_002);
    assert!(publications(&app, q1a).is_empty(), "the undone candidate was recorded");
    assert!(publications(&app, ids[1]).is_empty());
}

/// #186: the preview's thumbnails fill their square — the catalog's from the image layer and
/// Flickr's own — with a portrait frame (`flickr.tsx`: `objectFit: "cover"`), not a portrait
/// element taller than the square.
#[gpui_kit::test]
fn the_previews_thumbnails_fill_their_square(cx: &mut TestAppContext) {
    use crate::image_tests::{pixels, FakePool};
    use crate::loupe::fit_tests::{assert_fills, PORTRAIT};
    use chairphoto_core::image_pool::{ImageKind, JobKey};
    use gpui_kit::test::TestWindowExt as _;
    let dir = TempDir::new("flickr-import-fit");
    let pool = Arc::new(FakePool::default());
    let app = crate::tests::start_with_pool(cx, pool.clone());
    let ids = with_files(&app, &dir, 1, cx);
    work(cx);
    connect(&app);
    let api = Arc::new(Fake::default());
    let mut jpeg = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(PORTRAIT.0, PORTRAIT.1).write_to(&mut jpeg, image::ImageFormat::Jpeg).unwrap();
    *api.thumb_bytes.lock().unwrap() = Some(jpeg.into_inner());
    *api.stream.lock().unwrap() = vec![photo("1", "p0")];
    let view = import_panel(&app, api.clone(), cx);
    view.update(cx, |p, cx| p.preview(cx));
    work(cx);
    view.read_with(cx, |p, _| assert_eq!(p.status, "1 matched · 0 ambiguous · 0 not in catalog"));
    pool.finish(&JobKey::photo(ids[0], ImageKind::Thumb), Ok(pixels(PORTRAIT.0, PORTRAIT.1)));
    cx.run_until_parked();
    let url = photo("1", "p0").thumb_url.unwrap();
    for _ in 0..2 {
        cx.update_window(app.window(), |_, window, cx| window.render_frame(cx)).unwrap();
        cx.run_until_parked();
    }
    cx.update_window(app.window(), |_, window, _| {
        assert_fills("catalog thumbnail", window, ("flickr-thumb", ids[0] as u64), ("flickr-thumb-picture", ids[0] as u64), 0.);
        let (cell, picture) = (format!("flickr-remote-{url}"), format!("flickr-remote-picture-{url}"));
        assert_fills("Flickr thumbnail", window, gpui_kit::SharedString::from(cell), gpui_kit::SharedString::from(picture), 0.);
    })
    .unwrap();
}

/// An Import whose catalog was switched under it records nothing in the new one (whose ids
/// collide); `catalog:switched` clears the panel.
#[gpui_kit::test]
fn an_import_after_a_switch_records_nothing(cx: &mut TestAppContext) {
    let dir = TempDir::new("flickr-import-switch");
    let app = start(cx);
    let ids = with_files(&app, &dir, 1, cx);
    work(cx);
    connect(&app);
    let api = Arc::new(Fake::default());
    *api.stream.lock().unwrap() = vec![photo("1", "p0")];
    let view = import_panel(&app, api, cx);
    view.update(cx, |p, cx| p.preview(cx));
    work(cx);
    view.update(cx, |p, cx| p.apply(cx));
    let held = cx.update(|cx| Runner::get(cx).hold_pending());
    let (b, b_ids) = colliding_catalog(&dir, "b", 1);
    assert_eq!(b_ids, ids);
    core_switch(&app, b);
    cx.update(|cx| Runner::get(cx).release(held));
    step(cx);
    view.read_with(cx, |p, _| assert_eq!(p.status, CATALOG_CHANGED));
    assert!(publications(&app, ids[0]).is_empty(), "recorded into the catalog that opened since");
    deliver_switch(&app, cx);
    view.read_with(cx, |p, _| assert!(p.preview.is_none() && p.status.is_empty()));
}
