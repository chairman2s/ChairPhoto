//! Loupe stepping latency (#109), headless: from a key press (the step) to the requested
//! photo's `RenderImage` in hand, through the real decode pool and the loupe's navigation
//! policy — the current photo first, then N+1, N−1, N+2…N+5, N−2, as one batch
//! (`image_store::neighbour_window`, what `LoupeView` asks `ImageStore::navigate_window` for).
//! Checked against AGENTS.md's targets: under 50 ms when preloaded, under 500 ms cold.
//!
//! ```sh
//! # the agent library (the default catalog under XDG_DATA_HOME)
//! XDG_DATA_HOME=~/.local/share/chairphoto-agent/xdg-data \
//! XDG_CACHE_HOME=~/.local/share/chairphoto-agent/xdg-cache \
//!   cargo run --release -p chairphoto-app --example loupe_bench -- [--dwell-ms 150] [--steps 200]
//!
//! # a synthetic catalog of N generated JPEG originals in DIR (deleted afterwards); point
//! # XDG_CACHE_HOME at an empty scratch directory so the first visits are truly cold
//!   … --example loupe_bench -- --synthetic 60 --dir DIR
//! ```
//!
//! Refuses to run without `XDG_DATA_HOME` and `XDG_CACHE_HOME`, so it never touches the user's
//! own catalog or cache.
//!
//! **What is measured.** Each step waits `--dwell-ms` (a person culling at speed looks at a
//! photo for a few hundred ms; 0 = hold the arrow key), then asks for the next photo. A photo
//! already decoded by an earlier preload counts as *preloaded* (time to find it); otherwise the
//! window is submitted and the step waits for the current photo (*cold*, or *in flight* when a
//! preload of it was already running). Not included: the GPU texture upload and the frame that
//! shows it — at most one frame (16.7 ms at 60 Hz; 33.4 ms on the 29.9 Hz EIZO), which the
//! preloaded budget has to absorb. A visual check in the running app is the end-to-end proof.

use chairphoto_app::image_store::{neighbour_window, ImageKey, ImageLru, Loaded};
use chairphoto_core::app::{runtime, with_catalog, AppState};
use chairphoto_core::catalog::{Catalog, PhotoQuery};
use chairphoto_core::image_pool::{self, ImageKind, ImagePool, JobKey};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

const AHEAD: usize = 5;
const BEHIND: usize = 2;

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn summary(label: &str, target_ms: f64, mut v: Vec<Duration>) {
    if v.is_empty() {
        println!("{label:<34} (no samples)");
        return;
    }
    v.sort();
    let p = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
    let over = v.iter().filter(|d| ms(**d) > target_ms).count();
    println!(
        "{label:<34} n={:<4} p50 {:>8.2} ms  p95 {:>8.2} ms  max {:>8.2} ms  over {target_ms} ms: {over}",
        v.len(),
        ms(p(0.5)),
        ms(p(0.95)),
        ms(*v.last().unwrap()),
    );
}

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

/// `n` 3000×2000 JPEG originals with a different gradient each, in a catalog rooted in `dir`.
fn synthetic_catalog(state: &AppState, dir: &Path, n: usize) -> PathBuf {
    let root = dir.join("photos");
    std::fs::create_dir_all(root.join("bench")).expect("bench dir");
    let catalog = Catalog::open(&dir.join("bench.chairphoto"), &root).expect("open the bench catalog");
    for i in 0..n {
        let path = root.join(format!("bench/b{i:05}.jpg"));
        let img = image::RgbImage::from_fn(3000, 2000, |x, y| {
            image::Rgb([(x / 12) as u8, (y / 8) as u8, ((x + y + i as u32 * 37) / 20) as u8])
        });
        img.save(&path).expect("write a bench JPEG");
        catalog.upsert_photo(&path, None, 0, 1).expect("add a bench photo");
    }
    *state.catalog.lock().unwrap() = Some(catalog);
    root
}

fn main() {
    if std::env::var_os("XDG_DATA_HOME").is_none() || std::env::var_os("XDG_CACHE_HOME").is_none() {
        eprintln!("loupe_bench: set XDG_DATA_HOME and XDG_CACHE_HOME to isolated dirs (see the module docs)");
        std::process::exit(2);
    }
    let dwell = Duration::from_millis(arg("--dwell-ms").map_or(150, |v| v.parse().expect("--dwell-ms N")));
    let steps: usize = arg("--steps").map_or(200, |v| v.parse().expect("--steps N"));
    let state = AppState::default();
    let synthetic = arg("--synthetic").map(|n| n.parse::<usize>().expect("--synthetic N"));
    let dir = arg("--dir").map(PathBuf::from);
    match (synthetic, &dir) {
        (Some(n), Some(dir)) => {
            synthetic_catalog(&state, dir, n);
        }
        (Some(_), None) => panic!("--synthetic needs --dir"),
        _ => {
            runtime().block_on(chairphoto_core::app::open_default_catalog(&state)).expect("open the default catalog");
        }
    }
    let ids: Vec<i64> = with_catalog(&state, |c| c.list_photos(&PhotoQuery::default()))
        .expect("list photos")
        .into_iter()
        .map(|p| p.id)
        .collect();
    let threads = image_pool::default_thread_count();
    println!(
        "{} photos · {threads} workers · dwell {} ms · cache {:?}",
        ids.len(),
        dwell.as_millis(),
        std::env::var_os("XDG_CACHE_HOME")
    );
    if ids.is_empty() {
        return;
    }

    let pool: Arc<ImagePool<Loaded>> =
        ImagePool::start_with_runner(threads, chairphoto_app::image_store::runner(state.clone()));
    let (tx, rx) = mpsc::channel::<(i64, Result<Loaded, String>)>();
    let mut lru = ImageLru::new(chairphoto_app::image_store::DEFAULT_BUDGET_BYTES);
    let mut in_flight: HashSet<i64> = HashSet::new();
    let key = |id| ImageKey { photo: id, kind: ImageKind::Preview, version: 0 };
    let (mut preloaded, mut cold, mut flying) = (Vec::new(), Vec::new(), Vec::new());
    let land = |lru: &mut ImageLru<ImageKey>, in_flight: &mut HashSet<i64>, (id, r): (i64, Result<Loaded, String>)| {
        in_flight.remove(&id);
        match r {
            Ok(l) => {
                lru.insert(key(id), l);
            }
            Err(e) => eprintln!("photo {id}: {e}"),
        }
    };

    for i in 0..steps.min(ids.len()) {
        std::thread::sleep(dwell);
        while let Ok(done) = rx.try_recv() {
            land(&mut lru, &mut in_flight, done);
        }
        let current = ids[i];
        let t0 = Instant::now();
        let hit = lru.get(&key(current)).is_some();
        let was_flying = in_flight.contains(&current);
        // The navigation batch: everything in the window not cached or already asked for.
        let wanted: Vec<i64> = neighbour_window(ids.len(), i, AHEAD, BEHIND)
            .into_iter()
            .map(|ix| ids[ix])
            .filter(|&id| lru.peek(&key(id)).is_none() && (id == current || !in_flight.contains(&id)))
            .filter(|&id| !(hit && id == current))
            .collect();
        let batch: Vec<(JobKey, image_pool::Respond<Loaded>)> = wanted
            .into_iter()
            .map(|id| {
                in_flight.insert(id);
                let tx = tx.clone();
                let respond: image_pool::Respond<Loaded> = Box::new(move |r| {
                    let _ = tx.send((id, r));
                });
                (JobKey::photo(id, ImageKind::Preview), respond)
            })
            .collect();
        if !batch.is_empty() {
            pool.submit_batch(batch);
        }
        if hit {
            preloaded.push(t0.elapsed());
            continue;
        }
        // Wait for the current photo.
        while lru.peek(&key(current)).is_none() {
            match rx.recv_timeout(Duration::from_secs(30)) {
                Ok(done) => {
                    let failed = done.0 == current && done.1.is_err();
                    land(&mut lru, &mut in_flight, done);
                    if failed {
                        break;
                    }
                }
                Err(_) => {
                    eprintln!("photo {current}: no answer in 30 s");
                    break;
                }
            }
        }
        if was_flying { flying.push(t0.elapsed()) } else { cold.push(t0.elapsed()) }
    }
    summary("preloaded (target < 50 ms)", 50., preloaded);
    summary("preload in flight (target < 500 ms)", 500., flying);
    summary("cold (target < 500 ms)", 500., cold);

    if let (Some(_), Some(dir)) = (synthetic, dir) {
        drop(pool);
        let _ = std::fs::remove_dir_all(dir);
    }
}
