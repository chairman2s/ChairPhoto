//! Folder scanning. Walks a directory tree, detects supported image files, and
//! upserts them into the catalog. This is READ-ONLY on the photo files — it only
//! reads file size and mtime; it never writes into the user's photo folders.
//!
//! Extension lists are ported from the old Python `scanner.py`.

use crate::catalog::Catalog;
use crate::metadata::extract_batch;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use walkdir::WalkDir;

pub mod sidecars;

/// Commit a scan's writes every this many files rather than in one giant transaction, so
/// the WAL write lock is released periodically (concurrent edits get a window) and new rows
/// become visible in the grid as the scan progresses.
const COMMIT_EVERY: usize = 500;

/// Error string returned when a scan is cancelled through its abort flag (e.g. a catalog
/// switch, I4b). The caller (`run_blocking_scan`) treats this as a clean stop, not a failure.
pub const SCAN_ABORTED: &str = "scan aborted";

/// RAW formats. Their thumbnails come from embedded previews (see `thumbnails`).
pub const RAW_EXTENSIONS: &[&str] = &[
    "3fr", "ari", "arw", "bay", "braw", "cr2", "cr3", "crw", "dcr", "dng", "erf", "fff", "iiq",
    "k25", "kdc", "mef", "mos", "mrw", "nef", "nrw", "orf", "pef", "raf", "raw", "rw2", "rwl",
    "sr2", "srf", "srw", "x3f",
];

/// Standard raster formats decodable directly by the `image` crate.
pub const RASTER_EXTENSIONS: &[&str] = &[
    "avif", "bmp", "gif", "heic", "heif", "jpeg", "jpg", "jxl", "png", "qoi", "tif", "tiff", "webp",
];

/// Video formats — catalogued and played via the `video://` protocol (no thumbnail decode;
/// the grid shows a film placeholder). Includes slideshow output (`.mp4`).
pub const VIDEO_EXTENSIONS: &[&str] = &["mp4", "m4v", "mov", "avi", "webm", "mkv"];

/// Outcome of a scan, surfaced to the frontend.
#[derive(Debug, Default, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanResult {
    pub scanned: usize,
    pub imported: usize,
    pub created: usize,
    pub errors: usize,
    /// Ingest only: source files already present at the destination (same size), skipped.
    pub skipped: usize,
}

/// Live progress of a scan, streamed to the UI via the `scan:progress` event. `total = 0`
/// means indeterminate (the discovery phase doesn't know the count until it finishes).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanProgress {
    /// "indexing" | "metadata" | "finalizing" | "done"
    pub phase: String,
    pub done: usize,
    pub total: usize,
}

impl ScanProgress {
    fn new(phase: &str, done: usize, total: usize) -> Self {
        Self { phase: phase.to_string(), done, total }
    }
}

/// The hand-off from Phase A (fast walk) to Phase B (background enrichment) of the
/// two-phase live scan (I6). Phase A returns this; [`phase_b_enrich`] consumes it,
/// possibly on a **detached** worker thread with its own catalog connection (I6c).
///
/// `imported` is the `(path, photo_id, needs_extract)` list built by the walk;
/// `folder_id` is the scanned folder's catalog id for the local scan (so Phase B can
/// mark the scan finished), or `None` for a NAS in-place scan (which tracks no folder).
pub struct PendingEnrich {
    pub imported: Vec<(PathBuf, i64, bool)>,
    pub folder_id: Option<i64>,
}

/// A never-aborting flag, for callers (tests, one-shot indexing) that don't drive a
/// cancellable scan. Passing this to a scan function means it runs to completion.
pub fn never_abort() -> AtomicBool {
    AtomicBool::new(false)
}

/// Rebuild the `PendingEnrich` hand-off from the persisted `pending_enrichment` table
/// (I6d). Called on startup (or by `drain_enrichment_queue`) when rows remain from a
/// prior run that crashed or was quit mid-Phase-B. The result feeds directly into
/// [`phase_b_enrich`] on a detached worker.
///
/// `folder_id` is `None` because the resume path doesn't know which folder started the
/// original scan, so the finalizing pass in Phase B will skip `mark_folder_scan_finished`
/// — that's acceptable since the scan did complete its walk; we're only catching up on
/// enrichment.
pub fn resume_pending_enrichment(catalog: &Catalog) -> Result<Option<PendingEnrich>, String> {
    let imported = catalog.load_pending_enrichment().map_err(|e| e.to_string())?;
    if imported.is_empty() {
        return Ok(None);
    }
    Ok(Some(PendingEnrich { imported, folder_id: None }))
}

pub fn is_raw(path: &Path) -> bool {
    has_extension(path, RAW_EXTENSIONS)
}

pub fn is_video(path: &Path) -> bool {
    has_extension(path, VIDEO_EXTENSIONS)
}

pub fn is_supported_image(path: &Path) -> bool {
    has_extension(path, RAW_EXTENSIONS)
        || has_extension(path, RASTER_EXTENSIONS)
        || has_extension(path, VIDEO_EXTENSIONS)
}

fn has_extension(path: &Path, set: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| set.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Phase A of the two-phase live scan (I6b): walk `folder`, upsert every supported
/// image into `catalog` with `metadata_ready = 0` for new/changed files (unchanged
/// files stay at 1), commit every `COMMIT_EVERY` so rows become visible immediately,
/// and emit `scan:progress {phase:"indexing"}` after each commit.
///
/// The **UUID sidecar write (J5 invariant)** happens here — inside `upsert_fn` — so
/// every file has an identity before Phase B touches it.
///
/// Returns `(result, imported, newly_created)` where `imported` is the
/// `(path, photo_id, needs_extract)` list for Phase B and `newly_created` is the list
/// of photo_ids that were inserted for the first time (for import-batch assignment).
///
/// `upsert_fn` encapsulates `upsert_one` (local scan) or `upsert_external_one` (NAS
/// scan) so Phase A logic isn't duplicated across the two callers.
fn phase_a_walk<F>(
    catalog: &Catalog,
    folder: &Path,
    abort: &AtomicBool,
    progress: &dyn Fn(ScanProgress),
    mut upsert_fn: F,
) -> Result<(ScanResult, Vec<(PathBuf, i64, bool)>, Vec<i64>), String>
where
    F: FnMut(&Path) -> Result<(i64, bool, bool), String>,
{
    let mut result = ScanResult::default();
    let mut imported: Vec<(PathBuf, i64, bool)> = Vec::new();
    let mut newly_created: Vec<i64> = Vec::new();

    let mut tx = catalog.begin().map_err(|e| e.to_string())?;
    let mut since_commit = 0usize;
    for entry in WalkDir::new(folder)
        .into_iter()
        .filter_entry(|e| !is_hidden(e.path()))
        .filter_map(|e| e.ok())
    {
        // Cancellation point (e.g. a catalog switch): commit what's durable and bail so
        // nothing further is written into a catalog that's about to be torn down.
        if abort.load(Ordering::Relaxed) {
            tx.commit().map_err(|e| e.to_string())?;
            return Err(SCAN_ABORTED.to_string());
        }
        let path = entry.path();
        if !path.is_file() || !is_supported_image(path) || is_empty_file(path) {
            continue;
        }
        result.scanned += 1;
        match upsert_fn(path) {
            Ok((photo_id, created, unchanged)) => {
                result.imported += 1;
                let needs_extract = created || !unchanged;
                if created {
                    result.created += 1;
                    newly_created.push(photo_id);
                }
                // Mark new/changed files as not-yet-ready: Phase B will set metadata_ready=1
                // after extracting EXIF/IPTC/XMP. Unchanged files keep their existing flag
                // value (1 by default, or already set from a previous scan), so the grid
                // continues to display them immediately without any disruption.
                if needs_extract {
                    let _ = catalog.set_metadata_ready(photo_id, false);
                    // I6d: persist the enrichment intent so Phase B can auto-resume if the
                    // app is killed or crashes before it finishes.
                    let _ = catalog.enqueue_pending_enrichment(photo_id);
                }
                imported.push((path.to_path_buf(), photo_id, needs_extract));
            }
            Err(_) => result.errors += 1,
        }
        since_commit += 1;
        if since_commit >= COMMIT_EVERY {
            tx.commit().map_err(|e| e.to_string())?;
            tx = catalog.begin().map_err(|e| e.to_string())?;
            since_commit = 0;
            progress(ScanProgress::new("indexing", result.scanned, 0));
        }
    }
    tx.commit().map_err(|e| e.to_string())?;

    Ok((result, imported, newly_created))
}

/// Phase A of a local library scan: walk `folder` recursively, upsert every supported
/// image into `catalog` (new/changed rows marked `metadata_ready = 0`), form the import
/// batch, and write the batch UUID into each new photo's sidecar. Hidden files and
/// directories (dot-prefixed) are skipped, matching the old app.
///
/// Returns `(result, pending)` where `pending` is the hand-off for [`phase_b_enrich`]
/// (EXIF/IPTC/XMP extraction + finalizing). The caller runs Phase B — inline via
/// [`scan_folder`], or **detached** on its own connection so the grid stays live while
/// enrichment continues in the background (I6c).
pub fn scan_folder_phase_a(
    catalog: &Catalog,
    folder: &Path,
    abort: &AtomicBool,
    progress: &dyn Fn(ScanProgress),
) -> Result<(ScanResult, PendingEnrich), String> {
    if !folder.is_dir() {
        return Err(format!("Not a directory: {}", folder.display()));
    }
    let folder_id = catalog.add_folder(folder).map_err(|e| e.to_string())?;
    catalog
        .mark_folder_scan_started(folder_id)
        .map_err(|e| e.to_string())?;

    // Phase A: fast walk — upsert path/uuid/mtime/size, mark new/changed rows as
    // metadata_ready=0, emit scan:progress {phase:"indexing"}, return import list.
    // The UUID sidecar binding (J5) happens inside upsert_one, so identity exists on
    // disk before Phase B begins — or its repair is queued in pending_sidecar_identity.
    let (result, imported, newly_created) = phase_a_walk(
        catalog,
        folder,
        abort,
        progress,
        |path| upsert_one(catalog, path, folder_id),
    )?;

    // Each ingest that brought in new photos is an import batch ("negative film roll").
    // Only newly-created photos join it; existing photos keep their original batch.
    // K3: also write the batch UUID into each new photo's XMP sidecar so the batch
    // survives catalog loss / merge across machines.
    let batch_uuid_for_new: Option<String> = if !newly_created.is_empty() {
        let mut uuid_opt = None;
        if let Ok(batch_id) = catalog.create_import_batch(&folder.to_string_lossy()) {
            let _ = catalog.assign_photos_to_batch(batch_id, &newly_created);
            uuid_opt = catalog.get_import_batch_uuid(batch_id).ok().flatten();
        }
        // Auto-enqueue each new photo for backup (E4); drained when a backup volume is
        // reachable. No-op effect until the user configures a backup volume.
        for &photo_id in &newly_created {
            let _ = catalog.enqueue_operation("backup", photo_id);
        }
        uuid_opt
    } else {
        None
    };

    // Write the batch UUID into the sidecar of each newly-created photo (K3).
    // Done after the phase_a_walk commits so the batch row is durable before we touch
    // the filesystem. Best-effort: a sidecar write failure never aborts the scan.
    if let Some(ref batch_uuid) = batch_uuid_for_new {
        // Build a set of newly-created ids for O(1) lookup.
        let created_ids: std::collections::HashSet<i64> = newly_created.iter().copied().collect();
        for (path, photo_id, _) in &imported {
            if created_ids.contains(photo_id) {
                if let Err(e) = crate::xmp::write_import_batch(path, batch_uuid) {
                    eprintln!("scan: couldn't write ImportBatch sidecar for {}: {e}", path.display());
                }
            }
        }
    }

    // Phase A ends here. EXIF/IPTC/XMP extraction, external-editor detection, and the
    // finalizing pass (auto-tags, RAW/JPEG stacking, missing reconciliation, and marking
    // the scan finished) all happen in phase_b_enrich — run inline by scan_folder, or
    // detached on its own connection by the command layer (I6c).
    Ok((result, PendingEnrich { imported, folder_id: Some(folder_id) }))
}

/// Scan `folder` recursively: Phase A (fast walk) followed immediately by Phase B
/// (metadata enrichment + finalizing), inline on the same connection. Convenience for
/// tests and callers that want the full result before returning. The command layer runs
/// the two phases separately so Phase B can be detached (see `run_blocking_scan`).
pub fn scan_folder(
    catalog: &Catalog,
    folder: &Path,
    abort: &AtomicBool,
    progress: &dyn Fn(ScanProgress),
) -> Result<ScanResult, String> {
    let (result, pending) = scan_folder_phase_a(catalog, folder, abort, progress)?;
    phase_b_enrich(catalog, pending, abort, progress)?;
    Ok(result)
}

/// Phase B of the two-phase live scan (I6c): extract EXIF/IPTC/XMP for the new/changed
/// files in `pending`, persist it in `COMMIT_EVERY` batches (flipping each row's
/// `metadata_ready` to 1 and recording external-editor sidecars), then run the finalizing
/// pass (auto-tags, RAW/JPEG stacking, missing reconciliation, mark scan finished).
///
/// Only new/changed files are extracted — a re-scan of an unchanged library does no
/// exiftool work at all. exiftool runs per batch (and across cores), not per file. The
/// persist loop runs in its own transaction (WAL rationale: release the write lock
/// periodically so grid reads keep flowing).
///
/// Honours the `abort` flag (the `Arc<AtomicBool>` shared with the command layer, I4b):
/// a catalog switch or a second scan flips it, and Phase B commits what's durable and
/// stops promptly so it never writes into a catalog that's being torn down. Emits
/// `scan:progress {phase:"metadata", done, total}` per commit and `{phase:"finalizing"}`
/// before the final pass.
pub fn phase_b_enrich(
    catalog: &Catalog,
    pending: PendingEnrich,
    abort: &AtomicBool,
    progress: &dyn Fn(ScanProgress),
) -> Result<(), String> {
    let PendingEnrich { imported, folder_id } = pending;

    let to_extract: Vec<PathBuf> = imported
        .iter()
        .filter(|(_, _, needs)| *needs)
        .map(|(p, _, _)| p.clone())
        .collect();
    let mut meta = extract_batch(&to_extract);
    let meta_total = to_extract.len();
    let mut done = 0usize;
    // Collect ids of photos whose metadata was just extracted — their GPS columns are now
    // populated, so the fence-apply step in the finalizing pass has fresh coordinates.
    // Gated on the `map` feature so there is no overhead when the feature is off.
    #[cfg(feature = "map")]
    let mut fenced_ids: Vec<i64> = Vec::new();
    let mut tx = catalog.begin().map_err(|e| e.to_string())?;
    let mut since_commit = 0usize;
    for (path, photo_id, needs) in imported {
        // Cancellation point (catalog switch / second scan, I4b): commit what's durable
        // and bail so nothing further is written into a catalog about to be torn down.
        if abort.load(Ordering::Relaxed) {
            tx.commit().map_err(|e| e.to_string())?;
            return Err(SCAN_ABORTED.to_string());
        }
        if needs {
            if let Some(m) = meta.remove(&path) {
                let _ = catalog.set_photo_metadata(photo_id, &m.promoted, &m.entries);
            }
            let _ = catalog.set_metadata_ready(photo_id, true);
            // I6d: clear the queue entry now that this photo is enriched.
            let _ = catalog.dequeue_pending_enrichment(photo_id);
            done += 1;
            // Record for fence-apply (after the commit so GPS columns are durable).
            #[cfg(feature = "map")]
            fenced_ids.push(photo_id);
        }
        // Detect external-editor sidecars (.xmp/.pp3/.arp) — read-only, never our own.
        // Run for every file (even unchanged): a user may have edited a RAW in darktable
        // since the last scan, adding a sidecar without touching the photo's bytes.
        let editors = sidecars::detect_external_editors_joined(&path);
        let _ = catalog.set_external_editors(photo_id, &editors);
        since_commit += 1;
        if since_commit >= COMMIT_EVERY {
            tx.commit().map_err(|e| e.to_string())?;
            tx = catalog.begin().map_err(|e| e.to_string())?;
            since_commit = 0;
            progress(ScanProgress::new("metadata", done, meta_total));
        }
    }
    tx.commit().map_err(|e| e.to_string())?;

    // Refresh auto-tags (e.g. monochrome) now that metadata is in place.
    progress(ScanProgress::new("finalizing", 0, 0));
    let _ = catalog.apply_auto_tags();

    // Apply geofence place-tags to every photo that had its metadata extracted in this
    // phase. These are the photos that may carry fresh GPS data. Best-effort — a fence
    // error never aborts the scan.
    #[cfg(feature = "map")]
    {
        use crate::plugins::map;
        if let Ok(()) = map::ensure_schema(catalog.conn()) {
            for photo_id in &fenced_ids {
                let _ = map::apply_fences_to_photo(catalog, *photo_id);
            }
        }
    }

    // Stack any newly-imported camera JPEGs under their sibling RAW (same folder + stem).
    let _ = catalog.pair_raw_jpeg_stacks();

    // Self-heal the `missing` flag: hide rows whose original can't be found and
    // have no backup (e.g. orphan rows left by a re-root), un-hide resolvable ones.
    let _ = catalog.reconcile_missing();

    // Mark the scan finished for the local scan (NAS in-place scans track no folder).
    if let Some(folder_id) = folder_id {
        catalog
            .mark_folder_scan_finished(folder_id)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Scan a folder that lives on a non-root volume (e.g. an existing archive on the NAS)
/// and index every supported image **in place** — no files are copied. Each photo is
/// recorded as living on that volume (so it shows as "On NAS"), gets a UUID + merge-safe
/// sidecar, and has its metadata extracted. This is the "initial scan of old NAS photos"
/// path; for the local library use [`scan_folder`]. READ-ONLY on the photo files (only a
/// `.xmp` sidecar is written alongside each, to carry the UUID).
pub fn scan_external_folder_phase_a(
    catalog: &Catalog,
    folder: &Path,
    abort: &AtomicBool,
    progress: &dyn Fn(ScanProgress),
) -> Result<(ScanResult, PendingEnrich), String> {
    if !folder.is_dir() {
        return Err(format!("Not a directory: {}", folder.display()));
    }

    // Phase A: fast walk — upsert path/uuid/mtime/size, mark new/changed rows as
    // metadata_ready=0, emit scan:progress {phase:"indexing"}, return import list.
    // The UUID sidecar binding (J5) happens inside upsert_external_one, so identity
    // exists on disk before Phase B begins — or its repair is queued in
    // pending_sidecar_identity. A NAS in-place scan tracks no folder row, so
    // there is no folder_id to mark finished (folder_id = None).
    let (result, imported, _newly_created) = phase_a_walk(
        catalog,
        folder,
        abort,
        progress,
        |path| upsert_external_one(catalog, path),
    )?;

    Ok((result, PendingEnrich { imported, folder_id: None }))
}

/// Scan a NAS-resident folder in place: Phase A (fast walk) followed immediately by
/// Phase B (metadata enrichment + finalizing), inline on the same connection. The command
/// layer runs the two phases separately so Phase B can be detached (see `run_blocking_scan`).
pub fn scan_external_folder(
    catalog: &Catalog,
    folder: &Path,
    abort: &AtomicBool,
    progress: &dyn Fn(ScanProgress),
) -> Result<ScanResult, String> {
    let (result, pending) = scan_external_folder_phase_a(catalog, folder, abort, progress)?;
    phase_b_enrich(catalog, pending, abort, progress)?;
    Ok(result)
}

/// Index one in-place (NAS-resident) photo, returning (photo_id, created, unchanged).
pub(crate) fn upsert_external_one(catalog: &Catalog, path: &Path) -> Result<(i64, bool, bool), String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    let size = meta.len() as i64;
    let sidecar_uuid = crate::xmp::read_identifier(path);
    let res = catalog
        .upsert_photo_on_volume(path, mtime_ns, size, sidecar_uuid.as_deref())
        .map_err(|e| e.to_string())?;
    // Same binding invariant as the local scan (see `upsert_one`): bind the identity to
    // the file, or queue a repair. A NAS scan is exactly where this matters — an archive
    // mounted read-only would otherwise index thousands of photos whose identity exists
    // in one SQLite file and nowhere else.
    if let Err(e) = catalog.ensure_sidecar_identity(res.id, path, &res.uuid, sidecar_uuid.as_deref())
    {
        eprintln!("nas-scan: couldn't queue identity repair for {}: {e}", path.display());
    }
    Ok((res.id, res.created, res.unchanged))
}

/// One file copied off a card, ready to be indexed. Carries the source's metadata
/// (extracted once, in the copy phase) so the index phase doesn't re-run exiftool.
pub struct CopiedItem {
    pub dest: PathBuf,
    pub meta: Option<crate::metadata::PhotoMetadata>,
}

/// Import from a card (ingest): copy each supported image from `source` into the local
/// `dest_base` under a `YYYY/MM/DD` tree (by capture date, falling back to file mtime),
/// keeping camera filenames, then index the copies, batch them, and auto-enqueue backup.
/// Unlike scan-in-place, this COPIES files. `dest_base` must be under the catalog root.
/// See docs/storage-and-import.md (Import). A destination collision with the same byte
/// size is treated as already-imported (skipped); a different size gets a " (n)" name.
///
/// This is a convenience wrapper over the two phases [`copy_from_card`] (heavy, no
/// catalog) and [`index_ingested`] (brief catalog work). The Tauri command calls the
/// phases separately so the long copy never holds the catalog lock — see
/// `ingest_from_card_cmd`.
pub fn ingest_from_card(
    catalog: &Catalog,
    source: &Path,
    dest_base: &Path,
    batch_label: Option<&str>,
) -> Result<ScanResult, String> {
    let (result, copied) = copy_from_card(source, dest_base, None, |_, _| {})?;
    index_ingested(catalog, dest_base, source, copied, batch_label, result)
}

/// One importable photo on a card, with whether it's already in the library.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CardPhoto {
    pub path: String,
    pub name: String,
    pub size: i64,
    pub capture_time: Option<String>,
    /// A same-size file already exists at this photo's computed destination.
    pub is_duplicate: bool,
}

/// List the importable photos on a card/source folder, flagging each as a duplicate when a
/// same-size file already exists at its computed date-tree destination under `dest_base`.
/// Filesystem + metadata only — no catalog access; call off the UI thread.
pub fn list_card_photos(source: &Path, dest_base: &Path) -> Result<Vec<CardPhoto>, String> {
    if !source.is_dir() {
        return Err(format!("Not a directory: {}", source.display()));
    }
    let sources: Vec<PathBuf> = WalkDir::new(source)
        .into_iter()
        .filter_entry(|e| !is_hidden(e.path()))
        .filter_map(|e| e.ok())
        .map(|e| e.path().to_path_buf())
        .filter(|p| p.is_file() && is_supported_image(p))
        .collect();
    let meta = extract_batch(&sources);
    let mut out: Vec<CardPhoto> = sources
        .iter()
        .map(|src| {
            let capture = meta.get(src).and_then(|m| m.promoted.capture_time.clone());
            let md = std::fs::metadata(src).ok();
            let size = md.as_ref().map(|m| m.len() as i64).unwrap_or(0);
            let mtime_secs = md
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let is_duplicate = src
                .file_name()
                .map(|f| {
                    dest_base
                        .join(date_subdir(capture.as_deref(), mtime_secs))
                        .join(f)
                })
                .map(|d| d.exists() && same_len(src, &d))
                .unwrap_or(false);
            CardPhoto {
                path: src.to_string_lossy().into_owned(),
                name: src
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                size,
                capture_time: capture,
                is_duplicate,
            }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Phase 1 of import: walk the card, extract metadata, and COPY files into the local
/// date tree. Touches the filesystem only — NO catalog access — so it can run on a
/// worker thread without holding the catalog lock (the copy is the slow part). Returns
/// the partial counts (scanned/skipped/errors) and the list of files actually copied.
///
/// `progress(done, total)` is called as each source file is processed, so the caller can
/// stream progress to the UI. Pass `|_, _| {}` if not needed.
pub fn copy_from_card(
    source: &Path,
    dest_base: &Path,
    selected: Option<&std::collections::HashSet<String>>,
    progress: impl Fn(usize, usize),
) -> Result<(ScanResult, Vec<CopiedItem>), String> {
    if !source.is_dir() {
        return Err(format!("Not a directory: {}", source.display()));
    }
    let mut sources: Vec<PathBuf> = WalkDir::new(source)
        .into_iter()
        .filter_entry(|e| !is_hidden(e.path()))
        .filter_map(|e| e.ok())
        .map(|e| e.path().to_path_buf())
        .filter(|p| p.is_file() && is_supported_image(p))
        .collect();
    // Restrict to the user's chosen files (by full path), when a selection was given.
    if let Some(sel) = selected {
        sources.retain(|p| sel.contains(p.to_string_lossy().as_ref()));
    }
    let total = sources.len();
    // One metadata pass on the source files — reused for the capture date AND indexing.
    let mut meta = extract_batch(&sources);

    let mut result = ScanResult::default();
    let mut copied: Vec<CopiedItem> = Vec::new();
    for src in &sources {
        result.scanned += 1;
        progress(result.scanned, total);
        // Take ownership of this file's metadata so it travels to the index phase.
        let m = meta.remove(src);
        let capture = m.as_ref().and_then(|m| m.promoted.capture_time.clone());
        let mtime_secs = std::fs::metadata(src)
            .ok()
            .and_then(|md| md.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let dir = dest_base.join(date_subdir(capture.as_deref(), mtime_secs));
        if std::fs::create_dir_all(&dir).is_err() {
            result.errors += 1;
            continue;
        }
        let Some(filename) = src.file_name() else {
            result.errors += 1;
            continue;
        };
        let mut dest = dir.join(filename);
        if dest.exists() {
            if same_len(src, &dest) {
                result.skipped += 1; // identical-size copy already present
                continue;
            }
            // Different content under the same name → keep both, or error if we somehow
            // can't find a free name (never overwrite).
            match unique_dest(&dest) {
                Some(p) => dest = p,
                None => {
                    result.errors += 1;
                    continue;
                }
            }
        }
        if std::fs::copy(src, &dest).is_err() {
            result.errors += 1;
            continue;
        }
        copied.push(CopiedItem { dest, meta: m });
    }
    Ok((result, copied))
}

/// Phase 2 of import: index the already-copied files into the catalog, batch them, and
/// auto-enqueue backups. This is the only phase that touches the catalog, and it's all
/// fast DB writes, so the lock is held only briefly. `result` carries the counts from
/// the copy phase; `source` is the fallback batch label.
pub fn index_ingested(
    catalog: &Catalog,
    dest_base: &Path,
    source: &Path,
    copied: Vec<CopiedItem>,
    batch_label: Option<&str>,
    mut result: ScanResult,
) -> Result<ScanResult, String> {
    let folder_id = catalog.add_folder(dest_base).map_err(|e| e.to_string())?;

    let mut newly_created: Vec<i64> = Vec::new();
    // K3: track the dest path for each newly-created photo so we can write the batch
    // UUID to its sidecar after the catalog transaction commits.
    let mut newly_created_paths: Vec<PathBuf> = Vec::new();
    let tx = catalog.begin().map_err(|e| e.to_string())?;
    for item in &copied {
        match upsert_one(catalog, &item.dest, folder_id) {
            Ok((photo_id, created, _)) => {
                result.imported += 1;
                if created {
                    result.created += 1;
                    newly_created.push(photo_id);
                    newly_created_paths.push(item.dest.clone());
                }
                // Reuse the source file's metadata for the copy (same bytes).
                if let Some(m) = &item.meta {
                    let _ = catalog.set_photo_metadata(photo_id, &m.promoted, &m.entries);
                }
            }
            Err(_) => result.errors += 1,
        }
    }

    // K3: resolve the batch UUID before committing the transaction so the row exists
    // when we look it up; the sidecar write happens after commit (filesystem write is
    // outside the catalog lock, matching the pattern in scan_folder).
    let batch_uuid_for_new: Option<String> = if !newly_created.is_empty() {
        // Use the custom import name if given, else the source folder path.
        let label = batch_label
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| source.to_string_lossy().to_string());
        let mut uuid_opt = None;
        if let Ok(batch_id) = catalog.create_import_batch(&label) {
            let _ = catalog.assign_photos_to_batch(batch_id, &newly_created);
            uuid_opt = catalog.get_import_batch_uuid(batch_id).ok().flatten();
        }
        for &photo_id in &newly_created {
            let _ = catalog.enqueue_operation("backup", photo_id);
        }
        uuid_opt
    } else {
        None
    };
    tx.commit().map_err(|e| e.to_string())?;

    // Write the batch UUID into the sidecar of each newly-created photo (K3).
    // Best-effort: a sidecar write failure never aborts the import.
    if let Some(ref batch_uuid) = batch_uuid_for_new {
        for path in &newly_created_paths {
            if let Err(e) = crate::xmp::write_import_batch(path, batch_uuid) {
                eprintln!("ingest: couldn't write ImportBatch sidecar for {}: {e}", path.display());
            }
        }
    }

    let _ = catalog.apply_auto_tags();

    // Apply geofence place-tags to each newly-imported photo (the import hook).
    // Only new photos: existing photos already had their chance at import time, and
    // re-applying on re-import would be noisy. Best-effort — never aborts the import.
    #[cfg(feature = "map")]
    {
        use crate::plugins::map;
        if let Ok(()) = map::ensure_schema(catalog.conn()) {
            for &photo_id in &newly_created {
                let _ = map::apply_fences_to_photo(catalog, photo_id);
            }
        }
    }

    Ok(result)
}

/// Index a single newly-created file (e.g. a collage saved into the library) into the
/// catalog: assign a UUID (+ merge-safe sidecar), extract its metadata, refresh auto-tags,
/// and queue a backup. Unlike a card import it makes no import batch — a generated collage
/// isn't a "negative film roll". The file must already live under the catalog root. Returns
/// the photo id.
pub fn index_generated_file(catalog: &Catalog, path: &Path) -> Result<i64, String> {
    if !path.is_file() {
        return Err(format!("Not a file: {}", path.display()));
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let folder_id = catalog.add_folder(parent).map_err(|e| e.to_string())?;
    let (photo_id, created, _) = upsert_one(catalog, path, folder_id)?;

    // A generated file has no EXIF capture date, which would sort it to the bottom of the
    // (date-ordered) library — stamp it with "now" so it appears as a recent photo.
    let now_iso = {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        chrono::DateTime::from_timestamp(secs, 0)
            .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S").to_string())
            .unwrap_or_default()
    };
    let mut meta = extract_batch(&[path.to_path_buf()]);
    match meta.remove(path) {
        Some(mut m) => {
            if m.promoted.capture_time.as_deref().unwrap_or("").is_empty() {
                m.promoted.capture_time = Some(now_iso);
            }
            let _ = catalog.set_photo_metadata(photo_id, &m.promoted, &m.entries);
        }
        None => {
            let promoted = crate::catalog::PromotedMetadata {
                capture_time: Some(now_iso),
                ..Default::default()
            };
            let _ = catalog.set_photo_metadata(photo_id, &promoted, &[]);
        }
    }
    if created {
        let _ = catalog.enqueue_operation("backup", photo_id);
    }
    let _ = catalog.apply_auto_tags();
    Ok(photo_id)
}

/// `YYYY/MM/DD` from a capture timestamp (`YYYY-MM-DDThh:mm:ss`), else from the file's
/// mtime (seconds since epoch).
fn date_subdir(capture: Option<&str>, mtime_secs: i64) -> String {
    if let Some(c) = capture {
        if c.len() >= 10 {
            return c[..10].replace('-', "/");
        }
    }
    chrono::DateTime::from_timestamp(mtime_secs, 0)
        .map(|dt| dt.format("%Y/%m/%d").to_string())
        .unwrap_or_else(|| "unknown-date".into())
}

fn same_len(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.len() == b.len(),
        _ => false,
    }
}

/// A destination path that doesn't exist yet (`name (2).ext`, …), or `None` if no free
/// name was found — the caller must then NOT copy (never overwrite an existing file).
fn unique_dest(path: &Path) -> Option<PathBuf> {
    if !path.exists() {
        return Some(path.to_path_buf());
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = path.extension().and_then(|s| s.to_str());
    for n in 2..10_000 {
        let mut name = format!("{stem} ({n})");
        if let Some(ext) = ext {
            name.push('.');
            name.push_str(ext);
        }
        let candidate = dir.join(name);
        if !candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Upsert one photo, returning (photo_id, created, unchanged). `unchanged` is true when
/// the row already existed and its file stats (mtime + size) were identical — the caller
/// can then skip re-extracting metadata for it on a re-scan.
fn upsert_one(catalog: &Catalog, path: &Path, folder_id: i64) -> Result<(i64, bool, bool), String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    let size = meta.len() as i64;

    // Read the file's own UUID from its sidecar (if any) so a moved/re-rooted file is
    // matched to its existing row by UUID rather than duplicated.
    let sidecar_uuid = crate::xmp::read_identifier(path);
    let res = catalog
        .upsert_photo_with_identity(path, Some(folder_id), mtime_ns, size, sidecar_uuid.as_deref())
        .map_err(|e| e.to_string())?;

    // Honor the binding invariant: the file's sidecar must carry this photo's UUID so
    // future moves/re-roots can match by it. Writes only when the sidecar lacks it (don't
    // rewrite on every scan, never clobber a UUID already on disk); when the write can't
    // happen, the debt is queued for `repair_pending_identity` rather than logged and
    // forgotten. A sidecar failure still never aborts the scan — one unwritable file must
    // not cost the user the other 99,999 rows.
    if let Err(e) = catalog.ensure_sidecar_identity(res.id, path, &res.uuid, sidecar_uuid.as_deref())
    {
        eprintln!("scan: couldn't queue identity repair for {}: {e}", path.display());
    }
    Ok((res.id, res.created, res.unchanged))
}

/// A 0-byte file — an empty/corrupt placeholder (e.g. a long-ago bad sync). Skipped on
/// scan so it never enters the catalog as an unusable photo.
fn is_empty_file(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.len() == 0).unwrap_or(false)
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with('.') && n.len() > 1)
        .unwrap_or(false)
}

/// Merge stale `pending_enrichment` rows into a Phase A hand-off (I6d).
///
/// Invariant: a stale entry is **never** duplicated into the result when Phase A already
/// re-enqueued the same photo with `needs_extract = true`. Only entries where Phase B
/// will actually run extraction are considered "covered" — entries with
/// `needs_extract = false` (unchanged files that Phase B will skip) are *not* covered,
/// so a stale row for the same `photo_id` is still merged in to prevent that photo
/// from being stuck at `metadata_ready = 0` indefinitely.
///
/// `phase_a` is the `(path, photo_id, needs_extract)` list produced by [`phase_a_walk`].
/// `stale` is the result of [`crate::catalog::Catalog::load_pending_enrichment`], which
/// has already resolved each photo's path and silently dropped entries whose file is
/// unreachable (e.g. deleted photos). The merged list is returned; the caller appends it
/// to `PendingEnrich::imported` and passes the whole thing to [`phase_b_enrich`].
pub fn merge_stale_pending(
    phase_a: Vec<(PathBuf, i64, bool)>,
    stale: Vec<(PathBuf, i64, bool)>,
) -> Vec<(PathBuf, i64, bool)> {
    let already_covered: std::collections::HashSet<i64> = phase_a
        .iter()
        .filter(|(_, _, needs)| *needs)
        .map(|(_, id, _)| *id)
        .collect();
    let mut result = phase_a;
    for entry in stale {
        if !already_covered.contains(&entry.1) {
            result.push(entry);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rrdata_sidecar_is_not_a_supported_image() {
        // RapidRAW drops a `<filename>.rrdata` JSON sidecar next to the source. The scan walk
        // filters on `is_supported_image`, so an `.rrdata` file must never be indexed as a
        // photo (nor its double-extension `.ARW.rrdata` form).
        assert!(!is_supported_image(Path::new("DSC1.ARW.rrdata")));
        assert!(!is_supported_image(Path::new("DSC1.rrdata")));
        // Sanity: the RAW it sits next to *is* supported.
        assert!(is_supported_image(Path::new("DSC1.ARW")));
    }
}
