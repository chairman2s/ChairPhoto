//! Headless tests of the SmugMug module through its real contributions, over a fake
//! [`SmugMugApi`] (no network, no account): the album picker (fetched once, cached, created),
//! and a publish into the chosen album that records a `smugmug` publication with its URL.

use super::SmugMugModule;
use crate::modules::publishing::panel::{PublishPanel, ALBUMS_CACHE, LAST_ALBUM};
use crate::modules::publishing::tests::{publications, select, setting, with_files, work};
use crate::modules::{Module, ModuleHost, RestoredCatalog};
use crate::tests::{start, App, TempDir};
use chairphoto_core::app::oauth::{AccessToken, Credentials, OAuthApi, RequestToken};
use chairphoto_core::app::smugmug::{Album, SmugMugApi};
use gpui_kit::component::WindowExt as _;
use gpui_kit::prelude::*;
use gpui_kit::{AnyView, Entity, TestAppContext};
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Fake {
    listed: Mutex<usize>,
    albums: Mutex<Vec<Album>>,
    /// (album, title, caption) per upload.
    uploads: Mutex<Vec<(String, String, String)>>,
}

impl OAuthApi for Fake {
    fn request_token(&self, _: &str, _: &str) -> Result<RequestToken, String> {
        Err("not in this test".into())
    }
    fn access_token(&self, _: &str, _: &str, _: &str, _: &str, _: &str) -> Result<AccessToken, String> {
        Err("not in this test".into())
    }
}

impl SmugMugApi for Fake {
    fn list_albums(&self, _: &Credentials) -> Result<Vec<Album>, String> {
        *self.listed.lock().unwrap() += 1;
        Ok(self.albums.lock().unwrap().clone())
    }
    fn create_album(&self, _: &Credentials, name: &str) -> Result<Album, String> {
        Ok(Album { uri: format!("/api/v2/album/{}", name.to_lowercase()), name: name.into() })
    }
    fn upload(&self, c: &Credentials, album_uri: &str, image: &Path, title: &str, caption: &str) -> Result<String, String> {
        assert_eq!(c.token, "acc-tok");
        assert!(std::fs::read(image).unwrap().starts_with(&[0xFF, 0xD8]), "the upload is a rendered JPEG");
        self.uploads.lock().unwrap().push((album_uri.into(), title.into(), caption.into()));
        Ok("https://me.smugmug.com/Trips/i-abc".into())
    }
}

fn panel(app: &App, api: Arc<Fake>, cx: &mut TestAppContext) -> Entity<PublishPanel> {
    let module = SmugMugModule { api };
    let restored = RestoredCatalog::default();
    restored.set(Some(chairphoto_core::app::catalog_identity(&app.state).unwrap()));
    let (model, shell) = (app.wired.model.clone(), app.wired.shell.clone());
    let host = cx.update(|_| ModuleHost::new(module.meta(), app.state.clone(), restored, model, shell));
    let instance = cx.update(|cx| module.load(host, cx)).ok().unwrap();
    let c = instance.contributions();
    assert_eq!(c.publish_targets[0].label.as_ref(), "SmugMug");
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
    view.downcast::<PublishPanel>().ok().unwrap()
}

/// The albums are fetched once and cached; "+ New" creates and chooses one; Publish uploads
/// the render into it (title, and the description as the caption) and records a `smugmug`
/// publication with the image URL.
#[gpui_kit::test]
fn publish_into_a_chosen_album_records_a_smugmug_publication(cx: &mut TestAppContext) {
    let dir = TempDir::new("smugmug-publish");
    let app = start(cx);
    let ids = with_files(&app, &dir, 1, cx);
    work(cx);
    for (k, v) in [("smugmug.api_key", "k"), ("smugmug.api_secret", "s"), ("smugmug.access_token", "acc-tok"), ("smugmug.access_secret", "acc-sec")] {
        app.state.catalog.lock().unwrap().as_ref().unwrap().set_setting(k, v).unwrap();
    }
    select(&app, ids[0], cx);
    let api = Arc::new(Fake::default());
    *api.albums.lock().unwrap() = vec![Album { uri: "/api/v2/album/a".into(), name: "Birds".into() }];
    let view = panel(&app, api.clone(), cx);
    view.read_with(cx, |p, _| assert_eq!(p.album, "/api/v2/album/a"));
    assert!(setting(&app, &format!("smugmug.{ALBUMS_CACHE}")).unwrap().contains("Birds"));
    cx.update_window(app.window(), |_, window, cx| {
        view.update(cx, |p, cx| {
            p.new_album.update(cx, |i, cx| i.set_value("Trips", window, cx));
            p.title.update(cx, |i, cx| i.set_value("Fjord", window, cx));
            p.description.update(cx, |t, cx| t.set_value("Evening light", window, cx));
        })
    })
    .unwrap();
    view.update(cx, |p, cx| p.create_album(cx));
    work(cx);
    assert_eq!(setting(&app, &format!("smugmug.{LAST_ALBUM}")).as_deref(), Some("/api/v2/album/trips"));
    view.update(cx, |p, cx| p.publish(cx));
    work(cx);
    view.read_with(cx, |p, _| assert_eq!(p.status, "Published to SmugMug ✓"));
    assert_eq!(api.uploads.lock().unwrap()[0], ("/api/v2/album/trips".into(), "Fjord".into(), "Evening light".into()));
    let pubs = publications(&app, ids[0]);
    assert_eq!((pubs[0].platform.as_str(), pubs[0].url.as_deref()), ("smugmug", Some("https://me.smugmug.com/Trips/i-abc")));
    // A second form opens on the cached list.
    panel(&app, api.clone(), cx);
    assert_eq!(*api.listed.lock().unwrap(), 1, "the album list was fetched again");
}
