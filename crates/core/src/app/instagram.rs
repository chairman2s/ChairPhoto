//! Instagram posting (docs/instagram.md) — the body the GPUI Instagram module runs.
//!
//! A post is a publish job ([`super::uploads`], the Instagram family): claimed, rendered
//! 1080 px wide into its own private directory, then handed to Chrome by an
//! [`InstagramDriver`]. Supervised by default: the driver composes the post and stops before
//! Share ([`PostOutcome::AwaitingReview`]); only `publish = true` clicks Share. A stopped job
//! (its own Cancel or a catalog switch; a newer post stops no older one) stops before Chrome
//! sees the render; once the composer has it, the browser window ChairPhoto deliberately does
//! not own is the cancel.
//!
//! **The render outlives a supervised post**: Chrome reads the file only when the user clicks
//! Share, so [`post`] keeps the directory unless the outcome says the render is no longer
//! needed ([`render_still_needed`]); the sweep reclaims a kept one once it is stale.
//!
//! The production driver is [`ChromeDriver`]; tests hand in a fake and never launch a browser.
//! Every function is **blocking**: run it on a worker.

use super::uploads::{width, RenderedJob, UploadJob, UploadService};
use super::{with_catalog, with_catalog_as, AppState, CatalogIdentity};
pub use crate::instagram::PostOutcome;
use std::path::{Path, PathBuf};

/// The settings namespace, publication marker and job family name.
pub const SERVICE: UploadService = UploadService::Instagram;

/// Every aspect Instagram supports is 1080 px wide: render to that, avoiding a second
/// recompression on their side.
pub const WIDTH: u32 = 1080;

/// How many `#hashtags` the caption prefill carries (Instagram's limit).
pub const MAX_HASHTAGS: usize = 30;

/// Hands a rendered JPEG to Instagram's web composer. Blocking.
pub trait InstagramDriver: Send + Sync {
    /// Compose a post of `image` with `caption`; click Share only when `publish`.
    fn post(&self, image: &Path, caption: &str, publish: bool) -> Result<PostOutcome, String>;
}

/// The real driver: Chrome with ChairPhoto's own persistent profile, over the DevTools
/// Protocol (`crate::instagram::post`).
pub struct ChromeDriver;

impl InstagramDriver for ChromeDriver {
    fn post(&self, image: &Path, caption: &str, publish: bool) -> Result<PostOutcome, String> {
        let profile = profile_dir()?;
        let chrome = find_chrome().ok_or("Chrome/Chromium not found — install Google Chrome or Chromium to post to Instagram")?;
        super::runtime().block_on(crate::instagram::post(image, caption, &profile, &chrome, publish))
    }
}

/// The persistent Chrome profile for Instagram (keeps the login between posts).
fn profile_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").map(PathBuf::from).ok_or("HOME is not set")?;
    Ok(std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"))
        .join("chairphoto")
        .join("ig-profile"))
}

/// A Chrome/Chromium executable on PATH.
fn find_chrome() -> Option<String> {
    for bin in ["google-chrome-stable", "google-chrome", "chromium", "chromium-browser", "brave"] {
        if let Ok(out) = std::process::Command::new("which").arg(bin).output() {
            if out.status.success() {
                let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !path.is_empty() {
                    return Some(path);
                }
            }
        }
    }
    None
}

/// The caption prefill: the photo's IPTC title (else headline) and description, then its export
/// keywords as `#hashtags`. `from`: the catalog the id was read from (`None` = the open one).
pub fn caption(state: &AppState, from: Option<CatalogIdentity>, photo_id: i64) -> Result<String, String> {
    let read = |c: &crate::catalog::Catalog| {
        let iptc = c.get_iptc(photo_id).unwrap_or_default();
        let keywords = c.assemble_export_keywords(photo_id, &[]).map(|k| k.flat).unwrap_or_default();
        let title = if iptc.title.trim().is_empty() { iptc.headline } else { iptc.title };
        Ok(crate::instagram::build_caption(&title, &iptc.description, &keywords, MAX_HASHTAGS))
    };
    match from {
        Some(f) => with_catalog_as(state, f, read),
        None => with_catalog(state, read),
    }
}

/// Render a claimed post 1080 px wide.
pub fn render(job: UploadJob) -> Result<RenderedJob, String> {
    job.render(&width(Some(WIDTH)))
}

/// Hand the render to the driver — unless the job was tripped first — and keep the render on
/// disk while the composer may still read it.
pub fn post(driver: &dyn InstagramDriver, rendered: RenderedJob, caption: &str, publish: bool) -> Result<PostOutcome, String> {
    rendered.ensure_live()?;
    let outcome = driver.post(rendered.path(), caption, publish);
    if render_still_needed(&outcome) {
        rendered.keep();
    }
    outcome
}

/// Whether the composed post may still read the render from disk after the call returns.
///
/// `Posted` and `NeedsLogin` are finished with it: Instagram has the bytes, or the driver
/// returned before touching the file input. `AwaitingReview` is not — the composer is on screen
/// and the Share click reads the file (measured over CDP against Chrome 150: a deleted render
/// fails `arrayBuffer()` with `NotFoundError`). An `Err` can land on either side of the attach,
/// so it is treated as still in use; the startup and on-publish sweeps bound what that costs.
pub fn render_still_needed(outcome: &Result<PostOutcome, String>) -> bool {
    match outcome {
        Ok(PostOutcome::Posted | PostOutcome::NeedsLogin) => false,
        Ok(PostOutcome::AwaitingReview) | Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::uploads::{claim_upload, UploadRenderer, UPLOAD_CANCELLED};
    use crate::test_support::TestTmpDir;
    use std::sync::{Arc, Mutex};

    /// The render must survive exactly as long as the composed post can still read it. Both
    /// directions matter and each fails differently — deleting too early breaks a Share click
    /// the user is about to make; keeping forever leaks a full-resolution JPEG per publish.
    #[test]
    fn the_render_outlives_exactly_the_outcomes_that_still_read_it() {
        assert!(render_still_needed(&Ok(PostOutcome::AwaitingReview)), "a supervised post's render was deleted");
        assert!(!render_still_needed(&Ok(PostOutcome::Posted)), "a confirmed post kept its render");
        assert!(!render_still_needed(&Ok(PostOutcome::NeedsLogin)), "NeedsLogin never touched the file input");
        assert!(render_still_needed(&Err("couldn't find the New-post button".into())));
    }

    struct Fake(Mutex<Vec<(PathBuf, String, bool)>>, PostOutcome);

    impl InstagramDriver for Fake {
        fn post(&self, image: &Path, caption: &str, publish: bool) -> Result<PostOutcome, String> {
            assert!(image.exists(), "the driver got a render that is not on disk");
            self.0.lock().unwrap().push((image.to_path_buf(), caption.into(), publish));
            Ok(self.1)
        }
    }

    fn rendered(dir: &Path) -> (AppState, RenderedJob, Arc<std::sync::atomic::AtomicBool>) {
        let root = dir.join("library");
        std::fs::create_dir_all(&root).unwrap();
        let state = AppState::default();
        let c = crate::catalog::Catalog::open(&dir.join("c.chairphoto"), &root).unwrap();
        let p = root.join("IMG_1.jpg");
        std::fs::write(&p, b"jpeg").unwrap();
        let id = c.upsert_photo(&p, None, 0, 6).unwrap().id;
        *state.catalog.lock().unwrap() = Some(c);
        let job = claim_upload(&state, None, SERVICE, id, None).unwrap();
        let abort = job.abort_handle();
        let render: UploadRenderer = Arc::new(|_, out| std::fs::write(out, b"px").map_err(|e| e.to_string()));
        let rendered = job.render(&render).unwrap();
        (state, rendered, abort)
    }

    /// A supervised post keeps its render for the composer; a confirmed one removes it.
    #[test]
    fn a_supervised_post_keeps_its_render_and_a_posted_one_does_not() {
        let dir = TestTmpDir::new("instagram-keep");
        let (_state, job, _) = rendered(&dir);
        let driver = Fake(Mutex::default(), PostOutcome::AwaitingReview);
        assert_eq!(post(&driver, job, "Aurora #sky", false).unwrap(), PostOutcome::AwaitingReview);
        let (path, caption, publish) = driver.0.lock().unwrap()[0].clone();
        assert_eq!((caption.as_str(), publish), ("Aurora #sky", false));
        assert!(path.exists(), "the composer's render was deleted");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();

        let (_state, job, _) = rendered(&dir.join("posted"));
        let driver = Fake(Mutex::default(), PostOutcome::Posted);
        post(&driver, job, "", true).unwrap();
        let path = driver.0.lock().unwrap()[0].0.clone();
        assert!(!path.parent().unwrap().exists(), "a posted render was kept");
    }

    /// A post tripped before Chrome has the render never reaches the driver.
    #[test]
    fn a_cancelled_post_never_reaches_chrome() {
        let dir = TestTmpDir::new("instagram-cancel");
        let (_state, job, abort) = rendered(&dir);
        abort.store(true, std::sync::atomic::Ordering::Relaxed);
        let driver = Fake(Mutex::default(), PostOutcome::Posted);
        assert_eq!(post(&driver, job, "", false).unwrap_err(), UPLOAD_CANCELLED);
        assert!(driver.0.lock().unwrap().is_empty());
    }
}
