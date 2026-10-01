//! Importing a `.chairphoto` bundle (F1d) — the bodies of the Tauri `preview_bundle` and
//! `import_bundle_cmd` commands, and of the GPUI bundle-import dialog. Both **block** (zip
//! reads, copies of possibly gigabytes of RAW files): run them on a worker.
//!
//! An import owns the import generation (`JobRegistry::import`), like a card import: a newer
//! import, `scans::cancel_import` or a catalog switch trips it. A tripped import stops
//! unpacking before its next original, and merges nothing into a catalog it no longer owns.
//! What it already unpacked stays in the library folder (never deleted); importing the
//! bundle again finishes it.

use super::scans::{library_root, IMPORT_CANCELLED};
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
    /// Photos already present in the catalog (merge is a no-op for them).
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
    let (abort, job) = (&*claim.abort, claim.job);
    if abort.load(Ordering::Relaxed) {
        return Err(format!("{IMPORT_CANCELLED} before the bundle was opened."));
    }
    let dest = library_root(state)?;
    let db_path = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        guard.as_ref().ok_or("No catalog is open")?.db_path().to_path_buf()
    };
    let (manifest, mut archive) = crate::bundle::importer::open_bundle(bundle_path)?;
    let (extracted, partial, aborted) = {
        let events = state.clone();
        crate::bundle::importer::extract_originals_abortable(&manifest, &mut archive, &dest, abort, move |done, total| {
            events.send(CoreEvent::ImportProgress(ImportProgress { job, done, total }))
        })?
    };
    if aborted || abort.load(Ordering::Relaxed) {
        return Err(cancelled_message(partial.copied));
    }
    let sec = crate::catalog::Catalog::open_secondary(&db_path, &dest).map_err(|e| e.to_string())?;
    crate::bundle::importer::index_bundle(&sec, &manifest, &extracted, &dest, partial).map_err(|e| e.to_string())
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
}
