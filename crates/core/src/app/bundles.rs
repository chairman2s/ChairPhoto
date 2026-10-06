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
}

/// Peek at a bundle: parse its manifest (no catalog lock) and count its photos by UUID
/// against the open catalog with one IN-clause query under a brief lock. Writes nothing.
pub fn preview_bundle(state: &AppState, bundle_path: &Path) -> Result<BundlePreview, String> {
    let (manifest, _archive) = crate::bundle::importer::open_bundle(bundle_path)?;
    let uuids: Vec<String> = manifest.photos.iter().map(|bp| bp.uuid.clone()).collect();
    let existing = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_ref().ok_or("No catalog is open")?;
        catalog.count_existing_uuids(&uuids).map_err(|e| e.to_string())?
    };
    Ok(BundlePreview {
        batch_label: manifest.batch.source_label.clone(),
        batch_uuid: manifest.batch.uuid.clone(),
        total: manifest.photos.len(),
        new_count: uuids.len().saturating_sub(existing),
        existing,
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

    /// A bundle of `n` photos, each with a small original.
    fn bundle(dir: &Path, n: usize) -> std::path::PathBuf {
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
                relative_path: format!("2026/01/02/IMG_{i}.jpg"),
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
