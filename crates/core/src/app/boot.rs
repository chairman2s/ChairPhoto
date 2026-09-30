//! Process startup shared by every front end: [`boot`].
//!
//! Before this existed the Tauri shell's `setup` hook did all of it inline, so a second front
//! end (the GPUI app, `crates/app`) would have had to copy it — and a copy that forgets the
//! crash-marker init lets a file that crashed LibRaw twice crash it a third time. What stays in
//! each shell is what only that shell has: the Tauri deep-link registration, the loopback video
//! server and the WebKit environment workaround; a GPUI window.

use super::{app_data_dir, AppState};
use crate::image_pool::{self, ImagePool};
use std::sync::{Arc, Once};

/// What [`boot`] started, for the front end to hold and report.
pub struct Boot<T = Vec<u8>> {
    /// The bounded LIFO decode pool with the front end's runner — [`crate::media::render_bytes`]
    /// for [`boot`]. Hold it for the app's lifetime: the `Arc` keeps its worker threads alive.
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
/// 5. **Image pool** — returned in [`Boot::pool`].
///
/// Install the front end's event sink with [`AppState::set_events`] **first**: the watcher
/// sends through `state`, and an event sent before the sink exists is dropped.
///
/// Call once per process. The crash-marker store, the watcher and the analyzer registry are
/// process-global; a second call re-uses them (it starts no second watcher and registers no
/// second pair of analyzers) but builds a second pool.
pub fn boot(state: &AppState) -> Boot {
    let pool_state = state.clone();
    boot_with(state, Arc::new(move |key| crate::media::render_bytes(&pool_state, key)))
}

/// [`boot`] with the image pool's runner chosen by the front end: the Tauri shell's pool
/// renders encoded bytes (`media::render_bytes`, through [`boot`]); the GPUI app's renders
/// decoded pixels (`media::render_image`, converted to its texture format on the worker).
/// Everything else is the same startup, in the same order.
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

    // The bounded LIFO image pool every media request goes through.
    let n_threads = image_pool::default_thread_count();
    eprintln!("image pool: {n_threads} worker threads");
    let pool = ImagePool::start_with_runner(n_threads, runner);

    Boot { pool, strikes, following_omarchy }
}

/// H16b + H15a: score sharpness and hash every newly decoded preview on the fly — the
/// analyzers ride the decode that was already paid for (the I7b hook) instead of re-reading
/// the file. Score-on-index and `index_phashes` handle the backfill.
///
/// Each closure captures a clone of the catalog `Arc`. When a decode fires (inside the pool's
/// worker threads) it briefly locks the catalog to resolve the absolute path → photo id and
/// writes only when the photo is not yet scored/hashed (`IS NULL` guard), so it never fights
/// the batch indexer and never writes for a file with no catalog row. The lock scope is narrow
/// (one SELECT + one UPDATE), and `run_analyzers` snapshots the registry before invoking, so
/// the registry lock is not held during this work.
fn register_decode_analyzers(state: &AppState) {
    use rusqlite::OptionalExtension as _;

    let catalog_arc = state.catalog.clone();
    crate::thumbnails::register_analyzer(Arc::new(move |img, path| {
        use crate::sharpness_indexer::{
            photo_af_point, score_image_regions, write_sharpness, RegionInputs,
        };

        let guard = match catalog_arc.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let catalog = match guard.as_ref() {
            Some(c) => c,
            None => return,
        };
        // Convert the absolute path to a catalog-root-relative path — the same key used in
        // photos.path. If the file is outside the catalog root (e.g. a card import source
        // path), skip silently.
        let root = catalog.root();
        let rel = match path.strip_prefix(root) {
            Ok(r) => r.to_string_lossy().to_string(),
            Err(_) => return,
        };
        // Look up the photo only if it is not yet scored; don't overwrite an existing score
        // (e.g. from the batch indexer) and don't write 0 for photos that have no catalog
        // row yet (not imported).
        let photo_id: Option<i64> = catalog
            .conn()
            .query_row(
                "SELECT id FROM photos WHERE path = ?1 AND sharpness IS NULL",
                rusqlite::params![rel],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten();
        if let Some(id) = photo_id {
            // Region-aware scoring (H16c): faces → AF point → tiles. Regions are read under
            // the same lock we already hold. Faces are only available with the `faces`
            // feature; without it the chain falls through to AF/tile.
            let af_point = photo_af_point(catalog.conn(), id);
            #[cfg(feature = "faces")]
            let face_boxes = crate::plugins::faces::store::face_boxes_for_photo(catalog.conn(), id)
                .unwrap_or_default();
            #[cfg(not(feature = "faces"))]
            let face_boxes = Vec::new();
            let regions = RegionInputs { face_boxes, af_point };
            let (score, method) = score_image_regions(img, &regions);
            let _ = write_sharpness(catalog.conn(), id, score, method);
        }
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
