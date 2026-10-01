//! Folder scanning and card import.
//!
//! The bodies live in the core (`crate::app::scans`), shared with the GPUI app; each command
//! runs one on the blocking pool and, for a two-phase scan (I6), detaches the Phase B
//! enrichment job it returns. Both phases honour the scan generation `begin_scan_generation`
//! installs, and a card import owns the import generation, so a catalog switch or a second
//! scan/import stops the previous one before it can write to a torn-down catalog.

use super::*;
use crate::app::scans;
use tauri::{AppHandle, Manager};

/// Scan a folder (recursively) into the open catalog. Read-only on photo files.
/// A leading "~" is expanded to $HOME.
///
/// Runs the scan on a blocking worker thread (off the UI/runtime thread) so the window
/// stays responsive even on a large library — see the AGENTS.md "UI thread is never
/// blocked" invariant.
#[tauri::command]
pub async fn scan_folder_cmd(app: AppHandle, folder: String) -> Result<ScanResult, String> {
    let expanded = expand_home(&folder);
    run_two_phase(app, move |state| scans::scan_folder(state, expanded)).await
}

/// Index an existing archive that lives on a non-root volume (e.g. old photos already on
/// the NAS) **in place** — no files are copied. The photos are recorded as NAS-resident
/// (they appear under the "On NAS" tier). For the initial bring-your-NAS-archive scan.
#[tauri::command]
pub async fn scan_nas_folder_cmd(app: AppHandle, folder: String) -> Result<ScanResult, String> {
    let expanded = expand_home(&folder);
    run_two_phase(app, move |state| scans::scan_nas_folder(state, expanded)).await
}

/// Run a two-phase scan (I6) from the core: Phase A (the fast walk) is awaited on a blocking
/// worker, so this command returns as soon as the grid can show the new rows; Phase B
/// (EXIF/IPTC/XMP + finalizing) is then detached on its own blocking worker and emits the
/// terminal `scan:progress {phase:"done"}` itself.
pub(super) async fn run_two_phase<F>(app: AppHandle, scan: F) -> Result<ScanResult, String>
where
    F: FnOnce(&AppState) -> Result<(ScanResult, EnrichJob), String> + Send + 'static,
{
    let state = app.state::<AppState>().inner().clone();
    let (result, enrich) = crate::app::spawn_blocking(move || scan(&state))
        .await
        .map_err(|e| e.to_string())??;
    crate::app::spawn_blocking(move || enrich.run());
    Ok(result)
}

/// Import from a card: copy supported images from `source` into the library root under a
/// YYYY/MM/DD tree, index them, batch them, and auto-queue backup. `source` expands a leading
/// "~". Progress streams as `import:progress`; see `app::scans::ingest_from_card`.
///
/// Runs on a blocking worker thread (copies + exiftool) so the window stays responsive.
#[tauri::command]
pub async fn ingest_from_card_cmd(
    app: AppHandle,
    source: String,
    name: Option<String>,
    selected: Option<Vec<String>>,
) -> Result<ScanResult, String> {
    let source = expand_home(&source);
    // The chosen subset of card files (by full path), if the dialog made a selection.
    let selected: Option<std::collections::HashSet<String>> = selected.map(|v| v.into_iter().collect());
    let state = app.state::<AppState>().inner().clone();
    crate::app::spawn_blocking(move || scans::ingest_from_card(&state, &source, name.as_deref(), selected))
        .await
        .map_err(|e| e.to_string())?
}

/// List the photos on a card/source folder for the import dialog, each flagged as a
/// duplicate (already in the library). FS + metadata only — runs on a worker thread.
#[tauri::command]
pub async fn list_card_photos_cmd(
    app: AppHandle,
    source: String,
) -> Result<Vec<crate::scanner::CardPhoto>, String> {
    let source = expand_home(&source);
    let state = app.state::<AppState>().inner().clone();
    crate::app::spawn_blocking(move || scans::list_card_photos(&state, &source))
        .await
        .map_err(|e| e.to_string())?
}

/// A thumbnail (base64 data URL) for an arbitrary file path — used by the import dialog to
/// preview card photos that aren't in the catalog yet. Cached like other thumbnails.
#[tauri::command]
pub async fn card_thumbnail(path: String) -> Result<String, String> {
    let path = expand_home(&path);
    let bytes = crate::app::spawn_blocking(move || crate::thumbnails::thumbnail_bytes(&path))
        .await
        .map_err(|e| e.to_string())??;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("data:image/jpeg;base64,{b64}"))
}
