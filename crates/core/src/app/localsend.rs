//! Sending photos to a LocalSend device — the body of the Tauri `localsend_send` command and of
//! the GPUI LocalSend/Snapchat publish targets (docs/localsend.md).
//!
//! A send is a job (`JobRegistry::localsend`): [`claim_send`] resolves the photos' originals
//! and takes ownership (catalog → the LocalSend abort, one transition), and
//! [`LocalSendJob::run`] does the work on the caller's worker: one full-resolution JPEG per
//! photo into a private, job-scoped temp directory ([`crate::publishing::JobTempDir`], removed
//! however the job ends), then the LocalSend handshake and uploads. A newer send, Cancel
//! (tripping the job's own [`LocalSendJob::abort_handle`]) or a catalog switch trips it; the
//! send stops before its next render or file — mid-upload too — cancels the receiver's
//! session and answers [`SEND_CANCELLED`].
//!
//! Progress is `localsend:progress` carrying the job id; it is cosmetic. The terminal result
//! is what `run` returns: which photos reached the device and how many did not.
//!
//! **Privacy.** Nothing here runs unless the user picked a device and pressed Send: the
//! claim is the explicit, per-send action, and it is LAN-only (the device's own address).
//! Originals are only read.

use super::{AppState, CatalogIdentity, CoreEvent, EventSink, LocalSendProgress, CATALOG_CHANGED};
use crate::export::ResolvedItem;
pub use crate::localsend::{Device, SEND_CANCELLED};
use crate::publishing::{upload_file_name, JobTempDir};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Renders one photo to the JPEG that is sent (`item`, destination). The production one is
/// [`full_resolution`]; tests hand in one that needs no thumbnail cache.
pub type ItemRenderer = Arc<dyn Fn(&ResolvedItem, &Path) -> Result<(), String> + Send + Sync>;

/// The production renderer: the item (its chosen version where it matches) at full
/// resolution, EXIF/GPS and keywords carried over — the device decides what to do with it
/// (Snapchat downscales on the phone).
pub fn full_resolution() -> ItemRenderer {
    Arc::new(|item, out| crate::export::write_item_jpeg(item, None, out))
}

/// What a send produced: the photos that reached the device (in send order), and how many of
/// those asked for did not (original offline, render failed, name exhausted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendOutcome {
    pub sent: Vec<i64>,
    pub failed: usize,
}

/// A claimed send, not yet running. Dropping it sends nothing; its abort flag stays installed
/// until a newer send, Cancel or switch replaces it.
pub struct LocalSendJob {
    state: AppState,
    /// The catalog the originals were resolved from: the renders' parity tally goes there.
    read: CatalogIdentity,
    items: Vec<ResolvedItem>,
    requested: usize,
    abort: Arc<AtomicBool>,
    /// The send's job id: every `localsend:progress` it sends carries it.
    pub job: u64,
}

/// Claim a send of `photo_ids` (`version_id` applies to its own photo only; the others go
/// unedited, as in export).
///
/// `expected`: the catalog the ids were read from — `Some` from a front end that captured it
/// (fails closed with [`CATALOG_CHANGED`] once another catalog is open), `None` for "the open
/// one". The identity check, the resolve and the claim run under one catalog lock, taking the
/// LocalSend abort inside it (catalog → abort): a switch either lands first (the check fails)
/// or after (its trip reaches this send). Blocking (catalog lock) — not on a UI thread.
pub fn claim_send(
    state: &AppState,
    expected: Option<CatalogIdentity>,
    photo_ids: &[i64],
    version_id: Option<i64>,
) -> Result<LocalSendJob, String> {
    if photo_ids.is_empty() {
        return Err("Select at least one photo to send.".into());
    }
    let guard = state.catalog.lock().map_err(|e| e.to_string())?;
    let catalog = guard.as_ref().ok_or("No catalog is open")?;
    if expected.is_some_and(|e| !e.is(catalog)) {
        return Err(CATALOG_CHANGED.into());
    }
    // An unmounted NAS / offline original is skipped (and counted as failed), not fatal.
    let resolved = crate::export::resolve_originals(catalog, photo_ids, &[], version_id);
    if resolved.items.is_empty() {
        return Err("None of the selected photos are available (originals offline?).".into());
    }
    let (abort, job) = state.jobs.localsend.install_fresh_numbered()?;
    let read = super::identity_of(catalog);
    drop(guard);
    Ok(LocalSendJob { state: state.clone(), read, items: resolved.items, requested: photo_ids.len(), abort, job })
}

impl LocalSendJob {
    /// This send's own abort flag: tripping it cancels this send and no other.
    pub fn abort_handle(&self) -> Arc<AtomicBool> {
        self.abort.clone()
    }

    /// How many photos will be rendered (the reachable ones).
    pub fn photo_count(&self) -> usize {
        self.items.len()
    }

    /// Render and send with the production [`full_resolution`] renderer. Blocking: runs the
    /// transfer on the core runtime and waits, so call it from a worker thread (never an async
    /// task or the UI thread).
    pub fn run(self, device: &Device, pin: Option<&str>) -> Result<SendOutcome, String> {
        self.run_with(device, pin, &full_resolution())
    }

    /// [`run`](Self::run) with an explicit renderer.
    pub fn run_with(self, device: &Device, pin: Option<&str>, render: &ItemRenderer) -> Result<SendOutcome, String> {
        let LocalSendJob { state, read, items, requested, abort, job } = self;
        let cancelled = || abort.load(Ordering::Relaxed);
        // This send's own directory: removed when `dir` drops, whichever way this returns.
        let dir = JobTempDir::new("localsend")?;
        let (rendered, tally) = super::exports::collect_parity(|| -> Result<Vec<(i64, PathBuf)>, String> {
            let mut out = Vec::with_capacity(items.len());
            for item in &items {
                if cancelled() {
                    return Err(SEND_CANCELLED.into());
                }
                // Name the file after the source (with the version suffix). Two selected
                // photos can share a stem (the same basename in different folders), so
                // disambiguate inside this send's directory rather than rendering over the
                // earlier one and sending it twice under the other photo's name.
                let name = upload_file_name(&item.original, item.version_name.as_deref());
                let target = match unique_in_dir(&dir, &name) {
                    Ok(t) => t,
                    // Skip this photo, like a failed render below: it is then counted as
                    // failed, which is the truth. Sending it under a name another render
                    // already owns is the one thing we must not do.
                    Err(e) => {
                        eprintln!("localsend: {e}");
                        continue;
                    }
                };
                // Skip a photo whose render fails rather than aborting the whole send.
                match render(item, &target) {
                    Ok(()) => out.push((item.photo_id, target)),
                    Err(e) => eprintln!("localsend: render failed for a photo: {e}"),
                }
            }
            Ok(out)
        });
        super::exports::record_parity_tally(&state, Some(read), tally);
        let rendered = rendered?;
        if rendered.is_empty() {
            return Err("None of the selected photos could be rendered.".into());
        }
        let paths: Vec<PathBuf> = rendered.iter().map(|(_, p)| p.clone()).collect();
        let pin = pin.map(str::trim).filter(|p| !p.is_empty());
        crate::app::runtime().block_on(crate::localsend::send_files_abortable(device, &paths, pin, &abort, |done, total| {
            state.send(CoreEvent::LocalSendProgress(LocalSendProgress { done, total, job }));
        }))?;
        drop(dir);
        let sent: Vec<i64> = rendered.into_iter().map(|(id, _)| id).collect();
        let failed = requested.saturating_sub(sent.len());
        Ok(SendOutcome { sent, failed })
    }
}

/// The highest `" (n)"` suffix tried before `unique_in_dir` gives up.
const MAX_DISAMBIGUATION: u32 = 9_999;

/// `dir/name`, suffixed `" (2)"`, `" (3)"`… if a previous render in this send already took
/// it. The directory belongs to this send alone, so `exists()` sees only our own renders.
///
/// Erroring when every suffix is taken is the point: returning the taken path instead would
/// reintroduce exactly the overwrite this exists to prevent, in the one case where the
/// collision is not hypothetical but proven.
fn unique_in_dir(dir: &JobTempDir, name: &str) -> Result<PathBuf, String> {
    unique_in_dir_up_to(dir, name, MAX_DISAMBIGUATION)
}

fn unique_in_dir_up_to(dir: &JobTempDir, name: &str, max: u32) -> Result<PathBuf, String> {
    let first = dir.join(name);
    if !first.exists() {
        return Ok(first);
    }
    let base = Path::new(name);
    let stem = base.file_stem().and_then(|s| s.to_str()).unwrap_or("photo");
    let ext = base.extension().and_then(|s| s.to_str()).unwrap_or("jpg");
    for n in 2..=max {
        let candidate = dir.join(&format!("{stem} ({n}).{ext}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "no free name for \"{name}\" in this send's temp directory after {max} tries — \
         skipping this photo rather than overwriting an earlier render"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;
    use crate::localsend::test_receiver::{Receiver, Script};
    use crate::test_support::TestTmpDir;
    use std::sync::Mutex;

    /// Two selected photos sharing a basename must reach the device as two files.
    #[test]
    fn a_taken_name_is_disambiguated_rather_than_reused() {
        let dir = JobTempDir::new("localsend").unwrap();
        let first = unique_in_dir(&dir, "DSC01234.jpg").unwrap();
        std::fs::write(&first, b"first").unwrap();
        let second = unique_in_dir(&dir, "DSC01234.jpg").unwrap();
        assert_ne!(first, second, "the second render would overwrite the first");
        assert_eq!(second.file_name().unwrap(), "DSC01234 (2).jpg");
        std::fs::write(&second, b"second").unwrap();
        assert_eq!(std::fs::read(&first).unwrap(), b"first");
    }

    /// Running out of suffixes must fail, not hand back the name already in use.
    #[test]
    fn exhausting_the_suffixes_errors_instead_of_colliding() {
        let dir = JobTempDir::new("localsend").unwrap();
        for name in ["DSC01234.jpg", "DSC01234 (2).jpg", "DSC01234 (3).jpg"] {
            std::fs::write(dir.join(name), b"taken").unwrap();
        }
        let err = unique_in_dir_up_to(&dir, "DSC01234.jpg", 3).expect_err("every candidate name is taken");
        assert!(err.contains("DSC01234.jpg"), "{err}");
    }

    #[derive(Default)]
    struct Progress(Mutex<Vec<(usize, usize, u64)>>);
    impl EventSink for Progress {
        fn send(&self, event: CoreEvent) {
            if let CoreEvent::LocalSendProgress(p) = event {
                self.0.lock().unwrap().push((p.done, p.total, p.job));
            }
        }
    }

    /// A catalog in `dir` with `n` photos whose originals exist (a few bytes each).
    fn setup(dir: &Path, n: usize) -> (AppState, Arc<Progress>, Vec<i64>) {
        let root = dir.join("library");
        std::fs::create_dir_all(&root).unwrap();
        let state = AppState::default();
        let progress = Arc::new(Progress::default());
        state.set_events(progress.clone());
        let c = Catalog::open(&dir.join("c.chairphoto"), &root).unwrap();
        let ids = (0..n)
            .map(|i| {
                let p = root.join(format!("IMG_{i}.jpg"));
                std::fs::write(&p, format!("jpeg {i}")).unwrap();
                c.upsert_photo(&p, None, 0, 6).unwrap().id
            })
            .collect();
        *state.catalog.lock().unwrap() = Some(c);
        (state, progress, ids)
    }

    /// A renderer that copies the original, recording each render's directory.
    fn copying(dirs: Arc<Mutex<Vec<PathBuf>>>) -> ItemRenderer {
        Arc::new(move |item, out| {
            dirs.lock().unwrap().push(out.parent().unwrap().to_path_buf());
            std::fs::copy(&item.original, out).map(|_| ()).map_err(|e| e.to_string())
        })
    }

    fn device(receiver: &Receiver) -> Device {
        Device {
            alias: "Stub".into(),
            device_model: None,
            device_type: None,
            ip: "127.0.0.1".into(),
            port: receiver.port,
            protocol: "http".into(),
            fingerprint: "stub".into(),
        }
    }

    /// The whole path against a loopback receiver: every reachable photo is rendered into a
    /// private job directory, uploaded under its source name, progress carries this job's id,
    /// and the directory is gone afterwards. A PIN-protected receiver gets the PIN on retry.
    #[test]
    fn a_send_uploads_every_render_with_progress_and_leaves_no_temp_files() {
        let dir = TestTmpDir::new("localsend-send");
        let (state, progress, ids) = setup(&dir, 3);
        let receiver = Receiver::start(Script { pin: Some("4321".into()), ..Script::default() });
        let job = claim_send(&state, None, &ids, None).unwrap();
        let job_id = job.job;
        let dirs = Arc::new(Mutex::new(Vec::new()));
        let outcome = job.run_with(&device(&receiver), Some(" 4321 "), &copying(dirs.clone())).unwrap();
        assert_eq!(outcome, SendOutcome { sent: ids.clone(), failed: 0 });

        let log = receiver.log();
        let targets: Vec<&str> = log.iter().map(|r| r.target.as_str()).collect();
        assert!(targets[0].ends_with("/prepare-upload"), "{targets:?}");
        assert!(targets[1].ends_with("/prepare-upload?pin=4321"), "the PIN goes on the retry: {targets:?}");
        let uploads: Vec<_> = log.iter().filter(|r| r.target.contains("/upload?")).collect();
        assert_eq!(uploads.len(), 3);
        assert_eq!(uploads[0].body, b"jpeg 0", "the render's bytes, as the device receives them");
        let names = receiver.file_names();
        assert_eq!(names, ["IMG_0.jpg", "IMG_1.jpg", "IMG_2.jpg"], "named after the source photo");

        assert_eq!(*progress.0.lock().unwrap(), [(1, 3, job_id), (2, 3, job_id), (3, 3, job_id)]);
        let dirs = dirs.lock().unwrap();
        let job_dir = &dirs[0];
        assert!(dirs.iter().all(|d| d == job_dir), "one directory per send");
        #[cfg(unix)]
        {
            let name = job_dir.file_name().unwrap().to_string_lossy();
            assert!(name.starts_with("chairphoto-upload-localsend-") && name.len() > 40, "unpredictable: {name}");
        }
        assert!(!job_dir.exists(), "{} outlived the send", job_dir.display());
    }

    /// A receiver that requires a PIN refuses the send when none was given.
    #[test]
    fn a_pin_protected_receiver_without_a_pin_fails_and_cleans_up() {
        let dir = TestTmpDir::new("localsend-nopin");
        let (state, _, ids) = setup(&dir, 1);
        let receiver = Receiver::start(Script { pin: Some("4321".into()), ..Script::default() });
        let dirs = Arc::new(Mutex::new(Vec::new()));
        let job = claim_send(&state, None, &ids, None).unwrap();
        let err = job.run_with(&device(&receiver), None, &copying(dirs.clone())).unwrap_err();
        assert!(err.contains("requires a PIN"), "{err}");
        assert!(!dirs.lock().unwrap()[0].exists(), "a failed send removes its renders");
    }

    /// Cancel in the middle of an upload: the upload is dropped, the receiver's session is
    /// cancelled, the send answers `SEND_CANCELLED`, and the renders are removed.
    #[test]
    fn cancel_mid_upload_stops_the_send_and_cancels_the_session() {
        let dir = TestTmpDir::new("localsend-cancel");
        let (state, progress, ids) = setup(&dir, 2);
        let receiver = Receiver::start(Script { hold_uploads: true, ..Script::default() });
        let job = claim_send(&state, None, &ids, None).unwrap();
        let abort = job.abort_handle();
        let dirs = Arc::new(Mutex::new(Vec::new()));
        let dev = device(&receiver);
        let render = copying(dirs.clone());
        let run = std::thread::spawn(move || job.run_with(&dev, None, &render));
        receiver.wait_for(|log| log.iter().any(|r| r.target.contains("/upload?")));
        abort.store(true, Ordering::Relaxed);
        let err = run.join().unwrap().unwrap_err();
        assert_eq!(err, SEND_CANCELLED);
        receiver.wait_for(|log| log.iter().any(|r| r.target.contains("/cancel?sessionId=session-1")));
        assert!(progress.0.lock().unwrap().is_empty(), "no file completed");
        assert!(!dirs.lock().unwrap()[0].exists(), "a cancelled send removes its renders");
    }

    /// A catalog switch trips a claimed send: it renders and sends nothing. A newer claim
    /// trips an older one the same way.
    #[test]
    fn a_switch_or_a_newer_send_stops_a_claimed_send_before_it_renders() {
        let dir = TestTmpDir::new("localsend-switch");
        let (state, _, ids) = setup(&dir, 1);
        let receiver = Receiver::start(Script::default());
        let rendered = Arc::new(Mutex::new(Vec::new()));

        let job = claim_send(&state, None, &ids, None).unwrap();
        let other = TestTmpDir::new("localsend-switch-b");
        let (b, _, _) = setup(&other, 1);
        let catalog_b = b.catalog.lock().unwrap().take().unwrap();
        crate::app::publish_catalog_and_reset_jobs(&state, catalog_b).unwrap();
        let err = job.run_with(&device(&receiver), None, &copying(rendered.clone())).unwrap_err();
        assert_eq!(err, SEND_CANCELLED);

        let older = claim_send(&state, None, &ids, None).unwrap();
        let newer = claim_send(&state, None, &ids, None).unwrap();
        assert!(newer.job > older.job);
        let err = older.run_with(&device(&receiver), None, &copying(rendered.clone())).unwrap_err();
        assert_eq!(err, SEND_CANCELLED);
        assert!(rendered.lock().unwrap().is_empty(), "nothing was rendered");
        assert!(receiver.log().is_empty(), "nothing reached the device");
    }

    /// A send bound to the catalog its ids were read from refuses once another is open — even
    /// with colliding ids — and claims nothing.
    #[test]
    fn a_claim_bound_to_a_closed_catalog_refuses() {
        let dir = TestTmpDir::new("localsend-identity");
        let (state, _, ids) = setup(&dir, 1);
        let read = super::super::catalog_identity(&state).unwrap();
        let other = TestTmpDir::new("localsend-identity-b");
        let (b, _, ids_b) = setup(&other, 1);
        assert_eq!(ids, ids_b, "the ids collide");
        let catalog_b = b.catalog.lock().unwrap().take();
        *state.catalog.lock().unwrap() = catalog_b;
        let before = state.jobs.localsend.job_ids_issued();
        let err = claim_send(&state, Some(read), &ids, None).err().unwrap();
        assert_eq!(err, CATALOG_CHANGED);
        assert_eq!(state.jobs.localsend.job_ids_issued(), before, "nothing claimed");
    }

    /// An offline original is counted as failed; a failed render is skipped and counted; only
    /// the photos that reached the device are reported sent.
    #[test]
    fn unavailable_and_failed_photos_are_counted_not_sent() {
        let dir = TestTmpDir::new("localsend-partial");
        let (state, _, ids) = setup(&dir, 3);
        // Photo 0's original goes offline.
        std::fs::remove_file(dir.join("library/IMG_0.jpg")).unwrap();
        let receiver = Receiver::start(Script::default());
        let job = claim_send(&state, None, &ids, None).unwrap();
        assert_eq!(job.photo_count(), 2);
        let fail_second: ItemRenderer = {
            let id = ids[1];
            Arc::new(move |item, out| {
                if item.photo_id == id {
                    return Err("decoder said no".into());
                }
                std::fs::copy(&item.original, out).map(|_| ()).map_err(|e| e.to_string())
            })
        };
        let outcome = job.run_with(&device(&receiver), None, &fail_second).unwrap();
        assert_eq!(outcome, SendOutcome { sent: vec![ids[2]], failed: 2 });
        assert_eq!(receiver.file_names(), ["IMG_2.jpg"]);

        let all_fail: ItemRenderer = Arc::new(|_, _| Err("no".into()));
        let job = claim_send(&state, None, &ids, None).unwrap();
        let err = job.run_with(&device(&receiver), None, &all_fail).unwrap_err();
        assert!(err.contains("could be rendered"), "{err}");

        std::fs::remove_file(dir.join("library/IMG_1.jpg")).unwrap();
        std::fs::remove_file(dir.join("library/IMG_2.jpg")).unwrap();
        let err = claim_send(&state, None, &ids, None).err().unwrap();
        assert!(err.contains("None of the selected photos are available"), "{err}");
        assert!(claim_send(&state, None, &[], None).is_err(), "an empty selection is refused");
    }
}
