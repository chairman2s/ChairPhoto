//! Library grid scroll frame times (#106), in the real app: `run`'s own startup
//! (`start_core` + `wire`, the decode pool, the real `RootView` and `LibraryView`), then an
//! auto-scroll of the grid at 24 px per frame for 10 s, printing frame-interval percentiles —
//! the Phase 0 spike's harness (3e13e1b), on the shipped grid.
//!
//! ```sh
//! # the agent library (the default catalog under XDG_DATA_HOME)
//! XDG_DATA_HOME=~/.local/share/chairphoto-agent/xdg-data \
//! XDG_CACHE_HOME=~/.local/share/chairphoto-agent/xdg-cache \
//!   cargo run --release -p chairphoto-app --example grid_bench
//!
//! # a synthetic catalog of N photos, built in DIR (deleted afterwards)
//!   … --example grid_bench -- --synthetic 20000 --dir DIR
//! ```
//!
//! Refuses to run without `XDG_DATA_HOME` and `XDG_CACHE_HOME`, so it never touches the
//! user's own catalog or cache.
//!
//! **Synthetic catalog.** `N` rows whose originals do not exist, so each thumbnail comes from
//! the id-keyed persistent thumbnail store (`thumbnails::read_persistent_thumb`), which this
//! bench fills with hard links to the cache's existing persistent thumbnails (real 342×512
//! JPEGs, decoded per tile like any other). Row ids start at 10,000,000 so the links never
//! collide with a real catalog's ids in the same cache; they are removed at exit.
//!
//! The window is unthrottled (`inactive_frame_interval: None`): the bench never takes focus.

use chairphoto_app::{start_core, wire, WireOptions};
use chairphoto_core::app::{AppState, CoreEvent, EventSink as _};
use chairphoto_core::catalog::Catalog;
use gpui_kit::{px, App, AppContext as _, Window};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Ids of synthetic rows start here.
const FIRST_ID: i64 = 10_000_000;
const WARMUP: Duration = Duration::from_secs(3);
const RUN: Duration = Duration::from_secs(10);
const STEP_PX: f32 = 24.;

fn rss_mib() -> f64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: f64 = statm.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0.0);
    pages * 4096.0 / (1024.0 * 1024.0)
}

/// Build the synthetic catalog in `dir`; returns its path and the persistent thumbnails
/// this created (to remove at exit).
fn synthetic_catalog(dir: &Path, n: usize) -> (Catalog, Vec<PathBuf>) {
    std::fs::create_dir_all(dir).expect("bench dir");
    let db = dir.join("bench.chairphoto");
    let _ = std::fs::remove_file(&db);
    let root = dir.join("photos");
    let catalog = Catalog::open(&db, &root).expect("open the bench catalog");
    let conn = catalog.conn();
    // `INTEGER PRIMARY KEY`: new rows take max(id) + 1, so a placeholder sets the floor.
    conn.execute(
        "INSERT INTO photos(id, uuid, path, mtime_ns, size, extension, created_at, updated_at)
         VALUES(?1, 'bench-floor', 'bench-floor.jpg', 0, 0, 'jpg', 0, 0)",
        [FIRST_ID - 1],
    )
    .expect("placeholder row");
    let tx = conn.unchecked_transaction().expect("transaction");
    for i in 0..n {
        catalog.upsert_photo(&root.join(format!("bench/{i:06}.jpg")), None, 0, 1).expect("row");
    }
    conn.execute("UPDATE photos SET metadata_ready = 1", []).expect("metadata_ready");
    conn.execute("DELETE FROM photos WHERE id = ?1", [FIRST_ID - 1]).expect("drop the placeholder");
    tx.commit().expect("commit");

    // Thumbnails: hard links to the cache's existing persistent ones (the agent library's).
    let sources: Vec<PathBuf> = (1..=64)
        .map(chairphoto_core::thumbnails::persistent_thumb_path)
        .filter(|p| p.is_file())
        .collect();
    assert!(!sources.is_empty(), "no persistent thumbnails in XDG_CACHE_HOME to link to — run the agent library once");
    let ids: Vec<i64> = conn
        .prepare("SELECT id FROM photos ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(ids.iter().all(|&id| id >= FIRST_ID), "synthetic ids start at {FIRST_ID}");
    let mut made = Vec::with_capacity(ids.len());
    for (i, id) in ids.iter().enumerate() {
        let target = chairphoto_core::thumbnails::persistent_thumb_path(*id);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        let _ = std::fs::remove_file(&target);
        let source = &sources[i % sources.len()];
        if std::fs::hard_link(source, &target).is_err() {
            std::fs::copy(source, &target).expect("thumbnail copy");
        }
        made.push(target);
    }
    eprintln!("BENCH synthetic catalog: {} rows, thumbnails linked from {} sources", ids.len(), sources.len());
    (catalog, made)
}

/// End the process once the result is printed and the synthetic files are gone. Not
/// `cx.quit()`: this example links GPUI's leak detector (the `test-support` dev-dependency),
/// which panics at exit over handles the app's detached routers still hold.
fn finish(code: i32) -> ! {
    chairphoto_core::crash_marker::clean_exit();
    std::process::exit(code)
}

struct Bench {
    /// Frame callbacks seen; the stall detector watches it.
    ticks: u64,
    frames: Vec<Instant>,
    started: Option<Instant>,
    submitted_at_start: u64,
    cleanup: Vec<PathBuf>,
    dir: Option<PathBuf>,
}

fn main() {
    for var in ["XDG_DATA_HOME", "XDG_CACHE_HOME"] {
        assert!(std::env::var_os(var).is_some(), "{var} must point at the agent's isolated dirs");
    }
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let synthetic: Option<usize> = arg("--synthetic").map(|n| n.parse().expect("--synthetic N"));
    let dir = arg("--dir").map(PathBuf::from);

    let (state, events_rx, boot) = start_core(|state| {
        chairphoto_core::app::boot_with(state, chairphoto_app::image_store::runner(state.clone()))
    });
    let mut cleanup = Vec::new();
    if let Some(n) = synthetic {
        let dir = dir.clone().expect("--dir for the synthetic catalog");
        let (catalog, made) = synthetic_catalog(&dir, n);
        cleanup = made;
        let db = catalog.db_path().to_string_lossy().to_string();
        *state.catalog.lock().unwrap() = Some(catalog);
        state.send(CoreEvent::CatalogSwitched(db));
    }
    let initial_theme = chairphoto_core::appearance::read_current_theme();
    let bench = Rc::new(RefCell::new(Bench {
        ticks: 0,
        frames: Vec::new(),
        started: None,
        submitted_at_start: 0,
        cleanup,
        dir: synthetic.and(dir),
    }));
    let state2: AppState = state.clone();

    gpui_kit::application().with_assets(chairphoto_app::assets::Assets).run(move |cx| {
        let options = WireOptions {
            on_exit: Rc::new(chairphoto_core::crash_marker::clean_exit),
            open_default_catalog: synthetic.is_none(),
            unthrottled: true,
        };
        let wired = wire(cx, state2, events_rx, Some(boot.pool.clone()), &initial_theme, options);
        let handle = wired.main_window.clone().expect("main window");
        // Weak: a frame callback still pending at exit must not keep the entities alive
        // (GPUI's leak check panics on that).
        let root = wired.root.as_ref().expect("root view").downgrade();
        let shell = wired.shell.downgrade();
        let images = wired.images.downgrade();
        let launched = Instant::now();

        fn tick(
            window: &mut Window,
            cx: &mut App,
            launched: Instant,
            bench: Rc<RefCell<Bench>>,
            root_w: gpui_kit::WeakEntity<chairphoto_app::view::RootView>,
            shell_w: gpui_kit::WeakEntity<chairphoto_app::shell::ShellState>,
            images_w: gpui_kit::WeakEntity<chairphoto_app::image_store::ImageStore>,
        ) {
            let now = Instant::now();
            let (Some(root), Some(shell), Some(images)) = (root_w.upgrade(), shell_w.upgrade(), images_w.upgrade())
            else {
                return;
            };
            let rows = shell.read(cx).library.photos().len();
            let loaded = shell.read(cx).rows_loaded;
            let mut b = bench.borrow_mut();
            b.ticks += 1;
            if b.started.is_none() {
                if loaded && launched.elapsed() >= WARMUP {
                    b.started = Some(now);
                    b.submitted_at_start = images.read(cx).stats().submitted;
                    let cols = root.read(cx).library().read(cx).columns();
                    eprintln!("BENCH start: {rows} rows, {cols} columns, rss {:.0} MiB", rss_mib());
                }
            } else if now - b.started.unwrap() < RUN {
                b.frames.push(now);
                let library = root.read(cx).library().clone();
                let handle = library.read(cx).scroll_handle().clone();
                let base = &handle.0.borrow().base_handle;
                let mut offset = base.offset();
                // The grid opens at the newest (bottom) row: scroll up, towards the oldest.
                offset.y = (offset.y + px(STEP_PX)).min(px(0.));
                base.set_offset(offset);
            } else {
                let mut dts: Vec<f64> = b.frames.windows(2).map(|w| (w[1] - w[0]).as_secs_f64() * 1e3).collect();
                dts.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let pct = |p: f64| if dts.is_empty() { 0. } else { dts[((dts.len() - 1) as f64 * p) as usize] };
                let over = dts.iter().filter(|&&d| d > 33.4 * 1.5).count();
                let stats = images.read(cx).stats();
                eprintln!(
                    "BENCH frames={} p50={:.2}ms p95={:.2}ms p99={:.2}ms max={:.2}ms over-1.5x33.4ms={} \
                     thumbs-submitted={} lru={} rss={:.0}MiB rows={rows}",
                    b.frames.len(),
                    pct(0.5),
                    pct(0.95),
                    pct(0.99),
                    dts.last().copied().unwrap_or(0.),
                    over,
                    stats.submitted - b.submitted_at_start,
                    images.read(cx).lru().len(),
                    rss_mib(),
                );
                for path in b.cleanup.drain(..) {
                    let _ = std::fs::remove_file(path);
                }
                if let Some(dir) = b.dir.take() {
                    let _ = std::fs::remove_dir_all(dir);
                }
                finish(0);
            }
            drop(b);
            window.refresh();
            window.on_next_frame(move |window, cx| tick(window, cx, launched, bench, root_w, shell_w, images_w));
        }

        // Start the frame chain after a pause, then watch it: Wayland paces frames with the
        // compositor's frame callbacks, which never come while the window is not shown (its
        // workspace hidden, the display asleep). Report that and quit rather than hang.
        let bench = bench.clone();
        cx.spawn(async move |cx| {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let chain = bench.clone();
            cx.update_window(handle, move |_, window, _| {
                window.on_next_frame(move |window, cx| tick(window, cx, launched, chain, root, shell, images));
            })
            .unwrap_or_else(|e| eprintln!("BENCH could not reach the window: {e}"));
            let mut seen = 0;
            loop {
                cx.background_executor().timer(Duration::from_secs(5)).await;
                let ticks = bench.borrow().ticks;
                if ticks == seen {
                    eprintln!(
                        "BENCH stalled: no frame for 5 s after {ticks} frames — the window is not being \
                         shown (hidden workspace, or the display asleep: `hyprctl monitors` dpmsStatus)"
                    );
                    let mut b = bench.borrow_mut();
                    for path in b.cleanup.drain(..) {
                        let _ = std::fs::remove_file(path);
                    }
                    if let Some(dir) = b.dir.take() {
                        let _ = std::fs::remove_dir_all(dir);
                    }
                    finish(2);
                }
                seen = ticks;
            }
        })
        .detach();
    });
    chairphoto_core::crash_marker::clean_exit();
}
