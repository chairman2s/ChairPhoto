//! The legacy sharpness re-measure (#262) as an owned job — started by the GPUI app once a
//! catalog is open. It **blocks** (makes a preview of every photo holding a legacy score,
//! possibly off a NAS): run it on a worker.
//!
//! A legacy score is one with no `sharpness_basis` stamp, from before #245, when the batch
//! indexer scored previews enlarged to 2048 px (`docs/sharpness-culling.md`). The decode hook
//! settles one only when the photo is next decoded at preview size, which for a photo whose
//! new preview is already cached, one never opened again, or one readable only outside the
//! catalog root, is never. This job settles every one it can reach
//! (`sharpness_indexer::run_legacy_rescore`): measured again when its preview is under
//! 2048 px, kept and stamped otherwise. An unscored photo is left to the decode hook, as
//! before. A photo that is offline, or whose preview fails, keeps its legacy score for the
//! next run.
//!
//! **Ownership** (`JobRegistry::sharpness`). [`claim_sharpness`] trips the running re-measure
//! and installs a fresh generation, numbered for the `sharpness:progress` and
//! `sharpness:index_done` events — one abort lock, never the catalog's, so a front end
//! claims on its UI thread. A newer claim or a catalog switch trips it; the run stops at its
//! next photo. The catalog's paths are read under one catalog lock hold, with the generation
//! checked in the same hold — a switch trips every generation under the catalog lock, so a
//! run that sees its flag un-tripped there opens the catalog it was claimed against — and
//! every write goes through a secondary connection to that catalog's own database, so a
//! straggling write after a switch lands on the rows it was read from, never on the new
//! catalog's. The result returned is the terminal signal; `sharpness:index_done` carries the
//! same outcome for a listener.

use super::{AppState, CoreEvent, EventSink as _, SharpnessIndexDone, SharpnessProgressEvent};
use crate::catalog::Catalog;
use crate::sharpness_indexer::{self, IndexOutcome};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// What a stopped re-measure answers (a newer claim or a catalog switch before it started).
pub const SHARPNESS_CANCELLED: &str = "Sharpness re-measure cancelled";

/// A re-measure's ownership: its generation of the family's abort flag, and the job id its
/// events carry.
#[derive(Clone)]
pub struct SharpnessClaim {
    pub abort: Arc<AtomicBool>,
    pub job: u64,
}

impl SharpnessClaim {
    /// Whether a newer claim or a catalog switch tripped it.
    pub fn aborted(&self) -> bool {
        self.abort.load(Ordering::Relaxed)
    }
}

/// Claim the sharpness generation: trip the running job, install a fresh flag and number the
/// job. One abort lock — cheap enough for a UI thread.
pub fn claim_sharpness(state: &AppState) -> Result<SharpnessClaim, String> {
    let (abort, job) = state.jobs.sharpness.install_fresh_numbered()?;
    Ok(SharpnessClaim { abort, job })
}

/// Stop the running re-measure at its next photo. One abort lock.
pub fn cancel_sharpness(state: &AppState) -> Result<(), String> {
    state.jobs.sharpness.trip()
}

/// Run the re-measure `claim` owns. Blocks. Already tripped: it reads nothing and answers
/// [`SHARPNESS_CANCELLED`]. A run tripped part-way answers its outcome, `aborted` set: what
/// it settled before it stopped is written and stays written.
pub fn rescore_legacy_claimed(state: &AppState, claim: &SharpnessClaim) -> Result<IndexOutcome, String> {
    rescore_legacy_with(state, claim, &|path: &Path| crate::thumbnails::preview_bytes(path), &|_| {})
}

/// [`rescore_legacy_claimed`] with the preview source injected, calling `after_photo(n)`
/// after the `n`th settled photo — where a test puts a newer claim or a switch.
fn rescore_legacy_with(
    state: &AppState,
    claim: &SharpnessClaim,
    preview_fn: &(dyn Fn(&Path) -> Result<Vec<u8>, String> + Sync),
    after_photo: &dyn Fn(usize),
) -> Result<IndexOutcome, String> {
    let job = claim.job;
    let result = run(state, claim, preview_fn, after_photo);
    let done = match &result {
        Ok(o) => SharpnessIndexDone {
            ok: true,
            done: o.done,
            kept: o.kept,
            total: o.total,
            failed: o.failed,
            offline: o.offline,
            aborted: o.aborted,
            job,
            error: None,
        },
        Err(e) => SharpnessIndexDone {
            ok: false,
            done: 0,
            kept: 0,
            total: 0,
            failed: 0,
            offline: 0,
            aborted: e == SHARPNESS_CANCELLED,
            job,
            error: Some(e.clone()),
        },
    };
    state.send(CoreEvent::SharpnessIndexDone(done));
    result
}

fn run(
    state: &AppState,
    claim: &SharpnessClaim,
    preview_fn: &(dyn Fn(&Path) -> Result<Vec<u8>, String> + Sync),
    after_photo: &dyn Fn(usize),
) -> Result<IndexOutcome, String> {
    // The catalog's location, read with the generation checked in the same lock hold.
    let (db_path, root) = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        if claim.aborted() {
            return Err(SHARPNESS_CANCELLED.into());
        }
        let c = guard.as_ref().ok_or("No catalog is open")?;
        (c.db_path().to_path_buf(), c.root().to_path_buf())
    };
    // Its own connection: the UI's reads never wait on this run, and its writes reach only
    // the database it was opened on.
    let sec = Catalog::open_secondary(&db_path, &root).map_err(|e| format!("couldn't open catalog connection: {e}"))?;
    let job = claim.job;
    let abort = &*claim.abort;
    let resolve = |id: i64| sec.resolve_photo_path(id).map_err(|e| e.to_string());
    let regions = |id: i64| sharpness_indexer::photo_regions(sec.conn(), id);
    let emit = |p: sharpness_indexer::SharpnessProgress| {
        state.send(CoreEvent::SharpnessProgress(SharpnessProgressEvent { done: p.done, total: p.total, job }));
        after_photo(p.done);
    };
    sharpness_indexer::run_legacy_rescore(
        sec.conn(),
        resolve,
        regions,
        &|p: &Path| preview_fn(p),
        &sharpness_indexer::score_jpeg_regions,
        abort,
        emit,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sharpness_indexer::SHARPNESS_BASIS;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Events(Mutex<Vec<CoreEvent>>);

    impl super::super::EventSink for Events {
        fn send(&self, event: CoreEvent) {
            if matches!(event, CoreEvent::SharpnessProgress(_) | CoreEvent::SharpnessIndexDone(_)) {
                self.0.lock().unwrap().push(event);
            }
        }
    }

    impl Events {
        fn progress(&self) -> Vec<(u64, usize, usize)> {
            let events = self.0.lock().unwrap();
            events
                .iter()
                .filter_map(|e| match e {
                    CoreEvent::SharpnessProgress(p) => Some((p.job, p.done, p.total)),
                    _ => None,
                })
                .collect()
        }

        fn done(&self) -> Vec<SharpnessIndexDone> {
            let events = self.0.lock().unwrap();
            events
                .iter()
                .filter_map(|e| match e {
                    CoreEvent::SharpnessIndexDone(d) => Some(d.clone()),
                    _ => None,
                })
                .collect()
        }
    }

    fn jpeg(w: u32, h: u32) -> Vec<u8> {
        let img = image::GrayImage::from_fn(w, h, |x, y| image::Luma([if (x / 4 + y / 4) % 2 == 0 { 0 } else { 255 }]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageLuma8(img).write_to(&mut out, image::ImageFormat::Jpeg).unwrap();
        out.into_inner()
    }

    /// A preview source standing in for `thumbnails::preview_bytes`: `large*` files have a
    /// 2048 px preview, every other file a 1200 px one.
    fn previews(path: &Path) -> Result<Vec<u8>, String> {
        let name = path.file_name().unwrap().to_string_lossy();
        Ok(if name.starts_with("large") { jpeg(2048, 1365) } else { jpeg(1200, 800) })
    }

    /// A catalog whose originals exist, one per name. Names starting `legacy` hold a legacy
    /// score (0.5, no stamp), `current` a stamped one (0.5), anything else none.
    fn setup(tag: &str, names: &[&str]) -> (crate::test_support::TestTmpDir, AppState, Arc<Events>, Vec<i64>) {
        let dir = crate::test_support::TestTmpDir::new(&format!("sharpness-{tag}"));
        let root = dir.join("library");
        std::fs::create_dir_all(&root).unwrap();
        let catalog = Catalog::open(&dir.join("c.chairphoto"), &root).unwrap();
        let ids = names
            .iter()
            .map(|name| {
                let path = root.join(name);
                std::fs::write(&path, b"x").unwrap();
                let id = catalog.upsert_photo(&path, None, 0, 1).unwrap().id;
                let basis = if name.starts_with("current") { Some(SHARPNESS_BASIS) } else { None };
                if name.starts_with("legacy") || name.starts_with("large") || basis.is_some() {
                    catalog
                        .conn()
                        .execute(
                            "UPDATE photos SET sharpness = 0.5, sharpness_method = 'tile', sharpness_basis = ?2 WHERE id = ?1",
                            rusqlite::params![id, basis],
                        )
                        .unwrap();
                }
                id
            })
            .collect();
        let state = AppState::default();
        let events = Arc::new(Events::default());
        state.set_events(events.clone());
        *state.catalog.lock().unwrap() = Some(catalog);
        (dir, state, events, ids)
    }

    fn row(c: &Catalog, id: i64) -> (Option<f64>, Option<i64>) {
        c.conn()
            .query_row("SELECT sharpness, sharpness_basis FROM photos WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
    }

    fn row_in(state: &AppState, id: i64) -> (Option<f64>, Option<i64>) {
        row(state.catalog.lock().unwrap().as_ref().unwrap(), id)
    }

    /// M1: a legacy score that no decode will ever reach is settled by the job — measured
    /// again from a preview under 2048 px, kept and stamped from a 2048 px one — with no
    /// photo opened. A current score and an unscored photo are not touched.
    #[test]
    fn the_job_settles_legacy_scores_no_decode_reached() {
        let (_dir, state, events, ids) = setup("run", &["legacy.jpg", "large.jpg", "current.jpg", "new.jpg"]);
        let claim = claim_sharpness(&state).unwrap();
        let outcome = rescore_legacy_with(&state, &claim, &previews, &|_| {}).unwrap();
        assert_eq!((outcome.total, outcome.done, outcome.kept, outcome.aborted), (2, 1, 1, false));

        let (score, basis) = row_in(&state, ids[0]);
        assert!(score.is_some_and(|s| s != 0.5), "the enlarged-preview score is measured again, got {score:?}");
        assert_eq!(basis, Some(SHARPNESS_BASIS));
        assert_eq!(row_in(&state, ids[1]), (Some(0.5), Some(SHARPNESS_BASIS)), "kept and stamped");
        assert_eq!(row_in(&state, ids[2]), (Some(0.5), Some(SHARPNESS_BASIS)), "a current score is untouched");
        assert_eq!(row_in(&state, ids[3]), (None, None), "an unscored photo is left to the decode hook");

        let mut seen = events.progress();
        seen.sort();
        assert_eq!(seen, [(claim.job, 1, 2), (claim.job, 2, 2)]);
        let done = events.done();
        assert_eq!(done.len(), 1, "one terminal event");
        assert!(done[0].ok && done[0].job == claim.job && (done[0].done, done[0].kept) == (1, 1));
    }

    /// A photo the job cannot reach keeps its legacy score, still queued for the next run.
    #[test]
    fn an_offline_photo_keeps_its_legacy_score() {
        let (dir, state, _events, ids) = setup("offline", &["legacy.jpg"]);
        std::fs::remove_file(dir.join("library").join("legacy.jpg")).unwrap();
        let claim = claim_sharpness(&state).unwrap();
        let outcome = rescore_legacy_with(&state, &claim, &previews, &|_| {}).unwrap();
        assert_eq!((outcome.total, outcome.done, outcome.offline), (1, 0, 1));
        assert_eq!(row_in(&state, ids[0]), (Some(0.5), None));
    }

    /// **Forced interleaving.** A newer claim, or a catalog switch, lands after the first
    /// photo is settled: the run stops before the next, and after a switch the new catalog
    /// — whose rows reuse the old one's ids — is never written.
    #[test]
    fn a_newer_claim_or_a_switch_stops_the_run() {
        for how in ["newer", "switch"] {
            let names: Vec<String> = (0..3).map(|i| format!("legacy{i}.jpg")).collect();
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            let (dir, state, events, ids) = setup(&format!("abort-{how}"), &names);
            // The switch's catalog: same ids, legacy scores of its own.
            let b_root = dir.join("b");
            std::fs::create_dir_all(&b_root).unwrap();
            let b = Catalog::open(&dir.join("b.chairphoto"), &b_root).unwrap();
            for name in &names {
                let path = b_root.join(name);
                std::fs::write(&path, b"x").unwrap();
                let id = b.upsert_photo(&path, None, 0, 1).unwrap().id;
                b.conn().execute("UPDATE photos SET sharpness = 0.5, sharpness_method = 'tile' WHERE id = ?1", [id]).unwrap();
            }
            let b = Mutex::new(Some(b));
            let claim = claim_sharpness(&state).unwrap();
            let trip = |n: usize| {
                if n != 1 {
                    return;
                }
                match how {
                    "newer" => drop(claim_sharpness(&state).unwrap()),
                    _ => {
                        crate::app::catalogs::detach_catalog_and_trip_jobs(&state).unwrap();
                        let b = b.lock().unwrap().take().unwrap();
                        crate::app::catalogs::publish_catalog_and_reset_jobs(&state, b).unwrap();
                    }
                }
            };
            // The id order is the queue's, and the photos settle in parallel within a chunk,
            // so "stops before the next" is read from the rows written, not from which ones.
            let outcome = rescore_legacy_with(&state, &claim, &previews, &trip).unwrap();
            assert!(outcome.aborted, "{how}: the run noticed it was tripped");
            let a = Catalog::open_secondary(&dir.join("c.chairphoto"), &dir.join("library")).unwrap();
            let stamped = ids.iter().filter(|&&id| row(&a, id).1.is_some()).count();
            assert_eq!(stamped, 1, "{how}: the run went on after it was tripped");
            assert_eq!(events.progress().len(), 1, "{how}");
            if how == "switch" {
                for &id in &ids {
                    assert_eq!(row_in(&state, id), (Some(0.5), None), "{how}: the new catalog was written");
                }
            }
        }
    }

    /// A claim tripped before it ran reads nothing and answers cancelled, with its terminal
    /// event.
    #[test]
    fn a_tripped_claim_settles_nothing() {
        let (_dir, state, events, ids) = setup("pre-tripped", &["legacy.jpg"]);
        let claim = claim_sharpness(&state).unwrap();
        let _newer = claim_sharpness(&state).unwrap();
        assert_eq!(rescore_legacy_with(&state, &claim, &previews, &|_| {}).unwrap_err(), SHARPNESS_CANCELLED);
        assert!(events.progress().is_empty());
        assert_eq!(row_in(&state, ids[0]), (Some(0.5), None));
        let done = events.done();
        assert!(done.len() == 1 && !done[0].ok && done[0].aborted && done[0].job == claim.job);
    }
}
