//! Headless tests of the Slideshow module: the action opens the dialog over the selection;
//! Render claims and runs core's job on the worker (a fake ffmpeg — a shell script — never the
//! real one); progress is this job's only; Cancel before and after the claim; missing ffmpeg;
//! a catalog switch with and without `catalog:switched` delivered.

use super::view::SlideshowDialog;
use super::{SlideshowBackend, SLIDESHOW_ACTION, SLIDESHOW_ID};
use crate::modules::dialog::DialogHost;
use crate::modules::ModuleRegistry;
use crate::storage::Runner;
use crate::tests::{colliding_catalog, core_switch, deliver_switch, start, App, TempDir};
// `test_hooks` is `cfg(unix)` in core (#228); gated here too, so a non-Unix test build still
// compiles — it just falls back to the real cache dir in `catalog_with_files` below, same as
// before #228 fixed this isolation on the platform that runs these tests today.
#[cfg(unix)]
use chairphoto_core::app::slideshow::test_hooks::set_test_frame_root;
use chairphoto_core::app::slideshow::{FrameWriter, FFMPEG_MISSING, SLIDESHOW_CANCELLED};
use chairphoto_core::app::{CoreEvent, EventSink as _, SlideshowProgress, CATALOG_CHANGED};
use chairphoto_core::catalog::Catalog;
use gpui_kit::component::WindowExt as _;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AppContext as _, Entity, TestAppContext};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Open a catalog in `dir` with `n` photos whose files exist, select them all, and read the
/// Library's rows; returns their ids.
fn catalog_with_files(app: &App, dir: &TempDir, n: usize, cx: &mut TestAppContext) -> Vec<i64> {
    // Every render a test makes through `claim_slideshow(..).run_with(..)` (via the manual
    // `Runner`, which runs synchronously on this test thread — see its module doc) writes its
    // frames, and runs the stale-dir sweep, under this scratch cache, never the real
    // `~/.cache` (#228). The `test-hooks` feature is what makes `set_test_frame_root` reach
    // this crate at all: without it `FrameDir::create()` always calls the real
    // `crate::thumbnails::cache_dir()`, since core's own `cfg(test)` override does not exist
    // when core is an ordinary (non-`cfg(test)`) dependency. Unix-only, like the hook itself.
    #[cfg(unix)]
    set_test_frame_root(&dir.0.join("cache"));
    let db = dir.0.join("photos.chairphoto");
    let root = dir.0.join("photos");
    std::fs::create_dir_all(root.join("2026")).unwrap();
    let catalog = Catalog::open(&db, &root).unwrap();
    let ids = (0..n)
        .map(|i| {
            let p = root.join(format!("2026/p{i}.jpg"));
            std::fs::write(&p, format!("jpeg {i}")).unwrap();
            catalog.upsert_photo(&p, None, 0, 6).unwrap().id
        })
        .collect();
    *app.state.catalog.lock().unwrap() = Some(catalog);
    app.state.send(CoreEvent::CatalogSwitched(db.to_string_lossy().to_string()));
    cx.run_until_parked();
    work(cx);
    app.wired.shell.update(cx, |s, cx| {
        s.library.select_all();
        cx.notify();
    });
    ids
}

/// Run every queued worker step (and what those queue), parking in between.
fn work(cx: &mut TestAppContext) -> usize {
    let mut total = 0;
    loop {
        let ran = cx.update(|cx| Runner::get(cx).run_pending());
        cx.run_until_parked();
        if ran == 0 {
            return total;
        }
        total += ran;
    }
}

/// Run exactly the steps queued now.
fn step(cx: &mut TestAppContext) -> usize {
    let ran = cx.update(|cx| Runner::get(cx).run_pending());
    cx.run_until_parked();
    ran
}

/// A fake ffmpeg that reports progress and writes `movie` to its output (the last argument):
/// core's checked-in fixture script. Never written at run time — a script written and then
/// executed while other test threads fork can fail with ETXTBSY.
fn fake_ffmpeg() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../core/tests/fixtures/ffmpeg/ffmpeg-ok")
}

/// A frame writer that copies the original (no thumbnail cache, no exiftool).
fn copy_frames() -> FrameWriter {
    Arc::new(|item, _, out| std::fs::copy(&item.original, out).map(|_| ()).map_err(|e| e.to_string()))
}

/// [`copy_frames`], and also records every frame path it is given: without this, nothing in
/// this crate's own suite checks that a render actually lands its frames under the scratch
/// root `catalog_with_files` points it at, rather than the real `~/.cache` — removing its
/// `set_test_frame_root` call used to pass every app-crate slideshow test regardless (#228
/// review).
fn recording_copy_frames() -> (FrameWriter, Arc<std::sync::Mutex<Vec<PathBuf>>>) {
    let paths = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = paths.clone();
    let writer: FrameWriter = Arc::new(move |item, _, out| {
        recorded.lock().unwrap().push(out.to_path_buf());
        std::fs::copy(&item.original, out).map(|_| ()).map_err(|e| e.to_string())
    });
    (writer, paths)
}

fn backend(ffmpeg: Option<PathBuf>) -> SlideshowBackend {
    SlideshowBackend { ffmpeg: Arc::new(move || ffmpeg.clone()), frames: copy_frames() }
}

/// Open the dialog over the selection with `backend`, its output folder `dest`.
fn open(app: &App, backend: SlideshowBackend, dest: &Path, cx: &mut TestAppContext) -> Entity<SlideshowDialog> {
    let view = cx
        .update_window(app.window(), |_, window, cx| {
            let host = DialogHost::new(&app.wired.model, &app.wired.shell, None, cx);
            let view = super::open(host, backend, window, cx);
            let dest = dest.to_string_lossy().to_string();
            view.update(cx, |d, cx| d.dest.update(cx, |i, cx| i.set_value(dest, window, cx)));
            view
        })
        .unwrap();
    cx.run_until_parked();
    view
}

fn render(app: &App, view: &Entity<SlideshowDialog>, cx: &mut TestAppContext) {
    let _ = app;
    view.update(cx, |d, cx| d.render_movie(cx));
    cx.run_until_parked();
}

fn has_dialog(app: &App, cx: &mut TestAppContext) -> bool {
    cx.update_window(app.window(), |_, window, cx| window.has_active_dialog(cx)).unwrap()
}

fn present(app: &App, id: &'static str, cx: &mut TestAppContext) -> bool {
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.try_find(id).is_some()
    })
    .unwrap()
}

/// More ⋯ → Modules → "Make slideshow" opens the dialog over the selection, in grid order,
/// and needs two photos.
#[gpui_kit::test]
fn the_action_opens_the_dialog_over_the_selection(cx: &mut TestAppContext) {
    let dir = TempDir::new("slideshow-action");
    let app = start(cx);
    let ids = catalog_with_files(&app, &dir, 3, cx);
    cx.update(|cx| ModuleRegistry::enable(&app.wired.modules, SLIDESHOW_ID, cx));
    cx.run_until_parked();
    let groups = app.wired.modules.read_with(cx, |r, _| r.action_groups());
    let group = groups.iter().find(|g| g.module_id.as_ref() == SLIDESHOW_ID).expect("a Slideshow action group");
    assert_eq!(group.actions.iter().map(|a| a.label.to_string()).collect::<Vec<_>>(), ["Make slideshow"]);
    cx.update_window(app.window(), |_, window, cx| ModuleRegistry::activate(&app.wired.modules, SLIDESHOW_ID, SLIDESHOW_ACTION, window, cx))
        .unwrap();
    cx.run_until_parked();
    assert!(has_dialog(&app, cx));
    assert!(present(&app, "slideshow-dialog", cx));
    for i in 0..3usize {
        let id: gpui_kit::ElementId = ("slideshow-tile", i).into();
        let found = cx.update_window(app.window(), |_, window, cx| {
            window.render_frame(cx);
            window.try_find(id).is_some()
        });
        assert!(found.unwrap(), "tile {i}");
    }
    assert!(!present(&app, "slideshow-hint", cx));
    let _ = ids;

    // One photo selected: the hint instead of the form.
    cx.update_window(app.window(), |_, window, cx| window.close_dialog(cx)).unwrap();
    cx.run_until_parked();
    app.wired.shell.update(cx, |s, cx| {
        s.library.select(ids[0], Default::default());
        cx.notify();
    });
    let view = open(&app, backend(None), &dir.0, cx);
    assert!(present(&app, "slideshow-hint", cx));
    view.read_with(cx, |d, _| assert_eq!(d.order.len(), 1));
}

/// #186: the strip's 72×54 tiles are filled by their thumbnail inside their 1 px border,
/// portrait and landscape — not a portrait element taller than the tile.
#[gpui_kit::test]
fn the_strips_tiles_are_filled_by_their_thumbnail(cx: &mut TestAppContext) {
    use crate::image_tests::{pixels, FakePool};
    use crate::loupe::fit_tests::{assert_fills, LANDSCAPE, PORTRAIT};
    use chairphoto_core::image_pool::{ImageKind, JobKey};
    let dir = TempDir::new("slideshow-fit");
    let pool = Arc::new(FakePool::default());
    let app = crate::tests::start_with_pool(cx, pool.clone());
    catalog_with_files(&app, &dir, 2, cx);
    // As the module opens it: with the image layer.
    let view = cx
        .update_window(app.window(), |_, window, cx| {
            let host = DialogHost::new(&app.wired.model, &app.wired.shell, Some(app.wired.images.clone()), cx);
            super::open(host, backend(None), window, cx)
        })
        .unwrap();
    cx.run_until_parked();
    let order: Vec<i64> = view.read_with(cx, |d, _| d.order.iter().map(|p| p.id).collect());
    let frames = [PORTRAIT, LANDSCAPE];
    for (&id, &(w, h)) in order.iter().zip(&frames) {
        pool.finish(&JobKey::photo(id, ImageKind::Thumb), Ok(pixels(w, h)));
    }
    cx.run_until_parked();
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        for (i, image) in frames.iter().enumerate() {
            let what = format!("slideshow tile {i} {image:?}");
            assert_fills(&what, window, ("slideshow-tile", i), ("slideshow-picture", i), 1.);
        }
    })
    .unwrap();
}

/// Render → the movie: the claim and the run go to the worker in turn, the movie lands in the
/// chosen folder (never over an earlier one), the dialog shows it, and the originals are
/// untouched. Drag-reordering changes the play order the job gets.
#[gpui_kit::test]
fn render_writes_the_movie_through_the_worker(cx: &mut TestAppContext) {
    let dir = TempDir::new("slideshow-render");
    let app = start(cx);
    let ids = catalog_with_files(&app, &dir, 2, cx);
    let out = dir.0.join("out");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("slideshow.mp4"), b"earlier").unwrap();
    let (frames, frame_paths) = recording_copy_frames();
    let ffmpeg = fake_ffmpeg();
    let view = open(&app, SlideshowBackend { ffmpeg: Arc::new(move || Some(ffmpeg.clone())), frames }, &out, cx);
    view.read_with(cx, |d, _| assert_eq!(d.order.iter().map(|p| p.id).collect::<Vec<_>>(), ids));

    // Drag the second tile onto the first.
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.drag_to(("slideshow-tile", 1usize), ("slideshow-tile", 0usize), cx);
    })
    .unwrap();
    cx.run_until_parked();
    view.read_with(cx, |d, _| assert_eq!(d.order.iter().map(|p| p.id).collect::<Vec<_>>(), [ids[1], ids[0]]));

    render(&app, &view, cx);
    view.read_with(cx, |d, _| assert!(d.busy && d.job().is_none(), "queued, not claimed on the UI thread"));
    assert_eq!(step(cx), 1, "the claim");
    let job = view.read_with(cx, |d, _| d.job()).expect("the claim landed");
    view.read_with(cx, |d, _| assert!(d.busy));
    assert_eq!(step(cx), 1, "the run");
    view.read_with(cx, |d, _| {
        assert!(!d.busy, "{:?}", d.error);
        assert_eq!(d.error, None);
        assert_eq!(d.output.as_deref(), Some(out.join("slideshow (2).mp4").as_path()));
    });
    assert_eq!(std::fs::read(out.join("slideshow (2).mp4")).unwrap(), b"movie");
    assert_eq!(std::fs::read(out.join("slideshow.mp4")).unwrap(), b"earlier");
    assert!(present(&app, "slideshow-output", cx));
    assert!(job > 0);
    for i in 0..2 {
        assert_eq!(std::fs::read(dir.0.join(format!("photos/2026/p{i}.jpg"))).unwrap(), format!("jpeg {i}").as_bytes());
    }
    // #228: the render's frames went under this test's own TempDir, never the real
    // `~/.cache` `catalog_with_files`'s `set_test_frame_root` call steers them away from.
    // Unix-only, like that call: elsewhere the frames go to the real cache dir.
    #[cfg(unix)]
    {
        let paths = frame_paths.lock().unwrap();
        assert_eq!(paths.len(), 2, "one frame per photo");
        for p in paths.iter() {
            assert!(p.starts_with(&dir.0), "frame {} is not under this test's TempDir {}", p.display(), dir.0.display());
        }
    }
    #[cfg(not(unix))]
    drop(frame_paths);
}

/// Progress moves the bar only for this dialog's job; Cancel after the claim trips that job,
/// which then stops before its first frame and answers cancelled.
#[gpui_kit::test]
fn progress_is_this_jobs_and_cancel_stops_it(cx: &mut TestAppContext) {
    let dir = TempDir::new("slideshow-cancel");
    let app = start(cx);
    catalog_with_files(&app, &dir, 2, cx);
    let out = dir.0.join("out");
    let view = open(&app, backend(Some(fake_ffmpeg())), &out, cx);
    render(&app, &view, cx);
    assert!(present(&app, "slideshow-progress", cx));
    step(cx); // the claim
    let job = view.read_with(cx, |d, _| d.job()).unwrap();
    assert_eq!(view.read_with(cx, |d, _| d.progress), None, "Preparing frames…");

    app.state.send(CoreEvent::SlideshowProgress(SlideshowProgress { done: 5, total: 10, job: job + 100 }));
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |d, _| d.progress), None, "another job's progress is not ours");
    app.state.send(CoreEvent::SlideshowProgress(SlideshowProgress { done: 5, total: 10, job }));
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |d, _| d.progress), Some((5, 10)));

    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.click("slideshow-cancel", cx);
    })
    .unwrap();
    cx.run_until_parked();
    work(cx);
    view.read_with(cx, |d, _| {
        assert!(!d.busy);
        assert_eq!(d.error.as_deref(), Some(SLIDESHOW_CANCELLED));
        assert_eq!(d.output, None);
    });
    assert!(!out.exists() || std::fs::read_dir(&out).unwrap().next().is_none(), "no movie");
}

/// Cancel before the claim lands: the claimed job never runs.
#[gpui_kit::test]
fn cancel_before_the_claim_keeps_the_job_from_running(cx: &mut TestAppContext) {
    let dir = TempDir::new("slideshow-cancel-early");
    let app = start(cx);
    catalog_with_files(&app, &dir, 2, cx);
    let out = dir.0.join("out");
    let view = open(&app, backend(Some(fake_ffmpeg())), &out, cx);
    render(&app, &view, cx);
    view.update(cx, |d, cx| d.cancel(cx));
    assert_eq!(step(cx), 1, "the claim ran");
    assert_eq!(step(cx), 0, "and nothing after it");
    view.read_with(cx, |d, _| assert_eq!(d.error.as_deref(), Some(SLIDESHOW_CANCELLED)));
    assert!(!out.exists());
}

/// No ffmpeg: Render says so; no job is claimed and the rest of the app is unaffected.
#[gpui_kit::test]
fn missing_ffmpeg_is_reported_in_the_dialog(cx: &mut TestAppContext) {
    let dir = TempDir::new("slideshow-noffmpeg");
    let app = start(cx);
    catalog_with_files(&app, &dir, 2, cx);
    let view = open(&app, backend(None), &dir.0.join("out"), cx);
    render(&app, &view, cx);
    work(cx);
    view.read_with(cx, |d, _| {
        assert!(!d.busy);
        assert_eq!(d.error.as_deref(), Some(FFMPEG_MISSING));
    });
    assert!(present(&app, "slideshow-error", cx));
    assert_eq!(app.state.jobs.slideshow.job_ids_issued(), 0, "nothing claimed");
}

/// A switch to a catalog whose ids collide: with `catalog:switched` not yet delivered, Render
/// is refused (the ids were the old catalog's); once it is delivered the dialog closes.
#[gpui_kit::test]
fn a_catalog_switch_refuses_the_render_and_closes_the_dialog(cx: &mut TestAppContext) {
    let dir = TempDir::new("slideshow-switch");
    let app = start(cx);
    let ids = catalog_with_files(&app, &dir, 2, cx);
    let view = open(&app, backend(Some(fake_ffmpeg())), &dir.0.join("out"), cx);
    let (b, b_ids) = colliding_catalog(&dir, "b", 2);
    assert_eq!(b_ids, ids);
    core_switch(&app, b);
    render(&app, &view, cx);
    work(cx);
    view.read_with(cx, |d, _| assert_eq!(d.error.as_deref(), Some(CATALOG_CHANGED)));
    assert!(!dir.0.join("out").exists(), "B's photos were not rendered");
    assert!(has_dialog(&app, cx));
    deliver_switch(&app, cx);
    assert!(!has_dialog(&app, cx), "the dialog closed on catalog:switched");
}

/// A render in flight when the switch lands: core's switch trips it; the run stops.
#[gpui_kit::test]
fn a_switch_after_the_claim_stops_the_render(cx: &mut TestAppContext) {
    let dir = TempDir::new("slideshow-switch-run");
    let app = start(cx);
    catalog_with_files(&app, &dir, 2, cx);
    let out = dir.0.join("out");
    let view = open(&app, backend(Some(fake_ffmpeg())), &out, cx);
    render(&app, &view, cx);
    step(cx); // the claim
    let (b, _) = colliding_catalog(&dir, "b", 2);
    core_switch(&app, b);
    work(cx);
    view.read_with(cx, |d, _| assert_eq!(d.error.as_deref(), Some(SLIDESHOW_CANCELLED)));
    assert!(!out.exists() || std::fs::read_dir(&out).unwrap().next().is_none());
}
