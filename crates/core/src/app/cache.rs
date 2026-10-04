//! The batch cache warm-up as an owned job — called by the GPUI app's warm-up after a
//! rescan. It **blocks**
//! (decodes every reachable original, possibly off a NAS): run it on a worker.
//!
//! Every photo whose original is reachable gets its grid thumbnail generated and cached, and
//! its preview (and zoom) too when `include_previews` is set — the "Cache previews on import"
//! option. The B&W flag is computed from each thumbnail, and once the pass is over the flags
//! are stored and the auto-tags (monochrome) refreshed.
//!
//! **Ownership** (`JobRegistry::cache`). [`claim_cache`] trips the running warm-up and installs
//! a fresh generation, numbered for the `cache:progress` events — one abort lock, never the
//! catalog's, so a front end claims on its UI thread. A newer claim or a catalog switch trips
//! it; the workers stop before their next photo, and a tripped pass stores nothing and
//! answers [`CACHE_CANCELLED`]. The photos are read under one catalog lock hold, which also
//! captures that catalog's identity; the flags are written only to that catalog
//! (`with_catalog_as`), with the generation checked under the same lock hold — a switch trips
//! every generation under the catalog lock, so a pass that sees its flag un-tripped there is
//! still writing to the catalog it read. Progress events carry the job id, so a front end
//! drops a superseded pass's stragglers; the result returned is the terminal signal.

use super::{with_catalog_as, with_catalog_identified, AppState, CacheProgress, CoreEvent, EventSink};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

/// What a stopped warm-up answers (a newer warm-up or a catalog switch).
pub const CACHE_CANCELLED: &str = "Cache warm-up cancelled";

/// A warm-up's ownership: its generation of the family's abort flag, and the job id its
/// `cache:progress` events carry.
#[derive(Clone)]
pub struct CacheClaim {
    pub abort: Arc<AtomicBool>,
    pub job: u64,
}

impl CacheClaim {
    /// Whether a newer warm-up or a catalog switch tripped it.
    pub fn aborted(&self) -> bool {
        self.abort.load(Ordering::Relaxed)
    }
}

/// What a finished warm-up did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheResult {
    /// Photos whose original was reachable and so were warmed.
    pub total: usize,
}

/// Claim the warm-up generation: trip the running warm-up, install a fresh flag and number
/// the job. One abort lock — cheap enough for a UI thread.
pub fn claim_cache(state: &AppState) -> Result<CacheClaim, String> {
    let (abort, job) = state.jobs.cache.install_fresh_numbered()?;
    Ok(CacheClaim { abort, job })
}

/// Claim and run a warm-up ([`claim_cache`] + [`cache_images_claimed`]). Blocks.
pub fn cache_images(state: &AppState, include_previews: bool) -> Result<CacheResult, String> {
    let claim = claim_cache(state)?;
    cache_images_claimed(state, &claim, include_previews)
}

/// Run the warm-up `claim` owns, in parallel across the CPU cores. Blocks. Already tripped:
/// it decodes nothing.
pub fn cache_images_claimed(state: &AppState, claim: &CacheClaim, include_previews: bool) -> Result<CacheResult, String> {
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    cache_images_with(state, claim, include_previews, workers, &|_| {})
}

/// [`cache_images_claimed`] on at most `max_workers` threads, calling `after_photo(n)` after
/// the `n`th photo is warmed — where a test puts a newer claim or a switch.
fn cache_images_with(
    state: &AppState,
    claim: &CacheClaim,
    include_previews: bool,
    max_workers: usize,
    after_photo: &dyn Fn(usize),
) -> Result<CacheResult, String> {
    if claim.aborted() {
        return Err(CACHE_CANCELLED.into());
    }
    // Each photo's path CANDIDATES under one brief lock (pure SQL — no stats), so resolving
    // the whole library never holds the lock across NAS stats.
    let (from, candidate_lists) = with_catalog_identified(state, |c| {
        let mut lists = Vec::new();
        for photo in c.list_photos(&crate::catalog::PhotoQuery::default())? {
            lists.push((photo.id, c.photo_path_candidates(photo.id)?));
        }
        Ok(lists)
    })?;
    let health = state.volume_health.clone();
    let items: Vec<(i64, PathBuf)> = candidate_lists
        .into_iter()
        .filter_map(|(id, cands)| {
            // Skip photos whose originals aren't reachable now (an offline NAS, say).
            // OriginalRequired: this builds the cache *from* originals, so a stale
            // reachability flag must not silently drop a photo from the warm-up.
            crate::volume_health::pick_existing(&cands, &health, crate::catalog::ResolveMode::OriginalRequired)
                .map(|abs| (id, abs))
        })
        .collect();

    let total = items.len();
    if total == 0 {
        return if claim.aborted() { Err(CACHE_CANCELLED.into()) } else { Ok(CacheResult { total }) };
    }

    // (photo_id, is_grayscale), computed from each thumbnail and stored after the pass (the
    // workers never take the catalog lock).
    let job = claim.job;
    let abort = &*claim.abort;
    // One photo: the grid thumbnail (and, with previews, every size from ONE extraction +
    // decode, I7b: `warm_all_sizes` reads the RAW once and downscales thumb + preview + zoom),
    // then the B&W flag from the thumbnail. A decode failure records `false` (cannot confirm
    // B&W), so the flag is always set, never NULL.
    let warm = |photo_id: i64, path: &PathBuf| {
        if include_previews {
            let _ = crate::thumbnails::warm_all_sizes(path);
        }
        let gray = crate::thumbnails::thumbnail_bytes(path).map(|t| crate::thumbnails::is_grayscale_jpeg(&t)).unwrap_or(false);
        (photo_id, gray)
    };
    let progress = |n: usize| state.send(CoreEvent::CacheProgress(CacheProgress { done: n, total, job }));
    let workers = max_workers.clamp(1, total);
    let grayscale = if workers == 1 {
        let mut grayscale = Vec::<(i64, bool)>::with_capacity(total);
        for (photo_id, path) in &items {
            if abort.load(Ordering::Relaxed) {
                break;
            }
            grayscale.push(warm(*photo_id, path));
            progress(grayscale.len());
            after_photo(grayscale.len());
        }
        grayscale
    } else {
        // The workers report each photo over a channel and this thread sends the events, so
        // every `cache:progress` leaves from the thread that runs the job.
        let found = std::sync::Mutex::new(Vec::<(i64, bool)>::with_capacity(total));
        let done = AtomicUsize::new(0);
        let (tx, rx) = std::sync::mpsc::channel::<usize>();
        std::thread::scope(|scope| {
            for chunk in items.chunks(total.div_ceil(workers)) {
                let (tx, found, done, warm) = (tx.clone(), &found, &done, &warm);
                scope.spawn(move || {
                    for (photo_id, path) in chunk {
                        // Stop before the next photo once a newer warm-up or a switch tripped it.
                        if abort.load(Ordering::Relaxed) {
                            return;
                        }
                        let flag = warm(*photo_id, path);
                        found.lock().unwrap_or_else(|e| e.into_inner()).push(flag);
                        let _ = tx.send(done.fetch_add(1, Ordering::Relaxed) + 1);
                    }
                });
            }
            drop(tx);
            for n in rx {
                progress(n);
                after_photo(n);
            }
        });
        found.into_inner().unwrap_or_else(|e| e.into_inner())
    };

    // Store the flags and refresh the monochrome auto-tag — in the catalog the photos were
    // read from, and only while this pass still owns the generation (checked under the lock).
    // One transaction: one commit for the whole library rather than one per photo, and the
    // flags and the tag they drive land together or not at all.
    with_catalog_as(state, from, |c| {
        if abort.load(Ordering::Relaxed) {
            return Ok(false);
        }
        let tx = c.begin()?;
        for (photo_id, gray) in &grayscale {
            c.set_grayscale(*photo_id, *gray)?;
        }
        c.apply_auto_tags()?;
        tx.commit()?;
        Ok(true)
    })
    .and_then(|stored| if stored { Ok(CacheResult { total }) } else { Err(CACHE_CANCELLED.into()) })
    .map_err(|e| if claim.aborted() { CACHE_CANCELLED.into() } else { e })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;
    use std::sync::Mutex;

    /// Records each `cache:progress` event's `(job, done, total)`.
    #[derive(Default)]
    struct Progress(Mutex<Vec<(u64, usize, usize)>>);

    impl EventSink for Progress {
        fn send(&self, event: CoreEvent) {
            if let CoreEvent::CacheProgress(p) = event {
                self.0.lock().unwrap().push((p.job, p.done, p.total));
            }
        }
    }

    /// A catalog of `n` photos whose originals exist (not decodable: the thumbnail fails and
    /// the flag is recorded `false`), each flagged grayscale beforehand so a store shows.
    fn setup(tag: &str, n: usize) -> (crate::test_support::TestTmpDir, AppState, Arc<Progress>, Vec<i64>) {
        let dir = crate::test_support::TestTmpDir::new(&format!("cache-{tag}"));
        let root = dir.join("library");
        std::fs::create_dir_all(&root).unwrap();
        let catalog = Catalog::open(&dir.join("c.chairphoto"), &root).unwrap();
        let ids = (0..n)
            .map(|i| {
                let path = root.join(format!("p{i}.jpg"));
                std::fs::write(&path, b"not a jpeg").unwrap();
                let id = catalog.upsert_photo(&path, None, 0, 1).unwrap().id;
                catalog.set_grayscale(id, true).unwrap();
                id
            })
            .collect();
        let state = AppState::default();
        let progress = Arc::new(Progress::default());
        state.set_events(progress.clone());
        *state.catalog.lock().unwrap() = Some(catalog);
        (dir, state, progress, ids)
    }

    fn grayscale(state: &AppState, id: i64) -> bool {
        crate::app::with_catalog(state, |c| c.is_grayscale(id)).unwrap()
    }

    #[test]
    fn a_warm_up_reports_progress_under_its_job_and_stores_the_flags() {
        let (_dir, state, progress, ids) = setup("run", 3);
        let claim = claim_cache(&state).unwrap();
        let result = cache_images_claimed(&state, &claim, false).unwrap();
        assert_eq!(result, CacheResult { total: 3 });
        let mut seen = progress.0.lock().unwrap().clone();
        seen.sort();
        assert_eq!(seen, [(claim.job, 1, 3), (claim.job, 2, 3), (claim.job, 3, 3)]);
        for id in ids {
            assert!(!grayscale(&state, id), "the flag was stored");
        }
    }

    /// **Forced interleaving.** A newer claim lands after the first photo: the older pass stops
    /// before its next photo, stores nothing and answers cancelled. A catalog switch does the
    /// same, and the new catalog is never written.
    #[test]
    fn a_newer_claim_or_a_switch_stops_the_pass_and_stores_nothing() {
        for how in ["newer", "switch"] {
            let (dir, state, progress, ids) = setup(&format!("abort-{how}"), 4);
            let claim = claim_cache(&state).unwrap();
            let trip = |n: usize| {
                if n != 1 {
                    return;
                }
                match how {
                    "newer" => drop(claim_cache(&state).unwrap()),
                    _ => {
                        crate::app::catalogs::detach_catalog_and_trip_jobs(&state).unwrap();
                        let b = Catalog::open(&dir.join("b.chairphoto"), &dir.join("b")).unwrap();
                        crate::app::catalogs::publish_catalog_and_reset_jobs(&state, b).unwrap();
                    }
                }
            };
            // One worker, so "before its next photo" is observable.
            let err = cache_images_with(&state, &claim, false, 1, &trip).unwrap_err();
            assert_eq!(err, CACHE_CANCELLED, "{how}");
            assert_eq!(progress.0.lock().unwrap().len(), 1, "{how}: the pass ran on after it was tripped");
            if how == "newer" {
                for id in ids {
                    assert!(grayscale(&state, id), "{how}: nothing stored");
                }
            } else {
                let started = Catalog::open_secondary(&dir.join("c.chairphoto"), &dir.join("library")).unwrap();
                for id in ids {
                    assert!(started.is_grayscale(id).unwrap(), "{how}: nothing stored");
                }
            }
        }
    }

    /// The store is one transaction: when the auto-tag step fails, no flag is stored either.
    #[test]
    fn the_flags_and_the_auto_tags_land_together_or_not_at_all() {
        let (_dir, state, _progress, ids) = setup("atomic", 2);
        // The photos are tagged monochrome now (flagged B&W); the store's rebuild of that
        // membership is made to fail, after the flags were written in the same transaction.
        crate::app::with_catalog(&state, |c| {
            c.apply_auto_tags()?;
            let tx = c.begin()?;
            tx.execute_batch("CREATE TRIGGER fail_tags BEFORE DELETE ON photo_tags BEGIN SELECT RAISE(ABORT, 'no tags'); END;")?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
        let err = cache_images(&state, false).unwrap_err();
        assert!(err.contains("no tags"), "{err}");
        for id in ids {
            assert!(grayscale(&state, id), "a flag was stored although the store failed");
        }
    }

    #[test]
    fn a_tripped_claim_decodes_nothing() {
        let (_dir, state, progress, _ids) = setup("pre-tripped", 2);
        let claim = claim_cache(&state).unwrap();
        let _newer = claim_cache(&state).unwrap();
        assert_eq!(cache_images_claimed(&state, &claim, true).unwrap_err(), CACHE_CANCELLED);
        assert!(progress.0.lock().unwrap().is_empty());
    }
}
