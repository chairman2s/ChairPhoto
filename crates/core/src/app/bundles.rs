//! Importing a `.chairphoto` bundle (F1d) — the bodies the GPUI bundle-import dialog calls.
//! Both **block** (zip
//! reads, copies of possibly gigabytes of RAW files): run them on a worker.
//!
//! An import owns the import generation (`JobRegistry::import`), like a card import: a newer
//! import, `scans::cancel_import` or a catalog switch trips it. A tripped import stops
//! unpacking before its next original, and merges nothing into a catalog it no longer owns.
//! What it already unpacked stays in the library folder (never deleted); importing the
//! bundle again finishes it.

use super::scans::IMPORT_CANCELLED;
use super::{AppState, CoreEvent, EventSink, ImportProgress};
use crate::bundle::importer::BundleImportResult;
use std::path::Path;
use std::sync::atomic::Ordering;

/// Lightweight summary returned by [`preview_bundle`] for the pre-import dialog.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BundlePreview {
    /// Human label for the import batch (e.g. source folder name).
    pub batch_label: String,
    /// Stable UUID of the import batch.
    pub batch_uuid: String,
    /// Total photos in the bundle.
    pub total: usize,
    /// Photos not yet in the catalog (will be added on import).
    pub new_count: usize,
    /// Photos already present in the catalog (the merge keeps their values and fills only
    /// their blanks, #185).
    pub existing: usize,
    /// Photos whose identity is not in the catalog while a photo of another identity holds
    /// their path: likely the same capture imported on both machines (#249). The import
    /// merges each onto that photo when the capture is proven, else keeps it apart — so they
    /// are not counted as new. (The proof needs the originals' stamps, read on import.)
    pub here_under_another_identity: usize,
}

/// Peek at a bundle: parse its manifest (no catalog lock) and count its photos by UUID
/// against the open catalog with one IN-clause query under a brief lock. Writes nothing.
pub fn preview_bundle(state: &AppState, bundle_path: &Path) -> Result<BundlePreview, String> {
    let (manifest, _archive) = crate::bundle::importer::open_bundle(bundle_path)?;
    let uuids: Vec<String> = manifest.photos.iter().map(|bp| bp.uuid.clone()).collect();
    let (existing, elsewhere) = {
        use rusqlite::OptionalExtension;
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_ref().ok_or("No catalog is open")?;
        let existing = catalog.count_existing_uuids(&uuids).map_err(|e| e.to_string())?;
        let mut elsewhere = 0;
        for bp in &manifest.photos {
            let Some(identity) = crate::catalog::photo_identity_for(&bp.uuid) else { continue };
            let held: Option<String> = catalog
                .conn()
                .query_row("SELECT uuid FROM photos WHERE path = ?1", [&bp.relative_path], |r| r.get(0))
                .optional()
                .map_err(|e| e.to_string())?;
            let known = catalog.count_existing_uuids(std::slice::from_ref(&identity)).map_err(|e| e.to_string())? > 0;
            if !known && held.is_some_and(|uuid| uuid != identity) {
                elsewhere += 1;
            }
        }
        (existing, elsewhere)
    };
    Ok(BundlePreview {
        batch_label: manifest.batch.source_label.clone(),
        batch_uuid: manifest.batch.uuid.clone(),
        total: manifest.photos.len(),
        new_count: uuids.len().saturating_sub(existing).saturating_sub(elsewhere),
        existing,
        here_under_another_identity: elsewhere,
    })
}

/// Import a bundle: unpack its originals into `<root>/YYYY/MM/DD/` (no catalog lock;
/// `import:progress` per photo), then index and merge additively on a secondary connection
/// to the catalog the import started against (UUID-aware, so nothing is duplicated).
pub fn import_bundle(state: &AppState, bundle_path: &Path) -> Result<BundleImportResult, String> {
    let claim = super::scans::claim_import(state)?;
    import_bundle_claimed(state, &claim, bundle_path)
}

/// [`import_bundle`] under an import generation the caller already claimed
/// (`scans::claim_import`). Already tripped: it unpacks nothing.
pub fn import_bundle_claimed(
    state: &AppState,
    claim: &super::scans::ImportClaim,
    bundle_path: &Path,
) -> Result<BundleImportResult, String> {
    import_bundle_claimed_with(state, claim, bundle_path, &|_| {})
}

/// [`import_bundle_claimed`], calling `after_indexed(n)` after each original indexed — where
/// a test puts a Cancel.
fn import_bundle_claimed_with(
    state: &AppState,
    claim: &super::scans::ImportClaim,
    bundle_path: &Path,
    after_indexed: &dyn Fn(usize),
) -> Result<BundleImportResult, String> {
    let (abort, job) = (&*claim.abort, claim.job);
    if abort.load(Ordering::Relaxed) {
        return Err(format!("{IMPORT_CANCELLED} before the bundle was opened."));
    }
    // The root and the database are read under one lock, so they are one catalog's.
    let (db_path, dest) = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_ref().ok_or("No catalog is open")?;
        (catalog.db_path().to_path_buf(), catalog.root().to_path_buf())
    };
    let (manifest, mut archive) = crate::bundle::importer::open_bundle(bundle_path)?;
    // The import's own connection to the catalog it started against, opened before the
    // unpack: the unpack reads the names its rows hold (#247), and the index phase writes
    // through it. A switch meanwhile never redirects either to the catalog opened since.
    let sec = crate::catalog::Catalog::open_secondary(&db_path, &dest).map_err(|e| e.to_string())?;
    let (extracted, partial, aborted) = {
        let events = state.clone();
        crate::bundle::importer::extract_originals_abortable(&sec, &manifest, &mut archive, &dest, abort, move |done, total| {
            events.send(CoreEvent::ImportProgress(ImportProgress { job, done, total }))
        })?
    };
    if aborted || abort.load(Ordering::Relaxed) {
        return Err(cancelled_message(partial.copied));
    }
    // Stops before its next original once the flag trips (Cancel, a newer import, a switch),
    // with the originals indexed so far imported whole (`index_bundle_abortable`).
    let indexed =
        crate::bundle::importer::index_bundle_with(&sec, &manifest, &extracted, &dest, partial, abort, after_indexed)?;
    if indexed.aborted() {
        return Err(cancelled_while_indexing(indexed.indexed, indexed.total));
    }
    Ok(indexed.result)
}

/// What a bundle import stopped during indexing reports: the originals indexed so far are
/// imported; importing the bundle again finishes it.
fn cancelled_while_indexing(indexed: usize, total: usize) -> String {
    format!("{IMPORT_CANCELLED}: {indexed} of {total} unpacked originals imported — import the bundle again to finish.")
}

/// What a stopped bundle import reports. The unpacked copies are left in place, never
/// deleted (see `extract_originals_abortable`), and the message says how to finish.
fn cancelled_message(copied: usize) -> String {
    if copied == 0 {
        format!("{IMPORT_CANCELLED} before any original was unpacked.")
    } else {
        format!(
            "{IMPORT_CANCELLED}: {copied} original{} from the bundle {} in the library folder but not merged — \
             import the bundle again to finish.",
            if copied == 1 { "" } else { "s" },
            if copied == 1 { "is" } else { "are" },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::events::EventSink;
    use crate::bundle::writer::{write_bundle, GatheredBundle};
    use crate::bundle::{BundleBatch, BundleManifest, BundlePhoto};
    use crate::catalog::Catalog;
    use std::sync::Arc;

    /// A bundle of `n` photos, each with a small original, all in one date folder.
    fn bundle(dir: &Path, n: usize) -> std::path::PathBuf {
        bundle_in(dir, &vec!["2026/01/02"; n])
    }

    /// A bundle of one photo per entry of `days`, `IMG_<i>.jpg` in that date folder.
    fn bundle_in(dir: &Path, days: &[&str]) -> std::path::PathBuf {
        let n = days.len();
        let orig = dir.join("orig");
        std::fs::create_dir_all(&orig).unwrap();
        let mut manifest = BundleManifest::new(
            BundleBatch { uuid: "batch-abort".into(), source_label: "Trip".into(), note: String::new(), created_at: 1 },
            2,
        );
        let mut originals = std::collections::HashMap::new();
        for i in 0..n {
            let uuid = format!("00000000-0000-4000-8000-00000000000{i}");
            let f = orig.join(format!("IMG_{i}.jpg"));
            std::fs::write(&f, format!("original {i}")).unwrap();
            manifest.photos.push(BundlePhoto {
                uuid: uuid.clone(),
                relative_path: format!("{}/IMG_{i}.jpg", days[i]),
                rating: 0,
                label: String::new(),
                pick_state: crate::catalog::PickState::None,
                iptc: Default::default(),
                edit_record: None,
                versions: Vec::new(),
                tag_uuids: Vec::new(),
            });
            originals.insert(uuid, Some(f));
        }
        let path = dir.join("trip.chairphoto");
        write_bundle(&GatheredBundle { manifest, originals }, &path, |_, _| {}).unwrap();
        path
    }

    fn files_under(dir: &Path) -> usize {
        walkdir::WalkDir::new(dir).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_file()).count()
    }

    /// Trips something on the first `import:progress` — i.e. while the first original is
    /// being unpacked, with four still to go.
    struct OnFirstProgress(Box<dyn Fn() + Send + Sync>, std::sync::atomic::AtomicBool);
    impl EventSink for OnFirstProgress {
        fn send(&self, event: CoreEvent) {
            if matches!(event, CoreEvent::ImportProgress(_)) && !self.1.swap(true, Ordering::SeqCst) {
                (self.0)();
            }
        }
    }

    /// **Forced interleaving.** Cancel, a catalog switch and a newer import each land while
    /// the first of five originals is being unpacked: the unpack stops before the second,
    /// nothing is merged into any catalog, the one copy stays (with its identity sidecar),
    /// and the report says so.
    #[test]
    fn cancel_switch_or_a_newer_import_stops_a_bundle_mid_extraction() {
        for how in ["cancel", "switch", "newer"] {
            let dir = crate::test_support::TestTmpDir::new(&format!("bundle-abort-{how}"));
            let path = bundle(&dir, 5);
            let root = dir.join("library");
            let state = AppState::default();
            *state.catalog.lock().unwrap() = Some(Catalog::open(&dir.join("a.chairphoto"), &root).unwrap());
            let other = (dir.join("b.chairphoto"), dir.join("b"));
            let trip: Box<dyn Fn() + Send + Sync> = {
                let s = state.clone();
                match how {
                    "cancel" => Box::new(move || super::super::scans::cancel_import(&s).unwrap()),
                    "switch" => Box::new(move || {
                        crate::app::catalogs::detach_catalog_and_trip_jobs(&s).unwrap();
                        crate::app::catalogs::publish_catalog_and_reset_jobs(&s, Catalog::open(&other.0, &other.1).unwrap())
                            .unwrap();
                    }),
                    _ => Box::new(move || {
                        super::super::scans::claim_import(&s).unwrap();
                    }),
                }
            };
            let worker = AppState { catalog: state.catalog.clone(), jobs: state.jobs.clone(), ..AppState::default() };
            worker.set_events(Arc::new(OnFirstProgress(trip, Default::default())));

            let err = import_bundle(&worker, &path).unwrap_err();
            assert_eq!(
                err,
                "Import cancelled: 1 original from the bundle is in the library folder but not merged — \
                 import the bundle again to finish.",
                "{how}"
            );
            assert_eq!(files_under(&root), 2, "{how}: one original and its identity sidecar, nothing more");
            let count = |s: &AppState| crate::app::with_catalog(s, |c| c.count_photos(&Default::default())).unwrap();
            assert_eq!(count(&state), 0, "{how}: nothing merged into the open catalog");
            if how == "switch" {
                let a = Catalog::open_secondary(&dir.join("a.chairphoto"), &root).unwrap();
                assert_eq!(a.count_photos(&Default::default()).unwrap(), 0, "nothing merged into the left catalog");
            }
        }
    }

    // --- the preview (#249, review NIT-2) -----------------------------------------------------

    /// A bundle photo whose identity the catalog lacks, at a path a photo of another identity
    /// holds, is not counted as new: the import will merge it onto that photo (proven
    /// capture) or keep it apart.
    #[test]
    fn the_preview_counts_photos_here_under_another_identity_apart_from_new_ones() {
        let dir = crate::test_support::TestTmpDir::new("bundle-preview-elsewhere");
        let path = bundle(&dir, 3);
        let root = dir.join("library");
        let day = root.join("2026/01/02");
        std::fs::create_dir_all(&day).unwrap();
        let c = Catalog::open(&dir.join("a.chairphoto"), &root).unwrap();
        // IMG_0 is here as the bundle's photo; IMG_1's path is held under another identity.
        for (i, uuid) in [(0, "00000000-0000-4000-8000-000000000000"), (1, "11111111-1111-4111-8111-111111111111")] {
            let f = day.join(format!("IMG_{i}.jpg"));
            std::fs::write(&f, b"x").unwrap();
            c.upsert_photo_with_identity(&f, None, 1, 1, Some(uuid)).unwrap();
        }
        let state = AppState::default();
        *state.catalog.lock().unwrap() = Some(c);
        let p = preview_bundle(&state, &path).unwrap();
        assert_eq!((p.total, p.existing, p.here_under_another_identity, p.new_count), (3, 1, 1, 1));
    }

    // --- the names an unpack finds taken (#247, #231 F3) -------------------------------------

    /// **Forced interleaving** (AGENTS.md, "Catalog identity"). Catalog A, which the import
    /// starts against, has a row holding `IMG_1.jpg` under another identity, its file gone.
    /// `IMG_1.jpg` is in a date folder of its own, so its names are first read after the swap
    /// (review LOW-3). As the second original is unpacked the open catalog is swapped for B — whose row has
    /// A's row's id and holds no name there — once silently (no switch delivered, the import
    /// not tripped) and once as a real switch (`catalog:switched` sent, the import tripped).
    /// Either way the unpack decides names by A's rows: `IMG_1.jpg` goes to ` (2)`, never to
    /// the name A's row holds. The silent swap indexes into A only; the switch merges into
    /// neither. B's row is untouched.
    #[test]
    fn the_names_an_unpack_finds_taken_are_the_started_catalogs() {
        const HELD: &str = "11111111-1111-4111-8111-111111111111";
        for switched in [false, true] {
            let dir = crate::test_support::TestTmpDir::new(&format!("bundle-names-swap-{switched}"));
            let path = bundle_in(&dir, &["2026/01/02", "2026/01/03"]);
            let root = dir.join("library");
            let day = root.join("2026/01/03");
            std::fs::create_dir_all(&day).unwrap();
            let a = Catalog::open(&dir.join("a.chairphoto"), &root).unwrap();
            let held = day.join("IMG_1.jpg");
            std::fs::write(&held, b"the old capture").unwrap();
            let held_id = a.upsert_photo_with_identity(&held, None, 1, 15, Some(HELD)).unwrap().id;
            std::fs::remove_file(&held).unwrap();
            let _ = std::fs::remove_file(crate::xmp::sidecar_path(&held));
            let state = AppState::default();
            *state.catalog.lock().unwrap() = Some(a);

            let b_path = dir.join("b.chairphoto");
            let b = Catalog::open(&b_path, &dir.join("b")).unwrap();
            let elsewhere = dir.join("b/elsewhere.jpg");
            std::fs::create_dir_all(elsewhere.parent().unwrap()).unwrap();
            std::fs::write(&elsewhere, b"b's photo").unwrap();
            assert_eq!(b.upsert_photo_with_identity(&elsewhere, None, 1, 9, None).unwrap().id, held_id, "colliding ids");

            struct SwapOnSecond(AppState, std::sync::Mutex<Option<Catalog>>, bool, std::path::PathBuf);
            impl EventSink for SwapOnSecond {
                fn send(&self, event: CoreEvent) {
                    let CoreEvent::ImportProgress(p) = event else { return };
                    if p.done != 2 {
                        return;
                    }
                    let Some(b) = self.1.lock().unwrap().take() else { return };
                    if self.2 {
                        crate::app::catalogs::detach_catalog_and_trip_jobs(&self.0).unwrap();
                        crate::app::catalogs::publish_catalog_and_reset_jobs(&self.0, b).unwrap();
                        self.0.send(CoreEvent::CatalogSwitched(self.3.to_string_lossy().into_owned()));
                    } else {
                        *self.0.catalog.lock().unwrap() = Some(b);
                    }
                }
            }
            let worker = AppState { catalog: state.catalog.clone(), jobs: state.jobs.clone(), ..AppState::default() };
            worker.set_events(Arc::new(SwapOnSecond(state.clone(), std::sync::Mutex::new(Some(b)), switched, b_path.clone())));

            let outcome = import_bundle(&worker, &path);
            assert!(!held.exists(), "{switched}: the name A's row holds stays empty");
            assert!(day.join("IMG_1 (2).jpg").exists(), "{switched}");
            let rows = |c: &Catalog| -> Vec<(i64, String, i64)> {
                let mut stmt = c.conn().prepare("SELECT id, path, rating FROM photos ORDER BY path").unwrap();
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().collect::<Result<_, _>>().unwrap()
            };
            let started = Catalog::open_secondary(&dir.join("a.chairphoto"), &root).unwrap();
            let paths: Vec<String> = rows(&started).into_iter().map(|r| r.1).collect();
            if switched {
                assert!(outcome.unwrap_err().starts_with(IMPORT_CANCELLED));
                assert_eq!(paths, ["2026/01/03/IMG_1.jpg"], "nothing merged into A");
            } else {
                let result = outcome.unwrap();
                assert_eq!(result.merge.photos_added, 2, "{result:?}");
                assert_eq!(paths, ["2026/01/02/IMG_0.jpg", "2026/01/03/IMG_1 (2).jpg", "2026/01/03/IMG_1.jpg"]);
            }
            let in_b = rows(&Catalog::open_secondary(&b_path, &dir.join("b")).unwrap());
            assert_eq!(in_b, [(held_id, "elsewhere.jpg".to_string(), 0)], "{switched}: B gained nothing, its row untouched");
        }
    }

    /// **Forced interleaving** (#185, catalog identity). The bundle's photo is already in
    /// catalog A, which the import started against; a switch to catalog B — whose photo has
    /// the very same row id — lands once the photo is indexed. The bundle's data is filled
    /// in on A's row (and A's photo's sidecar) only, through the import's own connection to
    /// A's file: B's colliding row, and its file's sidecar, are untouched. (No front end is
    /// attached, so no `catalog:switched` is delivered; this path never reads it.)
    #[test]
    fn a_switch_during_indexing_fills_the_existing_row_of_the_catalog_the_import_started_against() {
        const UUID: &str = "00000000-0000-4000-8000-000000000000";
        let dir = crate::test_support::TestTmpDir::new("bundle-185-switch");
        let orig = dir.join("orig/IMG_0.jpg");
        std::fs::create_dir_all(orig.parent().unwrap()).unwrap();
        std::fs::write(&orig, "original 0").unwrap();
        let mut manifest = BundleManifest::new(
            BundleBatch { uuid: "batch-185".into(), source_label: "Trip".into(), note: String::new(), created_at: 1 },
            2,
        );
        manifest.photos.push(BundlePhoto {
            uuid: UUID.into(),
            relative_path: "2026/01/02/IMG_0.jpg".into(),
            rating: 3,
            label: "green".into(),
            pick_state: crate::catalog::PickState::Pick,
            iptc: crate::catalog::IptcFields { city: "Oslo".into(), ..Default::default() },
            edit_record: None,
            versions: vec![crate::bundle::BundleVersion { name: "Square".into(), edit_json: "{\"crop\":1}".into(), position: 0 }],
            tag_uuids: Vec::new(),
        });
        let path = dir.join("trip.chairphoto");
        let originals = std::collections::HashMap::from([(UUID.to_string(), Some(orig.clone()))]);
        write_bundle(&GatheredBundle { manifest, originals }, &path, |_, _| {}).unwrap();

        // A holds the photo (the same file at the bundle's path), B a photo with the same row id.
        let seed = |db: &Path, root: &Path, uuid: &str| -> (i64, std::path::PathBuf) {
            let file = root.join("2026/01/02/IMG_0.jpg");
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, "original 0").unwrap();
            let c = Catalog::open(db, root).unwrap();
            let id = c.upsert_photo_with_identity(&file, None, 1, 10, Some(uuid)).unwrap().id;
            (id, file)
        };
        let (root_a, root_b) = (dir.join("library"), dir.join("b"));
        let (a_id, a_file) = seed(&dir.join("a.chairphoto"), &root_a, UUID);
        let (b_id, b_file) = seed(&dir.join("b.chairphoto"), &root_b, "11111111-2222-4333-8444-555555555555");
        assert_eq!(a_id, b_id, "colliding row ids");

        let state = AppState::default();
        *state.catalog.lock().unwrap() = Some(Catalog::open(&dir.join("a.chairphoto"), &root_a).unwrap());
        let claim = super::super::scans::claim_import(&state).unwrap();
        let switch = |n: usize| {
            if n == 1 {
                crate::app::catalogs::detach_catalog_and_trip_jobs(&state).unwrap();
                let b = Catalog::open(&dir.join("b.chairphoto"), &root_b).unwrap();
                crate::app::catalogs::publish_catalog_and_reset_jobs(&state, b).unwrap();
            }
        };
        let result = import_bundle_claimed_with(&state, &claim, &path, &switch).unwrap();
        assert_eq!((result.skipped_duplicate, result.merge.photos_filled), (1, 1), "{result:?}");

        let a = Catalog::open_secondary(&dir.join("a.chairphoto"), &root_a).unwrap();
        let filled = a.get_photo(a_id).unwrap();
        assert_eq!((filled.rating, filled.label.as_str()), (3, "green"), "A's blank row is filled");
        assert_eq!(a.get_iptc(a_id).unwrap().city, "Oslo");
        assert_eq!(a.list_versions(a_id).unwrap().len(), 1);
        assert!(std::fs::read_to_string(crate::xmp::sidecar_path(&a_file)).unwrap().contains("Oslo"));

        let b = crate::app::with_catalog(&state, |c| {
            Ok((c.get_photo(b_id)?, c.get_iptc(b_id)?, c.list_versions(b_id)?.len()))
        })
        .unwrap();
        assert_eq!((b.0.rating, b.0.label.as_str(), b.1.city.as_str(), b.2), (0, "", "", 0), "B untouched");
        let b_sidecar = std::fs::read_to_string(crate::xmp::sidecar_path(&b_file)).unwrap_or_default();
        assert!(!b_sidecar.contains("Oslo"), "B's photo's sidecar untouched");
    }

    /// **Forced interleaving** (#114 Codex, finding D). Cancel, a newer import and a catalog
    /// switch each land after the first of three originals is indexed: indexing stops before
    /// the second. The first is imported whole into the catalog the import started against —
    /// its row with the bundle's identity, its batch and its queued backup — and the other two
    /// are not inserted as metadata-only rows; importing the bundle again finishes it without
    /// duplicating the first.
    #[test]
    fn cancel_switch_or_a_newer_import_stops_bundle_indexing_between_photos() {
        for how in ["cancel", "newer", "switch"] {
            let dir = crate::test_support::TestTmpDir::new(&format!("bundle-index-abort-{how}"));
            let path = bundle(&dir, 3);
            let root = dir.join("library");
            let state = AppState::default();
            *state.catalog.lock().unwrap() = Some(Catalog::open(&dir.join("a.chairphoto"), &root).unwrap());
            let claim = super::super::scans::claim_import(&state).unwrap();
            let trip = |n: usize| {
                if n != 1 {
                    return;
                }
                match how {
                    "cancel" => super::super::scans::cancel_import(&state).unwrap(),
                    "newer" => drop(super::super::scans::claim_import(&state).unwrap()),
                    _ => {
                        crate::app::catalogs::detach_catalog_and_trip_jobs(&state).unwrap();
                        let b = Catalog::open(&dir.join("b.chairphoto"), &dir.join("b")).unwrap();
                        crate::app::catalogs::publish_catalog_and_reset_jobs(&state, b).unwrap();
                    }
                }
            };
            let err = import_bundle_claimed_with(&state, &claim, &path, &trip).unwrap_err();
            assert_eq!(err, "Import cancelled: 1 of 3 unpacked originals imported — import the bundle again to finish.", "{how}");
            let a = Catalog::open_secondary(&dir.join("a.chairphoto"), &root).unwrap();
            let rows = a.list_photos(&Default::default()).unwrap();
            assert_eq!(rows.len(), 1, "{how}: one original indexed, not three");
            assert_eq!(rows[0].uuid, "00000000-0000-4000-8000-000000000000", "{how}: with the bundle's identity");
            let backups: Vec<i64> = a.list_pending_operations().unwrap().iter().map(|o| o.photo_id).collect();
            assert_eq!(backups, [rows[0].id], "{how}: its queued backup");
            assert_eq!(
                a.import_batch_uuid_for_photo(rows[0].id).unwrap().as_deref(),
                Some("batch-abort"),
                "{how}: its batch, merged for it alone"
            );
            if how == "switch" {
                assert_eq!(crate::app::with_catalog(&state, |c| c.count_photos(&Default::default())).unwrap(), 0, "B untouched");
                continue;
            }
            // Again, uninterrupted: finished, the first photo matched rather than duplicated.
            import_bundle(&state, &path).unwrap();
            let rows = a.list_photos(&Default::default()).unwrap();
            assert_eq!(rows.len(), 3, "{how}: the re-import finished it without a duplicate");
            assert!(rows.iter().all(|p| a.import_batch_uuid_for_photo(p.id).unwrap().is_some()), "{how}: all batched");
        }
    }
}
