//! Rendering a slideshow movie — the body of the Tauri `make_slideshow` command and of the
//! GPUI Slideshow module's Render (docs/slideshow.md).
//!
//! A render is a job (`JobRegistry::slideshow`): [`claim_slideshow`] resolves the photos'
//! Originals and takes ownership (catalog → the slideshow abort, one transition), and
//! [`SlideshowJob::run`] does the work on the caller's worker: one frame JPEG per photo into a
//! private cache directory ([`FrameDir`], not a quota-limited `/tmp`), then the ffmpeg encode.
//! A newer render, Cancel (tripping the job's own
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
    let dest_dir = output_folder(dest_dir)?;
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
        dest_dir,
        ffmpeg,
        abort,
        job,
    })
}

/// What a render answers when the output folder is not an absolute local path.
pub const OUTPUT_FOLDER_NOT_LOCAL: &str =
    "The output folder must be a full local folder path (like ~/Videos), not a URL or a relative path.";

/// The output folder as an absolute local path (`~` expanded). Anything else — a relative
/// path, a URL such as `ftp://…`, a name starting with `-` — is refused: the movie path is
/// handed to ffmpeg, which would read a URL as a network output and a leading `-` as an
/// option.
fn output_folder(dest_dir: &str) -> Result<PathBuf, String> {
    let dest_dir = dest_dir.trim();
    if dest_dir.is_empty() {
        return Err("Choose an output folder.".into());
    }
    let path = super::expand_home(dest_dir);
    if !path.is_absolute() {
        return Err(OUTPUT_FOLDER_NOT_LOCAL.into());
    }
    Ok(path)
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
    /// The frames go to a `FrameDir` private to this run, so no other render — and no other
    /// user — reads or removes them. The movie goes to a fresh `slideshow.mp4` /
    /// `slideshow (N).mp4` in the destination, reserved by an exclusive create before ffmpeg
    /// starts; nothing else there is touched, and a cancelled or failed encode removes its own
    /// reservation / partial movie.
    pub fn run_with(self, write_frame: &FrameWriter) -> Result<PathBuf, String> {
        let SlideshowJob { state, items, opts, dest_dir, ffmpeg, abort, job } = self;
        let cancelled = || abort.load(Ordering::Relaxed);
        let work = FrameDir::create()?;
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
                let frame = work.0.join(format!("frame_{i:04}.jpg"));
                write_frame(item, frame_max_width, &frame)?;
                frames.push(frame);
            }
            if cancelled() {
                return Err(SLIDESHOW_CANCELLED.to_string());
            }
            std::fs::create_dir_all(&dest_dir).map_err(|e| e.to_string())?;
            // The name is reserved (an exclusive create), so no other render — in this
            // process or another — can choose it, and the cleanup below removes only ours.
            // ffmpeg then overwrites that empty reservation (`-y`).
            let dest = super::reserve_unique_path(&dest_dir.join("slideshow.mp4"))?;
            let encoded = crate::slideshow::render_with(&ffmpeg, &frames, &opts, &dest, &abort, |done, total| {
                state.send(CoreEvent::SlideshowProgress(SlideshowProgress { done, total, job }));
            });
            match encoded {
                Ok(()) => Ok(dest),
                Err(e) => {
                    // Our own reservation / partial output, never another render's.
                    let _ = std::fs::remove_file(&dest);
                    Err(e)
                }
            }
        })();
        drop(work);
        result
    }
}

/// A render's frame directory: a fresh `<random>` directory under the app's cache dir
/// (`crate::thumbnails::cache_dir()/chairphoto/slideshow`, not `std::env::temp_dir()` — on
/// some machines `/tmp` is a quota-limited tmpfs, and up to 4096px frames for a long
/// slideshow could fill it, #211), created exclusively with mode 0700 (the frames are
/// full-size renders of the user's photos; the random name cannot be predicted or
/// pre-created by another user). Dropping it removes the directory, so it goes on every
/// exit — success, error, cancel, or a panic in a frame writer.
struct FrameDir(PathBuf);

impl FrameDir {
    fn create() -> Result<Self, String> {
        let base = crate::thumbnails::cache_dir().join("chairphoto").join("slideshow");
        std::fs::create_dir_all(&base).map_err(|e| format!("{}: {e}", base.display()))?;
        let path = base.join(uuid::Uuid::new_v4().simple().to_string());
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        // `create`, not `create_dir_all`: an existing directory of that name is an error,
        // never adopted.
        builder.create(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(FrameDir(path))
    }
}

impl Drop for FrameDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
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
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
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

    /// A fake ffmpeg (`tests/fixtures/ffmpeg/`): a checked-in shell script that reports two
    /// progress blocks and writes the output (the last argument), or — `hang` — sleeps until
    /// killed. Never written at run time: a script written and then executed while other test
    /// threads fork can fail with ETXTBSY (a forked child holding the write descriptor).
    fn fake_ffmpeg(hang: bool) -> PathBuf {
        fixture(if hang { "ffmpeg-hang" } else { "ffmpeg-ok" })
    }

    /// A checked-in fake ffmpeg by name; `ffmpeg-stall` reports one progress line and sleeps
    /// without ever creating its output.
    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ffmpeg").join(name)
    }

    /// A frame writer that copies the original (no thumbnail cache, no exiftool).
    fn copy_frames() -> FrameWriter {
        Arc::new(|item, _, out| std::fs::copy(&item.original, out).map(|_| ()).map_err(|e| e.to_string()))
    }

    /// [`copy_frames`], recording each frame's directory and its permission bits.
    fn recording_frames(seen: Arc<Mutex<Vec<(PathBuf, u32)>>>) -> FrameWriter {
        Arc::new(move |item, _, out| {
            let dir = out.parent().unwrap().to_path_buf();
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            seen.lock().unwrap().push((dir, mode));
            std::fs::copy(&item.original, out).map(|_| ()).map_err(|e| e.to_string())
        })
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
        let job = claim_slideshow(&state, None, &ids, opts(), out.to_str().unwrap(), Some(fake_ffmpeg(false))).unwrap();
        let id = job.job;
        let dirs = Arc::new(Mutex::new(Vec::new()));
        let path = job.run_with(&recording_frames(dirs.clone())).unwrap();
        assert_eq!(path, out.join("slideshow (2).mp4"), "never clobbers an earlier movie");
        assert_eq!(std::fs::read(&path).unwrap(), b"movie");
        assert_eq!(std::fs::read(out.join("slideshow.mp4")).unwrap(), b"earlier");
        let seen = progress.0.lock().unwrap().clone();
        assert_eq!(seen.first().map(|p| (p.0, p.2)), Some((10, id)));
        assert_eq!(seen.last().map(|p| (p.0 == p.1, p.2)), Some((true, id)), "progress=end reports done == total");
        let (work, _) = dirs.lock().unwrap()[0].clone();
        assert!(!work.exists(), "the job's frames are removed");
        for i in 0..2 {
            assert_eq!(std::fs::read(dir.join("library").join(format!("IMG_{i}.jpg"))).unwrap(), format!("jpeg {i}").as_bytes(), "originals untouched");
        }
    }

    /// #211: frames render into the app's cache dir, not `std::env::temp_dir()` — on this
    /// machine `/tmp` is a quota-limited tmpfs that a long slideshow's frames could fill.
    #[test]
    fn frames_render_into_the_cache_dir_not_the_system_temp_dir() {
        let (dir, state, _progress, ids) = setup("cachedir", 1);
        let out = dir.join("out");
        let job = claim_slideshow(&state, None, &ids, opts(), out.to_str().unwrap(), Some(fake_ffmpeg(false))).unwrap();
        let dirs = Arc::new(Mutex::new(Vec::new()));
        job.run_with(&recording_frames(dirs.clone())).unwrap();
        let (frame_dir, _mode) = dirs.lock().unwrap()[0].clone();
        let cache_root = crate::thumbnails::cache_dir().join("chairphoto").join("slideshow");
        assert!(frame_dir.starts_with(&cache_root), "{frame_dir:?} is under the cache dir {cache_root:?}");
        let tmp_root = std::env::temp_dir();
        if tmp_root != cache_root {
            assert!(!frame_dir.starts_with(&tmp_root), "{frame_dir:?} is not under the system temp dir {tmp_root:?}");
        }
    }

    /// The fake ffmpegs are executable checked-in files the helper only names: asking for
    /// one never opens it for writing, so no forked child can hold it busy (ETXTBSY).
    #[test]
    fn the_fake_ffmpegs_are_checked_in_and_never_written() {
        for hang in [false, true] {
            let path = fake_ffmpeg(hang);
            let before = std::fs::metadata(&path).unwrap();
            assert!(before.permissions().mode() & 0o111 != 0, "{path:?} is executable");
            std::thread::sleep(std::time::Duration::from_millis(20));
            assert_eq!(fake_ffmpeg(hang), path);
            let after = std::fs::metadata(&path).unwrap();
            assert_eq!(
                (after.mtime(), after.mtime_nsec(), after.ino()),
                (before.mtime(), before.mtime_nsec(), before.ino()),
                "{path:?} was rewritten"
            );
        }
    }

    /// The frame dir is 0700, named unpredictably (two renders with the same job id and pid
    /// get different dirs), and removed even when a frame writer panics or fails.
    #[test]
    fn the_frame_dir_is_private_unpredictable_and_removed_even_on_a_panic() {
        let (a_dir, a, _ap, a_ids) = setup("frames-a", 2);
        let (b_dir, b, _bp, b_ids) = setup("frames-b", 2);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let out = |d: &TestTmpDir| d.join("out").to_str().unwrap().to_string();
        let ja = claim_slideshow(&a, None, &a_ids, opts(), &out(&a_dir), Some(fake_ffmpeg(false))).unwrap();
        let jb = claim_slideshow(&b, None, &b_ids, opts(), &out(&b_dir), Some(fake_ffmpeg(false))).unwrap();
        assert_eq!(ja.job, jb.job, "same pid, same job id");
        ja.run_with(&recording_frames(seen.clone())).unwrap();
        jb.run_with(&recording_frames(seen.clone())).unwrap();
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 4);
        assert!(seen.iter().all(|(_, mode)| *mode == 0o700), "frame dirs are owner-only: {seen:?}");
        assert_ne!(seen[0].0, seen[2].0, "each render gets its own unpredictable dir");
        assert!(seen.iter().all(|(d, _)| !d.exists()), "removed after success");

        // **Forced interleaving.** Concurrent renders from two app states (same pid, both job
        // 1 — as parallel tests in one process are): while A's ffmpeg runs on its frames, B
        // renders to completion. With frame dirs named from pid + job id, B's frames landed
        // in A's dir and B's cleanup removed A's frames under it (an ENOENT flake seen in
        // a_render_writes_a_fresh_movie_…).
        let (c_dir, c, c_progress, c_ids) = setup("frames-c", 2);
        let (d_dir, d, _dp, d_ids) = setup("frames-d", 2);
        let jc = claim_slideshow(&c, None, &c_ids, opts(), &out(&c_dir), Some(fixture("ffmpeg-stall"))).unwrap();
        let jd = claim_slideshow(&d, None, &d_ids, opts(), &out(&d_dir), Some(fake_ffmpeg(false))).unwrap();
        assert_eq!(jc.job, jd.job, "same pid, same job id");
        let (c_abort, c_job) = (jc.abort_handle(), jc.job);
        let c_seen = Arc::new(Mutex::new(Vec::new()));
        let c_writer = recording_frames(c_seen.clone());
        let c_run = std::thread::spawn(move || jc.run_with(&c_writer));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !c_progress.0.lock().unwrap().iter().any(|p| p.2 == c_job) {
            assert!(std::time::Instant::now() < deadline && !c_run.is_finished(), "C's ffmpeg never started");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let c_frames = c_seen.lock().unwrap()[0].0.clone();
        jd.run_with(&copy_frames()).unwrap();
        for i in 0..2 {
            let frame = c_frames.join(format!("frame_{i:04}.jpg"));
            assert_eq!(std::fs::read(&frame).ok(), Some(format!("jpeg {i}").into_bytes()), "D left C's running frames alone: {frame:?}");
        }
        c_abort.store(true, Ordering::Relaxed);
        assert_eq!(c_run.join().unwrap().unwrap_err(), SLIDESHOW_CANCELLED);
        assert!(!c_frames.exists(), "C removed its own frames");

        for how in ["panic", "error"] {
            let (dir, state, _p, ids) = setup(&format!("frames-{how}"), 2);
            let job = claim_slideshow(&state, None, &ids, opts(), &out(&dir), Some(fake_ffmpeg(false))).unwrap();
            let used = Arc::new(Mutex::new(None));
            let u = used.clone();
            let panics = how == "panic";
            let writer: FrameWriter = Arc::new(move |_, _, out| {
                *u.lock().unwrap() = Some(out.parent().unwrap().to_path_buf());
                if panics {
                    panic!("frame writer panicked");
                }
                Err("frame writer failed".into())
            });
            let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job.run_with(&writer)));
            assert_eq!(ran.is_err(), panics, "{how}");
            let frames = used.lock().unwrap().clone().expect("the writer ran");
            assert!(!frames.exists(), "{how}: the frame dir is removed: {frames:?}");
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
    fn an_output_folder_that_is_not_an_absolute_local_path_is_refused_before_the_claim() {
        let (dir, state, _p, ids) = setup("folder", 2);
        let before = state.jobs.slideshow.job_ids_issued();
        for folder in ["-x", "ftp://example.com/movies", "relative/out", "file:out"] {
            let err = claim_slideshow(&state, None, &ids, opts(), folder, Some(fake_ffmpeg(false))).err();
            assert_eq!(err.as_deref(), Some(OUTPUT_FOLDER_NOT_LOCAL), "{folder}");
        }
        assert_eq!(state.jobs.slideshow.job_ids_issued(), before, "no job claimed");
        assert!(!dir.join("-x").exists() && !std::path::Path::new("-x").exists(), "nothing created");
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
        let err = claim_slideshow(&state, Some(identity), &ids, opts(), dir.to_str().unwrap(), Some(fake_ffmpeg(false)))
            .err()
            .unwrap();
        assert_eq!(err, CATALOG_CHANGED);
    }

    /// **Forced interleaving.** Cancel, a newer render and a catalog switch each land while
    /// ffmpeg is running: the fake writes a partial movie, reports one progress line — the
    /// "started" signal the test waits for — and sleeps. ffmpeg is killed, the render answers
    /// cancelled and its partial movie is gone.
    #[test]
    fn cancel_a_newer_render_or_a_switch_kills_a_running_encode() {
        // Switches catalogs: phase one releases develop's process-wide resident image, so
        // this must not interleave with the develop tests that assert on it (#133).
        let _serial = crate::develop::serial();
        for how in ["cancel", "newer", "switch"] {
            let (dir, state, progress, ids) = setup(&format!("abort-{how}"), 2);
            let out = dir.join("out");
            let job = claim_slideshow(&state, None, &ids, opts(), out.to_str().unwrap(), Some(fake_ffmpeg(true))).unwrap();
            let (handle, id) = (job.abort_handle(), job.job);
            let started = std::time::Instant::now();
            let runner = std::thread::spawn(move || job.run_with(&copy_frames()));
            // Wait for ffmpeg's own "started" line (its first progress, this job's id), so the
            // trip below lands while it runs, not before it was spawned.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !progress.0.lock().unwrap().iter().any(|p| p.2 == id) {
                assert!(std::time::Instant::now() < deadline, "{how}: ffmpeg never started");
                assert!(!runner.is_finished(), "{how}: the render ended before ffmpeg started");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(!runner.is_finished(), "{how}: ffmpeg is running at the trip");
            match how {
                "cancel" => handle.store(true, Ordering::Relaxed),
                "newer" => drop(claim_slideshow(&state, None, &ids, opts(), out.to_str().unwrap(), Some(fake_ffmpeg(false))).unwrap()),
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

    /// **Forced interleaving.** Two renders into one folder that do not trip each other (two
    /// app instances): A's ffmpeg is running but has not created its output yet when B
    /// renders. B must not get A's name, and A's cancel must not remove B's movie.
    #[test]
    fn a_render_never_takes_or_removes_a_name_another_render_chose() {
        let (dir, a_state, a_progress, a_ids) = setup("race-a", 2);
        let (_b_dir, b_state, _bp, b_ids) = setup("race-b", 2);
        let out = dir.join("out");
        let a = claim_slideshow(&a_state, None, &a_ids, opts(), out.to_str().unwrap(), Some(fixture("ffmpeg-stall"))).unwrap();
        let (a_abort, a_job) = (a.abort_handle(), a.job);
        let a_run = std::thread::spawn(move || a.run_with(&copy_frames()));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !a_progress.0.lock().unwrap().iter().any(|p| p.2 == a_job) {
            assert!(std::time::Instant::now() < deadline && !a_run.is_finished(), "A's ffmpeg never started");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let b = claim_slideshow(&b_state, None, &b_ids, opts(), out.to_str().unwrap(), Some(fake_ffmpeg(false))).unwrap();
        let b_movie = b.run_with(&copy_frames()).unwrap();
        a_abort.store(true, Ordering::Relaxed);
        assert_eq!(a_run.join().unwrap().unwrap_err(), SLIDESHOW_CANCELLED);
        assert_eq!(std::fs::read(&b_movie).ok().as_deref(), Some(&b"movie"[..]), "A's cleanup left B's movie alone");
        assert_eq!(b_movie, out.join("slideshow (2).mp4"), "A had reserved slideshow.mp4");
        assert!(!out.join("slideshow.mp4").exists(), "A removed its own reservation");
    }

    #[test]
    fn reserve_unique_path_takes_each_name_once() {
        let dir = TestTmpDir::new("reserve");
        let want = dir.join("slideshow.mp4");
        std::fs::write(&want, b"earlier").unwrap();
        let first = crate::app::reserve_unique_path(&want).unwrap();
        let second = crate::app::reserve_unique_path(&want).unwrap();
        assert_eq!((first.clone(), second.clone()), (dir.join("slideshow (2).mp4"), dir.join("slideshow (3).mp4")));
        assert_eq!(std::fs::read(&want).unwrap(), b"earlier");
        assert!(first.exists() && second.exists(), "each name is taken on disk");
        assert!(crate::app::reserve_unique_path(&dir.join("missing/x.mp4")).is_err());
    }

    #[test]
    fn a_cancel_before_the_encode_stops_between_frames() {
        let (dir, state, _p, ids) = setup("between", 3);
        let job = claim_slideshow(&state, None, &ids, opts(), dir.join("out").to_str().unwrap(), Some(fake_ffmpeg(false))).unwrap();
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
