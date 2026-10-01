//! Exports as owned jobs — the bodies of the Tauri `export_photos` and `export_bundle`
//! commands, and of the GPUI Export and bundle-export dialogs. Both **block** (file copies,
//! JPEG renders, a zip of possibly gigabytes of RAW files): run them on a worker.
//!
//! Export is one-way (docs/storage-and-import.md): it reads originals through the resolver
//! (`resolve_photo_path`) and writes *copies* to the destination; the catalog and the
//! originals are never modified. Sidecar writes land only on the destination copies, which
//! the in-library sidecar backup rule does not cover.
//!
//! **Ownership.** Each kind is its own job family (`JobRegistry::export`,
//! `JobRegistry::bundle_export`). [`claim_export`] / [`claim_bundle_export`] trip the running
//! job of that kind and install a fresh generation, numbered for the `export:progress` events
//! — one abort lock, never the catalog's, so a front end claims where the user pressed
//! Export, and a Cancel or a catalog switch before the worker starts still stops it. The
//! worker gathers what it needs under one catalog lock hold, checked against the identity of
//! the catalog the ids were read from (`from`), and fails closed with `CATALOG_CHANGED`
//! otherwise: photo, version, tag-group and batch ids are per catalog. It then writes off the
//! lock, checking its generation before each photo.

use super::{AppState, CatalogIdentity, CoreEvent, EventSink, ExportKind, ExportProgress, CATALOG_CHANGED};
use crate::bundle::writer::BundleWriteResult;
use crate::export::{ExportPreset, ExportResult};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// What a stopped photo export reports (a Cancel, a newer export, a catalog switch).
pub const EXPORT_CANCELLED: &str = "Export cancelled";

/// An export's ownership: its generation of the family's abort flag, and the job id its
/// `export:progress` events carry.
#[derive(Clone)]
pub struct ExportClaim {
    pub abort: Arc<AtomicBool>,
    pub job: u64,
}

impl ExportClaim {
    /// Whether a Cancel, a newer export of the same kind or a catalog switch tripped it.
    pub fn aborted(&self) -> bool {
        self.abort.load(Ordering::Relaxed)
    }
}

/// Claim the photo-export generation: trip the running export, install a fresh flag and
/// number the job. One abort lock — cheap enough for a UI thread.
pub fn claim_export(state: &AppState) -> Result<ExportClaim, String> {
    let (abort, job) = state.jobs.export.install_fresh_numbered()?;
    Ok(ExportClaim { abort, job })
}

/// Claim the bundle-export generation. See [`claim_export`].
pub fn claim_bundle_export(state: &AppState) -> Result<ExportClaim, String> {
    let (abort, job) = state.jobs.bundle_export.install_fresh_numbered()?;
    Ok(ExportClaim { abort, job })
}

/// Stop the running photo export before its next photo.
pub fn cancel_export(state: &AppState) -> Result<(), String> {
    state.jobs.export.trip()
}

/// Stop the running bundle export before its next photo; nothing reaches the destination.
pub fn cancel_bundle_export(state: &AppState) -> Result<(), String> {
    state.jobs.bundle_export.trip()
}

/// What the Export dialog asks for (the Tauri `export_photos` arguments).
#[derive(Debug, Clone)]
pub struct ExportRequest {
    pub photo_ids: Vec<i64>,
    pub preset: ExportPreset,
    /// The destination folder, `~` already expanded.
    pub dest_dir: PathBuf,
    /// A tag group written as `hashtags.txt` beside the export.
    pub hashtag_group_id: Option<i64>,
    pub hashtag_limit: Option<usize>,
    /// The version active in the UI: Show-off renders it for its photo.
    pub version_id: Option<i64>,
}

/// Export photos under a fresh claim, against whatever catalog is open (the Tauri command).
pub fn export_photos(state: &AppState, request: &ExportRequest) -> Result<ExportResult, String> {
    let claim = claim_export(state)?;
    export_photos_claimed(state, &claim, None, request)
}

/// Export photos under a claim the caller already made. `from`: the catalog the ids were
/// read from; `Some` fails closed with [`CATALOG_CHANGED`] once another catalog is open.
/// Stopped (by its generation) it returns `Err` starting with [`EXPORT_CANCELLED`], saying
/// how many were written — the copies already in the destination stay.
pub fn export_photos_claimed(
    state: &AppState,
    claim: &ExportClaim,
    from: Option<CatalogIdentity>,
    request: &ExportRequest,
) -> Result<ExportResult, String> {
    export_photos_claimed_with(state, claim, from, request, &|_| {})
}

/// [`export_photos_claimed`], calling `after_photo(done)` after each photo written — where a
/// test puts a Cancel or a catalog switch.
fn export_photos_claimed_with(
    state: &AppState,
    claim: &ExportClaim,
    from: Option<CatalogIdentity>,
    request: &ExportRequest,
    after_photo: &dyn Fn(usize),
) -> Result<ExportResult, String> {
    if claim.aborted() {
        return Err(format!("{EXPORT_CANCELLED} before any photo was exported."));
    }
    // Resolve originals + assemble the optional reach-hashtag bundle under one lock hold,
    // then release it so the file work never holds the catalog.
    let (resolved, hashtags) = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_ref().ok_or("No catalog is open")?;
        if from.is_some_and(|f| !f.is(catalog)) {
            return Err(CATALOG_CHANGED.into());
        }
        // Languages for keyword assembly: canonical + neutral synonyms for now.
        let resolved = crate::export::resolve_originals(catalog, &request.photo_ids, &[], request.version_id);
        let hashtags = match request.hashtag_group_id {
            Some(g) => catalog.assemble_hashtag_bundle(g, request.hashtag_limit).map_err(|e| e.to_string())?,
            None => Vec::new(),
        };
        (resolved, hashtags)
    };
    let job = claim.job;
    let events = state.clone();
    let run = crate::export::write_exports_with(
        &resolved,
        request.preset,
        &request.dest_dir,
        &hashtags,
        &claim.abort,
        &|done, total| {
            events.send(CoreEvent::ExportProgress(ExportProgress { kind: ExportKind::Photos, job, done, total }));
            if done > 0 {
                after_photo(done);
            }
        },
    )?;
    record_export_parity(state);
    if run.stopped {
        return Err(format!(
            "{EXPORT_CANCELLED}: {} of {} exported to {}.",
            run.result.exported,
            resolved.items.len(),
            request.dest_dir.display()
        ));
    }
    Ok(run.result)
}

/// Write import batch `batch_id` as a `.chairphoto` bundle at `dest_path`, under a claim the
/// caller already made, sending `export:progress` (kind `bundle`). `from` as in
/// [`export_photos_claimed`]. Stopped, it returns `Err` starting with
/// `bundle::writer::BUNDLE_EXPORT_CANCELLED` and leaves nothing at `dest_path`.
pub fn export_bundle_claimed(
    state: &AppState,
    claim: &ExportClaim,
    from: Option<CatalogIdentity>,
    batch_id: i64,
    dest_path: &Path,
) -> Result<BundleWriteResult, String> {
    let (events, job) = (state.clone(), claim.job);
    export_bundle_claimed_with(state, claim, from, batch_id, dest_path, &move |done, total| {
        events.send(CoreEvent::ExportProgress(ExportProgress { kind: ExportKind::Bundle, job, done, total }))
    })
}

/// [`export_bundle_claimed`] with the progress sink supplied — the Tauri command keeps
/// React's `import:progress` shape through it.
pub fn export_bundle_claimed_with(
    state: &AppState,
    claim: &ExportClaim,
    from: Option<CatalogIdentity>,
    batch_id: i64,
    dest_path: &Path,
    on_progress: &dyn Fn(usize, usize),
) -> Result<BundleWriteResult, String> {
    use crate::bundle::writer::BUNDLE_EXPORT_CANCELLED;
    if claim.aborted() {
        return Err(format!("{BUNDLE_EXPORT_CANCELLED} — nothing was written to the destination."));
    }
    // Phase 1 — gather the catalog data under one lock hold (pure DB work).
    let bundle = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_ref().ok_or("No catalog is open")?;
        if from.is_some_and(|f| !f.is(catalog)) {
            return Err(CATALOG_CHANGED.into());
        }
        crate::bundle::writer::gather_bundle(catalog, batch_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("Import batch {batch_id} not found"))?
    };
    let offline = bundle.originals.values().filter(|o| o.is_none()).count();
    if offline > 0 {
        eprintln!("export_bundle: {offline} original(s) are offline — their metadata is included, no bytes copied");
    }
    // Phase 2 — write the zip off the catalog lock.
    crate::bundle::writer::write_bundle_abortable(&bundle, dest_path, &claim.abort, on_progress)
}

/// The settings key holding this catalog's "export equals view" total
/// (`plugins::edit::parity::ParityTally` as JSON): exports checked, exports that differed.
#[cfg(feature = "edit")]
pub const EXPORT_PARITY_KEY: &str = "metrics.exportParity";

/// Add the engine-2 exports checked since the last call to this catalog's total. Called
/// after every command that writes an export (the Export dialog, publishing, Instagram,
/// LocalSend); best-effort — a failed write loses a count, never an export.
pub fn record_export_parity(state: &AppState) {
    #[cfg(feature = "edit")]
    {
        use crate::plugins::edit::parity::{take, ParityTally};
        let tally = take();
        if tally.checked == 0 {
            return;
        }
        let Ok(guard) = state.catalog.lock() else { return };
        let Some(catalog) = guard.as_ref() else { return };
        let total: ParityTally = catalog
            .get_setting(EXPORT_PARITY_KEY)
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or_default();
        let next = tally.plus(total);
        if let Ok(json) = serde_json::to_string(&next) {
            if let Err(e) = catalog.set_setting(EXPORT_PARITY_KEY, &json) {
                eprintln!("export: could not record the export-parity tally: {e}");
            }
        }
    }
    #[cfg(not(feature = "edit"))]
    let _ = state;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;
    use std::sync::Mutex;

    /// Records every `export:progress` event.
    #[derive(Default)]
    struct Progress(Mutex<Vec<ExportProgress>>);

    impl EventSink for Progress {
        fn send(&self, event: CoreEvent) {
            if let CoreEvent::ExportProgress(p) = event {
                self.0.lock().unwrap().push(p);
            }
        }
    }

    struct Fixture {
        state: AppState,
        progress: Arc<Progress>,
        dir: crate::test_support::TestTmpDir,
        ids: Vec<i64>,
        batch: i64,
    }

    /// A catalog with `n` photos (small files, each with a sidecar) in one import batch.
    fn fixture(tag: &str, n: usize) -> Fixture {
        let dir = crate::test_support::TestTmpDir::new(&format!("exports-{tag}"));
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let catalog = Catalog::open(&dir.join("catalog.chairphoto"), &root).unwrap();
        let batch = catalog.create_import_batch("card/DCIM").unwrap();
        let mut ids = Vec::new();
        for i in 0..n {
            let name = format!("IMG_{i:04}.CR3");
            let path = root.join(&name);
            std::fs::write(&path, format!("raw bytes {i}")).unwrap();
            std::fs::write(root.join(format!("{name}.xmp")), "<x:xmpmeta xmlns:x='adobe:ns:meta/'/>").unwrap();
            ids.push(catalog.upsert_photo(&path, None, 0, 11).unwrap().id);
        }
        catalog.assign_photos_to_batch(batch, &ids).unwrap();
        let state = AppState::default();
        *state.catalog.lock().unwrap() = Some(catalog);
        let progress = Arc::new(Progress::default());
        state.set_events(progress.clone());
        Fixture { state, progress, dir, ids, batch }
    }

    fn request(f: &Fixture, dest: &Path) -> ExportRequest {
        ExportRequest {
            photo_ids: f.ids.clone(),
            preset: ExportPreset::HandOff,
            dest_dir: dest.to_path_buf(),
            hashtag_group_id: None,
            hashtag_limit: None,
            version_id: None,
        }
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .map(|d| d.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        v.sort();
        v
    }

    #[test]
    fn a_hand_off_export_copies_originals_and_sidecars_with_progress_and_leaves_the_originals_alone() {
        let f = fixture("handoff", 2);
        let dest = f.dir.join("out");
        let claim = claim_export(&f.state).unwrap();
        let r = export_photos_claimed(&f.state, &claim, None, &request(&f, &dest)).unwrap();
        assert_eq!(r, ExportResult { exported: 2, skipped_offline: 0, errors: 0 });
        assert_eq!(files(&dest), ["IMG_0000.CR3", "IMG_0000.CR3.xmp", "IMG_0001.CR3", "IMG_0001.CR3.xmp"]);
        assert_eq!(std::fs::read(f.dir.join("photos/IMG_0000.CR3")).unwrap(), b"raw bytes 0");
        let p = f.progress.0.lock().unwrap();
        assert_eq!(p.iter().map(|p| (p.done, p.total)).collect::<Vec<_>>(), [(0, 2), (1, 2), (2, 2)]);
        assert!(p.iter().all(|p| p.job == claim.job && p.kind == ExportKind::Photos));
    }

    /// Cancel mid-run: the photo after the cancel is not written, and the result says so.
    #[test]
    fn a_cancel_stops_the_export_before_its_next_photo() {
        let f = fixture("cancel", 3);
        let dest = f.dir.join("out");
        let claim = claim_export(&f.state).unwrap();
        let cancel = |done: usize| {
            if done == 1 {
                cancel_export(&f.state).unwrap()
            }
        };
        let err = export_photos_claimed_with(&f.state, &claim, None, &request(&f, &dest), &cancel).unwrap_err();
        assert!(err.starts_with(EXPORT_CANCELLED), "{err}");
        assert!(err.contains("1 of 3"), "{err}");
        assert_eq!(files(&dest), ["IMG_0000.CR3", "IMG_0000.CR3.xmp"]);
    }

    /// A newer export trips the older one; a claim tripped before its worker ran writes nothing.
    #[test]
    fn a_newer_export_makes_the_older_unreachable() {
        let f = fixture("newer", 1);
        let dest = f.dir.join("out");
        let older = claim_export(&f.state).unwrap();
        let newer = claim_export(&f.state).unwrap();
        assert!(older.aborted() && !newer.aborted());
        assert!(newer.job > older.job);
        let err = export_photos_claimed(&f.state, &older, None, &request(&f, &dest)).unwrap_err();
        assert!(err.starts_with(EXPORT_CANCELLED), "{err}");
        assert!(files(&dest).is_empty());
        // A bundle export is another family: the photo export does not trip it.
        let bundle = claim_bundle_export(&f.state).unwrap();
        claim_export(&f.state).unwrap();
        assert!(!bundle.aborted());
    }

    /// A catalog switch between the user's Export and the worker's resolve: the worker
    /// refuses to resolve the old ids against the new catalog, and writes nothing.
    #[test]
    fn an_export_bound_to_a_catalog_fails_closed_once_another_is_open() {
        let f = fixture("identity", 1);
        let dest = f.dir.join("out");
        let from = super::super::catalog_identity(&f.state).unwrap();
        let claim = claim_export(&f.state).unwrap();
        // Another catalog with a photo under the same id.
        let other = fixture("identity-other", 1);
        let swapped = other.state.catalog.lock().unwrap().take();
        *f.state.catalog.lock().unwrap() = swapped;
        let err = export_photos_claimed(&f.state, &claim, Some(from), &request(&f, &dest)).unwrap_err();
        assert_eq!(err, CATALOG_CHANGED);
        assert!(files(&dest).is_empty());
        let err = export_bundle_claimed(&f.state, &claim_bundle_export(&f.state).unwrap(), Some(from), f.batch, &dest.join("b.chairphoto"))
            .unwrap_err();
        assert_eq!(err, CATALOG_CHANGED);
    }

    /// Both switch phases trip both export generations.
    #[test]
    fn a_catalog_switch_trips_both_export_families() {
        let state = AppState::default();
        let export = claim_export(&state).unwrap();
        let bundle = claim_bundle_export(&state).unwrap();
        state.jobs.lock_for_publish().unwrap().trip_and_replace_all();
        assert!(export.aborted() && bundle.aborted());
        assert!(!claim_export(&state).unwrap().aborted());
    }

    #[test]
    fn a_bundle_export_writes_the_zip_and_a_cancelled_one_leaves_nothing() {
        let f = fixture("bundle", 2);
        let dest = f.dir.join("out/trip.chairphoto");
        let claim = claim_bundle_export(&f.state).unwrap();
        let r = export_bundle_claimed(&f.state, &claim, None, f.batch, &dest).unwrap();
        assert_eq!((r.exported, r.skipped_offline, r.errors), (2, 0, 0));
        assert!(dest.is_file());
        let p = f.progress.0.lock().unwrap().clone();
        assert!(p.iter().all(|p| p.kind == ExportKind::Bundle && p.job == claim.job));
        assert_eq!(p.last().map(|p| (p.done, p.total)), Some((4, 4)));

        let dest2 = f.dir.join("out/cancelled.chairphoto");
        let claim = claim_bundle_export(&f.state).unwrap();
        let state = f.state.clone();
        let err = export_bundle_claimed_with(&f.state, &claim, None, f.batch, &dest2, &move |done, _| {
            if done == 1 {
                cancel_bundle_export(&state).unwrap();
            }
        })
        .unwrap_err();
        assert!(err.starts_with(crate::bundle::writer::BUNDLE_EXPORT_CANCELLED), "{err}");
        assert!(!dest2.exists());
        assert_eq!(files(&f.dir.join("out")), ["trip.chairphoto"], "no temp file is left behind");
    }

    /// The archive's entry names, sorted (panics on an unreadable zip).
    fn bundle_entries(path: &Path) -> Vec<String> {
        let mut zip = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
        let mut v: Vec<String> = (0..zip.len()).map(|i| zip.by_index(i).unwrap().name().to_string()).collect();
        v.sort();
        v
    }

    /// Wait for the other thread's step, failing rather than hanging.
    fn wait(rx: &std::sync::mpsc::Receiver<()>) {
        rx.recv_timeout(std::time::Duration::from_secs(60)).expect("the other export's step");
    }

    /// Two bundle exports to one destination, interleaved: the newer starts (tripping the
    /// older) while the older is mid-write after its first photo; the older then stops at its
    /// next check while the newer is itself mid-write; the newer then finishes. The older
    /// never truncates, deletes nor renames the newer's temp file: the newer's bundle lands
    /// whole, and no temp file is left behind.
    #[test]
    fn a_newer_bundle_export_to_the_same_destination_never_shares_the_older_ones_temp_file() {
        use std::sync::mpsc::channel;
        let f = fixture("bundle-newer", 3);
        let dest = f.dir.join("out/trip.chairphoto");
        let older = claim_bundle_export(&f.state).unwrap();
        let (go_tx, go_rx) = channel::<()>();
        let (mid_tx, mid_rx) = channel::<()>();
        let (done_tx, done_rx) = channel::<()>();
        let (newer, older_err) = std::thread::scope(|s| {
            let (f, dest) = (&f, &dest);
            let newer = s.spawn(move || {
                wait(&go_rx);
                let claim = claim_bundle_export(&f.state).unwrap();
                export_bundle_claimed_with(&f.state, &claim, None, f.batch, dest, &|done, _| {
                    if done == 1 {
                        mid_tx.send(()).unwrap();
                        wait(&done_rx);
                    }
                })
            });
            let older_err = export_bundle_claimed_with(&f.state, &older, None, f.batch, dest, &|done, _| {
                if done == 1 {
                    go_tx.send(()).unwrap();
                    wait(&mid_rx);
                }
            });
            done_tx.send(()).unwrap();
            (newer.join().unwrap(), older_err)
        });
        let err = older_err.unwrap_err();
        assert!(err.starts_with(crate::bundle::writer::BUNDLE_EXPORT_CANCELLED), "{err}");
        let r = newer.expect("the newer bundle export completes");
        assert_eq!((r.exported, r.skipped_offline, r.errors), (3, 0, 0));
        let entries = bundle_entries(&dest);
        let originals: Vec<_> = entries.iter().filter(|e| e.starts_with("originals/") && !e.ends_with(".xmp")).collect();
        assert_eq!(originals.len(), 3, "{entries:?}");
        assert_eq!(files(&f.dir.join("out")), ["trip.chairphoto"], "no temp file is left behind");
    }
}
