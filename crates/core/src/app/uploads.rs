//! Publishing one photo to an online service — Flickr, SmugMug, or a supervised Instagram post
//! (docs/publications.md) — as an owned job: the shared claim and render that `app::flickr`,
//! `app::smugmug` and `app::instagram` run before their upload, for the Tauri commands and the
//! GPUI publish targets alike.
//!
//! Every publish is its own job, numbered per service ([`UploadService`],
//! `JobRegistry::upload_*`), and several may run side by side — a second photo published
//! while the first still renders, or the same photo twice: a newer publish never stops an
//! older one. Each job has two ways to stop: its own cancel flag ([`UploadJob::abort_handle`],
//! the front end's Cancel for *that* publish), and the service's abort generation, which every
//! running publish of the service shares and a catalog switch trips.
//!
//! 1. [`claim_upload`] checks the catalog the photo id was read from, resolves the photo's
//!    original and chosen version, and joins the service's abort generation — under one catalog
//!    lock (catalog → that service's abort), so a switch either lands first (the check fails)
//!    or after (its trip reaches this job). Joining trips nothing.
//! 2. [`UploadJob::render`] renders the JPEG into a private, job-scoped directory
//!    ([`crate::publishing::JobTempDir`]); its export-parity checks go to the catalog it read.
//!    It stops before rendering, and refuses to hand the render on, once the job is stopped.
//! 3. The service's upload checks [`RenderedJob::ensure_live`] first, then sends. **An upload
//!    in flight is not interrupted**: the service may already hold the bytes and could commit
//!    them, so cancelling there would leave the user not knowing whether the photo is online.
//!    A switch during the upload lets it finish; recording the publication then fails closed
//!    (`record_publications_as` is bound to the catalog the id came from) and says so.
//!
//! Every step is blocking — run each on a worker. The terminal result is what the last step
//! returns; the steps in between are only progress.
//!
//! **Privacy.** Nothing here runs except on the user's Publish (or Post) for that service, after
//! they signed in. Originals are only read.

use super::{AppState, CatalogIdentity, CATALOG_CHANGED};
use crate::export::ResolvedItem;
use crate::publishing::{upload_file_name, JobTempDir};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// What a cancelled publish answers: it stopped before uploading anything.
pub const UPLOAD_CANCELLED: &str = "Cancelled — nothing was uploaded.";

/// The services a publish job can target; each is its own job family (its own job ids and
/// abort generation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadService {
    Flickr,
    SmugMug,
    Instagram,
}

impl UploadService {
    /// The service's id: its settings namespace, publication marker and temp-dir tag.
    pub fn id(self) -> &'static str {
        match self {
            UploadService::Flickr => "flickr",
            UploadService::SmugMug => "smugmug",
            UploadService::Instagram => "instagram",
        }
    }

    fn generation(self, state: &AppState) -> &super::jobs::AbortGeneration {
        match self {
            UploadService::Flickr => &state.jobs.upload_flickr,
            UploadService::SmugMug => &state.jobs.upload_smugmug,
            UploadService::Instagram => &state.jobs.upload_instagram,
        }
    }
}

/// One service's own settings — the catalog `settings` keys `<service>.<key>` (API key and
/// secret, OAuth tokens, max long edge, album cache). `key` is the part after the dot.
///
/// The Tauri commands use [`CatalogSettings`] (whichever catalog is open); the GPUI app's
/// module settings handle implements it bound to one catalog. **Blocking** (the catalog lock).
/// Values may be secrets (OAuth tokens): never log one.
pub trait ServiceSettings: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<String>, String>;
    fn set(&self, key: &str, value: &str) -> Result<(), String>;
}

/// [`ServiceSettings`] over whichever catalog is open, under `<prefix>.` (the Tauri commands).
#[derive(Clone)]
pub struct CatalogSettings {
    state: AppState,
    prefix: &'static str,
}

impl CatalogSettings {
    pub fn new(state: &AppState, service: UploadService) -> Self {
        CatalogSettings { state: state.clone(), prefix: service.id() }
    }
}

impl ServiceSettings for CatalogSettings {
    fn get(&self, key: &str) -> Result<Option<String>, String> {
        let key = format!("{}.{key}", self.prefix);
        super::with_catalog(&self.state, |c| c.get_setting(&key))
    }

    fn set(&self, key: &str, value: &str) -> Result<(), String> {
        let key = format!("{}.{key}", self.prefix);
        super::with_catalog(&self.state, |c| c.set_setting(&key, value))
    }
}

/// A setting's value, empty when unset.
pub fn setting(settings: &dyn ServiceSettings, key: &str) -> Result<String, String> {
    Ok(settings.get(key)?.unwrap_or_default())
}

/// The settings key of the user's max long edge (px) for uploads; empty or `0` = full size.
pub const MAX_LONG_EDGE: &str = "max_long_edge";

/// The max long edge an upload is rendered at: `None` when unset, empty, unparsable or `0`
/// (full resolution, the default).
pub fn max_long_edge(settings: &dyn ServiceSettings) -> Option<u32> {
    let raw = settings.get(MAX_LONG_EDGE).ok().flatten().unwrap_or_default();
    raw.trim().parse::<u32>().ok().filter(|&v| v > 0)
}

/// Renders one resolved photo to the JPEG that is uploaded (`item`, destination). The
/// production ones are [`long_edge`] and [`width`]; tests hand in one that needs no
/// thumbnail cache.
pub type UploadRenderer = Arc<dyn Fn(&ResolvedItem, &Path) -> Result<(), String> + Send + Sync>;

/// The Flickr/SmugMug render: the chosen version, its longer side capped at `max` (never
/// upscaled; `None` = full resolution), EXIF/GPS carried over.
pub fn long_edge(max: Option<u32>) -> UploadRenderer {
    Arc::new(move |item, out| crate::export::write_item_jpeg_with_long_edge(item, max, out))
}

/// The Instagram render: `max` px wide (every aspect Instagram supports is 1080 wide).
pub fn width(max: Option<u32>) -> UploadRenderer {
    Arc::new(move |item, out| crate::export::write_item_jpeg(item, max, out))
}

/// A claimed publish, not yet rendered. Dropping it uploads nothing.
pub struct UploadJob {
    state: AppState,
    service: UploadService,
    /// The catalog the photo was resolved from.
    read: CatalogIdentity,
    item: ResolvedItem,
    stop: Stop,
    /// This publish's job id (per service).
    pub job: u64,
}

/// What stops one publish: its own cancel flag, or the service generation it joined (a
/// catalog switch trips that one).
struct Stop {
    own: Arc<AtomicBool>,
    generation: Arc<AtomicBool>,
}

impl Stop {
    fn stopped(&self) -> bool {
        self.own.load(Ordering::Relaxed) || self.generation.load(Ordering::Relaxed)
    }
}

/// Claim a publish of `photo_id` (`version_id`: `None` = Original) to `service`.
///
/// `expected`: the catalog the id was read from — `Some` fails closed with
/// [`CATALOG_CHANGED`] once another catalog is open; `None` = the open one (the Tauri
/// commands). The identity check, the resolve and the claim run under one catalog lock, the
/// service's abort generation joined inside it (catalog → abort). Stops no other publish.
pub fn claim_upload(
    state: &AppState,
    expected: Option<CatalogIdentity>,
    service: UploadService,
    photo_id: i64,
    version_id: Option<i64>,
) -> Result<UploadJob, String> {
    let guard = state.catalog.lock().map_err(|e| e.to_string())?;
    let catalog = guard.as_ref().ok_or("No catalog is open")?;
    if expected.is_some_and(|e| !e.is(catalog)) {
        return Err(CATALOG_CHANGED.into());
    }
    let resolved = crate::export::resolve_originals(catalog, &[photo_id], &[], version_id);
    let item = resolved.items.into_iter().next().ok_or("Photo is unavailable (original offline?)")?;
    let read = super::identity_of(catalog);
    let (generation, job) = service.generation(state).join_numbered()?;
    drop(guard);
    let stop = Stop { own: Arc::new(AtomicBool::new(false)), generation };
    Ok(UploadJob { state: state.clone(), service, read, item, stop, job })
}

/// Why a stopped job stopped: the catalog it read is gone, or it was cancelled.
fn stopped(state: &AppState, read: CatalogIdentity) -> String {
    match super::catalog_identity(state) {
        Ok(open) if open == read => UPLOAD_CANCELLED.into(),
        _ => CATALOG_CHANGED.into(),
    }
}

impl UploadJob {
    /// This publish's own cancel flag: tripping it cancels this publish and no other.
    pub fn abort_handle(&self) -> Arc<AtomicBool> {
        self.stop.own.clone()
    }

    /// The catalog the photo was read from.
    pub fn catalog(&self) -> CatalogIdentity {
        self.read
    }

    /// The name of the version being published (`None` = the unedited original).
    pub fn version_name(&self) -> Option<&str> {
        self.item.version_name.as_deref()
    }

    /// Render the photo with `render` into this job's own temp directory. Blocking. Refuses
    /// (without rendering) once the job is stopped, and refuses to hand the render on when it
    /// was stopped while rendering.
    pub fn render(self, render: &UploadRenderer) -> Result<RenderedJob, String> {
        let UploadJob { state, service, read, item, stop, job } = self;
        if stop.stopped() {
            return Err(stopped(&state, read));
        }
        // Named after the source (with the version suffix), so the service shows a
        // meaningful filename; the job-scoped directory keeps that name collision-free.
        let dir = JobTempDir::new(service.id())?;
        let path = dir.join(&upload_file_name(&item.original, item.version_name.as_deref()));
        let (written, tally) = super::exports::collect_parity(|| render(&item, &path));
        super::exports::record_parity_tally(&state, Some(read), tally);
        written?;
        let rendered = RenderedJob { _dir: dir, path, state, read, stop, job };
        rendered.ensure_live()?;
        Ok(rendered)
    }
}

/// A rendered publish: the JPEG, the job directory holding it (removed when this drops), and
/// what stops the job. Keep it alive until the upload has finished.
pub struct RenderedJob {
    _dir: JobTempDir,
    path: PathBuf,
    state: AppState,
    read: CatalogIdentity,
    stop: Stop,
    pub job: u64,
}

impl RenderedJob {
    /// The rendered JPEG.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The catalog the photo was read from: where its publication is recorded.
    pub fn catalog(&self) -> CatalogIdentity {
        self.read
    }

    /// `Ok` while the job was neither cancelled nor stopped by a catalog switch: the last check
    /// before the upload starts. `Err` says why it stopped ([`UPLOAD_CANCELLED`] or
    /// [`CATALOG_CHANGED`]).
    pub fn ensure_live(&self) -> Result<(), String> {
        if self.stop.stopped() {
            return Err(stopped(&self.state, self.read));
        }
        Ok(())
    }

    /// Leave the render on disk when this drops: a supervised Instagram post, whose composer
    /// reads the file only when the user clicks Share (see [`JobTempDir::keep`]).
    #[cfg(feature = "instagram")]
    pub fn keep(self) {
        let RenderedJob { _dir, .. } = self;
        _dir.keep();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestTmpDir;
    use std::sync::Mutex;

    fn catalog_with_photo(dir: &Path) -> (AppState, i64) {
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

    /// A renderer that writes a marker file and counts its calls.
    fn fake(calls: &Arc<Mutex<usize>>) -> UploadRenderer {
        let calls = calls.clone();
        Arc::new(move |_, out| {
            *calls.lock().unwrap() += 1;
            std::fs::write(out, b"render").map_err(|e| e.to_string())
        })
    }

    /// Claim, render: the render lands in a private job directory named after the photo,
    /// which is removed when the rendered job drops.
    #[test]
    fn a_claimed_job_renders_into_its_own_directory() {
        let dir = TestTmpDir::new("uploads-render");
        let (state, id) = catalog_with_photo(&dir);
        let read = crate::app::catalog_identity(&state).unwrap();
        let calls = Arc::new(Mutex::new(0));
        let job = claim_upload(&state, Some(read), UploadService::Flickr, id, None).unwrap();
        assert_eq!(job.catalog(), read);
        let rendered = job.render(&fake(&calls)).unwrap();
        assert_eq!(rendered.path().file_name().unwrap(), "IMG_1.jpg");
        assert!(rendered.path().exists());
        rendered.ensure_live().unwrap();
        let parent = rendered.path().parent().unwrap().to_path_buf();
        drop(rendered);
        assert!(!parent.exists(), "the job directory outlived the job");
    }

    /// Cancel before the render: nothing is rendered; cancel after it: the upload's check
    /// refuses. Both say nothing was uploaded.
    #[test]
    fn a_cancelled_job_stops_before_its_render_and_before_its_upload() {
        let dir = TestTmpDir::new("uploads-cancel");
        let (state, id) = catalog_with_photo(&dir);
        let calls = Arc::new(Mutex::new(0));
        let job = claim_upload(&state, None, UploadService::SmugMug, id, None).unwrap();
        job.abort_handle().store(true, Ordering::Relaxed);
        assert_eq!(job.render(&fake(&calls)).err().unwrap(), UPLOAD_CANCELLED);
        assert_eq!(*calls.lock().unwrap(), 0, "a cancelled job rendered");

        let job = claim_upload(&state, None, UploadService::SmugMug, id, None).unwrap();
        let abort = job.abort_handle();
        let rendered = job.render(&fake(&calls)).unwrap();
        abort.store(true, Ordering::Relaxed);
        assert_eq!(rendered.ensure_live().unwrap_err(), UPLOAD_CANCELLED);
    }

    /// Publishes run side by side: a newer publish to the same service — of the same photo or
    /// another — stops no older one, each Cancel stops only its own job, and a catalog switch
    /// stops every one of them. Forced overlap: the older job is claimed, the newer claimed and
    /// rendered, then the older renders and both reach their upload check.
    #[test]
    fn publishes_run_side_by_side_and_a_switch_stops_them_all() {
        let dir = TestTmpDir::new("uploads-side-by-side");
        let (state, id) = catalog_with_photo(&dir);
        let calls = Arc::new(Mutex::new(0));
        let older = claim_upload(&state, None, UploadService::Flickr, id, None).unwrap();
        let newer = claim_upload(&state, None, UploadService::Flickr, id, None).unwrap();
        let third = claim_upload(&state, None, UploadService::Flickr, id, None).unwrap();
        assert!(older.job < newer.job && newer.job < third.job, "each publish has its own job id");
        let newer = newer.render(&fake(&calls)).unwrap();
        let older = older.render(&fake(&calls)).unwrap();
        assert_eq!(*calls.lock().unwrap(), 2, "a newer publish stopped an older one's render");
        older.ensure_live().expect("a newer publish stopped the older one");
        newer.ensure_live().unwrap();

        // Cancel the third: the other two run on.
        third.abort_handle().store(true, Ordering::Relaxed);
        assert_eq!(third.render(&fake(&calls)).err().unwrap(), UPLOAD_CANCELLED);
        older.ensure_live().expect("one publish's Cancel stopped another");
        newer.ensure_live().unwrap();

        // A catalog switch stops both.
        let (b, _) = catalog_with_photo(&dir.join("b"));
        crate::app::detach_catalog_and_trip_jobs(&state).unwrap();
        crate::app::publish_catalog_and_reset_jobs(&state, b.catalog.lock().unwrap().take().unwrap()).unwrap();
        assert_eq!(older.ensure_live().unwrap_err(), CATALOG_CHANGED);
        assert_eq!(newer.ensure_live().unwrap_err(), CATALOG_CHANGED);
        // A publish claimed after the switch is live.
        let after = claim_upload(&state, None, UploadService::Flickr, id, None).unwrap();
        after.render(&fake(&calls)).unwrap().ensure_live().unwrap();
    }

    /// Bound to a catalog that is no longer open: the claim refuses; a switch after the
    /// claim trips the job (phase one) and the render says the catalog changed.
    #[test]
    fn a_catalog_switch_refuses_the_claim_and_stops_a_claimed_job() {
        let dir = TestTmpDir::new("uploads-switch");
        let (state, id) = catalog_with_photo(&dir);
        let read = crate::app::catalog_identity(&state).unwrap();
        let job = claim_upload(&state, Some(read), UploadService::Instagram, id, None).unwrap();

        let other = TestTmpDir::new("uploads-switch-b");
        let (b, ids_b) = catalog_with_photo(&other);
        assert_eq!(ids_b, id, "the ids collide");
        state.jobs.lock_for_detach().unwrap().trip_and_clear_all();
        *state.catalog.lock().unwrap() = b.catalog.lock().unwrap().take();

        let calls = Arc::new(Mutex::new(0));
        assert_eq!(job.render(&fake(&calls)).err().unwrap(), CATALOG_CHANGED);
        assert_eq!(*calls.lock().unwrap(), 0);
        let err = claim_upload(&state, Some(read), UploadService::Instagram, id, None).err().unwrap();
        assert_eq!(err, CATALOG_CHANGED);
    }

    #[test]
    fn max_long_edge_reads_the_setting() {
        struct S(Option<&'static str>);
        impl ServiceSettings for S {
            fn get(&self, _: &str) -> Result<Option<String>, String> {
                Ok(self.0.map(String::from))
            }
            fn set(&self, _: &str, _: &str) -> Result<(), String> {
                Ok(())
            }
        }
        assert_eq!(max_long_edge(&S(None)), None);
        assert_eq!(max_long_edge(&S(Some(""))), None);
        assert_eq!(max_long_edge(&S(Some("0"))), None);
        assert_eq!(max_long_edge(&S(Some("nope"))), None);
        assert_eq!(max_long_edge(&S(Some(" 2048 "))), Some(2048));
    }
}
