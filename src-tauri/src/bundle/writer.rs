//! Bundle writer (F1b): serialize one import batch into a `.chairphoto` bundle zip.
//!
//! The zip layout is defined in `bundle/mod.rs` (F1a):
//!
//! ```text
//! <name>.chairphoto  (zip)
//! ├── manifest.json
//! ├── originals/<relative_path>          ← RAW/JPEG original
//! ├── originals/<relative_path>.xmp      ← XMP sidecar (if present), beside original
//! └── previews/<uuid>.jpg                ← cached JPEG preview (best-effort)
//! ```
//!
//! All DB work happens under the catalog lock in [`gather_bundle`]; the zip write
//! (file I/O) runs off the lock so it can be handed to `spawn_blocking`.

use std::collections::HashMap;
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::params;
use zip::write::{FileOptions, ZipWriter};
use zip::CompressionMethod;

use crate::bundle::{
    BundleBatch, BundleManifest, BundlePhoto, BundleTag, BundleTagTerm, BundleVersion,
    MANIFEST_FILENAME, ORIGINALS_DIR, PREVIEWS_DIR,
};
use crate::catalog::{Catalog, IptcFields, PickState};
use crate::xmp::sidecar_path;

// ---------------------------------------------------------------------------
// Result type used by the writer
// ---------------------------------------------------------------------------

/// Summary returned by the `export_bundle` command after a successful write.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleWriteResult {
    /// Number of photo originals successfully added to the zip.
    pub exported: usize,
    /// Number of photos whose original could not be resolved (offline NAS / missing).
    /// These photos are still included in the manifest (metadata travels), but no bytes
    /// land under `originals/`.
    pub skipped_offline: usize,
    /// Number of photos that encountered a non-fatal error during the write.
    pub errors: usize,
}

// ---------------------------------------------------------------------------
// Data gathered under the catalog lock
// ---------------------------------------------------------------------------

/// One photo's worth of data gathered from the catalog — everything needed to
/// build the manifest entry and copy the bytes.
struct GatheredPhoto {
    uuid: String,
    /// Catalog-root-relative logical path (also the archive path under `originals/`).
    relative_path: String,
    rating: i64,
    label: String,
    pick_state: PickState,
    iptc: IptcFields,
    edit_record: Option<String>,
    versions: Vec<(String, String, i64)>, // (name, edit_json, position)
    /// Tag UUIDs assigned to this photo.
    tag_uuids: Vec<String>,
    /// The resolved original path (None = offline or missing).
    original_path: Option<PathBuf>,
}

/// Everything gathered from the catalog for one import batch export. Cheap to clone /
/// send across threads because the heavy work (file I/O) runs after this is assembled.
pub struct GatheredBundle {
    pub manifest: BundleManifest,
    /// Per-photo original paths (keyed by uuid), assembled off the catalog lock.
    /// `None` = offline. The photo entry is always in the manifest; bytes are only
    /// added to the zip when `Some`.
    pub originals: HashMap<String, Option<PathBuf>>,
}

/// Gather all catalog data for `batch_id` under the catalog lock.
///
/// Unreachable originals are counted and included in the manifest without bytes (the
/// metadata still merges on import). Returns `None` if the batch does not exist.
pub fn gather_bundle(catalog: &Catalog, batch_id: i64) -> crate::catalog::Result<Option<GatheredBundle>> {
    // --- 1. Batch record -------------------------------------------------
    let batch_row = {
        let mut stmt = catalog.conn().prepare(
            "SELECT id, uuid, source_label, note, created_at FROM import_batches WHERE id = ?1",
        )?;
        let row = stmt
            .query_row(params![batch_id], |r| {
                Ok((
                    r.get::<_, String>(1)?,   // uuid
                    r.get::<_, String>(2)?,   // source_label
                    r.get::<_, String>(3)?,   // note
                    r.get::<_, i64>(4)?,      // created_at
                ))
            })
            .optional()?;
        match row {
            Some(r) => r,
            None => return Ok(None),
        }
    };
    let (batch_uuid, batch_source_label, batch_note, batch_created_at) = batch_row;

    // --- 2. Photo IDs in the batch (non-missing) -------------------------
    let photo_ids: Vec<i64> = {
        let mut stmt = catalog.conn().prepare(
            "SELECT id FROM photos WHERE import_batch_id = ?1 AND missing = 0 ORDER BY id",
        )?;
        let ids = stmt.query_map(params![batch_id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids
    };

    // --- 3. Gather per-photo data ----------------------------------------
    // Collect all tag UUIDs referenced across all photos (to build the taxonomy slice).
    let mut referenced_tag_ids: Vec<i64> = Vec::new();
    let mut gathered_photos: Vec<GatheredPhoto> = Vec::new();

    for &photo_id in &photo_ids {
        // Basic fields
        let photo = catalog.get_photo(photo_id)?;
        let iptc = catalog.get_iptc(photo_id).unwrap_or_default();
        let edit_record = catalog.get_edit_record(photo_id)?;

        // Versions ordered by position
        let versions: Vec<(String, String, i64)> = catalog
            .list_versions(photo_id)?
            .into_iter()
            .map(|v| (v.name, v.edit_json, v.position))
            .collect();

        // Tag assignments (UUIDs)
        let tags = catalog.get_photo_tags(photo_id)?;
        let tag_uuids: Vec<String> = tags.iter().map(|t| t.uuid.clone()).collect();
        for t in &tags {
            referenced_tag_ids.push(t.id);
        }

        // Resolve the original path via the resolver (never build from photos.path directly)
        let original_path = match catalog.resolve_photo_path(photo_id) {
            Ok(Some(p)) => Some(p),
            Ok(None) => None,
            Err(_) => None,
        };

        gathered_photos.push(GatheredPhoto {
            uuid: photo.uuid,
            relative_path: photo.path,
            rating: photo.rating,
            label: photo.label,
            pick_state: photo.pick_state,
            iptc,
            edit_record,
            versions,
            tag_uuids,
            original_path,
        });
    }

    // --- 4. Taxonomy slice (all referenced tags + ancestors) ---------------
    // Collect the full ancestor chain for every referenced tag so the importer can
    // build the path without gaps.
    let all_tag_ids = collect_with_ancestors(catalog, &referenced_tag_ids)?;

    let mut taxonomy: Vec<BundleTag> = Vec::new();
    for tid in all_tag_ids {
        let tag = catalog.get_tag(tid)?;
        let terms = catalog.list_terms(tid)?;
        let bundle_terms: Vec<BundleTagTerm> = terms
            .into_iter()
            .map(|t| BundleTagTerm {
                text: t.text,
                language: t.language,
                is_primary: t.is_primary,
                export: t.export,
            })
            .collect();

        // Fetch the exportable flag directly (not on the Tag model exposed to frontend).
        let exportable: bool = catalog
            .conn()
            .query_row(
                "SELECT exportable FROM tags WHERE id = ?1",
                params![tid],
                |r| r.get::<_, i64>(0),
            )
            .map(|v| v != 0)
            .unwrap_or(true);

        taxonomy.push(BundleTag {
            uuid: tag.uuid,
            full_path: tag.full_path,
            exportable,
            terms: bundle_terms,
        });
    }

    // --- 5. Build the manifest ------------------------------------------
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let mut manifest = BundleManifest::new(
        BundleBatch {
            uuid: batch_uuid,
            source_label: batch_source_label,
            note: batch_note,
            created_at: batch_created_at,
        },
        now,
    );

    let mut originals: HashMap<String, Option<PathBuf>> = HashMap::new();

    for gp in gathered_photos {
        let uuid = gp.uuid.clone();
        let original_path = gp.original_path.clone();

        manifest.photos.push(BundlePhoto {
            uuid: gp.uuid.clone(),
            relative_path: gp.relative_path,
            rating: gp.rating,
            label: gp.label,
            pick_state: gp.pick_state,
            iptc: gp.iptc,
            edit_record: gp.edit_record,
            versions: gp
                .versions
                .into_iter()
                .map(|(name, edit_json, position)| BundleVersion {
                    name,
                    edit_json,
                    position,
                })
                .collect(),
            tag_uuids: gp.tag_uuids,
        });

        originals.insert(uuid, original_path);
    }

    manifest.taxonomy = taxonomy;

    Ok(Some(GatheredBundle { manifest, originals }))
}

/// Walk tag IDs upward (parent_id) to collect the full ancestor set, deduped.
fn collect_with_ancestors(
    catalog: &Catalog,
    ids: &[i64],
) -> crate::catalog::Result<Vec<i64>> {
    let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
    let mut result: Vec<i64> = Vec::new();
    let mut queue: Vec<i64> = ids.to_vec();
    while let Some(id) = queue.pop() {
        if !seen.insert(id) {
            continue;
        }
        result.push(id);
        // Find parent
        if let Some(parent_id) = catalog
            .conn()
            .query_row(
                "SELECT parent_id FROM tags WHERE id = ?1",
                params![id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .ok()
            .flatten()
        {
            queue.push(parent_id);
        }
    }
    // Stable order (root → leaf) for the manifest
    result.sort_unstable();
    Ok(result)
}

/// Optional extension for the row_to_tag helper (needed by `get_tag`)
trait OptionalExtension<T>: Sized {
    fn optional(self) -> rusqlite::Result<Option<T>>;
}

impl<T> OptionalExtension<T> for rusqlite::Result<T> {
    fn optional(self) -> rusqlite::Result<Option<T>> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Zip writer — pure filesystem work, run off the catalog lock
// ---------------------------------------------------------------------------

/// Write a gathered bundle to `dest_path` as a zip archive. Streams progress via
/// `on_progress(done, total)` — `total` includes one slot per photo for the original
/// and one for the preview (previews are best-effort; failures don't count as errors).
///
/// Unreachable originals (offline NAS, missing file) are counted as `skipped_offline`;
/// the manifest still records their metadata. The zip file is written atomically via a
/// temp-file-then-rename pattern.
pub fn write_bundle(
    bundle: &GatheredBundle,
    dest_path: &Path,
    on_progress: impl Fn(usize, usize),
) -> Result<BundleWriteResult, String> {
    let total_photos = bundle.manifest.photos.len();
    // 2 steps per photo: original + preview
    let total_steps = total_photos * 2;

    if let Some(parent) = dest_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create dest dir: {e}"))?;
    }

    // Write to a temp file first; rename to dest on success (atomic-ish).
    let tmp_path = dest_path.with_extension("chairphoto.tmp");
    let result = write_bundle_to_file(bundle, &tmp_path, total_steps, &on_progress);

    match result {
        Ok(r) => {
            std::fs::rename(&tmp_path, dest_path)
                .map_err(|e| format!("rename bundle into place: {e}"))?;
            Ok(r)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_path);
            Err(e)
        }
    }
}

fn write_bundle_to_file(
    bundle: &GatheredBundle,
    tmp_path: &Path,
    total_steps: usize,
    on_progress: &impl Fn(usize, usize),
) -> Result<BundleWriteResult, String> {
    let file = std::fs::File::create(tmp_path)
        .map_err(|e| format!("create bundle file: {e}"))?;

    let mut zip = ZipWriter::new(file);

    // Stored (no compression) for originals — they're already compressed (RAW/JPEG).
    // Small entries (manifest, previews) use deflate for convenience; previews are JPEG
    // so deflate saves little, but the overhead is negligible.
    let stored: FileOptions<()> = FileOptions::default()
        .compression_method(CompressionMethod::Stored);
    let deflated: FileOptions<()> = FileOptions::default()
        .compression_method(CompressionMethod::Deflated);

    // --- manifest.json ---------------------------------------------------
    let manifest_json = bundle.manifest.to_json()
        .map_err(|e| format!("serialize manifest: {e}"))?;
    zip.start_file(MANIFEST_FILENAME, deflated)
        .map_err(|e| format!("zip: start manifest: {e}"))?;
    zip.write_all(manifest_json.as_bytes())
        .map_err(|e| format!("zip: write manifest: {e}"))?;

    // --- originals + previews -------------------------------------------
    let mut result = BundleWriteResult {
        exported: 0,
        skipped_offline: 0,
        errors: 0,
    };
    let mut done = 0usize;

    for bp in &bundle.manifest.photos {
        let original_path = bundle.originals.get(&bp.uuid).and_then(|o| o.as_ref());

        // Step 1: Original (and its sidecar)
        done += 1;
        on_progress(done, total_steps);

        match original_path {
            None => {
                result.skipped_offline += 1;
                eprintln!(
                    "bundle: original offline for photo {} ({}), metadata-only",
                    bp.uuid, bp.relative_path
                );
            }
            Some(orig_path) => {
                // The archive path mirrors the catalog-relative logical path.
                let arc_path = format!("{}/{}", ORIGINALS_DIR, bp.relative_path);
                match copy_file_to_zip(&mut zip, orig_path, &arc_path, stored) {
                    Ok(()) => {
                        result.exported += 1;
                        // Sidecar (best-effort, beside the original)
                        let sidecar = sidecar_path(orig_path);
                        if sidecar.is_file() {
                            let sidecar_arc = format!("{}.xmp", arc_path);
                            if let Err(e) = copy_file_to_zip(&mut zip, &sidecar, &sidecar_arc, stored) {
                                eprintln!(
                                    "bundle: sidecar copy failed for {} (non-fatal): {e}",
                                    bp.uuid
                                );
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("bundle: original copy failed for {}: {e}", bp.uuid);
                        result.errors += 1;
                    }
                }
            }
        }

        // Step 2: Preview (best-effort — a missing preview is not an error)
        done += 1;
        on_progress(done, total_steps);

        if let Some(orig_path) = original_path {
            match crate::thumbnails::preview_bytes(orig_path) {
                Ok(bytes) => {
                    let arc_path = format!("{}/{}.jpg", PREVIEWS_DIR, bp.uuid);
                    if let Err(e) = write_bytes_to_zip(&mut zip, &bytes, &arc_path, stored) {
                        eprintln!(
                            "bundle: preview write failed for {} (non-fatal): {e}",
                            bp.uuid
                        );
                    }
                }
                Err(e) => {
                    eprintln!(
                        "bundle: preview unavailable for {} (non-fatal): {e}",
                        bp.uuid
                    );
                }
            }
        }
    }

    zip.finish().map_err(|e| format!("zip finish: {e}"))?;
    Ok(result)
}

/// Copy a file from disk into the zip at `arc_path`.
fn copy_file_to_zip<W: std::io::Write + std::io::Seek>(
    zip: &mut ZipWriter<W>,
    src: &Path,
    arc_path: &str,
    options: FileOptions<()>,
) -> Result<(), String> {
    let bytes = std::fs::read(src)
        .map_err(|e| format!("read {}: {e}", src.display()))?;
    write_bytes_to_zip(zip, &bytes, arc_path, options)
}

/// Write bytes into the zip at `arc_path`.
fn write_bytes_to_zip<W: std::io::Write + std::io::Seek>(
    zip: &mut ZipWriter<W>,
    bytes: &[u8],
    arc_path: &str,
    options: FileOptions<()>,
) -> Result<(), String> {
    zip.start_file(arc_path, options)
        .map_err(|e| format!("zip: start {arc_path}: {e}"))?;
    zip.write_all(bytes)
        .map_err(|e| format!("zip: write {arc_path}: {e}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::{BundleManifest, MANIFEST_FILENAME};
    use std::io::Read;
    use zip::ZipArchive;

    /// Return a temp bundle-file path that will be cleaned up on drop.
    fn tmp_bundle(name: &str) -> crate::test_support::TestSubPath {
        crate::test_support::TestTmpDir::new(&format!("bundle-{name}"))
            .into_subpath("test.chairphoto")
    }

    /// Build a minimal manifest + empty originals map and write it to a zip; verify
    /// the zip is readable and the manifest round-trips correctly.
    #[test]
    fn write_bundle_produces_valid_zip_with_manifest() {
        let manifest = BundleManifest::new(
            crate::bundle::BundleBatch {
                uuid: "batch-test-1".into(),
                source_label: "Test trip".into(),
                note: String::new(),
                created_at: 1_700_000_000,
            },
            1_700_100_000,
        );
        let bundle = GatheredBundle {
            manifest,
            originals: HashMap::new(),
        };

        let dest = tmp_bundle("v1");
        write_bundle(&bundle, &dest, |_, _| {}).expect("write_bundle");

        // Verify the zip is readable and contains manifest.json
        let file = std::fs::File::open(&dest).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();

        let mut mf = archive.by_name(MANIFEST_FILENAME).unwrap();
        let mut json = String::new();
        mf.read_to_string(&mut json).unwrap();

        let back = BundleManifest::from_json(&json).expect("manifest round-trip");
        assert_eq!(back.batch.uuid, "batch-test-1");
        assert_eq!(back.batch.source_label, "Test trip");
        assert_eq!(back.format_version, crate::bundle::BUNDLE_FORMAT_VERSION);
    }

    /// Empty bundle (no photos): the zip only has manifest.json; result has all zeros.
    #[test]
    fn empty_bundle_has_only_manifest() {
        let manifest = BundleManifest::new(
            crate::bundle::BundleBatch {
                uuid: "b2".into(),
                source_label: String::new(),
                note: String::new(),
                created_at: 0,
            },
            0,
        );
        let bundle = GatheredBundle {
            manifest,
            originals: HashMap::new(),
        };

        let dest = tmp_bundle("v2");
        let result = write_bundle(&bundle, &dest, |_, _| {}).unwrap();

        assert_eq!(result.exported, 0);
        assert_eq!(result.skipped_offline, 0);
        assert_eq!(result.errors, 0);

        let file = std::fs::File::open(&dest).unwrap();
        let archive = ZipArchive::new(file).unwrap();
        assert_eq!(archive.len(), 1, "only manifest.json expected");
    }

    /// A photo whose original is None (offline) is counted as skipped, not an error.
    #[test]
    fn offline_original_counted_as_skipped_not_error() {
        let mut manifest = BundleManifest::new(
            crate::bundle::BundleBatch {
                uuid: "b3".into(),
                source_label: String::new(),
                note: String::new(),
                created_at: 0,
            },
            0,
        );
        manifest.photos.push(crate::bundle::BundlePhoto {
            uuid: "photo-offline".into(),
            relative_path: "2026/01/01/IMG001.ARW".into(),
            rating: 3,
            label: String::new(),
            pick_state: crate::catalog::PickState::None,
            iptc: crate::catalog::IptcFields::default(),
            edit_record: None,
            versions: Vec::new(),
            tag_uuids: Vec::new(),
        });

        let mut originals = HashMap::new();
        originals.insert("photo-offline".to_string(), None);

        let bundle = GatheredBundle { manifest, originals };

        let dest = tmp_bundle("v3");
        let result = write_bundle(&bundle, &dest, |_, _| {}).unwrap();

        assert_eq!(result.skipped_offline, 1, "offline should be skipped");
        assert_eq!(result.errors, 0, "offline is not an error");
        assert_eq!(result.exported, 0);

        // Manifest must still have the photo entry
        let file = std::fs::File::open(&dest).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        let mut mf = archive.by_name(MANIFEST_FILENAME).unwrap();
        let mut json = String::new();
        mf.read_to_string(&mut json).unwrap();
        let back = BundleManifest::from_json(&json).unwrap();
        assert_eq!(back.photos.len(), 1);
        assert_eq!(back.photos[0].uuid, "photo-offline");
    }

    /// Progress callback fires as expected: done goes from 0 → total_photos * 2.
    #[test]
    fn progress_called_with_increasing_done() {
        let mut manifest = BundleManifest::new(
            crate::bundle::BundleBatch {
                uuid: "b4".into(),
                source_label: String::new(),
                note: String::new(),
                created_at: 0,
            },
            0,
        );
        // Add two offline photos so we don't need real files.
        for i in 0..2u32 {
            let uuid = format!("p{i}");
            manifest.photos.push(crate::bundle::BundlePhoto {
                uuid: uuid.clone(),
                relative_path: format!("2026/{i:04}.ARW"),
                rating: 0,
                label: String::new(),
                pick_state: crate::catalog::PickState::None,
                iptc: crate::catalog::IptcFields::default(),
                edit_record: None,
                versions: Vec::new(),
                tag_uuids: Vec::new(),
            });
            // No originals map entry → treated as offline.
        }

        let bundle = GatheredBundle {
            manifest,
            originals: HashMap::new(),
        };

        let dest = tmp_bundle("v4");

        let calls = std::sync::Mutex::new(Vec::<(usize, usize)>::new());
        write_bundle(&bundle, &dest, |done, total| {
            calls.lock().unwrap().push((done, total));
        })
        .unwrap();

        let calls = calls.into_inner().unwrap();
        // 2 photos × 2 steps = 4 calls; total = 4 throughout; done increases.
        assert_eq!(calls.len(), 4);
        assert_eq!(calls.last().unwrap(), &(4, 4));
        for (i, (done, _total)) in calls.iter().enumerate() {
            assert_eq!(*done, i + 1, "done should increment each step");
        }
    }

    /// Manifest with photos + taxonomy round-trips through the zip writer.
    #[test]
    fn manifest_photos_and_taxonomy_round_trip() {
        let mut manifest = BundleManifest::new(
            crate::bundle::BundleBatch {
                uuid: "b5".into(),
                source_label: "test".into(),
                note: "note".into(),
                created_at: 1000,
            },
            2000,
        );
        manifest.photos.push(crate::bundle::BundlePhoto {
            uuid: "p-rt".into(),
            relative_path: "a/b.ARW".into(),
            rating: 5,
            label: "green".into(),
            pick_state: crate::catalog::PickState::Pick,
            iptc: crate::catalog::IptcFields::default(),
            edit_record: Some(r#"{"basic-editor":{"exposure":1.0}}"#.into()),
            versions: vec![crate::bundle::BundleVersion {
                name: "crop".into(),
                edit_json: "{}".into(),
                position: 0,
            }],
            tag_uuids: vec!["t-rt".into()],
        });
        manifest.taxonomy.push(crate::bundle::BundleTag {
            uuid: "t-rt".into(),
            full_path: "Animals/Birds".into(),
            exportable: true,
            terms: vec![crate::bundle::BundleTagTerm {
                text: "Fugler".into(),
                language: Some("nb".into()),
                is_primary: true,
                export: true,
            }],
        });

        let mut originals = HashMap::new();
        originals.insert("p-rt".to_string(), None); // offline

        let bundle = GatheredBundle { manifest: manifest.clone(), originals };

        let dest = tmp_bundle("v5");
        write_bundle(&bundle, &dest, |_, _| {}).unwrap();

        let file = std::fs::File::open(&dest).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        let mut mf = archive.by_name(MANIFEST_FILENAME).unwrap();
        let mut json = String::new();
        mf.read_to_string(&mut json).unwrap();
        let back = BundleManifest::from_json(&json).unwrap();

        assert_eq!(back, manifest);
    }

    /// A real file on disk gets copied into originals/ in the zip.
    #[test]
    fn real_file_is_copied_to_originals() {
        // Create a tiny fake "original" file.
        let src = crate::test_support::TestTmpDir::new("orig").into_subpath("orig.jpg");
        std::fs::write(&src, b"FAKE JPEG BYTES").unwrap();

        let mut manifest = BundleManifest::new(
            crate::bundle::BundleBatch {
                uuid: "b6".into(),
                source_label: String::new(),
                note: String::new(),
                created_at: 0,
            },
            0,
        );
        manifest.photos.push(crate::bundle::BundlePhoto {
            uuid: "p-real".into(),
            relative_path: "2026/06/01/img.jpg".into(),
            rating: 0,
            label: String::new(),
            pick_state: crate::catalog::PickState::None,
            iptc: crate::catalog::IptcFields::default(),
            edit_record: None,
            versions: Vec::new(),
            tag_uuids: Vec::new(),
        });

        let mut originals = HashMap::new();
        originals.insert("p-real".to_string(), Some(src.to_path_buf()));

        let bundle = GatheredBundle { manifest, originals };

        let dest = tmp_bundle("v6");
        let result = write_bundle(&bundle, &dest, |_, _| {}).unwrap();

        // The preview will fail (not a real JPEG), but the original should copy.
        assert_eq!(result.exported, 1, "original should be exported");
        assert_eq!(result.skipped_offline, 0);
        // errors may be 0 (preview failure is non-fatal and not counted)
        assert_eq!(result.errors, 0);

        // The original must appear under originals/ in the zip.
        let file = std::fs::File::open(&dest).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        let arc_path = format!("{}/2026/06/01/img.jpg", ORIGINALS_DIR);
        let mut entry = archive.by_name(&arc_path).unwrap();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"FAKE JPEG BYTES");
    }
}
