//! SmugMug publishing (docs/smugmug.md) — the bodies the GPUI SmugMug module runs: sign-in
//! ([`super::oauth`]), the albums, and the upload of a rendered publish job
//! ([`super::uploads`]) into one.
//!
//! The network is a [`SmugMugApi`]: [`LiveSmugMug`] calls `crate::smugmug`; tests hand in a
//! fake. Every function is **blocking**: run it on a worker.
//!
//! **Privacy.** A photo leaves only through [`upload`], on the user's Publish.

use super::oauth::{credentials, AccessToken, Credentials, OAuthApi, RequestToken};
use super::uploads::{long_edge, max_long_edge, RenderedJob, ServiceSettings, UploadJob, UploadRenderer, UploadService};
use super::AppState;
pub use crate::smugmug::Album;
use std::path::Path;

/// The settings namespace, publication marker and job family name.
pub const SERVICE: UploadService = UploadService::SmugMug;
const NAME: &str = "smugmug";

/// The SmugMug network: the token endpoints, the albums and the upload. Blocking.
pub trait SmugMugApi: OAuthApi {
    fn list_albums(&self, creds: &Credentials) -> Result<Vec<Album>, String>;
    fn create_album(&self, creds: &Credentials, name: &str) -> Result<Album, String>;
    /// Upload `image` into `album_uri`; returns the image's URL (or its API URI).
    fn upload(&self, creds: &Credentials, album_uri: &str, image: &Path, title: &str, caption: &str) -> Result<String, String>;
}

/// The real SmugMug API (`crate::smugmug`), run on the core runtime from the calling worker.
pub struct LiveSmugMug;

impl OAuthApi for LiveSmugMug {
    fn request_token(&self, key: &str, secret: &str) -> Result<RequestToken, String> {
        let rt = super::runtime().block_on(crate::smugmug::begin_auth(key, secret))?;
        Ok(RequestToken { token: rt.token, secret: rt.secret, authorize_url: rt.authorize_url })
    }

    fn access_token(&self, key: &str, secret: &str, token: &str, token_secret: &str, verifier: &str) -> Result<AccessToken, String> {
        let at = super::runtime().block_on(crate::smugmug::complete_auth(key, secret, token, token_secret, verifier))?;
        Ok(AccessToken { token: at.token, secret: at.secret, user_nsid: None })
    }
}

impl SmugMugApi for LiveSmugMug {
    fn list_albums(&self, c: &Credentials) -> Result<Vec<Album>, String> {
        super::runtime().block_on(crate::smugmug::list_albums(&c.key, &c.secret, &c.token, &c.token_secret))
    }

    fn create_album(&self, c: &Credentials, name: &str) -> Result<Album, String> {
        super::runtime().block_on(crate::smugmug::create_album(&c.key, &c.secret, &c.token, &c.token_secret, name))
    }

    fn upload(&self, c: &Credentials, album_uri: &str, image: &Path, title: &str, caption: &str) -> Result<String, String> {
        super::runtime().block_on(crate::smugmug::upload(&c.key, &c.secret, &c.token, &c.token_secret, album_uri, image, title, caption))
    }
}

/// The user's albums (upload targets).
pub fn list_albums(api: &dyn SmugMugApi, settings: &dyn ServiceSettings) -> Result<Vec<Album>, String> {
    api.list_albums(&credentials(settings, NAME)?)
}

/// Create an album under the user's root folder.
pub fn create_album(api: &dyn SmugMugApi, settings: &dyn ServiceSettings, name: &str) -> Result<Album, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Enter an album name.".into());
    }
    api.create_album(&credentials(settings, NAME)?, name)
}

/// What a publish without an album answers.
pub const CHOOSE_ALBUM: &str = "Choose a SmugMug album to upload into.";

/// Render a claimed publish at the user's max long edge (full size by default).
pub fn render(settings: &dyn ServiceSettings, job: UploadJob) -> Result<RenderedJob, String> {
    job.render(&long_edge(max_long_edge(settings)))
}

/// Upload a rendered publish into `album_uri`; returns the image URL. Refuses, uploading
/// nothing, without an album, when the job was cancelled or stopped by a catalog switch, or
/// when the service
/// is not connected.
pub fn upload(
    api: &dyn SmugMugApi,
    settings: &dyn ServiceSettings,
    rendered: &RenderedJob,
    album_uri: &str,
    title: &str,
    caption: &str,
) -> Result<String, String> {
    if album_uri.trim().is_empty() {
        return Err(CHOOSE_ALBUM.into());
    }
    let creds = credentials(settings, NAME)?;
    rendered.ensure_live()?;
    api.upload(&creds, album_uri, rendered.path(), title, caption)
}

/// The whole publish of `photo_id` from whichever catalog is open, for a caller with no steps
/// to show: check the album and the connection, then claim, render, upload — so a call that
/// cannot publish claims nothing. Another publish running meanwhile is neither stopped nor
/// stops this one.
#[allow(clippy::too_many_arguments)]
pub fn post(
    api: &dyn SmugMugApi,
    settings: &dyn ServiceSettings,
    state: &AppState,
    photo_id: i64,
    version_id: Option<i64>,
    album_uri: &str,
    title: &str,
    caption: &str,
) -> Result<String, String> {
    post_with(api, settings, state, photo_id, version_id, &long_edge(max_long_edge(settings)), album_uri, title, caption)
}

/// [`post`] with the renderer handed in (tests need no thumbnail cache).
#[allow(clippy::too_many_arguments)]
fn post_with(
    api: &dyn SmugMugApi,
    settings: &dyn ServiceSettings,
    state: &AppState,
    photo_id: i64,
    version_id: Option<i64>,
    renderer: &UploadRenderer,
    album_uri: &str,
    title: &str,
    caption: &str,
) -> Result<String, String> {
    if album_uri.trim().is_empty() {
        return Err(CHOOSE_ALBUM.into());
    }
    credentials(settings, NAME)?;
    let job = super::uploads::claim_upload(state, None, SERVICE, photo_id, version_id)?;
    let rendered = job.render(renderer)?;
    upload(api, settings, &rendered, album_uri, title, caption)
}

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use crate::app::oauth::fake::FakeTokens;
    use std::sync::Mutex;

    /// A SmugMug that answers from memory.
    #[derive(Default)]
    pub struct FakeSmugMug {
        pub tokens: FakeTokens,
        pub albums: Mutex<Vec<Album>>,
        /// (album uri, title, caption, bytes)
        pub uploads: Mutex<Vec<(String, String, String, Vec<u8>)>>,
    }

    impl OAuthApi for FakeSmugMug {
        fn request_token(&self, key: &str, secret: &str) -> Result<RequestToken, String> {
            self.tokens.request_token(key, secret)
        }
        fn access_token(&self, key: &str, secret: &str, t: &str, ts: &str, v: &str) -> Result<AccessToken, String> {
            self.tokens.access_token(key, secret, t, ts, v)
        }
    }

    impl SmugMugApi for FakeSmugMug {
        fn list_albums(&self, _: &Credentials) -> Result<Vec<Album>, String> {
            Ok(self.albums.lock().unwrap().clone())
        }
        fn create_album(&self, _: &Credentials, name: &str) -> Result<Album, String> {
            let album = Album { uri: format!("/api/v2/album/{}", name.to_lowercase()), name: name.into() };
            self.albums.lock().unwrap().insert(0, album.clone());
            Ok(album)
        }
        fn upload(&self, _: &Credentials, album_uri: &str, image: &Path, title: &str, caption: &str) -> Result<String, String> {
            let bytes = std::fs::read(image).map_err(|e| e.to_string())?;
            let mut uploads = self.uploads.lock().unwrap();
            uploads.push((album_uri.into(), title.into(), caption.into(), bytes));
            Ok(format!("https://me.smugmug.com/i-{}", uploads.len()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeSmugMug;
    use super::*;
    use crate::app::oauth::fake::MemSettings;
    use crate::app::oauth::{ACCESS_SECRET, ACCESS_TOKEN, API_KEY, API_SECRET};
    use crate::app::uploads::{claim_upload, UPLOAD_CANCELLED};
    use crate::test_support::TestTmpDir;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    fn connected() -> MemSettings {
        MemSettings::with(&[(API_KEY, "k"), (API_SECRET, "s"), (ACCESS_TOKEN, "access-tok"), (ACCESS_SECRET, "access-sec")])
    }

    fn catalog(dir: &Path) -> (AppState, i64) {
        let root = dir.join("library");
        std::fs::create_dir_all(&root).unwrap();
        let state = AppState::default();
        let c = crate::catalog::Catalog::open(&dir.join("c.chairphoto"), &root).unwrap();
        let p = root.join("IMG_1.jpg");
        std::fs::write(&p, b"jpeg").unwrap();
        let id = c.upsert_photo(&p, None, 0, 6).unwrap().id;
        *state.catalog.lock().unwrap() = Some(c);
        (state, id)
    }

    #[test]
    fn albums_and_an_upload_into_one() {
        let dir = TestTmpDir::new("smugmug-upload");
        let (state, id) = catalog(&dir);
        let api = FakeSmugMug::default();
        let s = connected();
        assert_eq!(create_album(&api, &s, "  ").unwrap_err(), "Enter an album name.");
        let trips = create_album(&api, &s, " Trips ").unwrap();
        assert_eq!(trips.name, "Trips");
        assert_eq!(list_albums(&api, &s).unwrap().len(), 1);
        let job = claim_upload(&state, None, SERVICE, id, None).unwrap();
        let abort = job.abort_handle();
        let render: crate::app::uploads::UploadRenderer = Arc::new(|_, out| std::fs::write(out, b"px").map_err(|e| e.to_string()));
        let rendered = job.render(&render).unwrap();
        assert_eq!(upload(&api, &s, &rendered, " ", "t", "c").unwrap_err(), CHOOSE_ALBUM);
        let url = upload(&api, &s, &rendered, &trips.uri, "Title", "Caption").unwrap();
        assert_eq!(url, "https://me.smugmug.com/i-1");
        assert_eq!(api.uploads.lock().unwrap()[0].0, trips.uri);
        abort.store(true, Ordering::Relaxed);
        assert_eq!(upload(&api, &s, &rendered, &trips.uri, "", "").unwrap_err(), UPLOAD_CANCELLED);
        assert_eq!(api.uploads.lock().unwrap().len(), 1, "a cancelled job uploaded");
        assert!(list_albums(&api, &MemSettings::default()).is_err(), "unconfigured");
    }

    /// `post()`'s path checks the album and the connection before it claims
    /// anything: a call that cannot publish takes no job id and leaves a running publish be;
    /// two posts both upload.
    #[test]
    fn a_post_that_cannot_publish_claims_nothing_and_posts_run_side_by_side() {
        let dir = TestTmpDir::new("smugmug-post");
        let (state, id) = catalog(&dir);
        let api = FakeSmugMug::default();
        let running = claim_upload(&state, None, SERVICE, id, None).unwrap();
        let issued = state.jobs.upload_smugmug.job_ids_issued();
        let render: UploadRenderer = Arc::new(|_, out| std::fs::write(out, b"px").map_err(|e| e.to_string()));
        assert_eq!(post_with(&api, &connected(), &state, id, None, &render, " ", "", "").unwrap_err(), CHOOSE_ALBUM);
        let unconnected = MemSettings::with(&[(API_KEY, "k"), (API_SECRET, "s")]);
        assert!(post_with(&api, &unconnected, &state, id, None, &render, "/a/1", "", "").unwrap_err().contains("Connect smugmug"));
        assert_eq!(state.jobs.upload_smugmug.job_ids_issued(), issued, "a post that cannot publish claimed a job");
        let running = running.render(&render).expect("a failing post stopped a running publish");
        assert_eq!(post_with(&api, &connected(), &state, id, None, &render, "/a/1", "second", "").unwrap(), "https://me.smugmug.com/i-1");
        assert_eq!(upload(&api, &connected(), &running, "/a/1", "first", "").unwrap(), "https://me.smugmug.com/i-2");
    }
}
