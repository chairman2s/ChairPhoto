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
//! For each of the first `DARKROOM_BENCH_PHOTOS` photos (default 3) it plays a 2 s Exposure
//! drag at 60 pointer events per second through the real image pool and the app's runner
//! (`media::render_edit_image` → BGRA), with the stage's own policy: a fast frame
//! (`darkroom::FAST_EDGE`) at most every `FAST_INTERVAL` on the leading edge, older queued
//! frames cancelled by a newer request, a frame taken only if newer than the one shown, and
//! the full frame (`FULL_EDGE`) `SETTLE` after the last change. It prints the
//! `render_timing` summary per photo: request → frame taken (latency), the cadence of
//! frames taken during the drag, and how many were superseded — against the 33.4 ms frame
//! budget of the 29.9 Hz EIZO (map #92, standing measurements).
//!
//! `DARKROOM_BENCH_RAW=1` first opens each photo for development and waits (≤ 60 s) for its
//! RAW working image, then drags on engine 2 — the expensive path. What this does not
//! measure: GPU upload and paint (the in-app `editor.renderTiming` log covers request → frame
//! taken in the running app; see docs/plans/gpui/parity.md, renderTiming row).

use chairphoto_app::darkroom::{FAST_EDGE, FAST_INTERVAL, FULL_EDGE, SETTLE};
use chairphoto_app::image_store::Loaded;
use chairphoto_core::app::{catalog_identity, editing, runtime, with_catalog, AppState};
use chairphoto_core::catalog::PhotoQuery;
use chairphoto_core::develop_source::DevelopSource;
use chairphoto_core::image_pool::{self, EditJob, ImagePool, JobKey, Respond};
use chairphoto_core::plugins::edit::SourceToken;
use chairphoto_model::darkroom::render_timing::{summarize, FrameSample, Tier};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

const EVENT: Duration = Duration::from_micros(16_667);
const DRAG: Duration = Duration::from_secs(2);
const BUDGET_MS: f64 = 33.4;

struct Answer {
    generation: u64,
    tier: Tier,
    ok: bool,
    cancelled: bool,
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
    if let Err(e) = editing::develop_open(state, from, photo, &[]) {
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

fn drag(pool: &ImagePool<Loaded>, photo: i64, source: SourceToken) -> Vec<FrameSample> {
    let engine2 = source != SourceToken::Preview;
    let (tx, rx) = mpsc::channel::<Answer>();
    let t0 = Instant::now();
    let mut samples: Vec<FrameSample> = Vec::new();
    let mut queued: Vec<(u64, JobKey)> = Vec::new();
    let mut shown: Option<(u64, Tier)> = None;
    let submit = |generation: u64, tier: Tier, ev: f64, queued: &mut Vec<(u64, JobKey)>, samples: &mut Vec<FrameSample>| {
        let key = JobKey::Edit(EditJob {
            photo_id: photo,
            edit_json: record(ev, engine2),
            max_edge: if tier == Tier::Fast { FAST_EDGE } else { FULL_EDGE },
            hi_res: false,
            base_only: false,
            source: source.clone(),
            clip: false,
            catalog_epoch: 0,
        });
        // A newer request cancels the older queued ones (DarkroomStage::cancel_older).
        queued.retain(|(_, k)| !pool.cancel(k));
        queued.push((generation, key.clone()));
        samples.push(FrameSample::new(generation, tier, ms_since(t0, Instant::now())));
        let tx = tx.clone();
        let respond: Respond<Loaded> = Box::new(move |r| {
            let cancelled = matches!(&r, Err(e) if e == image_pool::CANCELLED);
            let _ = tx.send(Answer { generation, tier, ok: r.is_ok(), cancelled, at: Instant::now() });
        });
        pool.submit_batch(vec![(key, respond)]);
    };
    let take = |a: Answer, samples: &mut Vec<FrameSample>, shown: &mut Option<(u64, Tier)>| {
        let Some(s) = samples.iter_mut().rev().find(|s| s.seq == a.generation && s.tier == a.tier && s.resolved.is_none()) else {
            return;
        };
        let at = ms_since(t0, a.at);
        s.resolved = Some(at);
        let newer = shown.is_none_or(|(g, t)| (a.generation, a.tier) > (g, t));
        if a.ok && newer {
            s.painted = Some(at);
            *shown = Some((a.generation, a.tier));
        } else if a.ok || a.cancelled || !newer {
            s.superseded = true;
        }
    };

    let mut generation = 0;
    let mut last_fast: Option<Instant> = None;
    let mut next_event = t0;
    while next_event.duration_since(t0) < DRAG {
        // The pointer moved: a new record.
        generation += 1;
        let ev = -1.0 + 2.0 * next_event.duration_since(t0).as_secs_f64() / DRAG.as_secs_f64();
        if last_fast.is_none_or(|t| t.elapsed() >= FAST_INTERVAL) {
            last_fast = Some(Instant::now());
            submit(generation, Tier::Fast, ev, &mut queued, &mut samples);
        }
        next_event += EVENT;
        while let Ok(a) = rx.recv_timeout(next_event.saturating_duration_since(Instant::now())) {
            queued.retain(|(g, _)| *g != a.generation);
            take(a, &mut samples, &mut shown);
        }
    }
    // Let go: the full frame SETTLE after the last change.
    let settle_at = next_event + SETTLE;
    while let Ok(a) = rx.recv_timeout(settle_at.saturating_duration_since(Instant::now())) {
        take(a, &mut samples, &mut shown);
    }
    submit(generation, Tier::Settled, 1.0, &mut queued, &mut samples);
    drop(tx);
    while let Ok(a) = rx.recv_timeout(Duration::from_secs(30)) {
        take(a, &mut samples, &mut shown);
        if samples.iter().all(|s| s.resolved.is_some()) {
            break;
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
    let path = runtime()
        .block_on(chairphoto_core::app::open_default_catalog(&state))
        .expect("open the default catalog");
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
    for id in ids {
        let source = source_for(&state, id);
        // Warm the framed-base cache and the proxy decode, as opening the photo does.
        let _ = drag(&pool, id, source.clone());
        let samples = drag(&pool, id, source.clone());
        let s = summarize(&samples);
        let fast_latency: Vec<f64> =
            samples.iter().filter(|s| s.tier == Tier::Fast).filter_map(|s| s.painted.map(|p| p - s.requested)).collect();
        let settled = samples.iter().find(|s| s.tier == Tier::Settled).and_then(|s| s.painted.map(|p| p - s.requested));
        let over = fast_latency.iter().filter(|l| **l > BUDGET_MS).count();
        println!("photo {id} ({}):", if source == SourceToken::Preview { "preview, engine 1" } else { "RAW, engine 2" });
        println!("  summary {}", s.to_json());
        println!(
            "  fast frames taken {} of {} requested ({} superseded); {} over the {BUDGET_MS} ms budget; settled frame {}",
            fast_latency.len(),
            s.by_tier.get(&Tier::Fast).copied().unwrap_or(0),
            s.superseded,
            over,
            settled.map_or("—".to_string(), |l| format!("{l:.1} ms"))
        );
    }
    let _ = editing::develop_close(&state);
}
