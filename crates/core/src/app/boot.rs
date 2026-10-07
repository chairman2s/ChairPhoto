//! Process startup for the front end: [`boot_with`].
//!
//! It began as the Tauri shell's `setup` hook, moved here so the GPUI app (`crates/app`) would
//! not copy it — a copy that forgets the crash-marker init lets a file that crashed LibRaw
//! twice crash it a third time. What stays in the front end is what only it has: a GPUI
//! window.

use super::{app_data_dir, AppState};
use crate::catalog::Catalog;
use crate::image_pool::{self, ImagePool};
use std::path::Path;
use std::sync::{Arc, Mutex, Once};

/// What [`boot_with`] started, for the front end to hold and report.
pub struct Boot<T> {
    /// The bounded LIFO decode pool with the front end's runner. Hold it for the app's
    /// lifetime: the `Arc` keeps its worker threads alive.
    pub pool: Arc<ImagePool<T>>,
    /// The previous run's crash strikes (already printed to stderr), for a front end that
    /// wants to show them.
    pub strikes: Vec<crate::crash_marker::Strikes>,
    /// Whether the Omarchy theme watcher started (it does not when Omarchy is absent, which
    /// is a normal state). Its switches arrive as `CoreEvent::ThemeChanged` through `state`.
    pub following_omarchy: bool,
}

/// Start the core's process-wide services, once, before the first window:
///
/// 1. **Crash markers** — the previous run's leftover markers become strikes (printed to
///    stderr), before anything can call into LibRaw.
/// 2. **Upload sweep** — reclaim publish renders a previous run left in the temp dir (with a
///    publishing feature).
/// 3. **Omarchy theme watcher** — `appearance:theme_changed` through `state`'s sink.
/// 4. **Decode analyzers** — sharpness and perceptual hash ride every preview decode.
/// 5. **Zoom cache cleanup** — a one-time, best-effort removal of the zoom tier's orphaned
///    pre-no-upscale-fix cache directory, on its own thread (disk I/O; nothing waits on it).
/// 6. **Image pool** — returned in [`Boot::pool`].
///
/// Install the front end's event sink with [`AppState::set_events`] **first**: the watcher
/// sends through `state`, and an event sent before the sink exists is dropped.
///
/// Call once per process. The crash-marker store, the watcher and the analyzer registry are
/// process-global; a second call re-uses them (it starts no second watcher and registers no
/// second pair of analyzers) but builds a second pool.
///
/// The front end chooses the pool's runner: the GPUI app's renders decoded pixels
/// (`media::render_image`, converted to its texture format on the worker).
pub fn boot_with<T: Clone + Send + 'static>(state: &AppState, runner: image_pool::Runner<T>) -> Boot<T> {
    // Before anything can call into LibRaw (or, one day, a GPU driver): turn the previous
    // run's leftover crash markers into strikes (crash_marker.rs).
    let strikes = match app_data_dir() {
        Ok(dir) => {
            let strikes = crate::crash_marker::init(&dir.join("crash-markers"));
            for s in &strikes {
                eprintln!(
                    "crash marker: the previous run died inside {} on {} ({} strike{}{})",
                    s.kind,
                    s.label,
                    s.strikes,
                    if s.strikes == 1 { "" } else { "s" },
                    if s.is_blocked() { " — skipped from now on" } else { "" },
                );
            }
            strikes
        }
        Err(e) => {
            eprintln!("crash marker: disabled, no app data dir ({e})");
            Vec::new()
        }
    };

    // Reclaim upload renders left behind by a previous run. Kept directories — a supervised
    // Instagram post, or any publish that errored — were otherwise reclaimed only when
    // someone happened to publish again, so a user who publishes once and hits an error kept
    // a full-resolution JPEG forever.
    #[cfg(any(
        feature = "flickr",
        feature = "smugmug",
        feature = "instagram",
        feature = "localsend"
    ))]
    crate::upload_sweep::sweep_abandoned_uploads_at_startup();

    // "Follow Omarchy" appearance (docs/appearance.md): watch the Omarchy runtime theme and
    // broadcast switches. Starts nothing when Omarchy is absent — a normal, non-degraded
    // state that must cost zero polling — and never fatal.
    let following_omarchy = crate::appearance::start_watcher(state.clone());
    if following_omarchy {
        eprintln!("appearance: following the Omarchy theme");
    }

    // The registry is process-global, so a second boot must not add a second pair.
    static ANALYZERS: Once = Once::new();
    ANALYZERS.call_once(|| register_decode_analyzers(state));

    // One-time, best-effort cleanup of the cache directories made before tiers stopped
    // upscaling (#168 review, #245): disk I/O that nothing waits on, so it runs on its own
    // thread rather than blocking boot.
    std::thread::spawn(crate::thumbnails::cleanup_stale_caches);

    // The bounded LIFO image pool every media request goes through.
    let n_threads = image_pool::default_thread_count();
    eprintln!("image pool: {n_threads} worker threads");
    let pool = ImagePool::start_with_runner(n_threads, runner);

    Boot { pool, strikes, following_omarchy }
}

/// The sharpness analyzer [`register_decode_analyzers`] registers: settle the photo at the
/// absolute `path` from the decode `img` (`sharpness_indexer::settle_from_decode`), under a
/// brief lock of the open catalog. `None` when there was nothing to do — no catalog, a
/// poisoned lock, a file outside the catalog root (a card import's source, or a copy on
/// another volume: those wait for the legacy re-measure, `app::sharpness`), or a photo
/// whose score is current.
fn settle_sharpness_from_decode(
    catalog: &Mutex<Option<Catalog>>,
    img: &image::DynamicImage,
    path: &Path,
) -> Option<crate::sharpness_indexer::Settled> {
    let guard = catalog.lock().ok()?;
    let catalog = guard.as_ref()?;
    // Convert the absolute path to a catalog-root-relative path — the same key used in
    // photos.path.
    let rel = path.strip_prefix(catalog.root()).ok()?.to_string_lossy().to_string();
    // Score an unscored photo, settle a legacy score, and leave a current score and a file
    // with no catalog row alone. Region-aware (H16c): faces → AF point → tiles, read under
    // the lock already held.
    crate::sharpness_indexer::settle_from_decode(catalog.conn(), &rel, img).ok().flatten()
}

/// H16b + H15a: score sharpness and hash every newly decoded preview on the fly — the
/// analyzers ride the decode that was already paid for (the I7b hook) instead of re-reading
/// the file. Score-on-index and `index_phashes` handle the backfill.
///
/// Each closure captures a clone of the catalog `Arc`. When a decode fires (inside the pool's
/// worker threads) it briefly locks the catalog to resolve the absolute path → photo id and
/// writes only when the photo is not yet scored/hashed (`IS NULL` guard) — or, for sharpness,
/// holds a legacy pre-#245 score, which the decode in hand settles
/// (`sharpness_indexer::settle_from_decode`) — so it never overwrites a current score and
/// never writes for a file with no catalog row. The lock scope is narrow (one SELECT + one
/// UPDATE), and `run_analyzers` snapshots the registry before invoking, so the registry lock
/// is not held during this work.
fn register_decode_analyzers(state: &AppState) {
    use rusqlite::OptionalExtension as _;

    let catalog_arc = state.catalog.clone();
    crate::thumbnails::register_analyzer(Arc::new(move |img, path| {
        settle_sharpness_from_decode(&catalog_arc, img, path);
    }));

    // Unlike sharpness, the dHash is resolution-invariant, so any decode that reaches the
    // analyzers (gated at PREVIEW_MAX) is fine. `hash_and_store` writes only when the photo
    // is not yet hashed (phash IS NULL).
    let catalog_arc = state.catalog.clone();
    crate::thumbnails::register_analyzer(Arc::new(move |img, path| {
        let guard = match catalog_arc.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let catalog = match guard.as_ref() {
            Some(c) => c,
            None => return,
        };
        let root = catalog.root();
        let rel = match path.strip_prefix(root) {
            Ok(r) => r.to_string_lossy().to_string(),
            Err(_) => return,
        };
        // Only look up unhashed photos so we don't decode-and-store redundantly.
        let photo_id: Option<i64> = catalog
            .conn()
            .query_row(
                "SELECT id FROM photos WHERE path = ?1 AND phash IS NULL",
                rusqlite::params![rel],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten();
        if let Some(id) = photo_id {
            crate::phash_indexer::hash_and_store(catalog.conn(), id, img);
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sharpness_indexer::{Settled, SHARPNESS_BASIS};

    // ── #262 L2: the sharpness decode hook's wiring ─────────────────────────────

    fn sharp(w: u32, h: u32) -> image::DynamicImage {
        image::DynamicImage::ImageLuma8(image::GrayImage::from_fn(w, h, |x, y| {
            image::Luma([if (x / 4 + y / 4) % 2 == 0 { 0 } else { 255 }])
        }))
    }

    fn row(c: &Catalog, id: i64) -> (Option<f64>, Option<i64>) {
        c.conn()
            .query_row("SELECT sharpness, sharpness_basis FROM photos WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
    }

    /// The analyzer `register_decode_analyzers` installs maps the decoded file's absolute
    /// path to its catalog row and settles a legacy score there: measured again from a decode
    /// under 2048 px, kept and stamped from a larger one. A file outside the catalog root,
    /// and a closed catalog, are left alone.
    #[test]
    fn the_sharpness_hook_settles_a_legacy_row_by_its_absolute_path() {
        let dir = crate::test_support::TestTmpDir::new("boot-sharpness-hook");
        let root = dir.join("library");
        std::fs::create_dir_all(&root).unwrap();
        let catalog = Catalog::open(&dir.join("c.chairphoto"), &root).unwrap();
        let mut ids = Vec::new();
        for name in ["small.jpg", "large.jpg"] {
            let path = root.join(name);
            std::fs::write(&path, b"x").unwrap();
            let id = catalog.upsert_photo(&path, None, 0, 1).unwrap().id;
            // As the pre-#245 indexer left it: a score with no basis stamp.
            catalog
                .conn()
                .execute(
                    "UPDATE photos SET sharpness = 0.5, sharpness_method = 'tile', sharpness_basis = NULL WHERE id = ?1",
                    [id],
                )
                .unwrap();
            ids.push(id);
        }
        let (small_id, large_id) = (ids[0], ids[1]);
        let catalog = Mutex::new(Some(catalog));
        let (small, large) = (sharp(1200, 800), sharp(2048, 1365));

        let outside = dir.join("card").join("small.jpg");
        assert_eq!(settle_sharpness_from_decode(&catalog, &small, &outside), None, "outside the root");
        assert_eq!(row(catalog.lock().unwrap().as_ref().unwrap(), small_id), (Some(0.5), None));

        assert_eq!(settle_sharpness_from_decode(&catalog, &small, &root.join("small.jpg")), Some(Settled::Scored));
        assert_eq!(settle_sharpness_from_decode(&catalog, &large, &root.join("large.jpg")), Some(Settled::Kept));
        {
            let guard = catalog.lock().unwrap();
            let c = guard.as_ref().unwrap();
            let (score, basis) = row(c, small_id);
            assert!(score.is_some_and(|s| s != 0.5), "the stale score is measured again, got {score:?}");
            assert_eq!(basis, Some(SHARPNESS_BASIS));
            assert_eq!(row(c, large_id), (Some(0.5), Some(SHARPNESS_BASIS)), "kept and stamped");
        }
        assert_eq!(settle_sharpness_from_decode(&catalog, &small, &root.join("small.jpg")), None, "settled once");

        *catalog.lock().unwrap() = None;
        assert_eq!(settle_sharpness_from_decode(&catalog, &small, &root.join("small.jpg")), None, "no catalog");
    }
}
