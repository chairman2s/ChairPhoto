//! Image-layer measurements (#101) on a real catalog, headless: no window, no GPU upload.
//!
//! ```sh
//! XDG_DATA_HOME=~/.local/share/chairphoto-agent/xdg-data \
//! XDG_CACHE_HOME=~/.local/share/chairphoto-agent/xdg-cache \
//!   cargo run --release -p chairphoto-app --example image_bench
//! ```
//!
//! Refuses to run without `XDG_DATA_HOME`, so it never opens the user's own catalog. Point
//! `XDG_CACHE_HOME` at an empty directory to measure cold tiers (extraction from the originals
//! included); at the app's cache for warm ones.
//!
//! Reports, per tier (thumb, preview):
//! - `render_image` (resolve + cached JPEG decode + rotation) and the BGRA conversion — the
//!   whole "decode → RenderImage" the pool worker does — against the Tauri path's
//!   `render_bytes` + a decode of its bytes;
//! - loupe navigation through a real `ImagePool<Loaded>`: from `submit_batch([N, N+1, N−1])`
//!   to the current photo's `RenderImage` in hand;
//! - the LRU under churn: decoded bytes held and process RSS while thousands of previews
//!   stream through a small budget.
//!
//! The decode analyzers `app::boot` registers (sharpness, pHash) are not installed here;
//! they run only on a cache miss that decodes at preview size or larger.

use chairphoto_app::image_store::{ImageKey, ImageLru, Loaded};
use chairphoto_core::app::{runtime, with_catalog, AppState};
use chairphoto_core::catalog::PhotoQuery;
use chairphoto_core::image_pool::{self, ImageKind, ImagePool, JobKey};
use chairphoto_core::media::{render_bytes, render_image};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn summary(label: &str, mut v: Vec<Duration>) {
    if v.is_empty() {
        println!("{label:<44} (no samples)");
        return;
    }
    v.sort();
    let p = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
    println!(
        "{label:<44} n={:<3} p50 {:>7.2} ms  p95 {:>7.2} ms  max {:>7.2} ms",
        v.len(),
        ms(p(0.5)),
        ms(p(0.95)),
        ms(*v.last().unwrap())
    );
}

fn rss_mib() -> f64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: f64 = statm.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0.0);
    pages * 4096.0 / (1024.0 * 1024.0)
}

fn main() {
    if std::env::var_os("XDG_DATA_HOME").is_none() {
        eprintln!("image_bench: set XDG_DATA_HOME to an isolated data dir (see the module docs)");
        std::process::exit(2);
    }
    let state = AppState::default();
    let path = runtime()
        .block_on(chairphoto_core::app::open_default_catalog(&state))
        .expect("open the default catalog");
    let ids: Vec<i64> = with_catalog(&state, |c| c.list_photos(&PhotoQuery::default()))
        .expect("list photos")
        .into_iter()
        .map(|p| p.id)
        .collect();
    println!("catalog {} · {} photos · cache {:?}", path.display(), ids.len(), std::env::var_os("XDG_CACHE_HOME"));
    if ids.is_empty() {
        return;
    }

    // --- decode → RenderImage, per tier -----------------------------------------------------
    for kind in [ImageKind::Thumb, ImageKind::Preview] {
        for pass in ["first pass", "second pass"] {
            let (mut decode, mut convert, mut total, mut tauri) = (vec![], vec![], vec![], vec![]);
            let mut pixels = 0u64;
            for &id in &ids {
                let t0 = Instant::now();
                let decoded = match render_image(&state, JobKey::photo(id, kind)) {
                    Ok(d) => d,
                    Err(e) => {
                        println!("  photo {id} {kind:?}: {e}");
                        continue;
                    }
                };
                let t1 = Instant::now();
                let loaded = Loaded::from_decoded(decoded);
                let t2 = Instant::now();
                pixels += loaded.bytes() as u64 / 4;
                decode.push(t1 - t0);
                convert.push(t2 - t1);
                total.push(t2 - t0);

                // The Tauri path, for comparison: the protocol's bytes, decoded as a webview would.
                let t3 = Instant::now();
                if let Ok(bytes) = render_bytes(&state, JobKey::photo(id, kind)) {
                    let _ = image::load_from_memory(&bytes);
                    tauri.push(t3.elapsed());
                }
            }
            println!("{kind:?}, {pass} (avg {:.2} MP):", pixels as f64 / ids.len() as f64 / 1e6);
            summary("  render_image (resolve + JPEG decode)", decode);
            summary("  to_bgra", convert);
            summary("  decode → RenderImage", total);
            // Runs after render_image, so on a first pass it finds the tier already cached.
            summary("  Tauri path: render_bytes + decode (after)", tauri);
        }
    }

    // --- navigation through the real pool ---------------------------------------------------
    let threads = image_pool::default_thread_count();
    let pool: Arc<ImagePool<Loaded>> =
        ImagePool::start_with_runner(threads, chairphoto_app::image_store::runner(state.clone()));
    for kind in [ImageKind::Preview, ImageKind::Zoom] {
        let mut current = Vec::new();
        for i in 0..ids.len() {
            let order = chairphoto_app::image_store::neighbours(ids.len(), i);
            let (tx, rx) = mpsc::channel::<(usize, Instant)>();
            let t0 = Instant::now();
            let batch = order
                .iter()
                .enumerate()
                .map(|(rank, &ix)| {
                    let tx = tx.clone();
                    let respond: image_pool::Respond<Loaded> = Box::new(move |r| {
                        if r.is_ok() {
                            let _ = tx.send((rank, Instant::now()));
                        }
                    });
                    (JobKey::photo(ids[ix], kind), respond)
                })
                .collect();
            pool.submit_batch(batch);
            drop(tx);
            let arrivals: Vec<(usize, Instant)> = rx.iter().collect();
            if let Some((_, at)) = arrivals.iter().find(|(rank, _)| *rank == 0) {
                current.push(*at - t0);
            }
        }
        summary(&format!("navigate {kind:?}: current ready ({threads} workers)"), current);
    }

    // --- LRU churn --------------------------------------------------------------------------
    let previews: Vec<Loaded> = ids
        .iter()
        .filter_map(|&id| render_image(&state, JobKey::photo(id, ImageKind::Preview)).ok())
        .map(Loaded::from_decoded)
        .collect();
    if previews.is_empty() {
        return;
    }
    // Fresh images each insert (a clone of the Arc would share memory and prove nothing).
    let budget = 128 * 1024 * 1024;
    let mut lru = ImageLru::new(budget);
    let rss0 = rss_mib();
    let (mut peak_bytes, mut peak_rss, mut evicted) = (0usize, rss0, 0usize);
    let inserts = 1500;
    let mut checkpoints = Vec::new();
    for n in 0..inserts {
        let src = &previews[n % previews.len()];
        let copy = src.image.as_bytes(0).map(|b| b.to_vec()).unwrap_or_default();
        let size = src.image.size(0);
        let frame = image::RgbaImage::from_raw(size.width.0 as u32, size.height.0 as u32, copy).unwrap();
        let loaded = Loaded {
            image: Arc::new(gpui_kit::RenderImage::new(vec![image::Frame::new(frame)])),
            video_tile: false,
        };
        evicted += lru.insert(ImageKey { photo: n as i64, kind: ImageKind::Preview, version: 0 }, loaded).len();
        peak_bytes = peak_bytes.max(lru.bytes());
        if n % 50 == 0 {
            peak_rss = peak_rss.max(rss_mib());
        }
        if (n + 1) % (inserts / 5) == 0 {
            checkpoints.push(format!("{}:{:.0}", n + 1, rss_mib()));
        }
    }
    println!(
        "LRU churn: {inserts} previews through a {} MiB budget → held {} ({:.1} MiB, peak {:.1} MiB), evicted {evicted}; RSS {:.0} MiB before, peak {:.0}, after {:.0}",
        budget / (1024 * 1024),
        lru.len(),
        lru.bytes() as f64 / (1024.0 * 1024.0),
        peak_bytes as f64 / (1024.0 * 1024.0),
        rss0,
        peak_rss,
        rss_mib()
    );
    println!("  RSS MiB after N inserts: {}", checkpoints.join("  "));
}
