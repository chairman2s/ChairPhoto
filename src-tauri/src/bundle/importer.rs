//! Bundle importer (F1d): unpack a `.chairphoto` bundle, copy originals + sidecars
//! into the local library, index them via the UUID-aware identity upsert, run the
//! F1c additive merge, and auto-enqueue backup for every new photo.
//!
//! ## Phases
//!
//! 1. **Parse** — open the zip, read `manifest.json`, validate `format_version`.
//! 2. **Copy** (off the catalog lock, slow) — extract each `originals/<relative_path>`
//!    into `<root>/YYYY/MM/DD/<filename>`, using E5's collision rules:
//!    - Same-size file at the destination → already imported, skip.
//!    - Different-size collision → rename with ` (n)` suffix (never overwrite).
//!    Sidecars (`<entry>.xmp`) are extracted beside their original.
//!    Progress events (`import:progress {done, total}`) stream the copy phase.
//! 3. **Index** (on a secondary connection, off the main catalog lock) — call
//!    `upsert_photo_with_identity` for each copied file (UUID from the sidecar
//!    prevents duplicates), then run the F1c `merge_bundle` for metadata/taxonomy/
//!    ratings/tags, auto-enqueue backup for new photos via E4, write K3 batch UUID
//!    sidecars, and call `reconcile_missing`.
//!
//! **Neither the copy phase nor the index phase holds the main catalog lock** (same
//! pattern as E5 `ingest_from_card` / `run_blocking_scan`). The index phase runs on
//! a `Catalog::open_secondary` connection inside `spawn_blocking`.

use std::io::Read;
use std::path::{Path, PathBuf};

use zip::ZipArchive;

use crate::bundle::{BundleManifest, BUNDLE_FORMAT_VERSION, MANIFEST_FILENAME, ORIGINALS_DIR};
use crate::catalog::{Catalog, MergeSummary};

// ---------------------------------------------------------------------------
// Result type
// ---------------------------------------------------------------------------

/// Summary returned by `import_bundle_cmd` after a successful import.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleImportResult {
    /// Photos copied from the bundle originals (new to the filesystem).
    pub copied: usize,
    /// Originals skipped because a same-size file already existed at the destination.
    pub skipped_duplicate: usize,
    /// Originals that encountered a non-fatal error during extraction (metadata-only).
    pub errors: usize,
    /// What the F1c merge did (new photos, new tags, etc.).
    pub merge: MergeSummary,
}

// ---------------------------------------------------------------------------
// Phase 1 — parse the bundle zip (no catalog access, cheap)
// ---------------------------------------------------------------------------

/// Open `bundle_path`, parse its manifest, and return the parsed manifest + the
/// archive file handle (for the copy phase). Fails if the format_version is
/// unsupported, so the importer never silently accepts a bundle it can't handle.
///
/// This is a pure filesystem open; it does not hold the catalog lock.
pub fn open_bundle(bundle_path: &Path) -> Result<(BundleManifest, ZipArchive<std::fs::File>), String> {
    let file = std::fs::File::open(bundle_path)
        .map_err(|e| format!("open bundle: {e}"))?;
    let mut archive = ZipArchive::new(file)
        .map_err(|e| format!("read zip: {e}"))?;

    // Read manifest.json
    let manifest_json = {
        let mut entry = archive
            .by_name(MANIFEST_FILENAME)
            .map_err(|_| "bundle is missing manifest.json — not a valid bundle".to_string())?;
        let mut s = String::new();
        entry.read_to_string(&mut s).map_err(|e| format!("read manifest: {e}"))?;
        s
    };

    let manifest = BundleManifest::from_json(&manifest_json)
        .map_err(|e| format!("parse manifest: {e}"))?;

    if manifest.format_version != BUNDLE_FORMAT_VERSION {
        return Err(format!(
            "unsupported bundle format version {} (this build supports {})",
            manifest.format_version, BUNDLE_FORMAT_VERSION
        ));
    }

    Ok((manifest, archive))
}

// ---------------------------------------------------------------------------
// Phase 2 — copy originals off the catalog lock
// ---------------------------------------------------------------------------

/// One original extracted from the bundle and placed on disk, ready for indexing.
pub struct ExtractedItem {
    /// The absolute path where the original now lives on the local filesystem.
    pub dest: PathBuf,
    /// The photo UUID from the bundle manifest (for the identity upsert).
    pub photo_uuid: String,
    /// The catalog-root-relative logical path from the manifest (for the date tree).
    pub relative_path: String,
}

/// Extract originals from `archive` into `dest_base` under a `YYYY/MM/DD` tree,
/// mirroring the `ORIGINALS_DIR/<relative_path>` archive entries. Sidecars
/// (`<entry>.xmp`) are extracted beside their original.
///
/// Collision rules (E5 parity):
/// - Same-size file already at the destination → `skipped_duplicate` (no write).
/// - Different-size collision → ` (n)` suffix (never overwrite).
///
/// `on_progress(done, total)` is called once per original (including skipped/error)
/// so the caller can stream `import:progress` events.
pub fn extract_originals(
    manifest: &BundleManifest,
    archive: &mut ZipArchive<std::fs::File>,
    dest_base: &Path,
    on_progress: impl Fn(usize, usize),
) -> Result<(Vec<ExtractedItem>, BundleImportResult), String> {
    let total = manifest.photos.len();
    let mut result = BundleImportResult {
        copied: 0,
        skipped_duplicate: 0,
        errors: 0,
        merge: MergeSummary::default(),
    };
    let mut extracted: Vec<ExtractedItem> = Vec::new();

    for (i, bp) in manifest.photos.iter().enumerate() {
        on_progress(i + 1, total);

        let arc_orig = format!("{}/{}", ORIGINALS_DIR, bp.relative_path);

        // Get the source bytes from the zip (original may be absent for metadata-only
        // bundles — offline originals). If missing, skip gracefully (metadata will
        // still land via the F1c merge).
        let orig_bytes = match archive.by_name(&arc_orig) {
            Ok(mut entry) => {
                let mut buf = Vec::new();
                match entry.read_to_end(&mut buf) {
                    Ok(_) => Some(buf),
                    Err(e) => {
                        eprintln!(
                            "bundle import: failed to read original for {} ({}): {e}",
                            bp.uuid, bp.relative_path
                        );
                        result.errors += 1;
                        None
                    }
                }
            }
            Err(_) => {
                // Original absent (metadata-only bundle or offline original).
                None
            }
        };

        let Some(orig_bytes) = orig_bytes else {
            // Nothing to copy — F1c merge will still apply the metadata.
            continue;
        };

        // Determine the destination path from the logical relative path.
        // The relative_path is already `YYYY/MM/DD/filename` from the manifest.
        let relative_path = Path::new(&bp.relative_path);
        let filename = match relative_path.file_name() {
            Some(f) => f,
            None => {
                eprintln!(
                    "bundle import: invalid relative path for {}: {}",
                    bp.uuid, bp.relative_path
                );
                result.errors += 1;
                continue;
            }
        };

        // Date subdirectory from the manifest relative path (e.g. "2026/06/28/DSC01234.ARW"
        // → "2026/06/28"). If the path has no parent, fall back to "unknown-date".
        let date_subdir = relative_path
            .parent()
            .and_then(|p| if p == Path::new("") { None } else { Some(p) })
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown-date".to_string());

        let dir = dest_base.join(&date_subdir);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!(
                "bundle import: create dir {} failed: {e}", dir.display()
            );
            result.errors += 1;
            continue;
        }

        let mut dest = dir.join(filename);

        // Collision rules (E5 parity): same-size → skip; different-size → rename.
        if dest.exists() {
            let dest_size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(u64::MAX);
            if dest_size == orig_bytes.len() as u64 {
                // Same-size → already imported, skip the copy.
                result.skipped_duplicate += 1;
                // Put the UUID in the sidecar so the upsert can match by it. The file was
                // here before this import, so it may already carry a *different*
                // identity — `bind_sidecar_identity` leaves that one alone rather than
                // overwriting another photo's identity on the strength of a same-size
                // filename collision. This phase holds no catalog connection, so an
                // unbound identity is only reported here; index_bundle records it durably.
                let found = crate::xmp::read_identifier(&dest);
                let outcome = crate::catalog::bind_sidecar_identity(
                    &dest,
                    &bp.uuid,
                    found.as_deref(),
                );
                if outcome != crate::catalog::SidecarIdentity::Bound {
                    eprintln!(
                        "bundle import: identity not bound for {} ({outcome:?}) — queued for repair",
                        dest.display()
                    );
                }
                // Still record as extracted so the indexer can upsert its location.
                extracted.push(ExtractedItem {
                    dest,
                    photo_uuid: bp.uuid.clone(),
                    relative_path: bp.relative_path.clone(),
                });
                continue;
            }
            // Different-size collision — rename.
            match unique_dest(&dest) {
                Some(p) => dest = p,
                None => {
                    eprintln!(
                        "bundle import: couldn't find a free name for {} ({}) — skipped",
                        bp.uuid, dest.display()
                    );
                    result.errors += 1;
                    continue;
                }
            }
        }

        if let Err(e) = std::fs::write(&dest, &orig_bytes) {
            eprintln!(
                "bundle import: write {} failed: {e}", dest.display()
            );
            result.errors += 1;
            continue;
        }
        result.copied += 1;

        // Sidecar handling — the UUID must land in the sidecar beside the original so
        // that index_bundle's `upsert_photo_with_identity` (and future re-scans) can
        // match this photo by UUID instead of minting a duplicate.
        //
        // Strategy:
        // 1. If the bundle carries a sidecar for this original, extract it as-is (it
        //    already contains `xmp:Identifier`).
        // 2. Otherwise, write a fresh merge-safe UUID sidecar from the bundle UUID now,
        //    before the index phase runs — this is the binding invariant (AGENTS.md).
        let sidecar_dest = {
            let mut s = dest.as_os_str().to_os_string();
            s.push(".xmp");
            PathBuf::from(s)
        };
        let arc_sidecar = format!("{}.xmp", arc_orig);
        let bundle_sidecar_extracted = if let Ok(mut entry) = archive.by_name(&arc_sidecar) {
            let mut sidecar_bytes = Vec::new();
            if entry.read_to_end(&mut sidecar_bytes).is_ok() {
                if let Err(e) = std::fs::write(&sidecar_dest, &sidecar_bytes) {
                    eprintln!(
                        "bundle import: sidecar write {} failed (non-fatal): {e}",
                        sidecar_dest.display()
                    );
                    false
                } else {
                    true
                }
            } else {
                false
            }
        } else {
            false
        };
        // If no bundle sidecar landed, write a bare UUID sidecar so the identity is
        // in place for the index phase. This satisfies the AGENTS.md invariant: every
        // catalogued photo's XMP sidecar carries its UUID. A failure here is not the
        // end of it — the photo has no catalog row yet, so index_bundle is the one that
        // records the repair once the row exists.
        if !bundle_sidecar_extracted && !sidecar_dest.exists() {
            if let Err(e) = crate::xmp::write_identifier(&dest, &bp.uuid) {
                eprintln!(
                    "bundle import: couldn't write UUID sidecar for {} — queued for repair: {e}",
                    dest.display()
                );
            }
        }

        extracted.push(ExtractedItem {
            dest,
            photo_uuid: bp.uuid.clone(),
            relative_path: bp.relative_path.clone(),
        });
    }

    Ok((extracted, result))
}

// ---------------------------------------------------------------------------
// Phase 3 — index (catalog lock, fast DB work)
// ---------------------------------------------------------------------------

/// Index the extracted originals into the catalog and run the F1c merge.
///
/// **Call site requirement**: `catalog` must be a *secondary* connection opened with
/// `Catalog::open_secondary`, running inside `spawn_blocking`. The caller must NOT hold
/// the main catalog `Mutex` across this call — it performs file I/O (XMP reads/writes,
/// `reconcile_missing` stat checks) in addition to DB work. See `import_bundle_cmd` in
/// `commands.rs` for the correct pattern, which mirrors `run_blocking_scan`.
///
/// ## Flow
///
/// **Step A — Upsert** (fast DB writes): for each extracted file, call
/// `upsert_photo_with_identity` with the UUID from the sidecar written by
/// `extract_originals`. A brand-new photo is created with the bundle's UUID; an
/// already-present photo (same UUID) is merely updated in place. For newly-created
/// photos the bundle's rating/label/pick/IPTC/edit-record/versions are applied now
/// (the same pattern as E5's `index_ingested` + `set_photo_metadata`).
///
/// **Step B — F1c merge**: run `merge_bundle` for the taxonomy, import batch, and
/// tag assignments. Since the upsert already placed photos that were freshly extracted,
/// the merge sees them as "existing" (never overwrites) and only unions assignments —
/// the correct additive behaviour. Photos that had no originals in the bundle (metadata-
/// only / offline originals) are inserted by the merge if not already present.
///
/// **Step C — Post-index** (file I/O): write the batch UUID sidecar (K3) per file,
/// apply auto-tags, pair RAW+JPEG stacks, and run `reconcile_missing` (O(n) stat
/// checks). These happen on the secondary connection so the main mutex stays free.
///
/// `dest_base` must be a path under the catalog root (same as E5's requirement).
pub fn index_bundle(
    catalog: &Catalog,
    manifest: &BundleManifest,
    extracted: &[ExtractedItem],
    dest_base: &Path,
    mut partial_result: BundleImportResult,
) -> Result<BundleImportResult, String> {
    use crate::catalog::PickState;

    let folder_id = catalog.add_folder(dest_base).map_err(|e| e.to_string())?;

    // Build a lookup table uuid → BundlePhoto for fast access during the upsert loop.
    let bp_by_uuid: std::collections::HashMap<&str, &crate::bundle::BundlePhoto> = manifest
        .photos
        .iter()
        .map(|bp| (bp.uuid.as_str(), bp))
        .collect();

    let mut newly_created: Vec<i64> = Vec::new();

    let tx = catalog.begin().map_err(|e| e.to_string())?;
    for item in extracted {
        let path = &item.dest;
        let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        let size = meta.len() as i64;

        // extract_originals normally leaves a UUID sidecar beside this file (either from
        // the bundle or written from the bundle's photo UUID). Reading it here is the
        // critical UUID-match step that prevents re-import duplicates.
        let sidecar_uuid = crate::xmp::read_identifier(path);

        // The sidecar wins when it has an identity — it describes the file that is
        // actually on disk, which for a same-size collision need not be the bundle's
        // photo. When it has none (the copy phase's write failed, or the existing
        // sidecar doesn't parse), fall back to the manifest: this photo's identity is
        // known, so minting a fresh UUID here would be inventing a second one for it
        // and leaving `merge_bundle` to insert the bundle's photo a second time.
        let identity = sidecar_uuid.as_deref().unwrap_or(item.photo_uuid.as_str());

        let upsert = match catalog.upsert_photo_with_identity(
            path,
            Some(folder_id),
            mtime_ns,
            size,
            Some(identity),
        ) {
            Ok(u) => u,
            Err(e) => {
                eprintln!("bundle import: upsert failed for {}: {e}", path.display());
                partial_result.errors += 1;
                continue;
            }
        };

        // Bind the identity to the file, or queue a repair (AGENTS.md, "Photo identity").
        // This runs for EVERY extracted photo, not only newly-created ones: an already-
        // catalogued photo whose sidecar lost its identity owes the same debt, and the
        // copy phase's sidecar write is best-effort — this is where its failure is caught.
        if let Err(e) =
            catalog.ensure_sidecar_identity(upsert.id, path, &upsert.uuid, sidecar_uuid.as_deref())
        {
            eprintln!(
                "bundle import: couldn't queue identity repair for {}: {e}",
                path.display()
            );
        }

        if upsert.created {
            newly_created.push(upsert.id);

            // Apply the bundle's non-destructive state for this brand-new photo.
            // The F1c merge won't do this because the upsert already made the photo
            // "existing" (merge is additive; it never overwrites an existing row).
            // This mirrors E5's set_photo_metadata call after upsert.
            if let Some(bp) = bp_by_uuid.get(item.photo_uuid.as_str()) {
                // Rating / label / pick (non-zero or non-default only — zero/empty are
                // already the column defaults from the INSERT in upsert_photo_with_identity).
                if bp.rating != 0 || !bp.label.is_empty() || bp.pick_state != PickState::None {
                    let _ = catalog.set_culling(
                        upsert.id,
                        Some(bp.rating),
                        Some(&bp.label),
                        Some(bp.pick_state),
                    );
                }

                // IPTC fields (any non-default value).
                let iptc = &bp.iptc;
                let has_iptc = !iptc.description.is_empty()
                    || !iptc.headline.is_empty()
                    || !iptc.title.is_empty()
                    || !iptc.creator.is_empty()
                    || !iptc.copyright.is_empty()
                    || !iptc.credit.is_empty()
                    || !iptc.source.is_empty()
                    || !iptc.city.is_empty()
                    || !iptc.state.is_empty()
                    || !iptc.country.is_empty()
                    || !iptc.country_code.is_empty();
                if has_iptc {
                    let _ = catalog.set_iptc(upsert.id, iptc);
                }

                // Edit record (opaque JSON for the editing module).
                if let Some(edit_json) = &bp.edit_record {
                    let trimmed = edit_json.trim();
                    if !trimmed.is_empty() {
                        let _ = catalog.set_edit_record(upsert.id, trimmed);
                    }
                }

                // Named versions, in bundle order.
                for v in &bp.versions {
                    if let Ok(vid) = catalog.create_version(upsert.id, &v.name) {
                        let edit_json = {
                            let t = v.edit_json.trim();
                            if t.is_empty() { "{}" } else { t }
                        };
                        let _ = catalog.set_version_edit(vid, edit_json);
                    }
                }
            }
        }
    }

    // Auto-enqueue backup for newly-created photos (E4). Failures are best-effort.
    for &photo_id in &newly_created {
        let _ = catalog.enqueue_operation("backup", photo_id);
    }

    tx.commit().map_err(|e| e.to_string())?;

    // Step B — F1c merge: apply taxonomy, import batch, tag assignments — additive.
    // Photos with originals in the bundle are already "existing" after the upsert above;
    // the merge only inserts metadata-only photos (no original in bundle) if not present,
    // and unions tag assignments for all.
    let merge_summary = catalog
        .merge_bundle(manifest)
        .map_err(|e| e.to_string())?;

    // Step B.1 — Batch assignment for newly-upserted photos.
    //
    // `upsert_photo_with_identity` creates photos with `import_batch_id = NULL` (it
    // doesn't know the batch at upsert time). `merge_bundle` has now ensured the bundle's
    // batch exists in this catalog. Look up its id and assign it to every photo that was
    // freshly created by this import — mirrors E5's `assign_photos_to_batch` call in
    // `ingest_from_card`.
    if !newly_created.is_empty() {
        if let Ok(Some(batch_id)) = catalog.get_import_batch_id_by_uuid(&manifest.batch.uuid) {
            let _ = catalog.assign_photos_to_batch(batch_id, &newly_created);
        }
    }

    // Step C — K3: write the bundle batch UUID into each extracted file's sidecar so
    // the batch survives catalog loss/merge across machines. `write_import_batch` is
    // merge-safe (preserves foreign XMP elements). Best-effort.
    let batch_uuid = &manifest.batch.uuid;
    for item in extracted {
        if let Err(e) = crate::xmp::write_import_batch(&item.dest, batch_uuid) {
            eprintln!(
                "bundle import: couldn't write ImportBatch sidecar for {}: {e}",
                item.dest.display()
            );
        }
    }

    // Apply auto-tags (monochrome, long-exposure, etc.) and pair RAW+JPEG stacks.
    let _ = catalog.apply_auto_tags();
    let _ = catalog.pair_raw_jpeg_stacks();
    let _ = catalog.reconcile_missing();

    partial_result.merge = merge_summary;
    Ok(partial_result)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A destination path that doesn't exist yet (`name (2).ext`, …), or `None` if no
/// free name was found. Never overwrites — the caller must not copy on `None`.
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::writer::{write_bundle, GatheredBundle};
    use crate::bundle::{BundleBatch, BundleManifest};
    use crate::catalog::Catalog;
    use std::collections::HashMap;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("chairphoto-import-test-{tag}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn temp_catalog(tag: &str) -> (Catalog, PathBuf) {
        let dir = temp_dir(tag);
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let catalog = Catalog::open(&dir.join("test.chairphoto"), &root).unwrap();
        (catalog, root)
    }

    /// Build a minimal bundle zip with one "original" file (fake bytes) and write it
    /// to a temp path.
    fn make_test_bundle(tag: &str, photo_uuid: &str, relative_path: &str) -> PathBuf {
        use crate::bundle::BundlePhoto;
        use crate::catalog::{IptcFields, PickState};

        let dir = temp_dir(&format!("{tag}-bundle"));

        // Create the fake original file so the writer can copy it.
        let orig_dir = dir.join("orig");
        std::fs::create_dir_all(&orig_dir).unwrap();
        let orig_path = orig_dir.join("DSC01234.ARW");
        std::fs::write(&orig_path, b"FAKE RAW BYTES").unwrap();

        let mut manifest = BundleManifest::new(
            BundleBatch {
                uuid: format!("batch-{tag}"),
                source_label: "Test batch".into(),
                note: String::new(),
                created_at: 1_700_000_000,
            },
            1_700_100_000,
        );
        manifest.photos.push(BundlePhoto {
            uuid: photo_uuid.to_string(),
            relative_path: relative_path.to_string(),
            rating: 3,
            label: "green".into(),
            pick_state: PickState::Pick,
            iptc: IptcFields { headline: "Test sunset".into(), ..Default::default() },
            edit_record: None,
            versions: Vec::new(),
            tag_uuids: Vec::new(),
        });

        let mut originals = HashMap::new();
        originals.insert(photo_uuid.to_string(), Some(orig_path));

        let bundle = GatheredBundle { manifest, originals };
        let dest = dir.join("test.chairphoto");
        write_bundle(&bundle, &dest, |_, _| {}).expect("write_bundle");
        dest
    }

    #[test]
    fn open_bundle_parses_manifest() {
        let bundle_path = make_test_bundle("open", "uuid-open-1", "2026/06/28/DSC01234.ARW");
        let (manifest, _archive) = open_bundle(&bundle_path).expect("open_bundle");
        assert_eq!(manifest.format_version, BUNDLE_FORMAT_VERSION);
        assert_eq!(manifest.batch.uuid, "batch-open");
        assert_eq!(manifest.photos.len(), 1);
        assert_eq!(manifest.photos[0].uuid, "uuid-open-1");
    }

    #[test]
    fn extract_originals_copies_file_to_date_tree() {
        let bundle_path =
            make_test_bundle("extract", "uuid-extract-1", "2026/06/28/DSC01234.ARW");
        let (manifest, mut archive) = open_bundle(&bundle_path).expect("open_bundle");

        let dest_base = temp_dir("extract-dest");
        let (extracted, partial) =
            extract_originals(&manifest, &mut archive, &dest_base, |_, _| {})
                .expect("extract_originals");

        assert_eq!(partial.copied, 1);
        assert_eq!(partial.skipped_duplicate, 0);
        assert_eq!(partial.errors, 0);
        assert_eq!(extracted.len(), 1);

        // The file must land under YYYY/MM/DD/.
        let expected = dest_base.join("2026").join("06").join("28").join("DSC01234.ARW");
        assert!(expected.exists(), "expected file at {}", expected.display());
        assert_eq!(std::fs::read(&expected).unwrap(), b"FAKE RAW BYTES");
    }

    #[test]
    fn extract_originals_skips_duplicate_same_size() {
        let bundle_path = make_test_bundle("dup", "uuid-dup-1", "2026/06/28/DSC01234.ARW");
        let (manifest, mut archive) = open_bundle(&bundle_path).expect("open_bundle");

        let dest_base = temp_dir("dup-dest");
        // Pre-place a file of the same size.
        let date_dir = dest_base.join("2026").join("06").join("28");
        std::fs::create_dir_all(&date_dir).unwrap();
        std::fs::write(date_dir.join("DSC01234.ARW"), b"FAKE RAW BYTES").unwrap();

        let (extracted, partial) =
            extract_originals(&manifest, &mut archive, &dest_base, |_, _| {})
                .expect("extract_originals");

        assert_eq!(partial.skipped_duplicate, 1, "same-size must be skipped");
        assert_eq!(partial.copied, 0);
        assert_eq!(partial.errors, 0);
        // The item is still recorded so the indexer can upsert its location.
        assert_eq!(extracted.len(), 1);
    }

    #[test]
    fn extract_originals_renames_on_size_collision() {
        let bundle_path =
            make_test_bundle("rename", "uuid-rename-1", "2026/06/28/DSC01234.ARW");
        let (manifest, mut archive) = open_bundle(&bundle_path).expect("open_bundle");

        let dest_base = temp_dir("rename-dest");
        // Pre-place a file with DIFFERENT content at the same path.
        let date_dir = dest_base.join("2026").join("06").join("28");
        std::fs::create_dir_all(&date_dir).unwrap();
        std::fs::write(date_dir.join("DSC01234.ARW"), b"DIFFERENT BYTES").unwrap();

        let (extracted, partial) =
            extract_originals(&manifest, &mut archive, &dest_base, |_, _| {})
                .expect("extract_originals");

        assert_eq!(partial.copied, 1, "different-size must copy with rename");
        assert_eq!(partial.skipped_duplicate, 0);
        // The renamed file must exist.
        let renamed = date_dir.join("DSC01234 (2).ARW");
        assert!(renamed.exists(), "renamed file must exist at {}", renamed.display());
        assert_eq!(extracted[0].dest, renamed);
    }

    #[test]
    fn index_bundle_creates_photo_and_merges_metadata() {
        let bundle_path =
            make_test_bundle("index", "uuid-index-1", "2026/06/28/DSC01234.ARW");
        let (manifest, mut archive) = open_bundle(&bundle_path).expect("open_bundle");

        let (catalog, root) = temp_catalog("index");
        let dest_base = root.clone();

        let (extracted, partial) =
            extract_originals(&manifest, &mut archive, &dest_base, |_, _| {})
                .expect("extract_originals");

        let result = index_bundle(&catalog, &manifest, &extracted, &dest_base, partial)
            .expect("index_bundle");

        // The copy phase copied one file with no errors.
        assert_eq!(result.copied, 1);
        assert_eq!(result.errors, 0);

        // The F1c merge adds the batch and sees the photo as existing (the upsert phase
        // already placed it via upsert_photo_with_identity, so merge sees photos_existing=1
        // and photos_added=0 — correct additive behaviour).
        assert!(result.merge.batch_added, "batch must be recorded");
        assert_eq!(result.merge.photos_existing, 1, "upsert pre-placed the photo");
        assert_eq!(result.merge.photos_added, 0, "merge should not duplicate the photo");

        // The photo is in the catalog with the bundle's state (rating applied by merge).
        let photo = catalog.get_photo_by_uuid("uuid-index-1").unwrap();
        assert_eq!(photo.rating, 3);
        let iptc = catalog.get_iptc(photo.id).unwrap();
        assert_eq!(iptc.headline, "Test sunset");
    }

    #[test]
    fn re_import_is_idempotent() {
        let bundle_path =
            make_test_bundle("idem", "uuid-idem-1", "2026/06/28/DSC01234.ARW");
        let (manifest, mut archive) = open_bundle(&bundle_path).expect("open_bundle");
        let (catalog, root) = temp_catalog("idem");

        // First import.
        let (extracted, partial) =
            extract_originals(&manifest, &mut archive, &root, |_, _| {})
                .expect("first extract");
        index_bundle(&catalog, &manifest, &extracted, &root, partial)
            .expect("first index");

        // Re-open the bundle for the second import.
        let (manifest2, mut archive2) = open_bundle(&bundle_path).expect("open_bundle 2");
        let (extracted2, partial2) =
            extract_originals(&manifest2, &mut archive2, &root, |_, _| {})
                .expect("second extract");

        // The file already exists with the same size → skipped.
        assert_eq!(partial2.skipped_duplicate, 1);
        assert_eq!(partial2.copied, 0);

        let result2 = index_bundle(&catalog, &manifest2, &extracted2, &root, partial2)
            .expect("second index");

        // No new photos/batches/tags from the re-import.
        assert_eq!(result2.merge.photos_added, 0);
        assert!(!result2.merge.batch_added);
        assert_eq!(result2.merge.tags_created, 0);

        // Still exactly one photo in the catalog.
        let count: i64 = catalog
            .conn()
            .query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn progress_called_for_each_photo() {
        let bundle_path =
            make_test_bundle("prog", "uuid-prog-1", "2026/06/28/DSC01234.ARW");
        let (manifest, mut archive) = open_bundle(&bundle_path).expect("open_bundle");

        let dest = temp_dir("prog-dest");
        let calls = std::sync::Mutex::new(Vec::<(usize, usize)>::new());
        extract_originals(&manifest, &mut archive, &dest, |done, total| {
            calls.lock().unwrap().push((done, total));
        })
        .expect("extract");

        let calls = calls.into_inner().unwrap();
        // 1 photo → 1 progress call.
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], (1, 1));
    }
}
