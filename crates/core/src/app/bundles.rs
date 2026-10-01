//! Importing a `.chairphoto` bundle (F1d) — the bodies of the Tauri `preview_bundle` and
//! `import_bundle_cmd` commands, and of the GPUI bundle-import dialog. Both **block** (zip
//! reads, copies of possibly gigabytes of RAW files): run them on a worker.
//!
//! An import owns the import generation (`JobRegistry::import`), like a card import: a newer
//! import, `scans::cancel_import` or a catalog switch trips it, and an import tripped after
//! its originals are extracted does not merge them into a catalog it no longer owns.

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
    let abort = super::scans::claim_import(state)?;
    import_bundle_claimed(state, &abort, bundle_path)
}

/// [`import_bundle`] under an import generation the caller already claimed
/// (`scans::claim_import`). Already tripped: it unpacks nothing.
pub fn import_bundle_claimed(
    state: &AppState,
    abort: &std::sync::atomic::AtomicBool,
    bundle_path: &Path,
) -> Result<BundleImportResult, String> {
    if abort.load(Ordering::Relaxed) {
        return Err(format!("{IMPORT_CANCELLED} before the bundle was opened."));
    }
    let dest = library_root(state)?;
    let db_path = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        guard.as_ref().ok_or("No catalog is open")?.db_path().to_path_buf()
    };
    let (manifest, mut archive) = crate::bundle::importer::open_bundle(bundle_path)?;
    let (extracted, partial) = {
        let events = state.clone();
        crate::bundle::importer::extract_originals(&manifest, &mut archive, &dest, move |done, total| {
            events.send(CoreEvent::ImportProgress(ImportProgress { done, total }))
        })?
    };
    if abort.load(Ordering::Relaxed) {
        return Err(format!(
            "{IMPORT_CANCELLED}: the bundle's originals were unpacked into the library folder but not merged — \
             import the bundle again to finish."
        ));
    }
    let sec = crate::catalog::Catalog::open_secondary(&db_path, &dest).map_err(|e| e.to_string())?;
    crate::bundle::importer::index_bundle(&sec, &manifest, &extracted, &dest, partial).map_err(|e| e.to_string())
}
