//! Rendering a slideshow movie — the body of the Tauri `make_slideshow` command and of the
//! GPUI Slideshow module's Render (docs/slideshow.md).
//!
//! A render is a job (`JobRegistry::slideshow`): [`claim_slideshow`] resolves the photos'
//! Originals and takes ownership (catalog → the slideshow abort, one transition), and
//! [`SlideshowJob::run`] does the work on the caller's worker: one frame JPEG per photo into a
//! private temp dir, then the ffmpeg encode. A newer render, Cancel (tripping the job's own
//! [`SlideshowJob::abort_handle`]) or a catalog switch trips it; the encode then kills ffmpeg,
//! removes the partial movie and answers [`SLIDESHOW_CANCELLED`].
//!
//! Progress is `slideshow:progress` carrying the job id; it is cosmetic. The terminal result
//! is what `run` returns.

use super::{AppState, CatalogIdentity, CoreEvent, EventSink, SlideshowProgress, CATALOG_CHANGED};
use crate::export::ResolvedItem;
pub use crate::slideshow::{SlideshowOptions, FFMPEG_MISSING, SLIDESHOW_CANCELLED};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Renders one photo's still frame (`item`, `max_width`, destination). The production one is
/// [`export_frame`]; tests hand in one that needs no thumbnail cache.
pub type FrameWriter = Arc<dyn Fn(&ResolvedItem, u32, &Path) -> Result<(), String> + Send + Sync>;

/// The production frame writer: the item's Original through the export path, then its EXIF
/// orientation baked into the pixels (ffmpeg ignores EXIF orientation on still inputs, so a
/// portrait shot would otherwise come out sideways).
pub fn export_frame() -> FrameWriter {
    Arc::new(|item, max_width, out| {
        crate::export::write_item_jpeg(item, Some(max_width), out)?;
        bake_orientation(out)
    })
}

/// Rotate a rendered frame's pixels to its EXIF display orientation and drop the tag.
fn bake_orientation(path: &Path) -> Result<(), String> {
    use image::ImageDecoder;
    let reader = image::ImageReader::open(path)
        .map_err(|e| e.to_string())?
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let orientation = decoder.orientation().map_err(|e| e.to_string())?;
    if orientation == image::metadata::Orientation::NoTransforms {
        return Ok(()); // already upright — nothing to bake
    }
    let mut img = image::DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    img.apply_orientation(orientation);
    img.save(path).map_err(|e| e.to_string())
}

/// A claimed render, not yet running. Dropping it runs nothing; its abort flag stays
/// installed until a newer render, Cancel or switch replaces it.
pub struct SlideshowJob {
    state: AppState,
    items: Vec<ResolvedItem>,
    opts: SlideshowOptions,
    dest_dir: PathBuf,
    ffmpeg: PathBuf,
    abort: Arc<AtomicBool>,
    /// The render's job id: every `slideshow:progress` it sends carries it.
    pub job: u64,
}

/// Claim a render of `photo_ids` (in play order) into `dest_dir` (`~` expanded).
///
/// `expected`: the catalog the ids were read from — `Some` from a front end that captured it
/// (fails closed with [`CATALOG_CHANGED`] once another catalog is open), `None` for "the open
/// one". `ffmpeg`: the binary to run, `None` when it is not installed — refused with
/// [`FFMPEG_MISSING`] before anything is claimed or rendered.
///
/// The identity check, the resolve and the claim run under one catalog lock, taking the
/// slideshow abort inside it (catalog → abort): a switch either lands first (the check
/// fails) or after (its trip reaches this render). Blocking (catalog lock) — not on a UI
/// thread.
pub fn claim_slideshow(
    state: &AppState,
    expected: Option<CatalogIdentity>,
    photo_ids: &[i64],
    opts: SlideshowOptions,
    dest_dir: &str,
    ffmpeg: Option<PathBuf>,
) -> Result<SlideshowJob, String> {
    if photo_ids.is_empty() {
        return Err("No photos selected for the slideshow".into());
    }
    let ffmpeg = ffmpeg.ok_or_else(|| FFMPEG_MISSING.to_string())?;
    if dest_dir.trim().is_empty() {
        return Err("Choose an output folder.".into());
    }
    let guard = state.catalog.lock().map_err(|e| e.to_string())?;
    let catalog = guard.as_ref().ok_or("No catalog is open")?;
    if expected.is_some_and(|e| !e.is(catalog)) {
        return Err(CATALOG_CHANGED.into());
    }
    // The export resolver: an unmounted NAS / offline original is skipped, not fatal.
    let resolved = crate::export::resolve_originals(catalog, photo_ids, &[], None);
    if resolved.items.is_empty() {
        return Err("None of the selected photos are reachable (their volumes may be offline)".into());
    }
    let (abort, job) = state.jobs.slideshow.install_fresh_numbered()?;
    drop(guard);
    Ok(SlideshowJob {
        state: state.clone(),
        items: resolved.items,
        opts,
        dest_dir: super::expand_home(dest_dir.trim()),
        ffmpeg,
        abort,
        job,
    })
}

impl SlideshowJob {
    /// This render's own abort flag: tripping it cancels this render and no other.
    pub fn abort_handle(&self) -> Arc<AtomicBool> {
        self.abort.clone()
    }

    /// How many photos will be in the movie (the reachable ones).
    pub fn photo_count(&self) -> usize {
        self.items.len()
    }

    /// Render the frames and encode the movie with the production [`export_frame`]. Returns
    /// the movie's path. Blocking.
    pub fn run(self) -> Result<PathBuf, String> {
        self.run_with(&export_frame())
    }

    /// [`run`](Self::run) with an explicit frame writer.
    ///
    /// The frames go to a temp dir private to this job (process id and job id), so a
    /// superseded render's cleanup never removes a newer render's frames. The movie goes to a
    /// fresh `slideshow.mp4` / `slideshow (N).mp4` in the destination; nothing else there is
    /// touched, and a cancelled or failed encode removes its own partial movie.
    pub fn run_with(self, write_frame: &FrameWriter) -> Result<PathBuf, String> {
        let SlideshowJob { state, items, opts, dest_dir, ffmpeg, abort, job } = self;
        let cancelled = || abort.load(Ordering::Relaxed);
        let work = std::env::temp_dir().join(format!("chairphoto_slideshow_{}_{job}", std::process::id()));
        std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
        let result = (|| {
            // Frames comfortably above the output, so the Ken Burns zoom (the engine
            // oversamples to 2× the target) has pixels to crop into; capped so a huge RAW
            // doesn't produce gigantic intermediate JPEGs.
            let frame_max_width = (opts.width.max(opts.height) * 2).min(4096);
            let mut frames = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                if cancelled() {
                    return Err(SLIDESHOW_CANCELLED.to_string());
                }
                let frame = work.join(format!("frame_{i:04}.jpg"));
                write_frame(item, frame_max_width, &frame)?;
                frames.push(frame);
            }
            if cancelled() {
                return Err(SLIDESHOW_CANCELLED.to_string());
            }
            std::fs::create_dir_all(&dest_dir).map_err(|e| e.to_string())?;
            let dest = super::unique_path(&dest_dir.join("slideshow.mp4"));
            let encoded = crate::slideshow::render_with(&ffmpeg, &frames, &opts, &dest, &abort, |done, total| {
                state.send(CoreEvent::SlideshowProgress(SlideshowProgress { done, total, job }));
            });
            match encoded {
                Ok(()) => Ok(dest),
                Err(e) => {
                    // Our own partial output: the unique path did not exist before this job.
                    let _ = std::fs::remove_file(&dest);
                    Err(e)
                }
            }
        })();
        let _ = std::fs::remove_dir_all(&work);
        result
    }
}

/// Trip the installed render (the Tauri shell's single-render Cancel). A no-op when idle.
pub fn cancel_slideshow(state: &AppState) -> Result<(), String> {
    state.jobs.slideshow.trip()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::catalog::Catalog;
    use crate::test_support::TestTmpDir;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Progress(Mutex<Vec<(u32, u32, u64)>>);
    impl EventSink for Progress {
        fn send(&self, event: CoreEvent) {
            if let CoreEvent::SlideshowProgress(p) = event {
                self.0.lock().unwrap().push((p.done, p.total, p.job));
            }
        }
    }

    /// A fake ffmpeg: a shell script that reports two progress blocks and writes the output
    /// (the last argument), or — `hang` — replaces itself with a long sleep.
    fn fake_ffmpeg(dir: &Path, hang: bool) -> PathBuf {
        let path = dir.join(if hang { "ffmpeg-hang" } else { "ffmpeg-ok" });
        let body = if hang {
            "#!/bin/sh\nexec sleep 30\n".to_string()
        } else {
            "#!/bin/sh\nfor last; do :; done\necho frame=10\necho progress=continue\necho frame=40\n\
             echo progress=end\nprintf movie > \"$last\"\n"
                .to_string()
        };
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// A frame writer that copies the original (no thumbnail cache, no exiftool).
    fn copy_frames() -> FrameWriter {
        Arc::new(|item, _, out| std::fs::copy(&item.original, out).map(|_| ()).map_err(|e| e.to_string()))
    }

    fn setup(tag: &str, photos: usize) -> (TestTmpDir, AppState, Arc<Progress>, Vec<i64>) {
        let dir = TestTmpDir::new(&format!("slideshow-{tag}"));
        let root = dir.join("library");
        std::fs::create_dir_all(&root).unwrap();
        let state = AppState::default();
        let progress = Arc::new(Progress::default());
        state.set_events(progress.clone());
        let c = Catalog::open(&dir.join("c.chairphoto"), &root).unwrap();
        let ids = (0..photos)
            .map(|i| {
                let p = root.join(format!("IMG_{i}.jpg"));
                std::fs::write(&p, format!("jpeg {i}")).unwrap();
                c.upsert_photo(&p, None, 0, 6).unwrap().id
            })
            .collect();
        *state.catalog.lock().unwrap() = Some(c);
        (dir, state, progress, ids)
    }

    fn opts() -> SlideshowOptions {
        SlideshowOptions { duration_per_photo: 1.0, transition: false, ..Default::default() }
    }

    #[test]
    fn a_render_writes_a_fresh_movie_reports_progress_with_its_job_and_cleans_its_frames() {
        let (dir, state, progress, ids) = setup("ok", 2);
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("slideshow.mp4"), b"earlier").unwrap();
        let job = claim_slideshow(&state, None, &ids, opts(), out.to_str().unwrap(), Some(fake_ffmpeg(&dir, false))).unwrap();
        let id = job.job;
        let path = job.run_with(&copy_frames()).unwrap();
        assert_eq!(path, out.join("slideshow (2).mp4"), "never clobbers an earlier movie");
        assert_eq!(std::fs::read(&path).unwrap(), b"movie");
        assert_eq!(std::fs::read(out.join("slideshow.mp4")).unwrap(), b"earlier");
        let seen = progress.0.lock().unwrap().clone();
        assert_eq!(seen.first().map(|p| (p.0, p.2)), Some((10, id)));
        assert_eq!(seen.last().map(|p| (p.0 == p.1, p.2)), Some((true, id)), "progress=end reports done == total");
        let work = std::env::temp_dir().join(format!("chairphoto_slideshow_{}_{id}", std::process::id()));
        assert!(!work.exists(), "the job's frames are removed");
        for i in 0..2 {
            assert_eq!(std::fs::read(dir.join("library").join(format!("IMG_{i}.jpg"))).unwrap(), format!("jpeg {i}").as_bytes(), "originals untouched");
        }
    }

    #[test]
    fn a_missing_ffmpeg_is_refused_before_anything_is_claimed() {
        let (_dir, state, _p, ids) = setup("missing", 2);
        let before = state.jobs.slideshow.job_ids_issued();
        let err = claim_slideshow(&state, None, &ids, opts(), "~/Videos", None).err().unwrap();
        assert_eq!(err, FFMPEG_MISSING);
        assert_eq!(state.jobs.slideshow.job_ids_issued(), before, "no job claimed");
    }

    #[test]
    fn a_claim_bound_to_another_catalog_fails_closed() {
        // Switches catalogs: phase one releases develop's process-wide resident image, so
        // this must not interleave with the develop tests that assert on it (#133).
        let _serial = crate::develop::serial();
        let (dir, state, _p, ids) = setup("identity", 2);
        let identity = crate::app::catalog_identity(&state).unwrap();
        crate::app::catalogs::detach_catalog_and_trip_jobs(&state).unwrap();
        let b = Catalog::open(&dir.join("b.chairphoto"), &dir.join("library")).unwrap();
        crate::app::catalogs::publish_catalog_and_reset_jobs(&state, b).unwrap();
        let err = claim_slideshow(&state, Some(identity), &ids, opts(), dir.to_str().unwrap(), Some(fake_ffmpeg(&dir, false)))
            .err()
            .unwrap();
        assert_eq!(err, CATALOG_CHANGED);
    }

    /// **Forced interleaving.** Cancel, a newer render and a catalog switch each land while
    /// ffmpeg is running (a fake that sleeps): ffmpeg is killed, the render answers
    /// cancelled and its partial movie is gone.
    #[test]
    fn cancel_a_newer_render_or_a_switch_kills_a_running_encode() {
        // Switches catalogs: phase one releases develop's process-wide resident image, so
        // this must not interleave with the develop tests that assert on it (#133).
        let _serial = crate::develop::serial();
        for how in ["cancel", "newer", "switch"] {
            let (dir, state, _p, ids) = setup(&format!("abort-{how}"), 2);
            let out = dir.join("out");
            let job = claim_slideshow(&state, None, &ids, opts(), out.to_str().unwrap(), Some(fake_ffmpeg(&dir, true))).unwrap();
            let handle = job.abort_handle();
            let started = std::time::Instant::now();
            let runner = std::thread::spawn(move || job.run_with(&copy_frames()));
            // The frames are copied quickly; give the encode a moment to be running.
            std::thread::sleep(std::time::Duration::from_millis(200));
            match how {
                "cancel" => handle.store(true, Ordering::Relaxed),
                "newer" => drop(claim_slideshow(&state, None, &ids, opts(), out.to_str().unwrap(), Some(fake_ffmpeg(&dir, false))).unwrap()),
                _ => {
                    crate::app::catalogs::detach_catalog_and_trip_jobs(&state).unwrap();
                    let b = Catalog::open(&dir.join("b.chairphoto"), &dir.join("b")).unwrap();
                    crate::app::catalogs::publish_catalog_and_reset_jobs(&state, b).unwrap();
                }
            }
            let err = runner.join().unwrap().unwrap_err();
            assert_eq!(err, SLIDESHOW_CANCELLED, "{how}");
            assert!(started.elapsed() < std::time::Duration::from_secs(10), "{how}: ffmpeg was killed, not waited out");
            let left: Vec<_> = std::fs::read_dir(&out).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
            assert!(left.is_empty(), "{how}: the partial movie is removed: {left:?}");
        }
    }

    #[test]
    fn a_cancel_before_the_encode_stops_between_frames() {
        let (dir, state, _p, ids) = setup("between", 3);
        let job = claim_slideshow(&state, None, &ids, opts(), dir.join("out").to_str().unwrap(), Some(fake_ffmpeg(&dir, false))).unwrap();
        let handle = job.abort_handle();
        let written = Arc::new(Mutex::new(0));
        let w = written.clone();
        let writer: FrameWriter = Arc::new(move |item, _, out| {
            *w.lock().unwrap() += 1;
            handle.store(true, Ordering::Relaxed); // Cancel lands during the first frame
            std::fs::copy(&item.original, out).map(|_| ()).map_err(|e| e.to_string())
        });
        assert_eq!(job.run_with(&writer).unwrap_err(), SLIDESHOW_CANCELLED);
        assert_eq!(*written.lock().unwrap(), 1, "no frame after the cancel");
        assert!(!dir.join("out").exists(), "the encode never started");
    }
}
