//! Smart Tagging's app layer: classifier training's lock phases, and the index job's
//! ownership across starts and catalog switches. Moved from the Tauri shell's
//! `commands/smarttags.rs` with the bodies they test (#126).
//!
//! The ownership tests drive the real transitions — [`begin_index_job`] and the two
//! `switch_catalog` phases — against a real `AppState` and real catalogs. The switch side is
//! forced by construction, never by timing: the switch runs inside the indexer's own progress
//! callback, and the start that runs between the two switch phases is driven to the exact
//! state a blocked start observes. `a_blocked_start_keeps_holding_the_catalog_lock` is the one
//! that discriminates the start side: it parks a start inside its claim and observes which
//! locks it holds there. The last ownership test is a contention net whose assertion holds
//! under every legal interleaving; it samples rather than forces, so it backs the others up.
//!
//! The slot-ownership guards are `JobSlot::publish` / `JobSlot::clear`, tested once in
//! `app::jobs`.

/// Classifier training holds the catalog lock only for its two SQLite phases, and abandons
/// the write if the catalog is replaced in between. Both interleavings are forced through
/// `train_classifiers_phased`'s hook rather than timed.
mod training_lock {
    use super::super::*;
    use crate::app::AppState;
    use crate::plugins::smarttags::store;
    use std::path::Path;

    /// A catalog with `photos` photos, half of them carrying one tag and **all** of them
    /// carrying a CLIP embedding — so the trainer sees exactly one stale tag with both
    /// positives and negatives to work on.
    fn trainable_catalog(tag: &str, photos: usize) -> (Catalog, crate::test_support::TestSubPath) {
        let dir = crate::test_support::TestTmpDir::new(&format!("smarttags-train-{tag}"));
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let db = dir.join("catalog.chairphoto");
        let catalog = Catalog::open(&db, &root).unwrap();

        store::ensure_schema(catalog.conn()).unwrap();
        classifier::ensure_schema(catalog.conn()).unwrap();
        // The staleness scan reads `smarttags__suggestions`, which neither of the schemas
        // above creates — `suggest::ensure_suggestions_schema` does, on the suggestion
        // path. A real catalog has it by the time training runs; the fixture matches that.
        crate::plugins::smarttags::suggest::ensure_suggestions_schema(catalog.conn()).unwrap();
        let tag_id = catalog.create_tag("Test/Trainable").unwrap();

        // A unit embedding, varied slightly per photo so positives and negatives are not
        // literally the same vector.
        for i in 0..photos {
            let p = root.join(format!("p{i}.jpg"));
            std::fs::write(&p, b"jpeg").unwrap();
            catalog.upsert_photo(&p, None, 1, 4).unwrap();
        }
        let ids: Vec<i64> = {
            let mut stmt = catalog.conn().prepare("SELECT id FROM photos ORDER BY id").unwrap();
            let rows = stmt
                .query_map([], |r| r.get::<_, i64>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            rows
        };
        for (n, id) in ids.iter().enumerate() {
            let mut v = vec![0.0f32; 512];
            v[n % 512] = 1.0;
            store::upsert_embedding(catalog.conn(), *id, &store::embedding_to_blob(&v)).unwrap();
            // Tag the first half; the rest are the negative class.
            if n < ids.len() / 2 {
                catalog.assign_tag(*id, tag_id).unwrap();
            }
        }
        (catalog, dir.into_subpath("catalog.chairphoto"))
    }

    /// An `AppState` holding `catalog` as the open catalog.
    fn state_with(catalog: Catalog) -> AppState {
        let state = AppState::default();
        *state.catalog.lock().unwrap() = Some(catalog);
        state
    }

    const TAG_A: &str = "Test/TrainableA";
    const TAG_B: &str = "Test/TrainableB";

    /// Two stale tags, 12 embedded photos each, disjoint. `scan_stale_tags` orders by
    /// `full_path`, so A is always trained before B.
    fn two_tag_trainable_catalog(tag: &str) -> (Catalog, crate::test_support::TestSubPath) {
        let dir = crate::test_support::TestTmpDir::new(&format!("smarttags-train2-{tag}"));
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let db = dir.join("catalog.chairphoto");
        let catalog = Catalog::open(&db, &root).unwrap();

        store::ensure_schema(catalog.conn()).unwrap();
        classifier::ensure_schema(catalog.conn()).unwrap();
        crate::plugins::smarttags::suggest::ensure_suggestions_schema(catalog.conn()).unwrap();
        let a = catalog.create_tag(TAG_A).unwrap();
        let b = catalog.create_tag(TAG_B).unwrap();

        for i in 0..24usize {
            let p = root.join(format!("p{i}.jpg"));
            std::fs::write(&p, b"jpeg").unwrap();
            let id = catalog.upsert_photo(&p, None, 1, 4).unwrap().id;
            let mut v = vec![0.0f32; 512];
            v[i % 512] = 1.0;
            store::upsert_embedding(catalog.conn(), id, &store::embedding_to_blob(&v)).unwrap();
            catalog.assign_tag(id, if i < 12 { a } else { b }).unwrap();
        }
        (catalog, dir.into_subpath("catalog.chairphoto"))
    }

    /// Photos currently carrying `tag_path`.
    fn tagged_photo_count(db: &Path, tag_path: &str) -> usize {
        let conn = rusqlite::Connection::open(db).unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM photo_tags pt JOIN tags t ON t.id = pt.tag_id
              WHERE t.full_path = ?1",
            [tag_path],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0) as usize
    }

    /// `sample_count` on the persisted classifier — the number of positives training
    /// actually used, which is what makes the snapshot observable after the fact.
    fn persisted_sample_count(db: &Path, tag_path: &str) -> i64 {
        let conn = rusqlite::Connection::open(db).unwrap();
        conn.query_row(
            "SELECT sample_count FROM smarttags__classifiers WHERE tag_path = ?1",
            [tag_path],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(-1)
    }

    /// Rows in `smarttags__classifiers`, counting an absent table as zero.
    fn classifier_count(db: &Path) -> i64 {
        let conn = rusqlite::Connection::open(db).unwrap();
        conn.query_row("SELECT COUNT(*) FROM smarttags__classifiers", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap_or(0)
    }

    /// The acceptance criterion, observed rather than argued: at the moment training is
    /// about to burn CPU, the catalog lock is free for the UI to take.
    ///
    /// Mutation-checked: wrapping the load/train loop in a held catalog guard (the shape
    /// this change replaced) makes the `try_lock` assertion fail.
    #[test]
    fn cpu_training_runs_with_the_catalog_lock_released() {
        let (catalog, db) = trainable_catalog("lock-free", 24);
        let state = state_with(catalog);

        let mut hook_fired = 0usize;
        let result = train_classifiers_phased(&state.catalog, None, || {
            assert!(
                state.catalog.try_lock().is_ok(),
                "the catalog lock was held while training burned CPU"
            );
            hook_fired += 1;
        })
        .unwrap();


        assert_eq!(hook_fired, 1, "hook never fired — the assertion above proved nothing");
        assert_eq!(result.examined, 1);
        assert_eq!(result.trained, 1);
        assert_eq!(result.skipped, 0);
        assert_eq!(classifier_count(&db), 1);
    }

    /// Everything the run reads comes from one snapshot, so a write that lands mid-run
    /// cannot be half-included.
    ///
    /// The hook adds a 13th tagged photo *after* the staleness scan and after this tag's
    /// embeddings were read. Without the read transaction the two reads are independent, so
    /// whether the new photo is trained on depends on when it landed. With it, the run is
    /// reading a catalog that predates the write, and `sample_count` — persisted from the
    /// positives actually used — proves which one it saw.
    ///
    /// The write goes through the primary connection while phase 2 holds none of it, which
    /// is the real interleaving: the Smart Tagging indexer writes embeddings the same way,
    /// through a connection that never takes the catalog lock.
    #[test]
    fn training_reads_one_snapshot_even_when_the_catalog_changes_under_it() {
        // Two stale tags, processed in `full_path` order, so tag B is still unread when the
        // hook fires for tag A. A single-tag fixture cannot test this: its only read has
        // already happened by the time the hook runs, so the write can never be observed
        // and the test passes with or without the transaction.
        let (catalog, db) = two_tag_trainable_catalog("snapshot");
        let b_before = tagged_photo_count(&db, TAG_B);
        let state = state_with(catalog);

        let mut wrote = false;
        let result = train_classifiers_phased(&state.catalog, None, || {
            if wrote {
                return;
            }
            wrote = true;
            // Add a photo to tag B while tag A trains — before B has been read.
            let guard = state.catalog.lock().unwrap();
            let c = guard.as_ref().unwrap();
            let tag_id = c.find_tag_id_by_path(TAG_B).unwrap().unwrap();
            let p = c.root().join("late.jpg");
            std::fs::write(&p, b"jpeg").unwrap();
            let id = c.upsert_photo(&p, None, 1, 4).unwrap().id;
            let mut v = vec![0.0f32; 512];
            v[7] = 1.0;
            store::upsert_embedding(c.conn(), id, &store::embedding_to_blob(&v)).unwrap();
            c.assign_tag(id, tag_id).unwrap();
        })
        .unwrap();

        assert!(wrote, "hook never fired — nothing was written under the run");
        assert_eq!(result.trained, 2, "both tags should have been trained");
        assert_eq!(
            tagged_photo_count(&db, TAG_B),
            b_before + 1,
            "the fixture's own write did not land, so this proves nothing"
        );
        assert_eq!(
            persisted_sample_count(&db, TAG_B),
            b_before as i64,
            "tag B was read after the write and picked it up — the run is not on one snapshot"
        );
    }

    /// A catalog swapped in while the lock is released must not receive this run's
    /// classifiers, and the run must say it failed rather than report a partial success.
    #[test]
    fn a_catalog_switch_mid_training_writes_nothing() {
        let (catalog, db) = trainable_catalog("switch-src", 24);
        let (other, other_db) = trainable_catalog("switch-dst", 24);
        let state = state_with(catalog);

        // Forced, not timed: the swap happens inside the one window where the trainer has
        // let go of the lock.
        let mut swap = Some(other);
        let err = train_classifiers_phased(&state.catalog, None, || {
            if let Some(next) = swap.take() {
                *state.catalog.lock().unwrap() = Some(next);
            }
        })
        .unwrap_err();

        assert_eq!(err, SWITCHED_MID_TRAIN);
        assert_eq!(classifier_count(&db), 0, "wrote into the catalog the user left");
        assert_eq!(classifier_count(&other_db), 0, "wrote into the catalog it switched to");
    }

    /// Bound to the catalog a front end showed, a run refuses a re-opening of the *same file*
    /// swapped in mid-run: the path check alone cannot see that (same `db_path`), the identity
    /// can (map #92, "Catalog identity").
    #[test]
    fn a_bound_run_writes_nothing_after_a_switch_away_and_back() {
        let (catalog, db) = trainable_catalog("reopen", 24);
        let (db_path, root) = (catalog.db_path().to_path_buf(), catalog.root().to_path_buf());
        let state = state_with(catalog);
        let shown = crate::app::catalog_identity(&state).unwrap();

        let mut swapped = false;
        let err = train_classifiers_phased(&state.catalog, Some(shown), || {
            if !swapped {
                swapped = true;
                *state.catalog.lock().unwrap() = Some(Catalog::open(&db_path, &root).unwrap());
            }
        })
        .unwrap_err();

        assert!(swapped, "the hook never fired");
        assert_eq!(err, SWITCHED_MID_TRAIN);
        assert_eq!(classifier_count(&db), 0);
    }

    /// Bound to a catalog that is no longer open, a run refuses before it reads anything.
    #[test]
    fn a_bound_run_refuses_another_open_catalog() {
        let (catalog, _db) = trainable_catalog("bound-src", 24);
        let (other, other_db) = trainable_catalog("bound-dst", 24);
        let state = state_with(catalog);
        let shown = crate::app::catalog_identity(&state).unwrap();
        *state.catalog.lock().unwrap() = Some(other);

        let err = train_classifiers(&state, Some(shown)).unwrap_err();
        assert_eq!(err, crate::app::CATALOG_CHANGED);
        assert_eq!(classifier_count(&other_db), 0);
    }
}

mod ownership {
    use super::super::*;
    use crate::app::{detach_catalog_and_trip_jobs, publish_catalog_and_reset_jobs, AppState};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    /// A fresh catalog in its own temp dir with `photos` files imported.
    fn temp_catalog(
        tag: &str,
        photos: usize,
    ) -> (Catalog, crate::test_support::TestSubPath, PathBuf) {
        let dir = crate::test_support::TestTmpDir::new(&format!("smarttags-own-{tag}"));
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let db = dir.join("catalog.chairphoto");
        let catalog = Catalog::open(&db, &root).unwrap();
        for i in 0..photos {
            let p = root.join(format!("p{i}.jpg"));
            std::fs::write(&p, b"jpeg").unwrap();
            catalog.upsert_photo(&p, None, 1, 4).unwrap();
        }
        (catalog, dir.into_subpath("catalog.chairphoto"), root)
    }

    /// An `AppState` holding `catalog` as the open catalog.
    fn state_with(catalog: Catalog) -> AppState {
        let state = AppState::default();
        *state.catalog.lock().unwrap() = Some(catalog);
        state
    }

    /// Rows in `smarttags__embeddings`, counting an absent table as zero — the table is
    /// created lazily by the first index run, so "never touched" reads as 0 either way.
    fn embedding_count(db: &Path) -> i64 {
        let conn = rusqlite::Connection::open(db).unwrap();
        conn.query_row("SELECT COUNT(*) FROM smarttags__embeddings", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap_or(0)
    }

    /// A synthetic unit embedding — the injected stand-in for CLIP, so these tests need
    /// neither a model nor an ONNX Runtime.
    fn unit_embedding() -> Vec<f32> {
        let mut v = vec![0.0f32; 512];
        v[0] = 1.0;
        v
    }

    /// Wait until `state.catalog` has been locked *continuously* for `window`, giving up
    /// after `timeout`.
    ///
    /// One failed `try_lock` would prove nothing: the pre-fix start held the catalog lock
    /// too, for the microseconds it took to copy two paths out of the catalog, so a single
    /// sample can catch it by luck. The observation that discriminates is that the lock
    /// stays held — which only happens when the holder is parked while owning it.
    ///
    /// `try_lock` never blocks, so a caller holding the abort lock can probe the catalog
    /// lock from here without inverting the catalog → abort order.
    fn catalog_stays_locked(state: &AppState, window: Duration, timeout: Duration) -> bool {
        let give_up = Instant::now() + timeout;
        while Instant::now() < give_up {
            let locked = state.catalog.try_lock().is_err();
            if locked {
                let until = Instant::now() + window;
                let mut still_locked = true;
                while Instant::now() < until {
                    if state.catalog.try_lock().is_ok() {
                        still_locked = false;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                if still_locked {
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        false
    }

    /// A catalog switch stops a running Smart Tagging job at its next cancellation point:
    /// no progress after the switch, no further rows in the catalog it was indexing, and
    /// nothing at all in the catalog the user switched to.
    ///
    /// The interleaving is forced rather than timed: the switch runs inside the indexer's
    /// own progress callback, so it lands between photo 1 and photo 2 every time.
    #[test]
    fn catalog_switch_stops_a_running_index_job() {
        let (cat_a, db_a, root_a) = temp_catalog("switch-a", 4);
        let (cat_b, db_b, _root_b) = temp_catalog("switch-b", 0);
        let state = state_with(cat_a);

        // Start a job exactly the way `smarttags_index_photos` does.
        let JobClaim { db_path, root, abort, job, slot: _ } = begin_index_job(&state, None).unwrap();
        assert_eq!(db_path, db_a.to_path_buf());
        assert_eq!(root, root_a);
        assert_eq!(
            state.jobs.smarttags.status().unwrap().map(|s| s.job),
            Some(job),
            "the start must claim the status slot"
        );

        // The worker: the real indexer over a real secondary connection to catalog A.
        let sec = Catalog::open_secondary(&db_path, &root).unwrap();
        let switch_to = Mutex::new(Some(cat_b));
        let mut progress: Vec<usize> = Vec::new();

        let outcome = indexer::run_index(
            sec.conn(),
            // One photo per chunk, so every photo is a cancellation point.
            1,
            |id| Ok(Some(root.join(format!("p{id}.jpg")))),
            |_path| Ok(vec![0xFFu8, 0xD8, 0xFF, 0xE0]),
            |_jpeg| Some(unit_embedding()),
            &abort,
            |p: indexer::SmarttagsProgress| {
                progress.push(p.done);
                // The user switches catalogs while this worker is still running.
                if let Some(cat) = switch_to.lock().unwrap().take() {
                    detach_catalog_and_trip_jobs(&state).unwrap();
                    publish_catalog_and_reset_jobs(&state, cat).unwrap();
                }
            },
        )
        .unwrap();

        assert!(outcome.aborted, "the switch must abort the running job");
        assert_eq!(outcome.total, 4, "all four photos were queued");
        assert_eq!(
            outcome.done, 1,
            "only the photo already committed when the switch happened"
        );
        assert_eq!(progress, vec![1], "no progress event after the switch");
        assert_eq!(
            embedding_count(&db_a),
            1,
            "the aborted worker must write no rows after the switch"
        );
        assert_eq!(
            embedding_count(&db_b),
            0,
            "nothing may land in the catalog that was switched to"
        );

        // The old job's flag stays tripped, and the generation installed for the new
        // catalog is a different, un-tripped Arc — so the old worker cannot be revived by
        // it, and a Cancel or a later switch acts on the new generation only.
        assert!(abort.load(Ordering::Relaxed), "the old flag must stay tripped");
        let installed = state.jobs.smarttags.installed().unwrap();
        assert!(
            !Arc::ptr_eq(&installed, &abort),
            "the switch must install a fresh generation, not reuse the aborted one"
        );
        assert!(
            !installed.load(Ordering::Relaxed),
            "the new catalog's generation starts un-tripped"
        );
    }

    /// Between the two switch phases the catalog reads as `None`, so a start returns having
    /// touched nothing: no generation installed, no job id burned, status slot unchanged.
    /// Phase two then publishes the new catalog without resurrecting the superseded job, and
    /// the next start targets the catalog that was switched to.
    ///
    /// Scope, deliberately narrow: this test starts with the catalog **already detached**, so
    /// it exercises the switch side only and would pass on the pre-fix start as well. It does
    /// not show that a start racing the switch ends up here rather than sailing past — that
    /// is `a_blocked_start_keeps_holding_the_catalog_lock`, which is what makes the blocked
    /// case and this case the same case.
    #[test]
    fn start_between_switch_phases_touches_nothing() {
        let (cat_a, db_a, _root_a) = temp_catalog("mid-a", 1);
        let (cat_b, db_b, root_b) = temp_catalog("mid-b", 0);
        let state = state_with(cat_a);

        let first = begin_index_job(&state, None).unwrap();
        assert_eq!(first.db_path, db_a.to_path_buf());

        // Phase one: the running job is tripped and the outgoing catalog is detached.
        detach_catalog_and_trip_jobs(&state).unwrap();
        assert!(
            first.abort.load(Ordering::Relaxed),
            "phase one must trip the running job"
        );

        let before = state.jobs.smarttags.installed().unwrap();
        let seq_before = state.jobs.smarttags.abort().job_ids_issued();
        let slot_before = state.jobs.smarttags.status().unwrap();

        let err = begin_index_job(&state, None).unwrap_err();
        assert_eq!(err, "No catalog is open");

        let after = state.jobs.smarttags.installed().unwrap();
        assert!(
            Arc::ptr_eq(&before, &after),
            "a start mid-switch must not install a generation"
        );
        assert!(
            after.load(Ordering::Relaxed),
            "the installed generation is still the tripped one"
        );
        assert_eq!(
            state.jobs.smarttags.abort().job_ids_issued(),
            seq_before,
            "a rejected start must not burn a job id"
        );
        assert_eq!(
            state.jobs.smarttags.status().unwrap().map(|s| s.job),
            slot_before.map(|s| s.job),
            "a rejected start must leave the status slot alone"
        );

        // Phase two publishes the new catalog with a fresh generation — and does not clear
        // the old job's flag, whose worker must stay aborted.
        publish_catalog_and_reset_jobs(&state, cat_b).unwrap();
        assert!(
            first.abort.load(Ordering::Relaxed),
            "phase two must not resurrect the superseded job"
        );

        // A start after the switch snapshots the NEW catalog and becomes the reachable
        // generation.
        let second = begin_index_job(&state, None).unwrap();
        assert_eq!(
            second.db_path,
            db_b.to_path_buf(),
            "the new job must index the catalog switched to"
        );
        assert_eq!(second.root, root_b);
        assert_ne!(second.job, first.job, "each start gets its own job id");
        assert!(!second.abort.load(Ordering::Relaxed));
        let installed = state.jobs.smarttags.installed().unwrap();
        assert!(
            Arc::ptr_eq(&second.abort, &installed),
            "the new job's flag must be the installed generation"
        );
        assert_eq!(
            state.jobs.smarttags.status().unwrap().map(|s| s.job),
            Some(second.job),
            "the new job owns the status slot"
        );
        assert!(second.slot.owns(), "and its slot handle agrees");
    }

    /// A start that is blocked partway through its claim is *still holding the catalog lock*.
    ///
    /// This is the start-side half of the protocol, and the only test here that discriminates
    /// it. The switch-side tests above pass on the pre-fix shape too: a start that runs after
    /// phase one reads no catalog either way. What closes the window is that the claim takes
    /// the catalog lock **first** and keeps it across the abort-flag install, leaving a switch
    /// only two outcomes — it takes the catalog lock before the start (and the start then
    /// finds no catalog open) or after it (and phase one trips the generation the start just
    /// installed). The pre-fix shape read the catalog under its own short-lived lock and
    /// released it before touching the abort flag, which admitted a third outcome: snapshot
    /// catalog A, let the switch trip every generation, then install a live generation that
    /// no Cancel, later start or subsequent switch can reach.
    ///
    /// Holding the family's abort lock parks a start exactly at that seam, because the order
    /// is catalog → abort → slot: to be waiting on the abort lock it must already own the
    /// catalog lock. So the catalog lock stays held for as long as this test holds the abort
    /// lock — and under the pre-fix shape it would be free, because that start had already
    /// let go of it before it ever reached the abort lock.
    #[test]
    fn a_blocked_start_keeps_holding_the_catalog_lock() {
        let (cat_a, db_a, _root_a) = temp_catalog("blocked-a", 0);
        let state = state_with(cat_a);

        // Take the lock the claim needs second, then park a start behind it.
        let abort_held = state.jobs.smarttags.abort().lock().unwrap();

        std::thread::scope(|scope| {
            let start = scope.spawn(|| begin_index_job(&state, None));

            assert!(
                catalog_stays_locked(&state, Duration::from_millis(100), Duration::from_secs(5)),
                "a start waiting on the abort lock must still be holding the catalog lock; \
                 one that reads the catalog and releases it first leaves a window in which a \
                 switch can trip every generation and still be overtaken by this start"
            );

            // Release it and confirm the claim it was parked in the middle of completes
            // normally — the test observes a stalled transition, it does not break one.
            drop(abort_held);
            let claim = start.join().unwrap().unwrap();
            assert_eq!(
                claim.db_path,
                db_a.to_path_buf(),
                "the parked start indexes the catalog it snapshotted"
            );
            let installed = state.jobs.smarttags.installed().unwrap();
            assert!(
                Arc::ptr_eq(&claim.abort, &installed),
                "the unblocked start's flag must be the installed generation"
            );
            assert_eq!(
                state.jobs.smarttags.status().unwrap().map(|s| s.job),
                Some(claim.job),
                "and it must own the status slot"
            );
        });
    }

    /// Under real contention between starts and switches, every start that returned `Ok`
    /// must satisfy: its generation is tripped (a switch reached it), or it is the installed
    /// generation AND it snapshotted the catalog that is now open. A live generation that is
    /// not installed is an orphan nothing can cancel; a live generation indexing a catalog
    /// the switch has already left is the write-after-switch this issue is about.
    ///
    /// The invariant holds under every legal interleaving, so the assertion never depends on
    /// which one the scheduler picks — but the interleavings it *samples* do, which is why
    /// the two deterministic tests above carry the load and this one is a net over the rest.
    /// It is checked after every round, not only at the end, so a bad install cannot be
    /// papered over by the next round's abort.
    #[test]
    fn concurrent_starts_and_switches_leave_no_unreachable_generation() {
        let (cat_a, db_a, root_a) = temp_catalog("race-a", 0);
        let (cat_b, db_b, root_b) = temp_catalog("race-b", 0);
        // Each round re-opens the catalog it switches to, as a real switch does; the two
        // alternate so a stale snapshot is distinguishable from a fresh one.
        drop(cat_b);
        let state = state_with(cat_a);

        let started: Mutex<Vec<(PathBuf, Arc<AtomicBool>)>> = Mutex::new(Vec::new());
        // One uncontended start, so the check below cannot be vacuous even in the unlikely
        // case that every racing start lands in the mid-switch window.
        let first = begin_index_job(&state, None).unwrap();
        started.lock().unwrap().push((first.db_path, first.abort));

        for round in 0..50 {
            let (next_db, next_root) = if round % 2 == 0 {
                (&db_b, &root_b)
            } else {
                (&db_a, &root_a)
            };
            std::thread::scope(|s| {
                s.spawn(|| {
                    if let Ok(c) = begin_index_job(&state, None) {
                        started.lock().unwrap().push((c.db_path, c.abort));
                    }
                });
                s.spawn(|| {
                    detach_catalog_and_trip_jobs(&state).unwrap();
                    let cat = Catalog::open(next_db, next_root).unwrap();
                    publish_catalog_and_reset_jobs(&state, cat).unwrap();
                });
            });

            let installed = state.jobs.smarttags.installed().unwrap();
            let open_db = state
                .catalog
                .lock()
                .unwrap()
                .as_ref()
                .map(|c| c.db_path().to_path_buf());
            for (i, (db, abort)) in started.lock().unwrap().iter().enumerate() {
                if abort.load(Ordering::Relaxed) {
                    continue;
                }
                assert!(
                    Arc::ptr_eq(abort, &installed),
                    "round {round}: start #{i} left a live generation that no cancel or \
                     switch can reach"
                );
                assert_eq!(
                    Some(db.clone()),
                    open_db,
                    "round {round}: start #{i} is still live against a catalog the switch \
                     has left"
                );
            }
        }
    }

    /// Acceptance criterion 3: "Status/progress/terminal updates remain scoped to the owning
    /// job id" — checked here on the **wiring**, not on the guard.
    ///
    /// The guard itself is `JobSlot::publish` / `JobSlot::clear`, tested once in
    /// `commands::jobs` (`slot_writes_are_scoped_to_the_owning_job`) where the same
    /// implementation also covers face indexing and matching. What that test cannot show is
    /// that *this* command's worker handle points at the very slot `smarttags_index_status`
    /// reads. So this one runs two real starts against a real `AppState` and drives the
    /// superseded claim's handle: if the wiring were wrong — a handle onto some other
    /// `Arc<Mutex<…>>` — the superseded run's clear would appear to succeed here.
    ///
    /// Both branches are asserted: the superseded run cannot clear, the owner can.
    #[test]
    fn the_worker_handle_writes_the_slot_status_queries_read() {
        let (cat, _db, _root) = temp_catalog("slot-wiring", 0);
        let state = state_with(cat);

        let superseded = begin_index_job(&state, None).unwrap();
        let owner = begin_index_job(&state, None).unwrap();

        // The superseded run reports progress after a newer one claimed the slot.
        superseded.slot.publish(|job| SmarttagsJobStatus { job, done: 42, total: 100 });
        assert_eq!(
            state.jobs.smarttags.status().unwrap().map(|s| (s.job, s.done)),
            Some((owner.job, 0)),
            "a superseded job overwrote the slot `smarttags_index_status` reads; \
             the newer run would become invisible to status queries"
        );

        // The owner's write lands, and reaches the same status query.
        owner.slot.publish(|job| SmarttagsJobStatus { job, done: 7, total: 100 });
        assert_eq!(
            state.jobs.smarttags.status().unwrap().map(|s| (s.job, s.done)),
            Some((owner.job, 7))
        );

        // The superseded run finishing must not clear the running job's slot.
        superseded.slot.clear();
        assert!(
            state.jobs.smarttags.status().unwrap().is_some(),
            "a superseded job cleared the slot out from under the running one; \
             the panel would read idle while indexing is still in progress"
        );

        owner.slot.clear();
        assert!(state.jobs.smarttags.status().unwrap().is_none());
    }

    /// A start bound to the catalog a front end showed (`begin_as`) refuses once another is
    /// open — before its first mutation: no generation tripped or installed, no job id
    /// burned, the status slot untouched.
    #[test]
    fn a_start_bound_to_another_catalog_touches_nothing() {
        let (cat_a, _db_a, _root_a) = temp_catalog("bound-a", 1);
        let (cat_b, _db_b, _root_b) = temp_catalog("bound-b", 1);
        let state = state_with(cat_a);
        let shown = crate::app::catalog_identity(&state).unwrap();
        let running = begin_index_job(&state, Some(shown)).unwrap();
        // A raw swap, without the switch's job reset: the bound start must refuse on
        // identity alone.
        *state.catalog.lock().unwrap() = Some(cat_b);

        let seq = state.jobs.smarttags.abort().job_ids_issued();
        let before = state.jobs.smarttags.installed().unwrap();
        let err = begin_index_job(&state, Some(shown)).unwrap_err();

        assert_eq!(err, crate::app::CATALOG_CHANGED);
        assert_eq!(state.jobs.smarttags.abort().job_ids_issued(), seq, "a refused start burned a job id");
        assert!(Arc::ptr_eq(&before, &state.jobs.smarttags.installed().unwrap()), "a refused start installed a generation");
        assert!(!running.abort.load(Ordering::Relaxed), "a refused start tripped the running job");
        assert_eq!(state.jobs.smarttags.status().unwrap().map(|s| s.job), Some(running.job));
    }

    /// The per-photo verbs under `with_catalog_as`: an accept keyed by a photo id read from
    /// catalog A refuses once catalog B (whose photo has the same id) is open.
    #[test]
    fn an_accept_bound_to_the_old_catalog_never_tags_the_new_one() {
        let (cat_a, _db_a, _root_a) = temp_catalog("accept-a", 1);
        let (cat_b, db_b, _root_b) = temp_catalog("accept-b", 1);
        let state = state_with(cat_a);
        let shown = crate::app::catalog_identity(&state).unwrap();
        *state.catalog.lock().unwrap() = Some(cat_b);

        let err = crate::app::with_catalog_as(&state, shown, |c| accept_suggestion(c, 1, "Animals/Gull")).unwrap_err();
        assert_eq!(err, crate::app::CATALOG_CHANGED);
        let conn = rusqlite::Connection::open(&db_b).unwrap();
        let tags: i64 = conn.query_row("SELECT COUNT(*) FROM photo_tags", [], |r| r.get(0)).unwrap();
        assert_eq!(tags, 0, "the accept tagged the new catalog's photo 1");
    }

    /// Accepting a suggestion of an auto-tag is refused (#181) before the suggestion changes:
    /// the photo isn't tagged and the suggestion isn't recorded as accepted feedback.
    #[test]
    fn accepting_an_auto_tag_suggestion_is_refused_and_records_nothing() {
        let (c, _db, _root) = temp_catalog("accept-auto", 1);
        let auto = c.create_tag("Technique/Long Exposure").unwrap();
        c.conn().execute("UPDATE tags SET auto_rule = 'long-exposure' WHERE id = ?1", [auto]).unwrap();

        let err = accept_suggestion(&c, 1, "Technique/Long Exposure").unwrap_err();
        assert!(matches!(err, CatalogError::AutoTag(_)), "{err:?}");
        let tags: i64 = c.conn().query_row("SELECT COUNT(*) FROM photo_tags", [], |r| r.get(0)).unwrap();
        let accepted: i64 = c
            .conn()
            .query_row("SELECT COUNT(*) FROM smarttags__suggestions WHERE state = 'accepted'", [], |r| r.get(0))
            .unwrap();
        assert_eq!((tags, accepted), (0, 0));
    }
}
