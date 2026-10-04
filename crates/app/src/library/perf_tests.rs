//! The grid's CPU cost per frame on a large catalog (#168), headless: the real `wire`d
//! window, `LibraryView` and `ImageStore` on the headless test platform, which runs GPUI's
//! layout, prepaint and paint but rasterises nothing. Ignored; run it in release
//! (`docs/performance-harness.md`, "GPUI grid frame bench"):
//!
//! ```sh
//! TMPDIR=~/.local/share/chairphoto-agent/tmp/<dir> CHAIRPHOTO_GRID_BENCH_ROWS=30000 \
//!   cargo test --release -p chairphoto-app --lib library::perf_tests -- --ignored --nocapture
//! ```
//!
//! What it prints, as `BENCH` lines (p50/p95/max):
//! - `landing`: one row read landing — `refresh_rows` through `on_page` and the rows-landed
//!   look scan — with the SQL read (`photo_page`) timed apart;
//! - `scroll 24px`: `render_frame` while the grid scrolls 24 px per frame, then the effects
//!   that frame queued (`run_until_parked`: the badge reads, the thumbnail answers landing);
//! - `fling 1 screen`: the same at a window height per frame (a scrollbar drag), where every
//!   frame's tiles are new;
//! - `badges`: one window's storage-badge read (a worker's, in the app);
//! - `look scan per landing`: `note_looks_when_rows_land`'s body (rv151 L5) with every row's
//!   look on record, as after scrolling through the whole catalog.
//!
//! Every thumbnail request is answered at once with one shared small image
//! ([`InstantPool`]), so tiles draw as images rather than placeholders; no decode cost is
//! counted (the pool's workers do that off the UI thread). `CHAIRPHOTO_GRID_BENCH_HOLD=1`
//! answers none. **Read `effects` with care:** the test platform draws every dirty window
//! after each effect flush, and the store lands each answer in its own update, so with
//! answers landing it counts one full redraw per thumbnail — a cost the real platform, which
//! draws once per frame callback, does not pay. What this cannot show at all: GPU upload and
//! rasterisation, and the compositor's frame pacing — the in-app pass on the display.

use crate::image_store::{Loaded, Submit};
use crate::image_tests::pixels;
use crate::tests::{start_with_pool, App, TempDir};
use chairphoto_core::app::{with_catalog_identified, CoreEvent, EventSink as _};
use chairphoto_core::catalog::Catalog;
use chairphoto_core::image_pool::{JobKey, Respond};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{px, size, AppContext as _, TestAppContext};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Answers every request at once with the same small image — or, holding
/// (`CHAIRPHOTO_GRID_BENCH_HOLD`), never: the frame cost without thumbnails landing.
struct InstantPool(Loaded, Option<Mutex<Vec<Respond<Loaded>>>>);

impl Submit for InstantPool {
    fn submit_batch(&self, batch: Vec<(JobKey, Respond<Loaded>)>) {
        if let Some(held) = &self.1 {
            held.lock().unwrap().extend(batch.into_iter().map(|(_, respond)| respond));
            return;
        }
        for (_, respond) in batch {
            respond(Ok(self.0.clone()));
        }
    }

    fn cancel(&self, _key: &JobKey) -> bool {
        false
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn report(label: &str, mut v: Vec<Duration>) {
    v.sort();
    let p = |q: f64| ms(v[((v.len() - 1) as f64 * q).round() as usize]);
    eprintln!(
        "BENCH {label:<28} n={:<4} p50 {:>7.2} ms  p95 {:>7.2} ms  max {:>7.2} ms",
        v.len(),
        p(0.5),
        p(0.95),
        ms(*v.last().unwrap())
    );
}

/// `n` rows in one transaction; their originals do not exist (the grid never reads them).
fn open_large_catalog(app: &App, dir: &TempDir, n: usize, cx: &mut TestAppContext) {
    let db = dir.0.join("bench.chairphoto");
    let root = dir.0.join("photos");
    let catalog = Catalog::open(&db, &root).unwrap();
    {
        let tx = catalog.conn().unchecked_transaction().unwrap();
        for i in 0..n {
            catalog.upsert_photo(&root.join(format!("{}/p{i:06}.ARW", 2000 + i / 5000)), None, 0, 1).unwrap();
        }
        catalog.conn().execute("UPDATE photos SET metadata_ready = 1", []).unwrap();
        tx.commit().unwrap();
    }
    *app.state.catalog.lock().unwrap() = Some(catalog);
    app.state.send(CoreEvent::CatalogSwitched(db.to_string_lossy().to_string()));
    cx.run_until_parked();
}

fn frame(app: &App, cx: &mut TestAppContext) -> (Duration, Duration) {
    let t0 = Instant::now();
    cx.update_window(app.window(), |_, window, cx| window.render_frame(cx)).unwrap();
    let drawn = t0.elapsed();
    let t1 = Instant::now();
    cx.run_until_parked();
    (drawn, t1.elapsed())
}

/// Scroll by `step` px per frame for `frames` frames (upwards from the newest row, as the
/// grid opens; wrapping to the bottom at the top) and time each.
fn scroll(app: &App, step: f32, frames: usize, cx: &mut TestAppContext) -> (Vec<Duration>, Vec<Duration>) {
    let library = app.wired.root.as_ref().unwrap().read_with(cx, |root, _| root.library().clone());
    let handle = library.read_with(cx, |l, _| l.scroll_handle().clone());
    let (mut drawn, mut parked) = (Vec::new(), Vec::new());
    for _ in 0..frames {
        {
            let state = handle.0.borrow();
            let base = &state.base_handle;
            let mut offset = base.offset();
            offset.y = if offset.y >= px(0.) { base.max_offset().y * -1. } else { (offset.y + px(step)).min(px(0.)) };
            base.set_offset(offset);
        }
        let (d, p) = frame(app, cx);
        drawn.push(d);
        parked.push(p);
    }
    (drawn, parked)
}

#[gpui_kit::test]
#[ignore = "a measurement (#168): run in release with --ignored --nocapture"]
fn grid_frame_cost_on_a_large_catalog(cx: &mut TestAppContext) {
    let n = env_usize("CHAIRPHOTO_GRID_BENCH_ROWS", 30_000);
    let frames = env_usize("CHAIRPHOTO_GRID_BENCH_FRAMES", 300);
    let (w, h) = (env_usize("CHAIRPHOTO_GRID_BENCH_W", 2560) as f32, env_usize("CHAIRPHOTO_GRID_BENCH_H", 1440) as f32);
    let dir = TempDir::new("grid-bench");
    let app = start_with_pool(cx, Arc::new(InstantPool(pixels(256, 171), std::env::var_os("CHAIRPHOTO_GRID_BENCH_HOLD").map(|_| Mutex::default()))));
    let t = Instant::now();
    open_large_catalog(&app, &dir, n, cx);
    eprintln!("BENCH catalog: {n} rows in {:.0} ms; window {w}x{h}", ms(t.elapsed()));
    cx.simulate_window_resize(app.window(), size(px(w), px(h)));
    for _ in 0..3 {
        frame(&app, cx);
    }
    let rows = app.wired.shell.read_with(cx, |s, _| s.library.photos().len());
    assert_eq!(rows, n, "every row listed");
    let cols = app.wired.root.as_ref().unwrap().read_with(cx, |r, cx| r.library().read(cx).columns());
    eprintln!("BENCH grid: {rows} rows, {cols} columns");

    // A row read landing, and the SQL read alone.
    let (mut landing, mut sql) = (Vec::new(), Vec::new());
    for _ in 0..10 {
        let query = app.wired.shell.read_with(cx, |s, _| s.library.query());
        let t = Instant::now();
        with_catalog_identified(&app.state, |c| c.photo_page(&query)).unwrap();
        sql.push(t.elapsed());
        let t = Instant::now();
        app.wired.shell.update(cx, |s, cx| s.refresh_rows(cx));
        cx.run_until_parked();
        landing.push(t.elapsed());
    }
    report("landing: photo_page (SQL)", sql);
    report("landing: refresh → landed", landing);

    let (drawn, parked) = scroll(&app, 24., frames, cx);
    report("scroll 24px: render_frame", drawn);
    report("scroll 24px: effects", parked);
    let (drawn, parked) = scroll(&app, h, frames.min(100), cx);
    report("fling 1 screen: render_frame", drawn);
    report("fling 1 screen: effects", parked);

    // The storage-badge read a window change sends (a worker's, in the app).
    let ids: Vec<i64> = app.wired.shell.read_with(cx, |s, _| s.library.photos().iter().map(|p| p.id).collect());
    let span = cols * 16;
    let mut badges = Vec::new();
    for i in 0..20 {
        let start = (i * 7919) % (ids.len() - span);
        let t = Instant::now();
        chairphoto_core::app::photo_storage_statuses(&app.state, &ids[start..start + span]).unwrap();
        badges.push(t.elapsed());
    }
    report(&format!("badges for {span} ids"), badges);

    // The rows-landed look scan alone (`note_looks_when_rows_land`'s body), with every row's
    // look asked for once (a grid scrolled through the whole catalog).
    let images = app.wired.images.clone();
    let shell = app.wired.shell.clone();
    let from = shell.read_with(cx, |s, _| s.rows_from().unwrap());
    let all: Vec<_> = shell.read_with(cx, |s, _| {
        s.library.photos().iter().map(|p| (p.id, chairphoto_model::darkroom::filmstrip::cover_look(p.cover_token.as_deref()))).collect()
    });
    images.update(cx, |store, cx| {
        for chunk in all.chunks(500) {
            store.request_look_batch(from, chunk, cx);
        }
    });
    cx.run_until_parked();
    let mut scan = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        cx.update(|cx| {
            let looks: Vec<_> = shell
                .read(cx)
                .library
                .photos()
                .iter()
                .map(|p| (p.id, chairphoto_model::darkroom::filmstrip::cover_look(p.cover_token.as_deref())))
                .collect();
            images.update(cx, |store, cx| store.note_looks(from, looks, cx));
        });
        scan.push(t.elapsed());
    }
    report("look scan per landing", scan);
}
