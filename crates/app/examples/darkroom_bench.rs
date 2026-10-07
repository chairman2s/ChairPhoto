//! Darkroom drag-frame timing (#111) on a real catalog, headless: no window, no GPU upload.
//!
//! ```sh
//! XDG_DATA_HOME=~/.local/share/chairphoto-agent/xdg-data \
//! XDG_CACHE_HOME=~/.local/share/chairphoto-agent/xdg-cache \
//!   cargo run --release -p chairphoto-app --example darkroom_bench
//! ```
//!
//! Refuses to run without `XDG_DATA_HOME`, so it never opens the user's own catalog. Nothing
//! is written to the catalog: it only renders.
//!
//! Or on a scratch catalog in DIR (deleted afterwards), as `loupe_bench` builds them: N
//! generated JPEG originals (`--synthetic N [--size WxH]`), or the files in ORIG
//! (`--originals ORIG`, left alone):
//!
//! ```sh
//!   … --example darkroom_bench -- --synthetic 3 --size 6000x4000 --dir DIR
//!   DARKROOM_BENCH_RAW=1 … --example darkroom_bench -- --originals ORIG --dir DIR
//! ```
//!
//! For each of the first `DARKROOM_BENCH_PHOTOS` photos (default 3) it plays a 2 s Exposure
//! drag at 60 pointer events per second through the real image pool and the app's runner
//! (`media::render_edit_image` → BGRA).
//!
//! **Not the `DarkroomStage` entity, its policy.** The stage's timers and frame drain run on
//! GPUI's executors: the headless test platform has a fake clock and rejects wakeups from the
//! pool's threads, and the real platform needs a display — neither measures wall-clock frame
//! time headless. So this loop drives the stage's own policy functions on real time instead
//! of re-implementing them: `darkroom::fast_wait` (the fast-frame throttle — a change's fast
//! frame is due at most every `max(FAST_INTERVAL_MIN, last fast render time)`, the timer replaced per change),
//! `darkroom::frame_outcome` (a frame is taken only if newer; cancelled or stale frames are
//! superseded; the rest failed), the full frame `SETTLE` after the last change, and older
//! queued requests cancelled by a newer one, as `DarkroomStage::cancel_older` does. What the
//! entity adds around them — the atlas, the failure banner, `notify` — is not timed here.
//!
//! It prints the `render_timing` summary per photo: request → frame taken (latency), the
//! cadence of frames taken during the drag, and how many were superseded — against the
//! 33.4 ms frame budget of the 29.9 Hz EIZO (map #92, standing measurements) — and, apart,
//! how many **failed** (`darkroom::failed_frames`; the summary counts them in neither
//! column) and how many never answered.
//!
//! `DARKROOM_BENCH_RAW=1` first opens each photo for development and waits (≤ 60 s) for its
//! RAW working image, then drags on engine 2 — the expensive path. What this does not
//! measure: GPU upload and paint (the in-app `editor.renderTiming` log covers request → frame
//! taken in the running app; see docs/plans/gpui/parity.md, renderTiming row).

#[path = "support/bench_catalog.rs"]
mod bench_catalog;

use chairphoto_app::darkroom::{failed_frames, fast_render_time, fast_wait, frame_outcome, FrameOutcome, FrameTier, SETTLE};
use chairphoto_app::image_store::Loaded;
use chairphoto_core::app::{catalog_identity, editing, runtime, with_catalog, AppState, CatalogIdentity};
use chairphoto_core::catalog::PhotoQuery;
use chairphoto_core::develop_source::DevelopSource;
use chairphoto_core::image_pool::{self, EditJob, ImagePool, JobKey, Respond};
use chairphoto_core::plugins::edit::SourceToken;
use chairphoto_model::darkroom::render_timing::{summarize, FrameSample, Tier};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

const EVENT: Duration = Duration::from_micros(16_667);
const DRAG: Duration = Duration::from_secs(2);
const BUDGET_MS: f64 = 33.4;

struct Answer {
    generation: u64,
    tier: FrameTier,
    result: Result<(), String>,
    at: Instant,
}

fn ms_since(t0: Instant, t: Instant) -> f64 {
    t.saturating_duration_since(t0).as_secs_f64() * 1e3
}

/// The working source for `photo`: its RAW once resident (`DARKROOM_BENCH_RAW=1`), else the
/// camera preview.
fn source_for(state: &AppState, photo: i64) -> SourceToken {
    if std::env::var("DARKROOM_BENCH_RAW").as_deref() != Ok("1") {
        return SourceToken::Preview;
    }
    let from = catalog_identity(state).ok();
    if let Err(e) = editing::develop_open(state, from, photo, &[], editing::develop_ticket(state)) {
        println!("  photo {photo}: develop open failed ({e}); the preview path");
        return SourceToken::Preview;
    }
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(60) {
        match editing::develop_current(state, from, photo) {
            Ok(DevelopSource::Raw { token: Some(t), .. }) => {
                println!("  photo {photo}: RAW ready in {:.0} ms", t0.elapsed().as_secs_f64() * 1e3);
                return SourceToken::parse(&t).unwrap_or(SourceToken::Preview);
            }
            Ok(DevelopSource::Preview { preparing: true }) => std::thread::sleep(Duration::from_millis(50)),
            Ok(other) => {
                println!("  photo {photo}: {other:?}; the preview path");
                return SourceToken::Preview;
            }
            Err(e) => {
                println!("  photo {photo}: {e}; the preview path");
                return SourceToken::Preview;
            }
        }
    }
    println!("  photo {photo}: RAW not ready after 60 s; the preview path");
    SourceToken::Preview
}

fn record(ev: f64, engine2: bool) -> String {
    if engine2 {
        format!(r#"{{"tone":{{"ev":{ev:.3}}},"engine":2,"display":"camera.2"}}"#)
    } else {
        format!(r#"{{"tone":{{"ev":{ev:.3}}}}}"#)
    }
}

fn drag(pool: &ImagePool<Loaded>, catalog: CatalogIdentity, photo: i64, source: SourceToken) -> Vec<FrameSample> {
    let engine2 = source != SourceToken::Preview;
    let (tx, rx) = mpsc::channel::<Answer>();
    let t0 = Instant::now();
    let mut samples: Vec<FrameSample> = Vec::new();
    let mut queued: Vec<(u64, JobKey)> = Vec::new();
    let mut shown: Option<(u64, FrameTier)> = None;
    let submit = |generation: u64, tier: FrameTier, ev: f64, queued: &mut Vec<(u64, JobKey)>, samples: &mut Vec<FrameSample>| {
        let key = JobKey::Edit(EditJob {
            photo_id: photo,
            edit_json: record(ev, engine2),
            max_edge: tier.max_edge(),
            hi_res: false,
            base_only: false,
            source: source.clone(),
            clip: false,
            catalog,
        });
        // A newer request cancels the older queued ones (DarkroomStage::cancel_older; every
        // generation's record differs, so none merged into an older request here).
        queued.retain(|(_, k)| !pool.cancel(k));
        queued.push((generation, key.clone()));
        samples.push(FrameSample::new(generation, tier.timing_tier(), ms_since(t0, Instant::now())));
        let tx = tx.clone();
        let respond: Respond<Loaded> = Box::new(move |r| {
            let _ = tx.send(Answer { generation, tier, result: r.map(|_| ()), at: Instant::now() });
        });
        pool.submit_batch(vec![(key, respond)]);
    };
    // DarkroomStage::frame_done's stamping, by the stage's own rule.
    let take = |a: Answer, samples: &mut Vec<FrameSample>, shown: &mut Option<(u64, FrameTier)>| {
        let result = a.result.as_ref().map(|_| ()).map_err(String::as_str);
        let outcome = frame_outcome(a.generation, a.tier, *shown, None, result);
        let Some(s) = samples
            .iter_mut()
            .rev()
            .find(|s| s.seq == a.generation && s.tier == a.tier.timing_tier() && s.resolved.is_none() && !s.superseded)
        else {
            return;
        };
        let at = ms_since(t0, a.at);
        s.resolved = Some(at);
        match outcome {
            FrameOutcome::Shown => {
                s.painted = Some(at);
                *shown = Some((a.generation, a.tier));
            }
            FrameOutcome::Superseded => s.superseded = true,
            FrameOutcome::Failed => {
                if let Err(e) = &a.result {
                    println!("    frame {} ({:?}) failed: {e}", a.generation, a.tier);
                }
            }
        }
    };

    // The pointer moves every EVENT for DRAG, each move a new record (`edit_changed`): its
    // fast frame is due `fast_wait` after it (the stage's timer, replaced per change), the
    // full frame SETTLE after the last change.
    let end = t0 + DRAG;
    let give_up = end + SETTLE + Duration::from_secs(30);
    let mut generation = 0;
    let mut ev = -1.0;
    let mut next_event = t0;
    let mut last_fast: Option<Instant> = None;
    let mut last_render: Option<Duration> = None;
    let mut fast_due: Option<Instant> = None;
    let mut settle_due: Option<Instant> = None;
    loop {
        let now = Instant::now();
        if next_event < end && now >= next_event {
            generation += 1;
            ev = -1.0 + 2.0 * next_event.duration_since(t0).as_secs_f64() / DRAG.as_secs_f64();
            fast_due = Some(now + fast_wait(last_fast, last_render, now));
            settle_due = Some(now + SETTLE);
            next_event += EVENT;
            continue;
        }
        if fast_due.is_some_and(|d| now >= d) {
            fast_due = None;
            last_fast = Some(now);
            submit(generation, FrameTier::Fast, ev, &mut queued, &mut samples);
            continue;
        }
        if settle_due.is_some_and(|d| now >= d) {
            settle_due = None;
            submit(generation, FrameTier::Full, ev, &mut queued, &mut samples);
            continue;
        }
        let quiet = next_event >= end && fast_due.is_none() && settle_due.is_none();
        if (quiet && samples.iter().all(|s| s.resolved.is_some())) || now >= give_up {
            break;
        }
        let wake = [(next_event < end).then_some(next_event), fast_due, settle_due, Some(give_up)]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(give_up);
        if let Ok(a) = rx.recv_timeout(wake.saturating_duration_since(now)) {
            queued.retain(|(g, k)| !(*g == a.generation && matches!(k, JobKey::Edit(j) if j.max_edge == a.tier.max_edge())));
            take(a, &mut samples, &mut shown);
            // The stage's own adaptive input: the newest taken fast frame's render time.
            if let Some(t) = samples.iter().rev().find_map(fast_render_time) {
                last_render = Some(t);
            }
        }
    }
    samples
}

fn main() {
    if std::env::var_os("XDG_DATA_HOME").is_none() {
        eprintln!("darkroom_bench: set XDG_DATA_HOME to an isolated data dir (see the module docs)");
        std::process::exit(2);
    }
    let state = AppState::default();
    let dir = bench_catalog::arg("--dir").map(PathBuf::from);
    let synthetic = bench_catalog::arg("--synthetic").map(|n| n.parse::<usize>().expect("--synthetic N"));
    let originals = bench_catalog::arg("--originals").map(PathBuf::from);
    let path = match (synthetic, &originals, &dir) {
        (Some(n), None, Some(dir)) => {
            bench_catalog::synthetic(&state, dir, n, bench_catalog::size_arg());
            dir.join("bench.chairphoto")
        }
        (None, Some(originals), Some(dir)) => {
            bench_catalog::originals(&state, dir, originals);
            dir.join("bench.chairphoto")
        }
        (None, None, _) => runtime()
            .block_on(chairphoto_core::app::open_default_catalog(&state))
            .expect("open the default catalog"),
        _ => panic!("--synthetic N or --originals ORIG, each with --dir DIR"),
    };
    let n: usize = std::env::var("DARKROOM_BENCH_PHOTOS").ok().and_then(|v| v.parse().ok()).unwrap_or(3);
    let ids: Vec<i64> = with_catalog(&state, |c| c.list_photos(&PhotoQuery::default()))
        .expect("list photos")
        .into_iter()
        .map(|p| p.id)
        .take(n)
        .collect();
    let threads = image_pool::default_thread_count();
    println!("catalog {} · {} photos · {threads} workers · build {}", path.display(), ids.len(), if cfg!(debug_assertions) { "debug (not release numbers)" } else { "release" });
    let pool: Arc<ImagePool<Loaded>> = ImagePool::start_with_runner(threads, chairphoto_app::image_store::runner(state.clone()));
    // Every frame is asked for the photos of the catalog open now (`EditJob::catalog`, #251).
    let catalog = catalog_identity(&state).expect("a catalog is open");
    let mut failed_total = 0;
    for id in ids {
        let source = source_for(&state, id);
        // Warm the framed-base cache and the proxy decode, as opening the photo does.
        let warm = drag(&pool, catalog, id, source.clone());
        let samples = drag(&pool, catalog, id, source.clone());
        let s = summarize(&samples);
        let fast_latency: Vec<f64> =
            samples.iter().filter(|s| s.tier == Tier::Fast).filter_map(|s| s.painted.map(|p| p - s.requested)).collect();
        let settled = samples.iter().find(|s| s.tier == Tier::Settled).and_then(|s| s.painted.map(|p| p - s.requested));
        let over = fast_latency.iter().filter(|l| **l > BUDGET_MS).count();
        let failed = failed_frames(&samples);
        let unanswered = samples.iter().filter(|s| s.resolved.is_none()).count();
        failed_total += failed + failed_frames(&warm);
        println!("photo {id} ({}):", if source == SourceToken::Preview { "preview, engine 1" } else { "RAW, engine 2" });
        println!("  summary {}", s.to_json());
        println!(
            "  fast frames taken {} of {} requested ({} superseded); {failed} failed, {unanswered} unanswered; {} over the {BUDGET_MS} ms budget; settled frame {}",
            fast_latency.len(),
            s.by_tier.get(&Tier::Fast).copied().unwrap_or(0),
            s.superseded,
            over,
            settled.map_or("—".to_string(), |l| format!("{l:.1} ms"))
        );
        if failed_frames(&warm) > 0 {
            println!("  warm-up drag: {} failed", failed_frames(&warm));
        }
    }
    let _ = editing::develop_close(&state, editing::develop_ticket(&state));
    if let (true, Some(dir)) = (synthetic.is_some() || originals.is_some(), &dir) {
        drop(pool);
        let _ = std::fs::remove_dir_all(dir);
    }
    if failed_total > 0 {
        println!("{failed_total} frames failed: the numbers above leave them out");
    }
}
