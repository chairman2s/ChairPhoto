//! Folder scans and card import — the bodies the GPUI app runs for Import ▾.
//!
//! Every function here **blocks** (walks, copies, SQLite on a secondary connection): run it
//! on a worker, never a UI thread. Progress goes out through `state`'s event sink —
//! `scan:progress` for scans, `import:progress` for the copy off a card.
//!
//! Scans run two-phase (I6): Phase A inserts rows fast so the grid populates, then returns
//! the Phase B [`EnrichJob`] for the caller to run on its own worker. Both honour the scan
//! generation installed by `begin_scan_generation`, so a catalog switch or a second scan
//! stops the previous one before it can write to a torn-down catalog.
//!
//! A card import owns the **import** generation (`JobRegistry::import`): a newer import,
//! [`cancel_import`] and a catalog switch each trip it, and the import then stops before its
//! next file and does not index what it copied into a catalog it no longer owns.

use super::catalogs::{send_scan_done, EnrichJob};
use super::{begin_scan_generation, AppState, CoreEvent, EventSink, ImportProgress};
use crate::catalog::Catalog;
use crate::scanner::{PendingEnrich, ScanProgress, ScanResult};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// A Phase A body: given a secondary connection, the scan generation's flag and a progress
/// sink, walk and insert, and hand Phase B its work.
pub type PhaseA<'a> =
    dyn FnOnce(&Catalog, &AtomicBool, &dyn Fn(ScanProgress)) -> Result<(ScanResult, PendingEnrich), String> + 'a;

/// Run a two-phase scan's Phase A and return its result with the Phase B job to run next.
///
/// Under one brief catalog lock, reads the catalog path and root and starts a fresh scan
/// generation (tripping any earlier scan, including a still-running detached Phase B), and
/// runs Phase A on
/// its own secondary connection so the shared one keeps serving reads. A Phase A failure
/// sends the terminal `scan:progress {phase:"done"}` itself, so a progress indicator always
/// clears. Stale queue rows a previous aborted Phase B left behind are merged into this
/// scan's Phase B (I6d), so a rescan drains them without re-walking.
pub fn scan_two_phase(state: &AppState, phase_a: Box<PhaseA<'_>>) -> Result<(ScanResult, EnrichJob), String> {
    scan_two_phase_as(state, None, phase_a)
}

/// [`scan_two_phase`], with `expected`: only into that catalog. The identity is checked
/// **before** the generation is installed, both under one catalog lock hold (lock order
/// catalog → scan abort, `app::jobs`). So a request bound to a catalog that is no longer
/// open fails closed with `CATALOG_CHANGED` without tripping the open catalog's scan, and a
/// switch, which takes the catalog lock first, lands either before both (refused) or after
/// both (and trips this scan).
fn scan_two_phase_as(
    state: &AppState,
    expected: Option<super::CatalogIdentity>,
    phase_a: Box<PhaseA<'_>>,
) -> Result<(ScanResult, EnrichJob), String> {
    let (path, root, abort) = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_ref().ok_or("No catalog is open")?;
        if expected.is_some_and(|e| !e.is(catalog)) {
            return Err(super::CATALOG_CHANGED.into());
        }
        let abort = begin_scan_generation(state)?;
        (catalog.db_path().to_path_buf(), catalog.root().to_path_buf(), abort)
    };
    let phase_a_out = Catalog::open_secondary(&path, &root).map_err(|e| e.to_string()).and_then(|scan_catalog| {
        let events = state.clone();
        let emit = move |p: ScanProgress| events.send(CoreEvent::ScanProgress(p));
        phase_a(&scan_catalog, &abort, &emit)
    });
    let (result, mut pending) = match phase_a_out {
        Ok(v) => v,
        Err(e) => {
            send_scan_done(state);
            return Err(e);
        }
    };
    // Best-effort: a failed load (e.g. the catalog closed mid-flight) enriches only what
    // Phase A produced.
    let stale = match state.catalog.lock() {
        Ok(guard) => match guard.as_ref() {
            Some(c) => c.load_pending_enrichment().unwrap_or_default(),
            None => Vec::new(),
        },
        Err(_) => Vec::new(),
    };
    pending.imported = crate::scanner::merge_stale_pending(pending.imported, stale);
    Ok((result, EnrichJob::after_scan(state.clone(), path, root, abort, pending)))
}

/// Rescan the whole library (the catalog root) in place.
pub fn rescan_library(state: &AppState) -> Result<(ScanResult, EnrichJob), String> {
    scan_two_phase(
        state,
        Box::new(|c, abort, progress| {
            // The root from the same connection as the scan (no re-root window).
            let root = c.root().to_path_buf();
            crate::scanner::scan_folder_phase_a(c, &root, abort, progress)
        }),
    )
}

/// Scan `folder` (recursively) into the open catalog. Read-only on photo files.
pub fn scan_folder(state: &AppState, folder: PathBuf) -> Result<(ScanResult, EnrichJob), String> {
    scan_two_phase(state, Box::new(move |c, abort, progress| crate::scanner::scan_folder_phase_a(c, &folder, abort, progress)))
}

/// Index an existing archive on a non-root volume in place — nothing is copied.
pub fn scan_nas_folder(state: &AppState, folder: PathBuf) -> Result<(ScanResult, EnrichJob), String> {
    scan_nas_folder_in(state, None, folder)
}

/// [`scan_nas_folder`] into the catalog `expected` names only; otherwise `CATALOG_CHANGED`.
pub fn scan_nas_folder_as(
    state: &AppState,
    expected: super::CatalogIdentity,
    folder: PathBuf,
) -> Result<(ScanResult, EnrichJob), String> {
    scan_nas_folder_in(state, Some(expected), folder)
}

fn scan_nas_folder_in(
    state: &AppState,
    expected: Option<super::CatalogIdentity>,
    folder: PathBuf,
) -> Result<(ScanResult, EnrichJob), String> {
    scan_two_phase_as(
        state,
        expected,
        Box::new(move |c, abort, progress| crate::scanner::scan_external_folder_phase_a(c, &folder, abort, progress)),
    )
}

/// The photos on a card/source folder, each flagged as a duplicate when the library already
/// holds a same-size file at its date-tree destination. Filesystem and metadata only.
pub fn list_card_photos(state: &AppState, source: &Path) -> Result<Vec<crate::scanner::CardPhoto>, String> {
    let dest = library_root(state)?;
    crate::scanner::list_card_photos(source, &dest)
}

/// What an import stopped by [`cancel_import`], a newer import or a catalog switch reports.
pub const IMPORT_CANCELLED: &str = "Import cancelled";

/// Import from a card: copy the supported images from `source` (only `selected`, by full
/// path, when given) into the library root under Year/Month/Day, index the copies into one
/// import batch labelled `name` (default: the source folder), and queue them for backup.
///
/// Claims the import generation first. The copy — the slow part, gigabytes — runs without
/// the catalog lock and streams `import:progress`; it stops before the next file once the
/// generation is tripped, and then the copies are **not** indexed: the error names how many
/// files are already in the library folder (a rescan picks them up; nothing is deleted). The
/// index phase runs on a secondary connection to the catalog the import started against,
/// and ends with the terminal `scan:progress {phase:"done"}`.
pub fn ingest_from_card(
    state: &AppState,
    source: &Path,
    name: Option<&str>,
    selected: Option<std::collections::HashSet<String>>,
) -> Result<ScanResult, String> {
    let claim = claim_import(state)?;
    ingest_from_card_claimed(state, &claim, source, name, selected)
}

/// An import's ownership: its generation of the import abort flag, and the job id its
/// `import:progress` events carry.
#[derive(Clone)]
pub struct ImportClaim {
    pub abort: std::sync::Arc<AtomicBool>,
    pub job: u64,
}

/// Claim the import generation: trip the running import, install a fresh flag and number the
/// job. One abort lock, never the catalog's — cheap enough for a UI thread, which is the
/// point: a front end that claims when the user presses Import can cancel (or a switch can
/// trip) the import before its worker has even started, and knows the job id its progress
/// will carry before any arrives.
pub fn claim_import(state: &AppState) -> Result<ImportClaim, String> {
    let (abort, job) = state.jobs.import.install_fresh_numbered()?;
    Ok(ImportClaim { abort, job })
}

/// [`ingest_from_card`] under an import generation the caller already claimed
/// ([`claim_import`]). Already tripped: it copies nothing.
pub fn ingest_from_card_claimed(
    state: &AppState,
    claim: &ImportClaim,
    source: &Path,
    name: Option<&str>,
    selected: Option<std::collections::HashSet<String>>,
) -> Result<ScanResult, String> {
    ingest_claimed_with(state, claim, source, name, selected, &|| {}, &|_| {})
}

/// [`ingest_from_card_claimed`], calling `after_copy` once the copy finished un-cancelled and
/// before the import commits to indexing, and `after_indexed(n)` after each copy indexed —
/// where a test puts a Cancel.
fn ingest_claimed_with(
    state: &AppState,
    claim: &ImportClaim,
    source: &Path,
    name: Option<&str>,
    selected: Option<std::collections::HashSet<String>>,
    after_copy: &dyn Fn(),
    after_indexed: &dyn Fn(usize),
) -> Result<ScanResult, String> {
    let (abort, job) = (&*claim.abort, claim.job);
    if abort.load(Ordering::Relaxed) {
        return Err(cancelled_message(0));
    }
    let (db_path, dest) = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_ref().ok_or("No catalog is open")?;
        (catalog.db_path().to_path_buf(), catalog.root().to_path_buf())
    };
    let (result, copied, aborted) = {
        let events = state.clone();
        crate::scanner::copy_from_card_abortable(source, &dest, selected.as_ref(), abort, move |done, total| {
            events.send(CoreEvent::ImportProgress(ImportProgress { job, done, total }))
        })?
    };
    if aborted || abort.load(Ordering::Relaxed) {
        return Err(cancelled_message(copied.len()));
    }
    after_copy();

    // Index the copies. A card import supersedes a running scan, as `run_blocking_scan`
    // always did, but only once it commits to indexing. The check of this import's flag and
    // the replacement of the scan generation happen under both locks
    // (`install_fresh_if_owner`), so an import cancelled after its copy never stops a
    // rescan's enrichment. The import flag is checked once more after that: a switch or
    // Cancel landing after the commit must not see these rows indexed. A tripped scan stays
    // tripped; its queued enrichment rows are drained by the next scan (I6d). The indexing
    // itself stops before its next copy once the flag trips (Cancel, a newer import, a
    // switch): what it indexed so far is committed whole, and the rest wait for a rescan
    // (`scanner::index_ingested_abortable`).
    let Some(_scan) = state.jobs.scan.install_fresh_if_owner(&state.jobs.import, abort)? else {
        return Err(cancelled_message(copied.len()));
    };
    let indexed = (|| {
        if abort.load(Ordering::Relaxed) {
            return Err(cancelled_message(copied.len()));
        }
        let catalog = Catalog::open_secondary(&db_path, &dest).map_err(|e| e.to_string())?;
        let total = copied.len();
        let indexed = crate::scanner::index_ingested_with(&catalog, &dest, source, copied, name, result, abort, after_indexed)?;
        if indexed.aborted() {
            return Err(cancelled_while_indexing(indexed.indexed, total));
        }
        Ok(indexed.result)
    })();
    send_scan_done(state);
    indexed
}

fn cancelled_message(copied: usize) -> String {
    if copied == 0 {
        format!("{IMPORT_CANCELLED} before any file was copied.")
    } else {
        format!(
            "{IMPORT_CANCELLED}: {copied} file{} already copied into the library folder \
             {} not indexed yet — Rescan library picks {} up.",
            if copied == 1 { "" } else { "s" },
            if copied == 1 { "is" } else { "are" },
            if copied == 1 { "it" } else { "them" },
        )
    }
}

/// What an import stopped during indexing reports: the copies indexed so far are in the
/// catalog, whole; the rest are in the library folder for the next rescan.
fn cancelled_while_indexing(indexed: usize, total: usize) -> String {
    let rest = total - indexed;
    format!(
        "{IMPORT_CANCELLED}: {indexed} of {total} copied files indexed; the other {rest} {} in the library folder, \
         not indexed yet — Rescan library picks {} up.",
        if rest == 1 { "is" } else { "are" },
        if rest == 1 { "it" } else { "them" },
    )
}

/// Stop the running card or bundle import before its next file. A no-op when none runs.
pub fn cancel_import(state: &AppState) -> Result<(), String> {
    state.jobs.import.trip()
}

/// The open catalog's library root, read under a brief lock.
pub(crate) fn library_root(state: &AppState) -> Result<PathBuf, String> {
    let guard = state.catalog.lock().map_err(|e| e.to_string())?;
    let catalog = guard.as_ref().ok_or("No catalog is open")?;
    Ok(catalog.root().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::events::EventSink;
    use std::sync::{Arc, Mutex};

    /// Records every event's name, so a test can see the progress and the terminal signal.
    #[derive(Default)]
    struct Names(Mutex<Vec<String>>);

    impl EventSink for Names {
        fn send(&self, event: CoreEvent) {
            self.0.lock().unwrap().push(event.name().to_string());
        }
    }

    fn setup(tag: &str, files: usize) -> (crate::test_support::TestTmpDir, AppState, Arc<Names>, PathBuf) {
        let dir = crate::test_support::TestTmpDir::new(&format!("scans-{tag}"));
        let root = dir.join("library");
        let card = dir.join("card");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&card).unwrap();
        for i in 0..files {
            std::fs::write(card.join(format!("IMG_{i}.jpg")), format!("jpeg {i}")).unwrap();
        }
        let state = AppState::default();
        let names = Arc::new(Names::default());
        state.set_events(names.clone());
        *state.catalog.lock().unwrap() = Some(Catalog::open(&dir.join("c.chairphoto"), &root).unwrap());
        (dir, state, names, card)
    }

    fn photos(state: &AppState) -> usize {
        state.catalog.lock().unwrap().as_ref().unwrap().count_photos(&Default::default()).unwrap()
    }

    #[test]
    fn a_card_import_copies_indexes_and_reports_progress() {
        let (_dir, state, names, card) = setup("ingest", 3);
        let result = ingest_from_card(&state, &card, Some("Trip"), None).unwrap();
        assert_eq!((result.scanned, result.created), (3, 3));
        assert_eq!(photos(&state), 3);
        let names = names.0.lock().unwrap();
        assert_eq!(names.iter().filter(|n| *n == "import:progress").count(), 3);
        assert_eq!(names.last().map(String::as_str), Some("scan:progress"), "the terminal done");
    }

    /// **Forced interleaving.** A Cancel between the first and second file: the copy stops,
    /// and the one copied file is not indexed (it waits for a rescan, and is not deleted).
    #[test]
    fn a_cancel_mid_copy_stops_and_indexes_nothing() {
        let (dir, state, _names, card) = setup("cancel", 4);
        struct CancelOnFirst(AppState, std::sync::atomic::AtomicBool);
        impl EventSink for CancelOnFirst {
            fn send(&self, event: CoreEvent) {
                if matches!(event, CoreEvent::ImportProgress(_)) && !self.1.swap(true, Ordering::SeqCst) {
                    cancel_import(&self.0).unwrap();
                }
            }
        }
        // A second state sharing everything but the sink: the cancel runs inside the copy's own
        // progress callback, so it lands after file 1 every time.
        let cancelling = AppState { catalog: state.catalog.clone(), jobs: state.jobs.clone(), ..AppState::default() };
        cancelling.set_events(Arc::new(CancelOnFirst(state.clone(), Default::default())));
        let err = ingest_from_card(&cancelling, &card, None, None).unwrap_err();
        assert!(err.starts_with(IMPORT_CANCELLED), "{err}");
        assert!(err.contains("1 file already copied"), "{err}");
        assert_eq!(photos(&state), 0, "nothing indexed after the cancel");
        let copied = walkdir::WalkDir::new(dir.join("library")).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_file()).count();
        assert_eq!(copied, 1, "the copy stopped before the second file, and kept the first");
    }

    /// **Forced interleaving.** A rescan's enrichment is running (its scan generation is
    /// live) when a card import finishes its copy; the import is cancelled right then, before
    /// it commits to indexing. The cancelled import indexes nothing and the rescan's
    /// enrichment keeps running. Without the Cancel, the import does supersede the scan.
    #[test]
    fn a_cancelled_import_leaves_a_running_rescan_alone() {
        let (_dir, state, _names, card) = setup("cancel-vs-scan", 1);
        let enrichment = state.jobs.scan.install_fresh().unwrap();
        let claim = claim_import(&state).unwrap();
        let cancel = || cancel_import(&state).unwrap();
        let err = ingest_claimed_with(&state, &claim, &card, None, None, &cancel, &|_| {}).unwrap_err();
        assert!(err.contains("1 file already copied"), "{err}");
        assert!(!enrichment.load(Ordering::Relaxed), "the cancelled import stopped the rescan's enrichment");
        assert_eq!(photos(&state), 0);

        let claim = claim_import(&state).unwrap();
        std::fs::write(card.join("IMG_new.jpg"), b"another").unwrap();
        ingest_claimed_with(&state, &claim, &card, None, None, &|| {}, &|_| {}).unwrap();
        assert!(enrichment.load(Ordering::Relaxed), "an import that indexes supersedes the scan");
    }

    /// **Forced interleaving** (#114 Codex, finding D). Cancel, a newer import and a catalog
    /// switch each land after the first of three copies is indexed: indexing stops before the
    /// second. The first is committed whole — its row, identity sidecar, import batch and
    /// queued backup — in the catalog the import started against; the other two stay in the
    /// library folder, unindexed, and the report says so.
    #[test]
    fn cancel_switch_or_a_newer_import_stops_indexing_between_photos() {
        for how in ["cancel", "newer", "switch"] {
            let (dir, state, _names, card) = setup(&format!("index-abort-{how}"), 3);
            let a = state.catalog.clone();
            let claim = claim_import(&state).unwrap();
            let trip = |n: usize| {
                if n != 1 {
                    return;
                }
                match how {
                    "cancel" => cancel_import(&state).unwrap(),
                    "newer" => drop(claim_import(&state).unwrap()),
                    _ => {
                        crate::app::catalogs::detach_catalog_and_trip_jobs(&state).unwrap();
                        let b = Catalog::open(&dir.join("b.chairphoto"), &dir.join("b")).unwrap();
                        crate::app::catalogs::publish_catalog_and_reset_jobs(&state, b).unwrap();
                    }
                }
            };
            let err = ingest_claimed_with(&state, &claim, &card, Some("Trip"), None, &|| {}, &trip).unwrap_err();
            assert_eq!(
                err,
                "Import cancelled: 1 of 3 copied files indexed; the other 2 are in the library folder, not indexed yet — \
                 Rescan library picks them up.",
                "{how}"
            );
            // The catalog the import started against (a secondary connection to it, after a switch).
            let started = Catalog::open_secondary(&dir.join("c.chairphoto"), &dir.join("library")).unwrap();
            let rows = started.list_photos(&Default::default()).unwrap();
            assert_eq!(rows.len(), 1, "{how}: one photo indexed, not three");
            let photo = &rows[0];
            let path = started.require_photo_path(photo.id).unwrap();
            assert_eq!(crate::xmp::read_identifier(&path).as_deref(), Some(photo.uuid.as_str()), "{how}: its identity sidecar");
            assert!(started.import_batch_uuid_for_photo(photo.id).unwrap().is_some(), "{how}: its import batch");
            let backups: Vec<i64> = started.list_pending_operations().unwrap().iter().map(|o| o.photo_id).collect();
            assert_eq!(backups, [photo.id], "{how}: its queued backup");
            let files = walkdir::WalkDir::new(dir.join("library"))
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "jpg"))
                .count();
            assert_eq!(files, 3, "{how}: every copy stays in the library folder");
            if how == "switch" {
                assert_eq!(crate::app::with_catalog(&state, |c| c.count_photos(&Default::default())).unwrap(), 0, "B untouched");
            }
            drop(a);
        }
    }

    #[test]
    fn a_newer_import_trips_the_older_one() {
        let (_dir, state, _names, _card) = setup("supersede", 0);
        let first = state.jobs.import.install_fresh().unwrap();
        let _second = state.jobs.import.install_fresh().unwrap();
        assert!(first.load(Ordering::Relaxed));
    }

    #[test]
    fn a_rescan_returns_its_phase_b_which_ends_with_done() {
        let (dir, state, names, _card) = setup("rescan", 0);
        std::fs::write(dir.join("library/a.jpg"), b"jpeg").unwrap();
        let (result, job) = rescan_library(&state).unwrap();
        assert_eq!(result.created, 1);
        names.0.lock().unwrap().clear();
        job.run();
        assert_eq!(names.0.lock().unwrap().last().map(String::as_str), Some("scan:progress"));
    }
}
