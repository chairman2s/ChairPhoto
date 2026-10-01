//! Export commands: one-way JPEG export, and the portable catalog bundle
//! (export / preview / import) used to move work between machines.
//!
//! Export is where catalog metadata reaches a sidecar the outside world reads —
//! keywords, rating/label and IPTC are written into the *destination* sidecar.

use super::*;
use tauri::{AppHandle, State};

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
    let result = crate::app::spawn_blocking(move || {
        crate::export::write_exports(&resolved, preset, &dest, &hashtags)
    })
    .await
    .map_err(|e| e.to_string())?;
    record_export_parity(&state);
    result
}

/// The settings key holding this catalog's "export equals view" total
/// (`plugins::edit::parity::ParityTally` as JSON): exports checked, exports that differed.
#[cfg(feature = "edit")]
pub const EXPORT_PARITY_KEY: &str = "metrics.exportParity";

/// Add the engine-2 exports checked since the last call to this catalog's total. Called
/// after every command that writes an export (the Export dialog, publishing, Instagram,
/// LocalSend); best-effort — a failed write loses a count, never an export.
pub(crate) fn record_export_parity(state: &AppState) {
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
    crate::app::spawn_blocking(move || {
        crate::bundle::writer::write_bundle(&bundle, &dest, |done, total| {
            let _ = app.send(CoreEvent::ImportProgress(ImportProgress { job: 0, done, total }));
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Import a `.chairphoto` bundle (F1d): unpack originals into `<root>/YYYY/MM/DD/`, index
/// via the UUID-aware upsert, run the additive merge and auto-enqueue backup — the core's
/// `app::bundles::import_bundle` on a blocking worker (the copy runs off the catalog lock,
/// the index on a secondary connection). Progress streams as `import:progress`. The import
/// owns the import generation, so a catalog switch or a newer import stops it before the
/// merge.
#[tauri::command]
pub async fn import_bundle_cmd(
    state: State<'_, AppState>,
    bundle_path: String,
) -> Result<crate::bundle::importer::BundleImportResult, String> {
    let bundle_path = expand_home(&bundle_path);
    let state = state.inner().clone();
    crate::app::spawn_blocking(move || crate::app::bundles::import_bundle(&state, &bundle_path))
        .await
        .map_err(|e| e.to_string())?
}

/// Peek at a `.chairphoto` bundle and return a lightweight pre-import summary ("N new / M
/// already present") so the user can confirm before the full import. Writes nothing.
#[tauri::command]
pub async fn preview_bundle(
    state: State<'_, AppState>,
    bundle_path: String,
) -> Result<BundlePreview, String> {
    let bundle_path = expand_home(&bundle_path);
    let state = state.inner().clone();
    crate::app::spawn_blocking(move || crate::app::bundles::preview_bundle(&state, &bundle_path))
        .await
        .map_err(|e| e.to_string())?
}

pub use crate::app::bundles::BundlePreview;
