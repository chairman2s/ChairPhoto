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
/// The core's `app::exports::export_photos` on a blocking worker: an owned job (a newer
/// export or a catalog switch stops it), progress as `export:progress`.
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
    let request = crate::app::exports::ExportRequest {
        photo_ids,
        preset,
        dest_dir: expand_home(&dest_dir),
        hashtag_group_id,
        hashtag_limit,
        version_id,
    };
    let state = state.inner().clone();
    crate::app::spawn_blocking(move || crate::app::exports::export_photos(&state, &request))
        .await
        .map_err(|e| e.to_string())?
}

/// Export one import batch as a `.chairphoto` bundle zip to `dest_path` — the core's
/// `app::exports::export_bundle_claimed_with` on a blocking worker, an owned job (a newer
/// bundle export or a catalog switch stops it, leaving nothing at the destination).
///
/// The bundle carries the full catalog metadata and, where the originals are reachable,
/// the raw files and their XMP sidecars. Unreachable originals are counted in
/// `skipped_offline`; their metadata still travels. Progress is streamed as
/// `import:progress` events with job 0 — the shape the React topbar listens for.
#[tauri::command]
pub async fn export_bundle(
    app: AppHandle,
    state: State<'_, AppState>,
    batch_id: i64,
    dest_path: String,
) -> Result<crate::bundle::writer::BundleWriteResult, String> {
    let dest = expand_home(&dest_path);
    let state = state.inner().clone();
    let claim = crate::app::exports::claim_bundle_export(&state)?;
    crate::app::spawn_blocking(move || {
        crate::app::exports::export_bundle_claimed_with(&state, &claim, None, batch_id, &dest, &|done, total| {
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
