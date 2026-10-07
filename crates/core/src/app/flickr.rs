//! Flickr publishing (docs/flickr.md) — the bodies the GPUI Flickr module runs: sign-in
//! ([`super::oauth`]), the tags prefill, the upload of a rendered publish job
//! ([`super::uploads`]), and importing the photostream as publications.
//!
//! The network is a [`FlickrApi`]: [`LiveFlickr`] calls `crate::flickr`; tests hand in a fake.
//! Every function is **blocking**: run it on a worker.
//!
//! **Privacy.** A photo leaves only through [`upload`], on the user's Publish. The importer only
//! reads the user's own photostream, after they asked for a preview, and never writes to Flickr.

use super::oauth::{credentials, AccessToken, Credentials, OAuthApi, RequestToken, USER_NSID};
use super::uploads::{long_edge, max_long_edge, setting, RenderedJob, ServiceSettings, UploadJob, UploadRenderer, UploadService};
use super::{with_catalog, with_catalog_as, AppState, CatalogIdentity};
use crate::flickr::{ExistingPublication, FlickrPhoto, MatchOutcome};
use std::path::Path;

/// The settings namespace, publication marker and job family name.
pub const SERVICE: UploadService = UploadService::Flickr;
const NAME: &str = "flickr";

/// The Flickr network: the token endpoints, the upload and the photostream. Blocking.
pub trait FlickrApi: OAuthApi {
    /// Upload `image` to the photostream; returns the new Flickr photo id.
    fn upload(&self, creds: &Credentials, image: &Path, title: &str, description: &str, tags: &str) -> Result<String, String>;
    /// Every photo in the user's photostream.
    fn photostream(&self, creds: &Credentials) -> Result<Vec<FlickrPhoto>, String>;
    /// A preview's small Flickr thumbnail (`url_s`), as JPEG bytes ([`fetch_thumb`]).
    fn thumbnail(&self, url: &str) -> Result<Vec<u8>, String>;
}

/// The real Flickr API (`crate::flickr`), run on the core runtime from the calling worker.
pub struct LiveFlickr;

impl OAuthApi for LiveFlickr {
    fn request_token(&self, key: &str, secret: &str) -> Result<RequestToken, String> {
        let rt = super::runtime().block_on(crate::flickr::begin_auth(key, secret))?;
        Ok(RequestToken { token: rt.token, secret: rt.secret, authorize_url: rt.authorize_url })
    }

    fn access_token(&self, key: &str, secret: &str, token: &str, token_secret: &str, verifier: &str) -> Result<AccessToken, String> {
        let at = super::runtime().block_on(crate::flickr::complete_auth(key, secret, token, token_secret, verifier))?;
        Ok(AccessToken { token: at.token, secret: at.secret, user_nsid: at.user_nsid })
    }
}

impl FlickrApi for LiveFlickr {
    fn upload(&self, c: &Credentials, image: &Path, title: &str, description: &str, tags: &str) -> Result<String, String> {
        super::runtime().block_on(crate::flickr::upload(&c.key, &c.secret, &c.token, &c.token_secret, image, title, description, tags))
    }

    fn photostream(&self, c: &Credentials) -> Result<Vec<FlickrPhoto>, String> {
        super::runtime().block_on(crate::flickr::fetch_photostream(&c.key, &c.secret, &c.token, &c.token_secret))
    }

    fn thumbnail(&self, url: &str) -> Result<Vec<u8>, String> {
        fetch_thumb(url)
    }
}

/// The photo's page URL: canonical `/photos/<nsid>/<id>/` when the NSID is known, else the
/// `photo.gne?id=` redirect form — both carry the photo id the importer's signal P reads.
pub fn page_url(nsid: &str, flickr_photo_id: &str) -> String {
    if nsid.is_empty() {
        format!("https://www.flickr.com/photo.gne?id={flickr_photo_id}")
    } else {
        format!("https://www.flickr.com/photos/{nsid}/{flickr_photo_id}/")
    }
}

/// The Tags prefill: the photo's export keywords (ancestors and export synonyms included) in
/// Flickr's `tags` format. `from`: the catalog the id was read from (`None` = the open one).
pub fn suggest_tags(state: &AppState, from: Option<CatalogIdentity>, photo_id: i64) -> Result<String, String> {
    let read = |c: &crate::catalog::Catalog| {
        let keywords = c.assemble_export_keywords(photo_id, &[]).map(|k| k.flat).unwrap_or_default();
        Ok(crate::flickr::format_tags(&keywords))
    };
    match from {
        Some(f) => with_catalog_as(state, f, read),
        None => with_catalog(state, read),
    }
}

/// Render a claimed publish at the user's max long edge (full size by default).
pub fn render(settings: &dyn ServiceSettings, job: UploadJob) -> Result<RenderedJob, String> {
    job.render(&long_edge(max_long_edge(settings)))
}

/// Upload a rendered publish; returns the photo's page URL. Refuses, uploading nothing, when
/// the job was cancelled or stopped by a catalog switch, or the service is not connected.
pub fn upload(
    api: &dyn FlickrApi,
    settings: &dyn ServiceSettings,
    rendered: &RenderedJob,
    title: &str,
    description: &str,
    tags: &str,
) -> Result<String, String> {
    let creds = credentials(settings, NAME)?;
    rendered.ensure_live()?;
    let id = api.upload(&creds, rendered.path(), title, description, tags)?;
    Ok(page_url(&setting(settings, USER_NSID).unwrap_or_default(), &id))
}

/// The whole publish of `photo_id` from whichever catalog is open, for a caller with no steps
/// to show: check the connection, then claim, render, upload — so a call that cannot
/// publish claims nothing. The caller records the publication. Another publish running
/// meanwhile is neither stopped nor stops this one.
#[allow(clippy::too_many_arguments)]
pub fn post(
    api: &dyn FlickrApi,
    settings: &dyn ServiceSettings,
    state: &AppState,
    photo_id: i64,
    version_id: Option<i64>,
    title: &str,
    description: &str,
    tags: &str,
) -> Result<String, String> {
    post_with(api, settings, state, photo_id, version_id, &long_edge(max_long_edge(settings)), title, description, tags)
}

/// [`post`] with the renderer handed in (tests need no thumbnail cache).
#[allow(clippy::too_many_arguments)]
fn post_with(
    api: &dyn FlickrApi,
    settings: &dyn ServiceSettings,
    state: &AppState,
    photo_id: i64,
    version_id: Option<i64>,
    renderer: &UploadRenderer,
    title: &str,
    description: &str,
    tags: &str,
) -> Result<String, String> {
    credentials(settings, NAME)?;
    let job = super::uploads::claim_upload(state, None, SERVICE, photo_id, version_id)?;
    let rendered = job.render(renderer)?;
    upload(api, settings, &rendered, title, description, tags)
}

// ── Importing the photostream as publications ─────────────────────────────────

/// One matched photo in the import preview.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlickrImportMatch {
    pub catalog_id: i64,
    pub catalog_path: String,
    pub flickr_id: String,
    pub flickr_url: String,
    /// Unix timestamp of the Flickr upload (the real historical date).
    pub published_at: i64,
    /// Small (240 px) Flickr thumbnail URL (`url_s`); absent on some photos.
    pub thumb_url: Option<String>,
}

/// One catalog candidate of an ambiguous match.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlickrImportCandidate {
    pub catalog_id: i64,
    pub catalog_path: String,
    /// "YYYY-MM-DDTHH:MM:SS", or `None`.
    pub capture_time: Option<String>,
}

/// A Flickr photo with too many or conflicting catalog candidates.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FlickrImportAmbiguous {
    pub flickr_id: String,
    pub flickr_url: String,
    pub published_at: i64,
    pub title: String,
    pub reason: String,
    pub thumb_url: Option<String>,
    /// Up to 10, for resolving by hand.
    pub candidates: Vec<FlickrImportCandidate>,
}

impl FlickrImportAmbiguous {
    /// The plan entry for resolving this photo to `candidate`.
    pub fn resolve(&self, candidate: &FlickrImportCandidate) -> FlickrImportMatch {
        FlickrImportMatch {
            catalog_id: candidate.catalog_id,
            catalog_path: candidate.catalog_path.clone(),
            flickr_id: self.flickr_id.clone(),
            flickr_url: self.flickr_url.clone(),
            published_at: self.published_at,
            thumb_url: self.thumb_url.clone(),
        }
    }
}

/// The dry-run preview: counts, up to [`DISPLAY_CAP`] matches and ambiguous photos to show,
/// and the full, uncapped `plan` to hand back to [`import_apply`].
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FlickrImportResult {
    pub matched_count: usize,
    pub ambiguous_count: usize,
    pub unmatched_count: usize,
    pub matches: Vec<FlickrImportMatch>,
    pub plan: Vec<FlickrImportMatch>,
    pub ambiguous: Vec<FlickrImportAmbiguous>,
}

/// How many matches and ambiguous photos a preview shows.
pub const DISPLAY_CAP: usize = 50;

/// Fetch the photostream and match it against the catalog — read-only toward Flickr, and
/// nothing is written to the catalog. `from`: the catalog to match against (`None` = the
/// open one); fails closed with `CATALOG_CHANGED` once another is open.
pub fn import_preview(
    api: &dyn FlickrApi,
    settings: &dyn ServiceSettings,
    state: &AppState,
    from: Option<CatalogIdentity>,
    marker: &str,
) -> Result<FlickrImportResult, String> {
    let creds = credentials(settings, NAME)?;
    let photos = api.photostream(&creds)?;
    // The catalog rows and this marker's recorded publications, from one lock hold: photos
    // ChairPhoto uploaded (or imported) match by the id in their URL (signal P).
    let read = |c: &crate::catalog::Catalog| Ok((c.photos_for_flickr_match()?, c.publications_for_platform(marker)?));
    let (rows, existing) = match from {
        Some(f) => with_catalog_as(state, f, read)?,
        None => with_catalog(state, read)?,
    };
    let existing: Vec<ExistingPublication> = existing
        .into_iter()
        .map(|(photo_id, url, published_at)| ExistingPublication {
            catalog_id: photo_id,
            flickr_id: url.as_deref().and_then(crate::flickr::flickr_id_from_url),
            published_at,
        })
        .collect();
    let mut plan = Vec::new();
    let mut ambiguous = Vec::new();
    let mut unmatched_count = 0;
    for outcome in crate::flickr::match_photos(&photos, &rows, &existing) {
        match outcome {
            MatchOutcome::Matched { flickr, catalog_id, catalog_path } => plan.push(FlickrImportMatch {
                catalog_id,
                catalog_path,
                flickr_url: flickr.page_url(),
                flickr_id: flickr.id,
                published_at: flickr.date_upload_unix,
                thumb_url: flickr.thumb_url,
            }),
            MatchOutcome::Ambiguous { flickr, reason, candidates } => ambiguous.push(FlickrImportAmbiguous {
                flickr_url: flickr.page_url(),
                flickr_id: flickr.id,
                published_at: flickr.date_upload_unix,
                title: flickr.title,
                reason,
                thumb_url: flickr.thumb_url,
                candidates: candidates
                    .into_iter()
                    .map(|c| FlickrImportCandidate { catalog_id: c.catalog_id, catalog_path: c.catalog_path, capture_time: c.capture_time })
                    .collect(),
            }),
            MatchOutcome::Unmatched { .. } => unmatched_count += 1,
        }
    }
    let (matched_count, ambiguous_count) = (plan.len(), ambiguous.len());
    let matches = plan.iter().take(DISPLAY_CAP).cloned().collect();
    ambiguous.truncate(DISPLAY_CAP);
    Ok(FlickrImportResult { matched_count, ambiguous_count, unmatched_count, matches, plan, ambiguous })
}

/// What [`import_apply`] recorded: how many publications, and why each other one was not.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ImportApplied {
    pub applied: usize,
    pub failed: Vec<String>,
}

/// Record each plan entry as a `marker` publication of the Original, with Flickr's upload
/// date and page URL — every entry on its own (a photo deleted since the preview fails alone,
/// the rest are recorded), under one catalog lock bound to `from` (`None` = the open one).
pub fn import_apply(state: &AppState, from: Option<CatalogIdentity>, marker: &str, plan: &[FlickrImportMatch]) -> Result<ImportApplied, String> {
    let apply = |c: &crate::catalog::Catalog| {
        let mut out = ImportApplied::default();
        for m in plan {
            match c.record_publication_historical(m.catalog_id, marker, &m.flickr_url, m.published_at) {
                Ok(_) => out.applied += 1,
                Err(e) => out.failed.push(format!("{}: {e}", m.catalog_path)),
            }
        }
        Ok(out)
    };
    match from {
        Some(f) => with_catalog_as(state, f, apply),
        None => with_catalog(state, apply),
    }
}

/// The most a preview thumbnail may weigh. `url_s` is Flickr's 240 px size, a few tens of KB;
/// anything far larger is not a thumbnail.
pub const THUMB_MAX_BYTES: usize = 1024 * 1024;

/// `url` as a Flickr static-image URL: HTTPS, no user info, no explicit port, and a domain
/// (not an IP address) that is `staticflickr.com` or ends in `.staticflickr.com` — judged on
/// the parsed URL, so `#`, `?`, `\` or `@` in the authority cannot smuggle another host past
/// the check. `None` for anything else.
pub fn flickr_thumb_url(url: &str) -> Option<reqwest::Url> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let host_ok = parsed.domain().is_some_and(|h| h == "staticflickr.com" || h.ends_with(".staticflickr.com"));
    let ok = parsed.scheme() == "https" && parsed.username().is_empty() && parsed.password().is_none() && parsed.port().is_none() && host_ok;
    ok.then_some(parsed)
}

/// Fetch a preview thumbnail (`url_s`) from Flickr's static image hosts — only those
/// ([`flickr_thumb_url`]), never following a redirect, at most [`THUMB_MAX_BYTES`], and only
/// for a preview the user asked for. Blocking.
pub fn fetch_thumb(url: &str) -> Result<Vec<u8>, String> {
    let url = flickr_thumb_url(url).ok_or("not a Flickr image URL")?;
    super::runtime().block_on(fetch_capped(url, THUMB_MAX_BYTES))
}

/// GET `url` without following redirects (a redirect is an error: it would leave the checked
/// host), refusing a body over `max` bytes, announced or streamed.
async fn fetch_capped(url: reqwest::Url, max: usize) -> Result<Vec<u8>, String> {
    let failed = |e: reqwest::Error| format!("Flickr thumbnail failed: {}", e.without_url());
    let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().map_err(failed)?;
    let mut resp = client.get(url).send().await.map_err(failed)?;
    if !resp.status().is_success() {
        return Err(format!("Flickr thumbnail failed: HTTP {}", resp.status()));
    }
    let too_large = || format!("Flickr thumbnail failed: larger than {max} bytes");
    if resp.content_length().is_some_and(|n| n > max as u64) {
        return Err(too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(failed)? {
        if body.len() + chunk.len() > max {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use crate::app::oauth::fake::FakeTokens;
    use std::sync::Mutex;

    /// A Flickr that answers from memory: uploads get ids `9001`, `9002`…; the photostream is
    /// whatever the test put there.
    #[derive(Default)]
    pub struct FakeFlickr {
        pub tokens: FakeTokens,
        pub uploads: Mutex<Vec<(String, String, String, Vec<u8>)>>,
        pub stream: Mutex<Vec<FlickrPhoto>>,
        pub fail_upload: Mutex<Option<String>>,
    }

    impl OAuthApi for FakeFlickr {
        fn request_token(&self, key: &str, secret: &str) -> Result<RequestToken, String> {
            self.tokens.request_token(key, secret)
        }
        fn access_token(&self, key: &str, secret: &str, t: &str, ts: &str, v: &str) -> Result<AccessToken, String> {
            self.tokens.access_token(key, secret, t, ts, v)
        }
    }

    impl FlickrApi for FakeFlickr {
        fn upload(&self, c: &Credentials, image: &Path, title: &str, description: &str, tags: &str) -> Result<String, String> {
            assert_eq!(c.token, "access-tok", "uploads with the stored access token");
            if let Some(e) = self.fail_upload.lock().unwrap().clone() {
                return Err(e);
            }
            let bytes = std::fs::read(image).map_err(|e| e.to_string())?;
            let mut uploads = self.uploads.lock().unwrap();
            uploads.push((title.into(), description.into(), tags.into(), bytes));
            Ok(format!("{}", 9000 + uploads.len()))
        }
        fn photostream(&self, _: &Credentials) -> Result<Vec<FlickrPhoto>, String> {
            Ok(self.stream.lock().unwrap().clone())
        }
        fn thumbnail(&self, _: &str) -> Result<Vec<u8>, String> {
            Err("no thumbnails in tests".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeFlickr;
    use super::*;
    use crate::app::oauth::fake::MemSettings;
    use crate::app::oauth::{ACCESS_SECRET, ACCESS_TOKEN, API_KEY, API_SECRET};
    use crate::app::uploads::{claim_upload, UPLOAD_CANCELLED};
    use crate::app::CATALOG_CHANGED;
    use crate::test_support::TestTmpDir;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    fn connected() -> MemSettings {
        MemSettings::with(&[(API_KEY, "k"), (API_SECRET, "s"), (ACCESS_TOKEN, "access-tok"), (ACCESS_SECRET, "access-sec")])
    }

    fn catalog(dir: &Path, names: &[&str]) -> (AppState, Vec<i64>) {
        let root = dir.join("library");
        std::fs::create_dir_all(&root).unwrap();
        let state = AppState::default();
        let c = crate::catalog::Catalog::open(&dir.join("c.chairphoto"), &root).unwrap();
        let ids = names
            .iter()
            .map(|n| {
                let p = root.join(n);
                std::fs::write(&p, b"jpeg").unwrap();
                c.upsert_photo(&p, None, 0, 6).unwrap().id
            })
            .collect();
        *state.catalog.lock().unwrap() = Some(c);
        (state, ids)
    }

    fn fake_render(job: UploadJob) -> Result<RenderedJob, String> {
        let render: crate::app::uploads::UploadRenderer = Arc::new(|_, out| std::fs::write(out, b"pixels").map_err(|e| e.to_string()));
        job.render(&render)
    }

    /// Render then upload: the page URL is canonical with the NSID, the redirect form without.
    #[test]
    fn an_upload_returns_the_page_url() {
        let dir = TestTmpDir::new("flickr-upload");
        let (state, ids) = catalog(&dir, &["IMG_1.jpg"]);
        let api = FakeFlickr::default();
        let s = connected();
        let rendered = fake_render(claim_upload(&state, None, SERVICE, ids[0], None).unwrap()).unwrap();
        let url = upload(&api, &s, &rendered, "Aurora", "Night", "sky \"northern lights\"").unwrap();
        assert_eq!(url, "https://www.flickr.com/photo.gne?id=9001");
        s.set(USER_NSID, "1@N01").unwrap();
        let url = upload(&api, &s, &rendered, "Aurora", "", "").unwrap();
        assert_eq!(url, "https://www.flickr.com/photos/1@N01/9002/");
        let uploads = api.uploads.lock().unwrap();
        assert_eq!((uploads[0].0.as_str(), uploads[0].2.as_str(), uploads[0].3.as_slice()), ("Aurora", "sky \"northern lights\"", &b"pixels"[..]));
    }

    /// A cancelled job uploads nothing; neither does a job whose service is not connected.
    #[test]
    fn a_cancelled_or_unconnected_upload_sends_nothing() {
        let dir = TestTmpDir::new("flickr-cancel");
        let (state, ids) = catalog(&dir, &["IMG_1.jpg"]);
        let api = FakeFlickr::default();
        let job = claim_upload(&state, None, SERVICE, ids[0], None).unwrap();
        let abort = job.abort_handle();
        let rendered = fake_render(job).unwrap();
        assert!(upload(&api, &MemSettings::with(&[(API_KEY, "k"), (API_SECRET, "s")]), &rendered, "", "", "").unwrap_err().contains("Connect flickr"));
        abort.store(true, Ordering::Relaxed);
        assert_eq!(upload(&api, &connected(), &rendered, "", "", "").unwrap_err(), UPLOAD_CANCELLED);
        assert!(api.uploads.lock().unwrap().is_empty());
    }

    /// `post()`'s path checks the connection before it claims anything: a call that
    /// cannot publish takes no job id and leaves a publish already running untouched.
    #[test]
    fn a_post_that_cannot_publish_claims_nothing() {
        let dir = TestTmpDir::new("flickr-post-unconnected");
        let (state, ids) = catalog(&dir, &["IMG_1.jpg"]);
        let api = FakeFlickr::default();
        let running = claim_upload(&state, None, SERVICE, ids[0], None).unwrap();
        let issued = state.jobs.upload_flickr.job_ids_issued();
        let err = post(&api, &MemSettings::with(&[(API_KEY, "k"), (API_SECRET, "s")]), &state, ids[0], None, "", "", "").unwrap_err();
        assert!(err.contains("Connect flickr"), "{err}");
        assert_eq!(state.jobs.upload_flickr.job_ids_issued(), issued, "an unconnected post claimed a job");
        let rendered = fake_render(running).unwrap();
        rendered.ensure_live().expect("a failing post stopped a running publish");
    }

    /// React parity (two `post_to_flickr` calls, as the React UI makes them when a dialog is
    /// closed and another photo is published): both upload, though the second is called while
    /// the first is still rendering. Forced: the first call's render waits until the second
    /// call has uploaded.
    #[test]
    fn two_overlapping_posts_both_upload() {
        let dir = TestTmpDir::new("flickr-post-overlap");
        let (state, ids) = catalog(&dir, &["A.jpg", "B.jpg"]);
        let api = Arc::new(FakeFlickr::default());
        let (second_done_tx, second_done_rx) = std::sync::mpsc::channel::<()>();
        let (first_rendering_tx, first_rendering_rx) = std::sync::mpsc::channel::<()>();
        let second_done = std::sync::Mutex::new(second_done_rx);
        let slow: UploadRenderer = Arc::new(move |_, out| {
            first_rendering_tx.send(()).unwrap();
            second_done.lock().unwrap().recv().unwrap();
            std::fs::write(out, b"first").map_err(|e| e.to_string())
        });
        let first = {
            let (api, state, id) = (api.clone(), state.clone(), ids[0]);
            std::thread::spawn(move || post_with(&*api, &connected(), &state, id, None, &slow, "first", "", ""))
        };
        first_rendering_rx.recv().unwrap();
        let quick: UploadRenderer = Arc::new(|_, out| std::fs::write(out, b"second").map_err(|e| e.to_string()));
        let second = post_with(&*api, &connected(), &state, ids[1], None, &quick, "second", "", "");
        second_done_tx.send(()).unwrap();
        let first = first.join().unwrap();
        assert_eq!(second.unwrap(), "https://www.flickr.com/photo.gne?id=9001");
        assert_eq!(first.expect("the second post stopped the first"), "https://www.flickr.com/photo.gne?id=9002");
        let titles: Vec<String> = api.uploads.lock().unwrap().iter().map(|u| u.0.clone()).collect();
        assert_eq!(titles, ["second", "first"]);
    }

    fn photo(id: &str, title: &str, taken: &str) -> FlickrPhoto {
        FlickrPhoto {
            id: id.into(),
            owner: "1@N01".into(),
            title: title.into(),
            date_taken: taken.into(),
            date_upload_unix: 1_600_000_000,
            thumb_url: Some(format!("https://live.staticflickr.com/1/{id}_s.jpg")),
        }
    }

    /// Preview matches by title, applies with the historical date, and is bound to the
    /// catalog it read: once another opens, nothing is matched against or written into it.
    #[test]
    fn the_import_previews_applies_and_refuses_another_catalog() {
        let dir = TestTmpDir::new("flickr-import");
        let (state, ids) = catalog(&dir, &["DSC_1.jpg", "DSC_2.jpg"]);
        let read = crate::app::catalog_identity(&state).unwrap();
        let api = FakeFlickr::default();
        *api.stream.lock().unwrap() = vec![photo("501", "DSC_1", ""), photo("502", "nothing like it", "")];
        let preview = import_preview(&api, &connected(), &state, Some(read), "flickr").unwrap();
        assert_eq!((preview.matched_count, preview.ambiguous_count, preview.unmatched_count), (1, 0, 1));
        assert_eq!(preview.plan[0].catalog_id, ids[0]);
        let applied = import_apply(&state, Some(read), "flickr", &preview.plan).unwrap();
        assert_eq!(applied, ImportApplied { applied: 1, failed: vec![] });
        let pubs = with_catalog(&state, |c| c.list_publications(ids[0])).unwrap();
        assert_eq!((pubs[0].platform.as_str(), pubs[0].published_at, pubs[0].url.as_deref()), ("flickr", 1_600_000_000, Some("https://www.flickr.com/photos/1@N01/501/")));

        let other = TestTmpDir::new("flickr-import-b");
        let (b, _) = catalog(&other, &["DSC_1.jpg", "DSC_2.jpg"]);
        *state.catalog.lock().unwrap() = b.catalog.lock().unwrap().take();
        assert_eq!(import_preview(&api, &connected(), &state, Some(read), "flickr").unwrap_err(), CATALOG_CHANGED);
        assert_eq!(import_apply(&state, Some(read), "flickr", &preview.plan).unwrap_err(), CATALOG_CHANGED);
        assert!(with_catalog(&state, |c| c.list_publications(ids[0])).unwrap().is_empty());
    }

    /// One bad entry (a photo deleted since the preview) fails alone; the rest are recorded.
    #[test]
    fn an_import_records_every_entry_it_can() {
        let dir = TestTmpDir::new("flickr-import-partial");
        let (state, ids) = catalog(&dir, &["A.jpg", "B.jpg"]);
        with_catalog(&state, |c| c.remove_photo(ids[0])).unwrap();
        let entry = |id: i64, f: &str| FlickrImportMatch {
            catalog_id: id,
            catalog_path: format!("{id}.jpg"),
            flickr_id: f.into(),
            flickr_url: format!("https://www.flickr.com/photos/1@N01/{f}/"),
            published_at: 1,
            thumb_url: None,
        };
        let applied = import_apply(&state, None, "flickr", &[entry(ids[0], "1"), entry(ids[1], "2")]).unwrap();
        assert_eq!(applied.applied, 1);
        assert_eq!(applied.failed.len(), 1);
        assert_eq!(with_catalog(&state, |c| c.list_publications(ids[1])).unwrap().len(), 1);
    }

    /// The host check is made on the parsed URL: the authority tricks that fooled a check on
    /// the text up to the first `/` (`#`, `?`, `\`, user info, a port) are all refused, before
    /// any request is made.
    #[test]
    fn thumbnails_come_only_from_flickrs_image_hosts() {
        for bad in [
            "http://live.staticflickr.com/x.jpg",
            "https://evil.example/x.jpg",
            "https://staticflickr.com.evil.example/x.jpg",
            "https://evilstaticflickr.com/x.jpg",
            "file:///etc/passwd",
            "https://127.0.0.1:1#.staticflickr.com",
            "https://127.0.0.1:1?.staticflickr.com",
            "https://127.0.0.1:1/#.staticflickr.com",
            r"https://x@127.0.0.1:1\.staticflickr.com",
            r"https://evil.example\.staticflickr.com/x.jpg",
            "https://live.staticflickr.com@evil.example/x.jpg",
            "https://user:pw@live.staticflickr.com/x.jpg",
            "https://live.staticflickr.com:8443/x.jpg",
            "https://live.staticflickr.com:1/x.jpg",
            "https://127.0.0.1/x.staticflickr.com",
            "not a url",
        ] {
            assert!(flickr_thumb_url(bad).is_none(), "accepted {bad}");
            assert_eq!(fetch_thumb(bad).unwrap_err(), "not a Flickr image URL", "{bad}");
        }
        for good in ["https://live.staticflickr.com/65535/1_abc_s.jpg", "https://farm5.staticflickr.com/x.jpg", "https://LIVE.StaticFlickr.com:443/x.jpg"] {
            assert!(flickr_thumb_url(good).is_some(), "refused {good}");
        }
    }

    /// A loopback HTTP server answering every request with `response`; returns its base URL
    /// and how many requests it saw.
    fn serve(response: Vec<u8>) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                seen.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(&response);
            }
        });
        (base, hits)
    }

    /// The fetch behind [`fetch_thumb`] (loopback only, no network): a redirect is an error,
    /// not followed; a body over the cap is refused, whether announced or streamed.
    #[test]
    fn a_thumbnail_fetch_follows_no_redirect_and_caps_the_body() {
        let run = |url: String, max| crate::app::runtime().block_on(fetch_capped(reqwest::Url::parse(&url).unwrap(), max));
        let (base, hits) = serve(b"HTTP/1.1 302 Found\r\nLocation: /elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec());
        let err = run(format!("{base}/x.jpg"), 1024).unwrap_err();
        assert!(err.contains("HTTP 302"), "{err}");
        assert_eq!(hits.load(Ordering::SeqCst), 1, "the redirect was followed");

        // Refused on the announced length alone, before the body is read (only 16 bytes come).
        let mut announced = b"HTTP/1.1 200 OK\r\nContent-Length: 2048\r\nConnection: close\r\n\r\n".to_vec();
        announced.extend(vec![0u8; 16]);
        let (base, _) = serve(announced);
        assert!(run(format!("{base}/x.jpg"), 1024).unwrap_err().contains("larger than 1024 bytes"));

        let mut streamed = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
        streamed.extend(vec![0u8; 4096]);
        let (base, _) = serve(streamed);
        assert!(run(format!("{base}/x.jpg"), 1024).unwrap_err().contains("larger than 1024 bytes"));

        let mut small = b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\n".to_vec();
        small.extend(b"jpg");
        let (base, _) = serve(small);
        assert_eq!(run(format!("{base}/x.jpg"), 1024).unwrap(), b"jpg");
    }
}
