//! Export commands: one-way JPEG export, and the portable catalog bundle
//! (export / preview / import) used to move work between machines.
//!
//! Export is where catalog metadata reaches a sidecar the outside world reads —
//! keywords, rating/label and IPTC are written into the *destination* sidecar.

use super::*;
use tauri::{AppHandle, Emitter, State};

/// Export photos to a destination folder using a preset (Hand-off RAW+XMP, or
/// Show-off JPEG). `destDir`'s leading "~" is expanded. Unreachable originals are
/// reported in the result so the UI can warn instead of silently exporting a subset.
#[tauri::command]
pub async fn export_photos(
    state: State<'_, AppState>,
    photo_ids: Vec<i64>,
    preset: crate::export::ExportPreset,
    dest_dir: String,
    hashtag_group_id: Option<i64>,
    hashtag_limit: Option<usize>,
    version_id: Option<i64>,
) -> Result<crate::export::ExportResult, String> {
    let dest = expand_home(&dest_dir);
    // Resolve originals + assemble the optional reach-hashtag bundle under the lock,
    // then release it so the file copying runs off the UI thread.
    let (resolved, hashtags) = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_ref().ok_or("No catalog is open")?;
        // Languages for keyword assembly: canonical + neutral synonyms for now (a
        // per-language export option can pass real codes here later).
        let resolved = crate::export::resolve_originals(catalog, &photo_ids, &[], version_id);
        let hashtags = match hashtag_group_id {
            Some(g) => catalog
                .assemble_hashtag_bundle(g, hashtag_limit)
                .map_err(|e| e.to_string())?,
            None => Vec::new(),
        };
        (resolved, hashtags)
    };
    tauri::async_runtime::spawn_blocking(move || {
        crate::export::write_exports(&resolved, preset, &dest, &hashtags)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Export one import batch as a `.chairphoto` bundle zip to `dest_path`.
///
/// The bundle carries the full catalog metadata (ratings, tags, versions, IPTC, edit
/// records) and — where the originals are reachable — copies the raw files and their
/// XMP sidecars into `originals/`, plus a cached JPEG preview under `previews/`.
///
/// Unreachable originals (offline NAS, missing file) are counted and reported in the
/// result; their metadata still travels so the importing catalog can merge it. Silently
/// truncating is not allowed: the UI must surface `skipped_offline` to the user.
///
/// Progress is streamed as `import:progress` events (`{done, total}`) — the same shape
/// E5/ingest uses — so the frontend can reuse its progress bar.
#[tauri::command]
pub async fn export_bundle(
    app: AppHandle,
    state: State<'_, AppState>,
    batch_id: i64,
    dest_path: String,
) -> Result<crate::bundle::writer::BundleWriteResult, String> {
    let dest = expand_home(&dest_path);

    // Phase 1 — gather all catalog data under the lock, then release it.
    // This is pure DB work (no file IO), so it completes quickly.
    let bundle = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_ref().ok_or("No catalog is open")?;
        crate::bundle::writer::gather_bundle(catalog, batch_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("Import batch {batch_id} not found"))?
    };

    // Warn on offline originals (count, never silently truncate). We log here; the
    // frontend should surface the `skipped_offline` field in the returned result.
    let offline_count = bundle
        .originals
        .values()
        .filter(|o| o.is_none())
        .count();
    if offline_count > 0 {
        eprintln!(
            "export_bundle: {offline_count} original(s) are offline — \
             their metadata will be included but no bytes copied"
        );
    }

    // Phase 2 — write the zip off the catalog lock (file IO can be slow for large RAW sets).
    // Progress events mirror the `import:progress` shape used by E5 (ingest_from_card).
    tauri::async_runtime::spawn_blocking(move || {
        crate::bundle::writer::write_bundle(&bundle, &dest, |done, total| {
            let _ = app.emit("import:progress", ImportProgress { done, total });
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Import a `.chairphoto` bundle (F1d): unpack originals into `<root>/YYYY/MM/DD/`,
/// index via the UUID-aware upsert (prevents duplicates), run the F1c additive merge
/// (metadata/taxonomy/ratings/tags/import batch), and auto-enqueue backup. The copy
/// phase runs off the catalog lock to keep the UI responsive; the index phase holds
/// the lock only briefly (fast DB work).
///
/// Progress is streamed as `import:progress` events (`{done, total}`) during the copy
/// phase — the same shape as E5/ingest so the frontend reuses its progress bar.
#[tauri::command]
pub async fn import_bundle_cmd(
    app: AppHandle,
    state: State<'_, AppState>,
    bundle_path: String,
) -> Result<crate::bundle::importer::BundleImportResult, String> {
    let bundle_path = expand_home(&bundle_path);

    // The destination is always the library root (catalog root = local volume base).
    // Read it under a brief lock before releasing for the long copy phase.
    let dest = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_ref().ok_or("No catalog is open")?;
        catalog.root().to_path_buf()
    };

    // Phase 1+2 — open + extract originals. File IO only; no catalog lock.
    // This is the slow part (potentially gigabytes of RAW files).
    let (manifest, extracted, partial) = {
        let app = app.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let (manifest, mut archive) =
                crate::bundle::importer::open_bundle(&bundle_path)?;
            let (extracted, partial) = crate::bundle::importer::extract_originals(
                &manifest,
                &mut archive,
                &dest,
                |done, total| {
                    let _ = app.emit("import:progress", ImportProgress { done, total });
                },
            )?;
            Ok::<_, String>((manifest, extracted, partial))
        })
        .await
        .map_err(|e| e.to_string())??
    };

    // Phase 3 — index + merge. Read catalog path+root under a brief lock, then release
    // it. The actual indexing (DB writes, XMP sidecar writes, reconcile_missing) runs on
    // a secondary catalog connection inside spawn_blocking — the main mutex is never held
    // across file I/O. This matches the run_blocking_scan / ingest_from_card pattern and
    // satisfies AGENTS.md "UI thread is never blocked" + the spec's "copy phase off the
    // catalog lock" requirement.
    let (db_path, root) = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_ref().ok_or("No catalog is open")?;
        (catalog.db_path().to_path_buf(), catalog.root().to_path_buf())
    };
    let result = tauri::async_runtime::spawn_blocking(move || {
        let sec = crate::catalog::Catalog::open_secondary(&db_path, &root)
            .map_err(|e| e.to_string())?;
        crate::bundle::importer::index_bundle(&sec, &manifest, &extracted, &root, partial)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;

    Ok(result)
}

/// Peek at a `.chairphoto` bundle and return a lightweight pre-import summary so the
/// frontend can show the user "N new / M already present" before committing to the full
/// import. Opens the bundle zip, reads `manifest.json`, and counts photos by UUID against
/// the open catalog. No files are written and no catalog data is changed.
///
/// Returns a [`BundlePreview`] with the batch label, total photo count, and the number
/// that are already present in the catalog (so `new = total - existing`).
#[tauri::command]
pub async fn preview_bundle(
    state: State<'_, AppState>,
    bundle_path: String,
) -> Result<BundlePreview, String> {
    let bundle_path = expand_home(&bundle_path);

    // Phase 1 — open and parse the manifest (pure filesystem; no catalog lock).
    let manifest = tauri::async_runtime::spawn_blocking(move || {
        let (manifest, _archive) = crate::bundle::importer::open_bundle(&bundle_path)?;
        Ok::<_, String>(manifest)
    })
    .await
    .map_err(|e| e.to_string())??;

    // Phase 2 — count new vs existing with a single IN-clause query under a brief
    // catalog lock.  Collecting all UUIDs first avoids holding the lock for N
    // individual round-trips (one per photo), which stalls thumbnails and grid
    // refreshes on large bundles.
    let uuids: Vec<String> = manifest.photos.iter().map(|bp| bp.uuid.clone()).collect();
    let (existing, new_count) = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_ref().ok_or("No catalog is open")?;
        let existing = catalog.count_existing_uuids(&uuids).map_err(|e| e.to_string())?;
        let new_count = uuids.len().saturating_sub(existing);
        (existing, new_count)
    };

    Ok(BundlePreview {
        batch_label: manifest.batch.source_label.clone(),
        batch_uuid: manifest.batch.uuid.clone(),
        total: manifest.photos.len(),
        new_count,
        existing,
    })
}

/// Lightweight summary returned by [`preview_bundle`] for the pre-import dialog.
#[derive(Debug, serde::Serialize)]
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

